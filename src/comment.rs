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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{self, NewNode};

    fn db_conn() -> (Connection, Project) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-comment-{nanos}"));
        let outcome = crate::db::init(&dir, "CMT", "comments").expect("init");
        let conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let project = crate::project::first_project(&conn).expect("project");
        (conn, project)
    }

    fn with_task() -> (Connection, Project, String) {
        let (mut conn, project) = db_conn();
        let task = node::create(
            &mut conn,
            &project,
            NewNode {
                parent: None,
                title: "задача",
                kind: None,
                description: None,
                priority: None,
                due_date: None,
                estimated_hours: None,
                weight: None,
                source_ref: None,
                branch: None,
            },
        )
        .expect("node");
        let reference = task.dot_id.clone();
        (conn, project, reference)
    }

    /// Пустой и из пробелов комментарий отвергается: пустая запись в ленте не читается.
    #[test]
    fn an_empty_comment_is_refused() {
        let (mut conn, project, reference) = with_task();
        for content in ["", "   ", "\n\t"] {
            let err = add(&mut conn, &project, &reference, content, "tester")
                .expect_err("пустой комментарий принят");
            assert_eq!(err.code(), "INVALID_VALUE", "другой код отказа: {err}");
            assert!(
                err.to_string().contains("content"),
                "в отказе не назван текст: {err}"
            );
        }
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM comments", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 0, "пустой комментарий записан");
    }

    /// Отсутствующий комментарий называется в отказе своим id.
    #[test]
    fn a_missing_comment_is_reported_by_id() {
        let (conn, project) = db_conn();
        let err = by_id(&conn, &project, 4242).expect_err("отсутствующий комментарий найден");
        assert_eq!(err.code(), "INVALID_VALUE", "другой код отказа: {err}");
        let text = err.to_string();
        assert!(text.contains("4242"), "в отказе нет id: {err}");
        assert!(
            text.contains("not found"),
            "отказ не говорит об отсутствии: {err}"
        );
    }

    /// Восстановление не удалённого комментария — отказ: состояние уже такое.
    #[test]
    fn restoring_a_present_comment_is_refused() {
        let (mut conn, project, reference) = with_task();
        let comment = add(&mut conn, &project, &reference, "текст", "tester").expect("add");
        let err = restore(&mut conn, &project, comment.id, "tester")
            .expect_err("восстановление живого комментария прошло");
        assert_eq!(err.code(), "ALREADY_IN_STATE", "другой код отказа: {err}");
        assert!(
            err.to_string().contains("present"),
            "отказ не называет состояние: {err}"
        );
    }

    /// Повторное удаление — отказ «deleted», а не вторая запись в журнале.
    #[test]
    fn deleting_twice_is_refused() {
        let (mut conn, project, reference) = with_task();
        let comment = add(&mut conn, &project, &reference, "текст", "tester").expect("add");
        delete(&mut conn, &project, comment.id, "tester", Some("дубль")).expect("delete");
        let err = delete(&mut conn, &project, comment.id, "tester", None)
            .expect_err("повторное удаление прошло");
        assert_eq!(err.code(), "ALREADY_IN_STATE", "другой код отказа: {err}");
        assert!(
            err.to_string().contains("deleted"),
            "отказ не называет состояние: {err}"
        );
    }

    /// Удалённый комментарий скрыт из ленты, пока не попросили явно, и возвращается восстановлением.
    #[test]
    fn a_deleted_comment_is_hidden_until_asked_and_comes_back() {
        let (mut conn, project, reference) = with_task();
        let comment = add(&mut conn, &project, &reference, "текст", "tester").expect("add");
        delete(&mut conn, &project, comment.id, "tester", Some("устарел")).expect("delete");

        let visible = list(&conn, &project, &reference, false).expect("list");
        assert!(visible.is_empty(), "удалённый комментарий остался в ленте");
        let all = list(&conn, &project, &reference, true).expect("list all");
        assert_eq!(
            all.len(),
            1,
            "удалённый комментарий пропал из полной выдачи"
        );
        assert!(all[0].is_deleted, "признак удаления не выставлен");
        assert_eq!(
            all[0].delete_reason.as_deref(),
            Some("устарел"),
            "причина удаления не сохранена"
        );

        restore(&mut conn, &project, comment.id, "tester").expect("restore");
        let visible = list(&conn, &project, &reference, false).expect("list");
        assert_eq!(visible.len(), 1, "восстановленный комментарий не вернулся");
    }
}
