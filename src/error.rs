//! Domain errors with stable machine-readable codes (README §18).

use serde::Serialize;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Файла базы нет: работа без `tasq init` бессмысленна.
    #[error("no tasq database found (looked for .tasq/tasq.db upwards from {0})")]
    DbNotFound(String),

    /// `meta.schema_version` больше, чем знает этот бинарь.
    #[error("database schema version {found} is newer than supported {supported}")]
    SchemaTooNew { found: i64, supported: i64 },

    /// Проект с таким ключом уже есть.
    #[error("project key {0} is already used")]
    DuplicateKey(String),

    /// Ключ проекта не проходит `[A-Z0-9]{2,8}`.
    #[error("invalid project key {0}: expected 2-8 characters of A-Z and 0-9")]
    InvalidKey(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Error::DbNotFound(_) => "DB_NOT_FOUND",
            Error::SchemaTooNew { .. } => "SCHEMA_TOO_NEW",
            Error::DuplicateKey(_) => "DUPLICATE_KEY",
            Error::InvalidKey(_) => "INVALID_KEY",
            Error::Io(_) | Error::Sqlite(_) => "STORAGE_ERROR",
        }
    }

    pub fn hint(&self) -> &'static str {
        match self {
            Error::DbNotFound(_) => "run `tasq init` in the project root",
            Error::SchemaTooNew { .. } => "upgrade the tasq binary; downgrades are not supported",
            Error::DuplicateKey(_) => "pick another key or reuse the existing project",
            Error::InvalidKey(_) => "use uppercase letters and digits, for example TASQ or API2",
            Error::Io(_) | Error::Sqlite(_) => "check file permissions and disk space",
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "error": {
                "code": self.code(),
                "message": self.to_string(),
                "hint": self.hint(),
            }
        })
    }
}

/// Единая форма успешного вывода при `--json`.
#[derive(Debug, Serialize)]
pub struct JsonError {
    pub code: &'static str,
    pub message: String,
    pub hint: &'static str,
}
