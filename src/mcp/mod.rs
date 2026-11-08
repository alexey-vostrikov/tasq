//! MCP-сервер: JSON-RPC поверх stdio и каталог инструментов (README §13).
//!
//! Сервер поднимается командой `tasq mcp [--profile …]`. Он не открывает сокетов и не ходит
//! в сеть: единственный вход — stdin, единственный выход — stdout, диагностика — stderr.

pub mod profile;
pub mod rpc;
pub mod tools;

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::project;

use profile::Profile;

/// Сервер с выбранным профилем.
///
/// База открывается при старте, но её отсутствие **не** мешает серверу подняться: клиент
/// должен получить подключение и каталог инструментов, а не «connection closed» без причины.
/// Внятный отказ приходит на сам вызов инструмента, и `tasq_init` умеет создать базу
/// прямо из сессии.
pub struct Server {
    /// Открытая база и проект; `None` — базы нет, см. `unavailable`.
    context: Option<tools::Context>,
    /// Почему база не открыта: код и текст ошибки для ответа на вызов.
    unavailable: Option<(String, String)>,
    cwd: PathBuf,
    profile: Profile,
    /// Сколько вызовов инструментов обслужено — используется в тестах и `doctor`.
    pub calls: usize,
}

impl Server {
    /// Пытается открыть базу и проект; `db` — явный путь, иначе поиск `.tasq/tasq.db` вверх.
    /// Неудача запоминается, а не возвращается: сервер обязан ответить на рукопожатие.
    pub fn open(
        cwd: &std::path::Path,
        db: Option<&std::path::Path>,
        profile: Profile,
    ) -> Result<Self> {
        let (context, unavailable) = match Self::connect(cwd, db) {
            Ok(context) => (Some(context), None),
            Err(err) => (None, Some((err.code().to_string(), err.to_string()))),
        };
        Ok(Server {
            context,
            unavailable,
            cwd: cwd.to_path_buf(),
            profile,
            calls: 0,
        })
    }

    fn connect(cwd: &std::path::Path, db: Option<&std::path::Path>) -> Result<tools::Context> {
        let db_path = crate::db::discover(cwd, db)?;
        let conn = crate::db::open_checked(&db_path)?;
        let project = project::default_project(&conn)?;
        Ok(tools::Context {
            conn,
            project,
            db_path,
            cwd: cwd.to_path_buf(),
        })
    }

    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// Путь к базе, если она открыта.
    pub fn db_path(&self) -> Option<&std::path::Path> {
        self.context
            .as_ref()
            .map(|context| context.db_path.as_path())
    }

    /// Каталог инструментов для текущего профиля.
    pub fn catalogue(&self) -> Vec<Value> {
        self.profile
            .tools()
            .iter()
            .filter_map(|name| tools::definition(name))
            .collect()
    }

    /// Отказ «база не открыта» в том же формате, что и доменные ошибки инструментов.
    fn no_database(&self, err: &(String, String)) -> Value {
        json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&json!({
                    "code": err.0,
                    "message": err.1,
                    "hint": format!(
                        "no database from {} upward; call tasq_init to create one here, or pass --db / set TASQ_DB",
                        self.cwd.display()
                    ),
                }))
                .unwrap_or_default(),
            }],
            "isError": true,
        })
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

        // Базы нет: `tasq_init` создаёт её здесь же, остальные вызовы получают отказ с кодом.
        if self.context.is_none() {
            if name != "tasq_init" {
                let err = self.unavailable.clone().unwrap_or_else(|| {
                    let err = Error::DbNotFound(self.cwd.display().to_string());
                    (err.code().to_string(), err.to_string())
                });
                return self.no_database(&err);
            }
            let key = arguments
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or("TASQ");
            let project_name = arguments.get("name").and_then(Value::as_str).unwrap_or(key);
            match crate::db::init(&self.cwd, key, project_name).and_then(|outcome| {
                let db_path = PathBuf::from(&outcome.db_path);
                let conn = crate::db::open_checked(&db_path)?;
                let project = project::default_project(&conn)?;
                Ok(tools::Context {
                    conn,
                    project,
                    db_path,
                    cwd: self.cwd.clone(),
                })
            }) {
                Ok(context) => {
                    self.context = Some(context);
                    self.unavailable = None;
                }
                Err(err) => {
                    let code = err.code().to_string();
                    let message = err.to_string();
                    return self.no_database(&(code, message));
                }
            }
        }

        self.calls += 1;
        let context = self.context.as_mut().expect("база открыта выше");
        tools::call(context, name, arguments)
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
    match server.db_path() {
        Some(path) => eprintln!(
            "tasq mcp: profile {}, {} tool(s), database {}",
            profile.as_str(),
            server.catalogue().len(),
            path.display()
        ),
        None => eprintln!(
            "tasq mcp: profile {}, {} tool(s), no database yet (tasq_init will create one)",
            profile.as_str(),
            server.catalogue().len()
        ),
    }
    Ok(rpc::serve(&mut server)?)
}
