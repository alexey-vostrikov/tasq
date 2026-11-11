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

#[cfg(test)]
mod tests {
    use super::*;

    /// Своя база на тест: файл в каталоге процесса, имя уникально по времени.
    fn db_conn() -> (Connection, Project) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-note-{nanos}"));
        let outcome = crate::db::init(&dir, "NOTE", "notes").expect("init");
        let conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let project = crate::project::first_project(&conn).expect("project");
        (conn, project)
    }

    fn note_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
            .expect("count")
    }

    /// Все восемь имён принимаются, остальное — нет. По случаю на каждое имя: `match` из восьми
    /// ветвей иначе проверяется одной и мутация в любой ветви выживает.
    #[test]
    fn every_declared_type_is_accepted_and_nothing_else_is() {
        for (text, expected) in [
            ("general", NoteType::General),
            ("decision", NoteType::Decision),
            ("context", NoteType::Context),
            ("meeting", NoteType::Meeting),
            ("technical", NoteType::Technical),
            ("blocker", NoteType::Blocker),
            ("progress", NoteType::Progress),
            ("release", NoteType::Release),
        ] {
            assert_eq!(
                NoteType::parse(text),
                Some(expected),
                "имя {text} не принято"
            );
        }
        for text in ["", "general ", "Decision", "todo", "unknown"] {
            assert_eq!(
                NoteType::parse(text),
                None,
                "{text:?} принято как тип заметки"
            );
        }
    }

    /// Неизвестный тип отвергается ДО записи: заметки в базе после отказа нет.
    #[test]
    fn an_unknown_type_is_refused_and_nothing_is_written() {
        let (mut conn, project) = db_conn();
        let err = save(
            &mut conn,
            &project,
            SaveNote {
                title: Some("t"),
                note_type: Some("bogus"),
                ..SaveNote::default()
            },
        )
        .expect_err("неизвестный тип принят");
        assert_eq!(err.code(), "INVALID_VALUE", "другой код отказа: {err}");
        assert!(
            err.to_string().contains("note_type"),
            "в отказе не назван тип заметки: {err}"
        );
        assert_eq!(note_count(&conn), 0, "заметка записана вопреки отказу");
    }

    /// Тот же отказ на фильтре списка: искать по несуществующему типу нечего.
    #[test]
    fn an_unknown_type_is_refused_in_the_filter() {
        let (conn, project) = db_conn();
        let filter = Filter {
            note_type: Some("bogus"),
            ..Filter::default()
        };
        let err = list(&conn, &project, &filter).expect_err("фильтр по мусору принят");
        assert_eq!(err.code(), "INVALID_VALUE", "другой код отказа: {err}");
    }

    /// Обновление несуществующей заметки — отказ, а не молчаливое создание.
    #[test]
    fn updating_a_missing_note_is_refused() {
        let (mut conn, project) = db_conn();
        let err = save(
            &mut conn,
            &project,
            SaveNote {
                id: Some(9999),
                title: Some("t"),
                ..SaveNote::default()
            },
        )
        .expect_err("обновление несуществующей заметки прошло");
        assert_eq!(err.code(), "INVALID_VALUE", "другой код отказа: {err}");
        assert_eq!(note_count(&conn), 0, "заметка завелась при отказе");
    }

    /// Смена заголовка видна в журнале: старое значение, новое, поле и автор.
    #[test]
    fn renaming_a_note_leaves_an_activity_row() {
        let (mut conn, project) = db_conn();
        let created = save(
            &mut conn,
            &project,
            SaveNote {
                title: Some("старое имя"),
                actor: Some("tester"),
                ..SaveNote::default()
            },
        )
        .expect("create");
        save(
            &mut conn,
            &project,
            SaveNote {
                id: Some(created.id),
                title: Some("новое имя"),
                actor: Some("tester"),
                ..SaveNote::default()
            },
        )
        .expect("rename");

        let (old, new, field, actor): (String, String, String, String) = conn
            .query_row(
                "SELECT old_value, new_value, field_name, actor FROM activity_log
                  WHERE entity_type = 'note' AND entity_id = ?1 AND action = 'update'
                    AND field_name = 'title'",
                [created.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("строка журнала о смене заголовка");
        assert_eq!(old, "старое имя");
        assert_eq!(new, "новое имя");
        assert_eq!(field, "title");
        assert_eq!(actor, "tester");
    }

    /// Смена типа без смены заголовка заголовок не трогает — и заметка остаётся с новым типом.
    #[test]
    fn a_type_change_keeps_the_title() {
        let (mut conn, project) = db_conn();
        let created = save(
            &mut conn,
            &project,
            SaveNote {
                title: Some("имя"),
                note_type: Some("general"),
                ..SaveNote::default()
            },
        )
        .expect("create");
        let updated = save(
            &mut conn,
            &project,
            SaveNote {
                id: Some(created.id),
                note_type: Some("blocker"),
                ..SaveNote::default()
            },
        )
        .expect("retype");
        assert_eq!(updated.title, "имя", "заголовок затёрт сменой типа");
        assert_eq!(updated.note_type, "blocker", "тип не сменился");
    }
}
