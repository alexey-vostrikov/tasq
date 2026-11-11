//! Сессии: чем одна работающая сессия отличается от другой.
//!
//! Зачем это нужно вообще. `attempts.holder` — имя агента, и три параллельных сессии «hermes»
//! им неразличимы: нельзя ответить ни «что взяла эта сессия», ни «в каком проекте она занята».
//! С появлением привязки `next` к проекту это перестало быть удобством: склейка трёх сессий в
//! одну означала бы, что они тянут друг друга из проекта в проект.
//!
//! Идентификатор сессии приходит СНАРУЖИ (`TASQ_SESSION` у CLI, собственный id у MCP-процесса):
//! у ядра нет способа отличить один вызов от другого, и придумывать его здесь значило бы
//! выдавать догадку за факт. Нет идентификатора — нет привязки, и `next` работает по-старому.
//! Это осознанно: сессия — не обязательное понятие, а дополнительное.

use rusqlite::{Connection, OptionalExtension, params};

use crate::db::now_iso;
use crate::error::Result;
use crate::project::Project;

/// Переменная окружения, которой CLI сообщает свой идентификатор сессии.
pub const ENV: &str = "TASQ_SESSION";

/// Новый идентификатор сессии: 16 случайных байт в hex.
///
/// Случайный, а не производный от pid или времени: два процесса, запущенных в одну секунду,
/// иначе получили бы одинаковый id и склеились бы в одну сессию — ровно то, чего избегаем.
pub fn new_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|err| crate::error::Error::InvalidValue {
        field: "session".to_string(),
        value: err.to_string(),
    })?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Идентификатор сессии из окружения.
///
/// Пустое значение читается как отсутствие: `TASQ_SESSION=` в скрипте — обычная случайность, и
/// превращать её в сессию с пустым id значило бы завести невидимую общую привязку.
pub fn from_env() -> Option<String> {
    from_lookup(|key| std::env::var(key).ok())
}

/// Разбор значения, взятого откуда угодно.
///
/// Отдельно от `from_env`, потому что окружение процесса — общий ресурс: тест, правящий
/// `TASQ_SESSION`, перетирает соседние тесты того же бинаря, и набор начинает падать по порядку
/// запуска. Тест подставляет карту, а правило «пустое равно отсутствию, пробелы срезаются»
/// проверяется без `set_var` и без уборки за собой.
pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    lookup(ENV)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Заводит сессию, если её ещё нет, и обновляет `last_seen_at`.
///
/// Повторный вызов идемпотентен: MCP-процесс регистрирует себя при старте и дальше только
/// отмечается, а не создаёт новую строку на каждый инструмент.
pub fn ensure(conn: &Connection, id: &str, holder: &str) -> Result<()> {
    let now = now_iso();
    conn.execute(
        "INSERT INTO sessions (id, holder, project_key, created_at, last_seen_at)
         VALUES (?1, ?2, NULL, ?3, ?3)
         ON CONFLICT(id) DO UPDATE SET last_seen_at = excluded.last_seen_at",
        params![id, holder, now],
    )?;
    Ok(())
}

/// Ключ проекта, к которому привязана сессия.
pub fn affinity(conn: &Connection, id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT project_key FROM sessions WHERE id = ?1",
            params![id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// Привязывает сессию к проекту.
///
/// Привязка — это «где сессия работает», а не «где она смотрела»: вызывается после успешного
/// взятия работы, поэтому переезд в другой проект происходит тогда, когда сессия там что-то
/// взяла, а не когда на него посмотрела.
pub fn bind(conn: &Connection, id: &str, project: &Project) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET project_key = ?1, last_seen_at = ?2 WHERE id = ?3",
        params![project.key, now_iso(), id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::project;

    fn db_conn() -> (Connection, Project) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-sess-{nanos}"));
        let outcome = db::init(&dir, "SESS", "sessions").expect("init");
        let conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let project = project::first_project(&conn).expect("project");
        (conn, project)
    }

    /// Сессия создаётся один раз, повторный `ensure` не плодит строк.
    #[test]
    fn ensure_is_idempotent() {
        let (conn, _project) = db_conn();
        ensure(&conn, "s-1", "agent").expect("ensure");
        ensure(&conn, "s-1", "agent").expect("ensure again");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 1);
    }

    /// До первого взятия работы привязки нет: «смотрела» не равно «работала».
    #[test]
    fn a_new_session_has_no_affinity() {
        let (conn, _project) = db_conn();
        ensure(&conn, "s-1", "agent").expect("ensure");
        assert_eq!(affinity(&conn, "s-1").expect("affinity"), None);
    }

    /// Привязка записывается и читается по ключу проекта.
    #[test]
    fn binding_records_the_project_key() {
        let (conn, project) = db_conn();
        ensure(&conn, "s-1", "agent").expect("ensure");
        bind(&conn, "s-1", &project).expect("bind");
        assert_eq!(
            affinity(&conn, "s-1").expect("affinity").as_deref(),
            Some("SESS")
        );
    }

    /// Неизвестная сессия: привязки нет, и это не ошибка — вызывающий просто работает без неё.
    #[test]
    fn an_unknown_session_has_no_affinity() {
        let (conn, _project) = db_conn();
        assert_eq!(affinity(&conn, "ghost").expect("affinity"), None);
    }

    /// Правило чтения идентификатора: пустое и из пробелов — это отсутствие, а не сессия.
    ///
    /// Каждое значение — отдельный случай карты, а не отдельный тест: переменная окружения
    /// процесса одна на весь бинарь, и тесты, правящие её, перетирают друг друга.
    #[test]
    fn a_blank_identifier_means_no_session() {
        let value = |v: Option<&'static str>| {
            from_lookup(move |key| {
                assert_eq!(key, ENV, "читается ровно объявленная переменная");
                v.map(str::to_string)
            })
        };
        assert_eq!(value(None), None, "не задана — сессии нет");
        assert_eq!(value(Some("")), None, "пустая — сессии нет");
        assert_eq!(value(Some("   ")), None, "из одних пробелов — сессии нет");
        assert_eq!(
            value(Some("  s-1  ")),
            Some("s-1".to_string()),
            "пробелы по краям срезаются"
        );
    }

    /// Идентификаторы двух сессий не совпадают: иначе две параллельные «hermes» склеятся в одну,
    /// а привязка `next` к проекту начнёт тянуть их друг за другом.
    #[test]
    fn two_identifiers_differ_and_are_hex() {
        let first = new_id().expect("id");
        let second = new_id().expect("id");
        assert_ne!(first, second, "два вызова дали один идентификатор");
        assert_eq!(first.len(), 32, "16 байт в hex — это 32 знака: {first}");
        assert!(
            first.chars().all(|c| c.is_ascii_hexdigit()),
            "идентификатор не hex: {first}"
        );
    }
}
