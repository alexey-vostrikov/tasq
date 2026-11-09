//! Заметки: решения, контекст, встречи, блокеры, прогресс, релизы (README §5).
//!
//! Заметка привязана к проекту и, необязательно, к узлу. Используется как долговременная
//! память между сессиями агентов: то, что не помещается в комментарий к задаче.

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};

use crate::activity::{self, Entry};
use crate::db::now_iso;
use crate::error::{Error, Result};
use crate::node;
use crate::project::Project;

/// Восемь типов заметок — те же значения, что в saga.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteType {
    General,
    Decision,
    Context,
    Meeting,
    Technical,
    Blocker,
    Progress,
    Release,
}

impl NoteType {
    pub fn as_str(self) -> &'static str {
        match self {
            NoteType::General => "general",
            NoteType::Decision => "decision",
            NoteType::Context => "context",
            NoteType::Meeting => "meeting",
            NoteType::Technical => "technical",
            NoteType::Blocker => "blocker",
            NoteType::Progress => "progress",
            NoteType::Release => "release",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "general" => NoteType::General,
            "decision" => NoteType::Decision,
            "context" => NoteType::Context,
            "meeting" => NoteType::Meeting,
            "technical" => NoteType::Technical,
            "blocker" => NoteType::Blocker,
            "progress" => NoteType::Progress,
            "release" => NoteType::Release,
            _ => return None,
        })
    }
}

/// Заметка в выдаче.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: i64,
    pub dot_id: Option<String>,
    pub node_id: Option<i64>,
    pub title: String,
    pub content: String,
    pub note_type: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Заявка на сохранение заметки: без `id` создаётся новая, с `id` — обновляется.
#[derive(Default)]
pub struct SaveNote<'a> {
    pub id: Option<i64>,
    pub title: Option<&'a str>,
    pub content: Option<&'a str>,
    pub note_type: Option<&'a str>,
    pub node: Option<&'a str>,
    pub actor: Option<&'a str>,
}

const NOTE_SELECT: &str = "SELECT n.id, n.node_id, n.title, n.content, n.note_type,
        n.created_at, n.updated_at, x.path
   FROM notes n
   LEFT JOIN nodes x ON x.id = n.node_id";

