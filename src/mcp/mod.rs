//! MCP-сервер: JSON-RPC поверх stdio и каталог инструментов (README §13).
//!
//! Сервер поднимается командой `tasq mcp [--profile …]`. Он не открывает сокетов и не ходит
//! в сеть: единственный вход — stdin, единственный выход — stdout, диагностика — stderr.

pub mod profile;
pub mod rpc;
pub mod tools;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::project;

use profile::Profile;

/// Периодический сброс журнала на диск в отдельном потоке.
///
/// `synchronous = NORMAL` в WAL-режиме означает: коммит не делает fsync, и до checkpoint
/// данные лежат в кэше страниц. Пока не сбросить их, они пропадут при отключении питания —
/// независимо от того, сколько времени прошло. Поток закрывает это окно интервалом.
///
/// Своё соединение к тому же файлу, а не общий `Connection`: rusqlite не `Sync`, и делить
/// его между потоками пришлось бы под мьютексом, который блокировал бы обработку вызовов.
/// Решение «пора предупреждать», отдельно от потока: его проверяет тест, а не время.
///
/// Сброс считается застрявшим, если кадры остаются неперелитыми несколько тиков подряд.
/// Предупреждение печатается один раз на эпизод, иначе stderr превращается в поток
/// одинаковых строк, и его перестают читать.
#[derive(Default)]
struct Stall {
    silent_ticks: u32,
    warned: bool,
}

impl Stall {
    /// Порог тиков до первого предупреждения: короткая заминка на чужой транзакции —
    /// норма, длинная означает, что окно потерь растёт.
    const TICKS_BEFORE_WARNING: u32 = 3;

    fn observe(&mut self, pending: i64) -> Option<&'static str> {
        if pending == 0 {
            self.silent_ticks = 0;
            self.warned = false;
            return None;
        }
        self.silent_ticks += 1;
        if self.silent_ticks >= Self::TICKS_BEFORE_WARNING && !self.warned {
            self.warned = true;
            return Some(
                "tasq mcp: journal is not being flushed: another connection holds a read \
                 transaction, so data stays unwritten until it finishes",
            );
        }
        None
    }
}

struct Flusher {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Flusher {
    /// `interval` — максимум, сколько данные могут пролежать незаписанными.
    fn start(db_path: PathBuf, interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("tasq-flush".to_string())
            .spawn(move || {
                let conn = match crate::db::open_light(&db_path) {
                    Ok(conn) => conn,
                    Err(err) => {
                        eprintln!("tasq mcp: flusher disabled ({err})");
                        return;
                    }
                };
                // Спим короткими кусками, чтобы остановка не ждала целый интервал.
                let slice = Duration::from_millis(200);
                let mut waited = Duration::ZERO;
                let mut stall = Stall::default();
                while !flag.load(Ordering::Relaxed) {
                    std::thread::sleep(slice);
                    waited += slice;
                    if waited < interval {
                        continue;
                    }
                    waited = Duration::ZERO;
                    match crate::db::checkpoint(&conn) {
                        Ok(pending) => {
                            if let Some(warning) = stall.observe(pending) {
                                eprintln!("{warning}");
                            }
                        }
                        Err(err) => eprintln!("tasq mcp: checkpoint failed: {err}"),
                    }
                }
            })
            .ok();
        Flusher { stop, handle }
    }
}

impl Drop for Flusher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

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
        project_key: Option<&str>,
    ) -> Result<Self> {
        let (context, unavailable) = match Self::connect(cwd, db, project_key) {
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

    fn connect(
        cwd: &std::path::Path,
        db: Option<&std::path::Path>,
        project_key: Option<&str>,
    ) -> Result<tools::Context> {
        let db_path = crate::db::discover(cwd, db)?;
        let conn = crate::db::open_checked(&db_path)?;
        let project = project::select(&conn, project_key)?;
        Ok(tools::Context {
            conn,
            project,
            db_path,
            cwd: cwd.to_path_buf(),
        })
    }

    /// Ключ активного проекта сессии (если база открыта).
    pub fn project_key(&self) -> Option<&str> {
        self.context
            .as_ref()
            .map(|context| context.project.key.as_str())
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
                let project = project::select(&conn, None)?;
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
        // Контекст живёт в сессии: `project_select` меняет `ctx.project`, и следующий
        // вызов инструмента обязан его видеть.
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
    project_key: Option<&str>,
    flush: Option<Duration>,
) -> Result<usize> {
    let mut server = Server::open(cwd, db, profile, project_key)?;

    // Сброс журнала по времени: пока сервер жив, данные не должны висеть в кэше
    // страниц дольше интервала, иначе отключение питания съест не «последнюю секунду»,
    // а всю сессию.
    let _flusher = match (flush, server.context.as_ref()) {
        (Some(interval), Some(context)) if !interval.is_zero() => {
            Some(Flusher::start(context.db_path.clone(), interval))
        }
        _ => None,
    };
    if let Some(key) = server.project_key() {
        eprintln!("tasq mcp: project {key}");
    }
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

#[cfg(test)]
mod tests {
    use super::Stall;

    /// Заминка на один-два тика — норма и повод молчать.
    #[test]
    fn short_stall_is_silent() {
        let mut stall = Stall::default();
        assert_eq!(stall.observe(11), None);
        assert_eq!(stall.observe(11), None);
    }

    /// Третий тик подряд с неперелитыми кадрами — предупреждение.
    #[test]
    fn a_long_stall_warns_once() {
        let mut stall = Stall::default();
        assert_eq!(stall.observe(11), None);
        assert_eq!(stall.observe(11), None);
        let warning = stall.observe(11).expect("третий тик предупреждает");
        assert!(
            warning.contains("journal is not being flushed"),
            "{warning}"
        );
        // Второй раз то же самое не печатается: иначе поток строк, который не читают.
        assert_eq!(stall.observe(11), None);
        assert_eq!(stall.observe(11), None);
    }

    /// Успешный сброс сбрасывает счётчик: предупреждение должно прозвучать снова,
    /// если застревание повторится.
    #[test]
    fn a_successful_flush_rearms_the_warning() {
        let mut stall = Stall::default();
        stall.observe(11);
        stall.observe(11);
        assert!(stall.observe(11).is_some());
        assert_eq!(stall.observe(0), None, "сброс прошёл");
        assert_eq!(stall.observe(11), None, "счёт тиков начат заново");
        assert_eq!(stall.observe(11), None);
        assert!(
            stall.observe(11).is_some(),
            "новый эпизод предупреждает снова"
        );
    }
}
