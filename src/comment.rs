//! Комментарии к узлам: хронологическая нить с мягким удалением (README §5).

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::activity::{self, Entry};
use crate::db::now_iso;
use crate::error::{Error, Result};
use crate::node;
use crate::project::Project;

/// Комментарий в выдаче.
#[derive(Debug, Clone, Serialize)]
pub struct Comment {
    pub id: i64,
    pub dot_id: String,
    pub author: Option<String>,
    pub content: String,
    pub is_deleted: bool,
    pub deleted_at: Option<String>,
    pub deleted_by: Option<String>,
    pub delete_reason: Option<String>,
    pub created_at: String,
}

const SELECT: &str = "SELECT c.id, c.author, c.content, c.is_deleted, c.deleted_at,
        c.deleted_by, c.delete_reason, c.created_at, n.path
   FROM comments c JOIN nodes n ON n.id = c.node_id";

fn map_row(row: &rusqlite::Row<'_>, project: &Project) -> rusqlite::Result<Comment> {
    let path: String = row.get(8)?;
    Ok(Comment {
        id: row.get(0)?,
        dot_id: project.dot_id(&path),
        author: row.get(1)?,
        content: row.get(2)?,
        is_deleted: row.get::<_, i64>(3)? != 0,
        deleted_at: row.get(4)?,
        deleted_by: row.get(5)?,
        delete_reason: row.get(6)?,
        created_at: row.get(7)?,
    })
}

/// Добавляет комментарий к узлу.
pub fn add(
    conn: &mut Connection,
    project: &Project,
    reference: &str,
    content: &str,
    author: &str,
) -> Result<Comment> {
    if content.trim().is_empty() {
        return Err(Error::InvalidValue {
            field: "content".to_string(),
            value: "empty".to_string(),
        });
    }
    let node = node::resolve(conn, project, reference)?;
    let now = now_iso();

    let tx = crate::db::write_tx(conn)?;
    tx.execute(
        "INSERT INTO comments (node_id, author, content, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![node.id, author, content, now],
    )?;
    let id = tx.last_insert_rowid();
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id: Some(node.id),
            entity_type: "comment",
            entity_id: id,
            action: "create",
            field_name: None,
            old_value: None,
            new_value: None,
            summary: Some(node.dot_id.as_str()),
            actor: author,
        },
    )?;
    tx.commit()?;
    by_id(conn, project, id)
}

/// Комментарий по идентификатору.
pub fn by_id(conn: &Connection, project: &Project, id: i64) -> Result<Comment> {
    conn.query_row(
        &format!("{SELECT} WHERE c.id = ?1 AND n.project_id = ?2"),
        params![id, project.id],
        |row| map_row(row, project),
    )
    .optional()?
    .ok_or_else(|| Error::InvalidValue {
        field: "comment id".to_string(),
        value: format!("{id} not found"),
    })
}

/// Комментарии узла в хронологическом порядке.
///
/// Удалённые скрыты, пока не запрошены явно (`include_deleted`).
pub fn list(
    conn: &Connection,
    project: &Project,
    reference: &str,
    include_deleted: bool,
) -> Result<Vec<Comment>> {
    let node = node::resolve(conn, project, reference)?;
    let mut sql = format!("{SELECT} WHERE c.node_id = ?1");
    if !include_deleted {
        sql.push_str(" AND c.is_deleted = 0");
    }
    sql.push_str(" ORDER BY c.created_at, c.id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![node.id], |row| map_row(row, project))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Мягкое удаление: строка остаётся ради аудита, но скрывается из выдач.
pub fn delete(
    conn: &mut Connection,
    project: &Project,
    id: i64,
    actor: &str,
    reason: Option<&str>,
) -> Result<Comment> {
    let comment = by_id(conn, project, id)?;
    if comment.is_deleted {
        return Err(Error::AlreadyInState {
            dot_id: comment.dot_id,
            state: "deleted".to_string(),
        });
    }
    let now = now_iso();
    let node_id = comment_node(conn, id)?;
    let tx = crate::db::write_tx(conn)?;
    tx.execute(
        "UPDATE comments SET is_deleted = 1, deleted_at = ?1, deleted_by = ?2, delete_reason = ?3
         WHERE id = ?4",
        params![now, actor, reason, id],
    )?;
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id,
            entity_type: "comment",
            entity_id: id,
            action: "delete",
            field_name: Some("is_deleted"),
            old_value: Some("0"),
            new_value: Some("1"),
            summary: Some(comment.dot_id.as_str()),
            actor,
        },
    )?;
    tx.commit()?;
    by_id(conn, project, id)
}

/// Возвращает мягко удалённый комментарий.
pub fn restore(conn: &mut Connection, project: &Project, id: i64, actor: &str) -> Result<Comment> {
    let comment = by_id(conn, project, id)?;
    if !comment.is_deleted {
        return Err(Error::AlreadyInState {
            dot_id: comment.dot_id,
            state: "present".to_string(),
        });
    }
    let node_id = comment_node(conn, id)?;
    let tx = crate::db::write_tx(conn)?;
    tx.execute(
        "UPDATE comments SET is_deleted = 0, deleted_at = NULL, deleted_by = NULL,
                             delete_reason = NULL
         WHERE id = ?1",
        params![id],
    )?;
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id,
            entity_type: "comment",
            entity_id: id,
            action: "restore",
            field_name: Some("is_deleted"),
            old_value: Some("1"),
            new_value: Some("0"),
            summary: Some(comment.dot_id.as_str()),
            actor,
        },
    )?;
    tx.commit()?;
    by_id(conn, project, id)
}

fn comment_node(conn: &Connection, id: i64) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT node_id FROM comments WHERE id = ?1",
            params![id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?)
}