fn map_note(row: &rusqlite::Row<'_>, project: &Project) -> rusqlite::Result<Note> {
    let path: Option<String> = row.get(7)?;
    Ok(Note {
        id: row.get(0)?,
        dot_id: path.map(|p| project.dot_id(&p)),
        node_id: row.get(1)?,
        title: row.get(2)?,
        content: row.get(3)?,
        note_type: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

/// Сохраняет заметку (создание или обновление по `id`).
pub fn save(conn: &mut Connection, project: &Project, req: SaveNote<'_>) -> Result<Note> {
    let note_type = req.note_type.unwrap_or("general");
    if NoteType::parse(note_type).is_none() {
        return Err(Error::InvalidValue {
            field: "note_type".to_string(),
            value: note_type.to_string(),
        });
    }
    let node_id = match req.node {
        Some(reference) => Some(node::resolve(conn, project, reference)?.id),
        None => None,
    };
    let now = now_iso();
    let actor = req.actor.unwrap_or("-");

    if let Some(id) = req.id {
        let existing: Option<(String, String, String)> = conn
            .query_row(
                "SELECT title, content, note_type FROM notes WHERE id = ?1 AND project_id = ?2",
                params![id, project.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((old_title, old_content, old_type)) = existing else {
            return Err(Error::InvalidValue {
                field: "note id".to_string(),
                value: format!("{id} not found"),
            });
        };
        let title = req.title.unwrap_or(&old_title);
        let content = req.content.unwrap_or(&old_content);
        let note_type = req.note_type.unwrap_or(&old_type);

        let tx = crate::db::write_tx(conn)?;
        tx.execute(
            "UPDATE notes SET title = ?1, content = ?2, note_type = ?3, node_id = COALESCE(?4, node_id),
                             updated_at = ?5
             WHERE id = ?6",
            params![title, content, note_type, node_id, now, id],
        )?;
        if old_title != title {
            // Заметка может быть не привязана к узлу: в логе node_id тогда NULL.
            // Подставлять 0 нельзя — это нарушит внешний ключ activity_log.node_id.
            activity::log(
                &tx,
                &Entry {
                    project_id: project.id,
                    node_id,
                    entity_type: "note",
                    entity_id: id,
                    action: "update",
                    field_name: Some("title"),
                    old_value: Some(&old_title),
                    new_value: Some(title),
                    summary: None,
                    actor,
                },
            )?;
        }
        activity::log(
            &tx,
            &Entry {
                project_id: project.id,
                node_id,
                entity_type: "note",
                entity_id: id,
                action: "update",
                field_name: Some("content"),
                old_value: None,
                new_value: None,
                summary: Some(title),
                actor,
            },
        )?;
        tx.commit()?;
        return by_id(conn, project, id);
    }

    let Some(title) = req.title else {
        return Err(Error::InvalidValue {
            field: "title".to_string(),
            value: "required when creating a note".to_string(),
        });
    };
    let content = req.content.unwrap_or("");

    let tx = crate::db::write_tx(conn)?;
    tx.execute(
        "INSERT INTO notes (project_id, node_id, title, content, note_type, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![project.id, node_id, title, content, note_type, now],
    )?;
    let id = tx.last_insert_rowid();
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id,
            entity_type: "note",
            entity_id: id,
            action: "create",
            field_name: Some("note_type"),
            old_value: None,
            new_value: Some(note_type),
            summary: Some(title),
            actor,
        },
    )?;
    tx.commit()?;
    by_id(conn, project, id)
}

/// Заметка по идентификатору.
pub fn by_id(conn: &Connection, project: &Project, id: i64) -> Result<Note> {
    conn.query_row(
        &format!("{NOTE_SELECT} WHERE n.id = ?1 AND n.project_id = ?2"),
        params![id, project.id],
        |row| map_note(row, project),
    )
    .optional()?
    .ok_or_else(|| Error::InvalidValue {
        field: "note id".to_string(),
        value: format!("{id} not found"),
    })
}

/// Фильтр для списка заметок.
#[derive(Default)]
pub struct Filter<'a> {
    pub note_type: Option<&'a str>,
    pub node: Option<&'a str>,
    pub limit: Option<i64>,
}

/// Заметки проекта, свежие первыми.
pub fn list(conn: &Connection, project: &Project, filter: &Filter<'_>) -> Result<Vec<Note>> {
    let mut sql = format!("{NOTE_SELECT} WHERE n.project_id = ?");
    let mut values: Vec<Value> = vec![Value::Integer(project.id)];
    if let Some(note_type) = filter.note_type {
        if NoteType::parse(note_type).is_none() {
            return Err(Error::InvalidValue {
                field: "note_type".to_string(),
                value: note_type.to_string(),
            });
        }
        sql.push_str(" AND n.note_type = ?");
        values.push(Value::Text(note_type.to_string()));
    }
    if let Some(reference) = filter.node {
        let node = node::resolve(conn, project, reference)?;
        sql.push_str(" AND n.node_id = ?");
        values.push(Value::Integer(node.id));
    }
    sql.push_str(" ORDER BY n.created_at DESC, n.id DESC");
    if let Some(limit) = filter.limit {
        sql.push_str(" LIMIT ?");
        values.push(Value::Integer(limit));
    }

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values.iter()), |row| {
        map_note(row, project)
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Поиск по заголовку и тексту заметок (регистронезависимо).
pub fn search(conn: &Connection, project: &Project, query: &str) -> Result<Vec<Note>> {
    let needle = format!("%{}%", query.trim().to_lowercase());
    let mut stmt = conn.prepare(&format!(
        "{NOTE_SELECT} WHERE n.project_id = ?1
           AND (uni_lower(n.title) LIKE ?2 OR uni_lower(n.content) LIKE ?2)
         ORDER BY n.created_at DESC"
    ))?;
    let rows = stmt.query_map(params![project.id, needle], |row| map_note(row, project))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Удаляет заметку. Возвращает `true`, если запись была.
/// Удаляет заметку.
///
/// Заметки не удаляются мягко: у них нет ни статуса, ни детей, и восстанавливать
/// нечего. Отсутствующая заметка — отказ из `by_id`, поэтому «удалено ли что-то»
/// здесь не возвращается: после успешного поиска строка есть всегда.
pub fn delete(conn: &mut Connection, project: &Project, id: i64, actor: &str) -> Result<()> {
    let note = by_id(conn, project, id)?;
    let tx = crate::db::write_tx(conn)?;
    tx.execute(
        "DELETE FROM notes WHERE id = ?1 AND project_id = ?2",
        params![id, project.id],
    )?;
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id: note.node_id,
            entity_type: "note",
            entity_id: id,
            action: "delete",
            field_name: None,
            old_value: Some(note.title.as_str()),
            new_value: None,
            summary: None,
            actor,
        },
    )?;
    tx.commit()?;
    Ok(())
}
