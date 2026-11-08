//! MCP-сервер: JSON-RPC поверх stdio и каталог инструментов (README §13).
//!
//! Сервер поднимается командой `tasq mcp [--profile …]`. Он не открывает сокетов и не ходит
//! в сеть: единственный вход — stdin, единственный выход — stdout, диагностика — stderr.

pub mod profile;
pub mod rpc;
pub mod tools;

use std::path::PathBuf;

use rusqlite::Connection;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::project::{self, Project};

use profile::Profile;

/// Сервер с открытой базой и выбранным профилем.
pub struct Server {
    conn: Connection,
    project: Project,
    db_path: PathBuf,
    cwd: PathBuf,
    profile: Profile,
    /// Сколько вызовов инструментов обслужено — используется в тестах и `doctor`.
    pub calls: usize,
}

impl Server {
    /// Открывает базу и проект; `db` — явный путь, иначе поиск `.tasq/tasq.db` вверх.
    pub fn open(
        cwd: &std::path::Path,
        db: Option<&std::path::Path>,
        profile: Profile,
    ) -> Result<Self> {
        let db_path = crate::db::discover(cwd, db)?;
        let conn = crate::db::open_checked(&db_path)?;
        let project = project::default_project(&conn)?;
        Ok(Server {
            conn,
            project,
            db_path,
            cwd: cwd.to_path_buf(),
            profile,
            calls: 0,
        })
    }

    pub fn profile(&self) -> Profile {
        self.profile
    }

    pub fn db_path(&self) -> &std::path::Path {
        &self.db_path
    }

    /// Каталог инструментов для текущего профиля.
    pub fn catalogue(&self) -> Vec<Value> {
        self.profile
            .tools()
            .iter()
            .filter_map(|name| tools::definition(name))
            .collect()
    }

    /// Вызов инструмента с проверкой профиля.
    pub fn call(&mut self, name: &str, arguments: &Value) -> Value {
        // Порядок важен: сперва «такого инструмента нет», потом «он не в этом профиле».
        // Иначе опечатка в имени выглядит как ограничение профиля и уводит в сторону.
        if tools::definition(name).is_none() {
            return json!({
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string_pretty(&json!({
                        "code": "UNKNOWN_TOOL",
                        "message": format!("unknown tool {name}"),
                        "hint": "call tools/list to see the catalogue",
                    }))
                    .unwrap_or_default(),
                }],
                "isError": true,
            });
        }
        if !self.profile.allows(name) {
            let err = Error::InvalidValue {
                field: "tool".to_string(),
                value: format!(
                    "{name} is not available in profile {}",
                    self.profile.as_str()
                ),
            };
            return json!({
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string_pretty(&json!({
                        "code": "TOOL_NOT_IN_PROFILE",
                        "message": err.to_string(),
                        "hint": "run the server with --profile full",
                    }))
                    .unwrap_or_default(),
                }],
                "isError": true,
            });
        }
        self.calls += 1;
        let mut ctx = tools::Context {
            conn: std::mem::replace(
                &mut self.conn,
                Connection::open_in_memory().expect("scratch"),
            ),
            project: self.project.clone(),
            db_path: self.db_path.clone(),
            cwd: self.cwd.clone(),
        };
        let result = tools::call(&mut ctx, name, arguments);
        self.conn = ctx.conn;
        result
    }
}

impl rpc::Handler for Server {
    fn initialize(&mut self) -> Value {
        json!({
            "protocolVersion": rpc::PROTOCOL_VERSION,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {
                "name": "tasq",
                "version": crate::VERSION,
                "profile": self.profile.as_str(),
            },
        })
    }

    fn list_tools(&mut self) -> Value {
        json!({"tools": self.catalogue()})
    }

    fn call_tool(&mut self, name: &str, arguments: &Value) -> Value {
        self.call(name, arguments)
    }
}

/// Запускает сервер на stdin/stdout.
pub fn serve(
    cwd: &std::path::Path,
    db: Option<&std::path::Path>,
    profile: Profile,
) -> Result<usize> {
    let mut server = Server::open(cwd, db, profile)?;
    eprintln!(
        "tasq mcp: profile {}, {} tool(s), database {}",
        profile.as_str(),
        server.catalogue().len(),
        server.db_path().display()
    );
    Ok(rpc::serve(&mut server)?)
}
