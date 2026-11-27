//! Лог активности: каждая мутация пишет строку в той же транзакции (README §5.3, инвариант 8).

use rusqlite::{Connection, params};

use crate::db::now_iso;
use crate::error::Result;

/// Одна запись аудита.
#[derive(Debug, Clone)]
pub struct Entry<'a> {
    pub project_id: i64,
    pub node_id: Option<i64>,
    pub entity_type: &'a str,
    pub entity_id: i64,
    pub action: &'a str,
    pub field_name: Option<&'a str>,
    pub old_value: Option<&'a str>,
    pub new_value: Option<&'a str>,
    pub summary: Option<&'a str>,
    pub actor: &'a str,
}

/// Пишет строку аудита. Вызывается из той же транзакции, что и сама мутация.
pub fn log(conn: &Connection, entry: &Entry<'_>) -> Result<i64> {
    conn.execute(
        "INSERT INTO activity_log (project_id, node_id, entity_type, entity_id, action,
                                   field_name, old_value, new_value, summary, actor, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            entry.project_id,
            entry.node_id,
            entry.entity_type,
            entry.entity_id,
            entry.action,
            entry.field_name,
            entry.old_value,
            entry.new_value,
            entry.summary,
            entry.actor,
            now_iso(),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Номер последней записи журнала: точка отсчёта для «что изменилось с прошлого раза».
///
/// Берётся `MAX(id)`, а не время: `id` растёт строго по порядку и не имеет ни склейки в одну
/// миллисекунду, ни зависимости от часов машины. Ноль означает пустой журнал — это законное
/// состояние свежей базы, а не ошибку.
pub fn watermark(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT COALESCE(MAX(id), 0) FROM activity_log", [], |row| {
            row.get(0)
        })?,
    )
}

/// Узлы проекта, которых коснулись записи журнала после `since`.
///
/// Журнал пишется в той же транзакции, что и мутация, поэтому «что изменилось» не приходится
/// угадывать по `updated_at`: тот не поднимается намеренно при замене ссылок, а в одной
/// миллисекунде помещаются две правки. Узлы других проектов отсеиваются здесь же — иначе
/// правка в соседнем проекте заставила бы перечитывать свой.
pub fn touched_since(conn: &Connection, project_id: i64, since: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT node_id FROM activity_log
         WHERE id > ?1 AND project_id = ?2 AND node_id IS NOT NULL
         ORDER BY node_id",
    )?;
    let rows = stmt.query_map(params![since, project_id], |row| row.get::<_, i64>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Строка лога в выдаче.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Record {
    pub id: i64,
    pub node_id: Option<i64>,
    pub dot_id: Option<String>,
    pub action: String,
    pub field_name: Option<String>,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub summary: Option<String>,
    pub actor: String,
    pub created_at: String,
}

/// Последние записи проекта (свежие первыми).
pub fn recent(conn: &Connection, project_id: i64, limit: i64) -> Result<Vec<Record>> {
    // Источник — объединение с архивом: «последние записи» не должны обрываться на границе
    // выноса, иначе лента выглядит так, будто история началась в день усечения.
    let source = crate::archive::log_source(conn);
    let mut stmt = conn.prepare(&format!(
        "SELECT a.id, a.node_id, n.path, a.action, a.field_name, a.old_value, a.new_value,
                a.summary, a.actor, a.created_at
         FROM ({source}) a
         LEFT JOIN nodes n ON n.id = a.node_id
         WHERE a.project_id = ?1
         ORDER BY a.id DESC
         LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![project_id, limit], |row| {
        Ok(Record {
            id: row.get(0)?,
            node_id: row.get(1)?,
            dot_id: row.get::<_, Option<String>>(2)?,
            action: row.get(3)?,
            field_name: row.get(4)?,
            old_value: row.get(5)?,
            new_value: row.get(6)?,
            summary: row.get(7)?,
            actor: row.get(8)?,
            created_at: row.get(9)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
