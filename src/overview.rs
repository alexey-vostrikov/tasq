//! Обзор проекта: поиск, цепочка контекста, доска, пакетные правки, учёт времени.
//!
//! Это «одна точка входа» для агента: что происходит, что искать, что делать дальше.

use rusqlite::types::Value;
use rusqlite::{Connection, params, params_from_iter};
use serde::Serialize;

use crate::activity;
use crate::error::{Error, Result};
use crate::node::{self, Filter};
use crate::project::Project;
use crate::status::Status;

/// Найденная сущность.
#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub entity: String,
    pub dot_id: Option<String>,
    pub title: String,
    pub snippet: String,
}

/// Поиск по узлам и заметкам.
///
/// FTS5 понимает префиксы (`migr*`), фразы в кавычках и `OR`. Если запрос не разобрался
/// как выражение FTS (например, в нём `-` или `:`), он трактуется как обычный текст:
/// падение поиска из-за пунктуации пользователя — не ошибка, а повод искать иначе.
pub fn search(conn: &Connection, project: &Project, query: &str) -> Result<Vec<Hit>> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidValue {
            field: "query".to_string(),
            value: "empty".to_string(),
        });
    }

    let fts_query = fts_expression(trimmed);
    let mut hits = Vec::new();

    let nodes: Vec<Hit> = match fts_nodes(conn, project, &fts_query) {
        Ok(rows) => rows,
        Err(_) => like_nodes(conn, project, trimmed)?,
    };
    hits.extend(nodes);

    let notes: Vec<Hit> = match fts_notes(conn, project, &fts_query) {
        Ok(rows) => rows,
        Err(_) => like_notes(conn, project, trimmed)?,
    };
    hits.extend(notes);

    Ok(hits)
}

