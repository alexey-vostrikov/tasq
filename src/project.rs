//! Проекты: ключ из dot-ID, выбор проекта для работы, перечисление и создание.

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::error::{Error, Result};

/// Проект в объёме, нужном для сборки dot-ID.
#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub id: i64,
    pub key: String,
    pub name: String,
    pub status: String,
}

/// Проект для списка: к нему добавлено число живых узлов.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub id: i64,
    pub key: String,
    pub name: String,
    pub status: String,
    pub nodes: i64,
    pub created_at: String,
}

/// Проект, к которому относится работа: первый по порядку создания.
///
/// Это значение по умолчанию для базы с одним проектом; при нескольких проектах
/// нужен явный выбор — `select` ниже.
pub fn default_project(conn: &Connection) -> Result<Project> {
    conn.query_row(
        "SELECT id, key, name, status FROM projects ORDER BY id LIMIT 1",
        [],
        |row| {
            Ok(Project {
                id: row.get(0)?,
                key: row.get(1)?,
                name: row.get(2)?,
                status: row.get(3)?,
            })
        },
    )
    .optional()?
    .ok_or(Error::NoProject)
}

/// Проект по ключу.
pub fn by_key(conn: &Connection, key: &str) -> Result<Option<Project>> {
    Ok(conn
        .query_row(
            "SELECT id, key, name, status FROM projects WHERE key = ?1",
            params![key],
            |row| {
                Ok(Project {
                    id: row.get(0)?,
                    key: row.get(1)?,
                    name: row.get(2)?,
                    status: row.get(3)?,
                })
            },
        )
        .optional()?)
}

/// Ключи всех проектов одной строкой — для сообщений об ошибке.
pub fn known_keys(conn: &Connection) -> Result<String> {
    let mut stmt = conn.prepare("SELECT key FROM projects ORDER BY id")?;
    let keys: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if keys.is_empty() {
        return Ok("none yet".to_string());
    }
    Ok(keys.join(", "))
}

/// Проект, в котором идёт работа.
///
/// Явный ключ важнее порядка создания: без этого второй проект был бы недостижим —
/// все инструменты молча работали бы с первым. Значение по умолчанию сохранено,
/// чтобы существующие базы и скрипты продолжали работать без правок.
pub fn select(conn: &Connection, key: Option<&str>) -> Result<Project> {
    match key.map(str::trim).filter(|value| !value.is_empty()) {
        Some(key) => by_key(conn, key)?.ok_or_else(|| Error::ProjectNotFound {
            key: key.to_string(),
            known: known_keys(conn).unwrap_or_else(|_| "unknown".to_string()),
        }),
        None => default_project(conn),
    }
}

/// Все проекты базы, в порядке создания, с числом живых узлов.
pub fn list(conn: &Connection) -> Result<Vec<Summary>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.key, p.name, p.status, p.created_at,
                (SELECT COUNT(*) FROM nodes n
                  WHERE n.project_id = p.id AND n.deleted_at IS NULL) AS nodes
           FROM projects p
          ORDER BY p.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Summary {
            id: row.get(0)?,
            key: row.get(1)?,
            name: row.get(2)?,
            status: row.get(3)?,
            created_at: row.get(4)?,
            nodes: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Создаёт проект в существующей базе.
///
/// Ключ приводится к верхнему регистру: dot-ID всё равно пишется заглавными, а
/// `tasq create project prj` с отказом «ожидалось A-Z» раздражает без пользы.
pub fn create(conn: &mut Connection, key: &str, name: Option<&str>) -> Result<Project> {
    let key = key.trim().to_uppercase();
    crate::db::validate_key(&key)?;
    if by_key(conn, &key)?.is_some() {
        return Err(Error::DuplicateKey(key));
    }
    let name = name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&key)
        .to_string();
    let now = crate::db::now_iso();
    let tx = crate::db::write_tx(conn)?;
    tx.execute(
        "INSERT INTO projects (key, name, status, timezone, tags, metadata, created_at, updated_at)
         VALUES (?1, ?2, 'active', 'UTC', '[]', '{}', ?3, ?3)",
        params![key, name, now],
    )?;
    let id = tx.last_insert_rowid();
    tx.commit()?;
    Ok(Project {
        id,
        key,
        name,
        status: "active".to_string(),
    })
}

impl Project {
    /// Собирает dot-ID из пути: `TASQ-12.3.1`.
    pub fn dot_id(&self, path: &str) -> String {
        format!("{}-{}", self.key, path)
    }
}
