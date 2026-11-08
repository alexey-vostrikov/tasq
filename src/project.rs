//! Проекты: ключ из dot-ID и проект по умолчанию.

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

/// Проект, к которому относится работа: первый по порядку создания.
///
/// Пока в файле один проект, ключ однозначен; с несколькими проектами появится `--project`.
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

impl Project {
    /// Собирает dot-ID из пути: `TASQ-12.3.1`.
    pub fn dot_id(&self, path: &str) -> String {
        format!("{}-{}", self.key, path)
    }
}