fn like_notes(conn: &Connection, project: &Project, query: &str) -> Result<Vec<Hit>> {
    let words = query_words(query);
    if words.is_empty() {
        return Ok(Vec::new());
    }
    let mut sql = String::from(
        "SELECT nt.title, nt.content, x.path FROM notes nt
         LEFT JOIN nodes x ON x.id = nt.node_id
         WHERE nt.project_id = ?",
    );
    let mut values: Vec<Value> = vec![Value::Integer(project.id)];
    for word in &words {
        sql.push_str(
            " AND (instr(uni_lower(nt.title), ?) > 0 OR instr(uni_lower(nt.content), ?) > 0)",
        );
        values.push(Value::Text(word.clone()));
        values.push(Value::Text(word.clone()));
    }
    sql.push_str(" ORDER BY nt.created_at DESC LIMIT 50");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values.iter()), |row| {
        let node_path: Option<String> = row.get(2)?;
        let content: String = row.get(1)?;
        Ok(Hit {
            entity: "note".to_string(),
            dot_id: node_path.map(|p| project.dot_id(&p)),
            title: row.get(0)?,
            snippet: first_line(&content),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

/// Слова запроса: пунктуация отбрасывается, регистр не важен.
pub(crate) fn query_words(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|word| {
            word.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect()
}

/// Готовит выражение FTS: несколько слов склеиваются по И, спецсимволы отбрасываются.
fn fts_expression(query: &str) -> String {
    if query.contains('"') || query.contains(" OR ") || query.contains(" AND ") {
        return query.to_string();
    }
    query_words(query)
        .into_iter()
        .map(|word| format!("\"{word}\"*"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn fts_nodes(conn: &Connection, project: &Project, query: &str) -> Result<Vec<Hit>> {
    let mut stmt = conn.prepare(
        "SELECT n.path, n.title, snippet(nodes_fts, -1, '', '', '…', 12)
         FROM nodes_fts f
         JOIN nodes n ON n.id = f.rowid
         WHERE nodes_fts MATCH ?1 AND n.project_id = ?2 AND n.deleted_at IS NULL
         ORDER BY rank
         LIMIT 50",
    )?;
    let rows = stmt.query_map(params![query, project.id], |row| {
        let path: String = row.get(0)?;
        Ok(Hit {
            entity: "node".to_string(),
            dot_id: Some(project.dot_id(&path)),
            title: row.get(1)?,
            snippet: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn fts_notes(conn: &Connection, project: &Project, query: &str) -> Result<Vec<Hit>> {
    let mut stmt = conn.prepare(
        "SELECT nt.title, snippet(notes_fts, -1, '', '', '…', 12), n.node_id, x.path
         FROM notes_fts f
         JOIN notes nt ON nt.id = f.rowid
         LEFT JOIN nodes x ON x.id = nt.node_id
         WHERE notes_fts MATCH ?1 AND nt.project_id = ?2
         ORDER BY rank
         LIMIT 50",
    )?;
    let rows = stmt.query_map(params![query, project.id], |row| {
        let node_path: Option<String> = row.get(3)?;
        Ok(Hit {
            entity: "note".to_string(),
            dot_id: node_path.map(|p| project.dot_id(&p)),
            title: row.get(0)?,
            snippet: row.get(1)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn like_nodes(conn: &Connection, project: &Project, query: &str) -> Result<Vec<Hit>> {
    // Запасной путь для запросов, которые FTS не разобрал (пунктуация, двоеточия):
    // каждое слово ищется отдельно и все слова должны встретиться.
    let words = query_words(query);
    if words.is_empty() {
        return Ok(Vec::new());
    }

    let mut sql = String::from(
        "SELECT path, title, COALESCE(description, '') FROM nodes
         WHERE project_id = ? AND deleted_at IS NULL",
    );
    let mut values: Vec<Value> = vec![Value::Integer(project.id)];
    for word in &words {
        sql.push_str(
            " AND (instr(uni_lower(title), ?) > 0 OR instr(uni_lower(COALESCE(description, '')), ?) > 0)",
        );
        values.push(Value::Text(word.clone()));
        values.push(Value::Text(word.clone()));
    }
    sql.push_str(" ORDER BY path LIMIT 50");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values.iter()), |row| {
        let path: String = row.get(0)?;
        let description: String = row.get(2)?;
        Ok(Hit {
            entity: "node".to_string(),
            dot_id: Some(project.dot_id(&path)),
            title: row.get(1)?,
            snippet: first_line(&description),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Узел в цепочке контекста: от корня вниз.
#[derive(Debug, Clone, Serialize)]
pub struct ContextEntry {
    pub dot_id: String,
    pub kind: String,
    pub status: String,
    pub title: String,
    pub description: Option<String>,
    pub due_date: Option<String>,
    pub lease_holder: Option<String>,
}

/// Цепочка контекста: родители сверху вниз, затем сам узел (README §6).
///
/// Агент получает «зачем» от эпика и «что именно» от своего уровня в одном ответе —
/// ровно то, ради чего заведён dot-notation.
pub fn context(conn: &Connection, project: &Project, reference: &str) -> Result<Vec<ContextEntry>> {
    let node = node::resolve(conn, project, reference)?;
    let mut chain = Vec::new();
    let mut current = Some(node);
    while let Some(item) = current {
        chain.push(ContextEntry {
            dot_id: item.dot_id.clone(),
            kind: item.kind.clone(),
            status: item.effective_status.clone(),
            title: item.title.clone(),
            description: item.description.clone(),
            due_date: item.due_date.clone(),
            lease_holder: item.lease_holder.clone(),
        });
        current = if item.depth > 1 {
            parent_of(conn, project, &item.path)?
        } else {
            None
        };
    }
    chain.reverse();
    Ok(chain)
}

fn parent_of(conn: &Connection, project: &Project, path: &str) -> Result<Option<node::Node>> {
    let parent_path = match path.rsplit_once('.') {
        Some((head, _)) => head,
        None => return Ok(None),
    };
    Ok(node::resolve(conn, project, parent_path).ok())
}

/// Строка доски: узел в разрезе статуса.
#[derive(Debug, Clone, Serialize)]
pub struct BoardRow {
    pub dot_id: String,
    pub title: String,
    pub priority: String,
    pub due_date: Option<String>,
    pub days_overdue: i64,
    pub lease_holder: Option<String>,
    pub progress: String,
}

/// Колонка доски.
#[derive(Debug, Clone, Serialize)]
pub struct BoardColumn {
    pub status: String,
    pub count: usize,
    pub rows: Vec<BoardRow>,
}

/// Доска: узлы по эффективному статусу, с просрочкой и живыми арендами.
#[derive(Debug, Clone, Serialize)]
pub struct Board {
    pub project: String,
    pub columns: Vec<BoardColumn>,
    pub overdue: usize,
    pub leased: usize,
    pub open: usize,
    pub done: usize,
}

pub fn board(conn: &Connection, project: &Project) -> Result<Board> {
    let nodes = node::list(
        conn,
        project,
        &Filter {
            include_archived: false,
            ..Filter::default()
        },
    )?;
    let today = today_days(conn)?;
    let order = [
        "planned",
        "todo",
        "in_progress",
        "blocked",
        "review",
        "deferred",
        "done",
        "cancelled",
    ];

    let mut columns: Vec<BoardColumn> = order
        .iter()
        .map(|status| BoardColumn {
            status: status.to_string(),
            count: 0,
            rows: Vec::new(),
        })
        .collect();

    let mut overdue = 0;
    let mut leased = 0;
    let mut open = 0;
    let mut done = 0;

    for node in &nodes {
        if matches!(node.effective_status.as_str(), "done" | "cancelled") {
            if node.effective_status == "done" {
                done += 1;
            }
        } else {
            open += 1;
        }
        if node.lease_holder.is_some() {
            leased += 1;
        }
        let days_overdue = match (&node.due_date, node.effective_status.as_str()) {
            (Some(due), status) if !matches!(status, "done" | "cancelled") => {
                let due_days = date_to_days(due).unwrap_or(today);
                let delta = today - due_days;
                if delta > 0 { delta } else { 0 }
            }
            _ => 0,
        };
        if days_overdue > 0 {
            overdue += 1;
        }

        let progress = if node.progress_total > 0.0 {
            format!("{:.0}%", node.progress_done / node.progress_total * 100.0)
        } else {
            "-".to_string()
        };

        let row = BoardRow {
            dot_id: node.dot_id.clone(),
            title: node.title.clone(),
            priority: node.priority.clone(),
            due_date: node.due_date.clone(),
            days_overdue,
            lease_holder: node.lease_holder.clone(),
            progress,
        };
        if let Some(column) = columns
            .iter_mut()
            .find(|column| column.status == node.effective_status)
        {
            column.count += 1;
            column.rows.push(row);
        }
    }
    columns.retain(|column| column.count > 0);

    Ok(Board {
        project: project.key.clone(),
        columns,
        overdue,
        leased,
        open,
        done,
    })
}

fn today_days(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT CAST(julianday(date('now')) AS INTEGER)",
        [],
        |row| row.get(0),
    )?)
}

fn date_to_days(value: &str) -> Option<i64> {
    use time::Date;
    use time::format_description::well_known::Iso8601;
    Date::parse(value, &Iso8601::DATE)
        .ok()
        .map(|date| i64::from(date.to_julian_day()))
}

/// Результат пакетной правки.
#[derive(Debug, Clone, Serialize)]
pub struct BulkResult {
    pub updated: Vec<String>,
    pub failed: Vec<BulkFailure>,
}

/// Неудача по одному узлу пакета.
#[derive(Debug, Clone, Serialize)]
pub struct BulkFailure {
    pub dot_id: String,
    pub code: String,
    pub message: String,
}

/// Пакетная смена статуса: одна ошибка не отменяет остальные, но и не прячется.
pub fn bulk_status(
    conn: &mut Connection,
    project: &Project,
    references: &[String],
    target: Status,
    force: bool,
    actor: &str,
) -> Result<BulkResult> {
    let mut updated = Vec::new();
    let mut failed = Vec::new();
    for reference in references {
        let result = node::update(
            conn,
            project,
            reference,
            node::Patch {
                status: Some(target),
                force,
                // Массовая правка аренды не держит: `in_progress` через неё запрещён.
                lease: None,
                actor: Some(actor),
                ..node::Patch::default()
            },
        );
        match result {
            Ok(node) => updated.push(node.dot_id),
            Err(err) => failed.push(BulkFailure {
                dot_id: reference.clone(),
                code: err.code().to_string(),
                message: err.to_string(),
            }),
        }
    }
    Ok(BulkResult { updated, failed })
}

/// Часы по узлам: оценка против факта из попыток.
#[derive(Debug, Clone, Serialize)]
pub struct TimeRow {
    pub dot_id: String,
    pub title: String,
    pub estimated_hours: Option<f64>,
    pub actual_hours: f64,
    pub attempts: i64,
}

/// Учёт времени: суммарная длительность попыток по каждому узлу.
pub fn time_report(
    conn: &Connection,
    project: &Project,
    reference: Option<&str>,
) -> Result<Vec<TimeRow>> {
    let subtree = match reference {
        Some(reference) => {
            let node = node::resolve(conn, project, reference)?;
            Some(node.path)
        }
        None => None,
    };

    let mut sql = String::from(
        "SELECT n.path, n.title, n.estimated_hours, COALESCE(h.hours, 0.0),
                (SELECT COUNT(*) FROM attempts a WHERE a.node_id = n.id)
         FROM nodes n
         LEFT JOIN v_attempt_hours h ON h.node_id = n.id
         WHERE n.project_id = ?1 AND n.deleted_at IS NULL",
    );
    let mut values: Vec<Value> = vec![Value::Integer(project.id)];
    if let Some(path) = &subtree {
        sql.push_str(" AND (n.path = ? OR n.path GLOB ?)");
        values.push(Value::Text(path.clone()));
        values.push(Value::Text(format!("{path}.*")));
    }
    sql.push_str(" ORDER BY n.path");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values.iter()), |row| {
        let path: String = row.get(0)?;
        Ok(TimeRow {
            dot_id: project.dot_id(&path),
            title: row.get(1)?,
            estimated_hours: row.get(2)?,
            actual_hours: row.get(3)?,
            attempts: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Запись в лог о пакетной операции: одна строка на всю пачку.
pub fn log_bulk(
    conn: &Connection,
    project: &Project,
    action: &str,
    summary: &str,
    actor: &str,
) -> Result<()> {
    activity::log(
        conn,
        &activity::Entry {
            project_id: project.id,
            node_id: None,
            entity_type: "batch",
            entity_id: 0,
            action,
            field_name: None,
            old_value: None,
            new_value: None,
            summary: Some(summary),
            actor,
        },
    )?;
    Ok(())
}
