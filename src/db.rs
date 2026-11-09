//! Хранилище: поиск файла базы, миграции, инициализация, проверка целостности.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use time::OffsetDateTime;

use crate::error::{Error, Result};

pub const SUPPORTED_SCHEMA_VERSION: i64 = 2;
pub const DB_DIR: &str = ".tasq";
pub const DB_FILE: &str = "tasq.db";

/// Миграции применяются по возрастанию версии, каждая — один `execute_batch`.
const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/0001_init.sql")),
    (2, include_str!("../migrations/0002_search_and_time.sql")),
];

/// Время в UTC, ровно в том виде, в каком его печатает `strftime('%Y-%m-%dT%H:%M:%fZ','now')`,
/// чтобы строковые сравнения в SQL и в коде были согласованы.
pub fn now_iso() -> String {
    iso_at(OffsetDateTime::now_utc())
}

/// Та же форма времени для произвольного момента: `expires_at` считается из `now + ttl`.
pub fn iso_at(moment: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        moment.year(),
        u8::from(moment.month()),
        moment.day(),
        moment.hour(),
        moment.minute(),
        moment.second(),
        moment.millisecond()
    )
}

/// Путь к базе внутри указанного каталога.
pub fn db_path_in(dir: &Path) -> PathBuf {
    dir.join(DB_DIR).join(DB_FILE)
}

/// Ищет `.tasq/tasq.db` вверх от `start`; `explicit` отменяет поиск.
pub fn discover(start: &Path, explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        // Явно указанный путь не создаётся молча: опечатка в `--db` иначе превратилась бы
        // в пустую базу и ошибку схемы вместо «файла нет».
        if !path.is_file() {
            return Err(Error::DbNotFound(path.display().to_string()));
        }
        return Ok(path.to_path_buf());
    }
    let mut dir = start.to_path_buf();
    loop {
        let candidate = db_path_in(&dir);
        if candidate.is_file() {
            return Ok(candidate);
        }
        if !dir.pop() {
            return Err(Error::DbNotFound(start.display().to_string()));
        }
    }
}

/// Открывает соединение с нужными прагмами. Файл базы должен существовать.
///
/// Регистрирует две функции, которых нет в SQLite по умолчанию:
/// - `uni_lower(text)` — приведение к нижнему регистру по правилам Unicode. Встроенный
///   `lower()` в SQLite понимает только ASCII, поэтому `lower('Миграции')` возвращает строку
///   без изменений и кириллический поиск молча становится регистрозависимым
///   (аналог истории с utf8 против utf8mb4 в MySQL);
/// - коллация `UNICODE_NOCASE` — сравнение и сортировка без учёта регистра для любого письма.
///   Встроенная `NOCASE` тоже ограничена ASCII.
pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         -- Потолок размера журнала: после checkpoint WAL не остаётся на максимуме.
         PRAGMA journal_size_limit = 8388608;",
    )?;
    register_unicode(&conn)?;
    Ok(conn)
}

/// Соединение для служебных задач вроде сброса журнала.
///
/// Отличие от `open`: здесь **не** выставляется `journal_mode`. Переключение режима требует
/// кратковременной исключительной блокировки, и если другое соединение держит читательскую
/// транзакцию, `PRAGMA journal_mode = WAL` ждёт `busy_timeout` и поток сброса не стартует
/// вовсе — то есть не сбрасывает именно тогда, когда это нужнее всего.
/// Заодно не регистрируются Unicode-функции: они нужны только запросам с текстом.
pub fn open_light(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    // Короткий `busy_timeout`: сброс — фоновая задача, и ждать блокировку пять секунд
    // ей нельзя. Пока читатель держит транзакцию, `wal_checkpoint(TRUNCATE)` вернёт busy
    // сразу, и поток успеет зафиксировать это предупреждением, а не зависнет на весь тик.
    conn.execute_batch(
        "PRAGMA busy_timeout = 200;
         PRAGMA journal_size_limit = 8388608;",
    )?;
    Ok(conn)
}

/// Число несохранённых кадров журнала: 0 — всё уже в базе.
pub fn pending_frames(conn: &Connection) -> Result<i64> {
    let (busy, log, checkpointed): (i64, i64, i64) =
        conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    let _ = busy;
    Ok(log - checkpointed)
}

/// Сбрасывает журнал на диск и усекает его.
///
/// Зачем это нужно при `synchronous = NORMAL`: в WAL-режиме коммит не делает fsync, а лишь
/// дописывает кадры в журнал. Пока не случится checkpoint, данные живут в кэше страниц и
/// пропадут при отключении питания — сколько бы времени ни прошло. Checkpoint копирует кадры
/// в основной файл, делает fsync и (в режиме TRUNCATE) обнуляет журнал: окно потерь
/// ограничивается интервалом между вызовами этой функции, то есть временем, а не объёмом
/// записи.
///
/// README §17: допустимая потеря — несколько секунд работы.
pub fn checkpoint(conn: &Connection) -> Result<i64> {
    // TRUNCATE усечёт журнал, только когда нет активных читателей; иначе остаётся
    // PASSIVE — он тоже копирует кадры и делает fsync, но журнал не обнуляет.
    let (busy, log, checkpointed): (i64, i64, i64) =
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    if busy != 0 {
        conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            let _: (i64, i64, i64) = (row.get(0)?, row.get(1)?, row.get(2)?);
            Ok(())
        })?;
    }
    let _ = log;
    Ok(checkpointed)
}

/// Регистрирует Unicode-осведомлённые `uni_lower` и `UNICODE_NOCASE`.
fn register_unicode(conn: &Connection) -> Result<()> {
    use rusqlite::functions::FunctionFlags;
    conn.create_scalar_function(
        "uni_lower",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            Ok(text.to_lowercase())
        },
    )?;
    conn.create_collation("UNICODE_NOCASE", |a, b| {
        a.to_lowercase().cmp(&b.to_lowercase())
    })?;
    Ok(())
}

/// Транзакция записи: `BEGIN IMMEDIATE` (README §5).
///
/// Немедленный захват блокировки записи убирает гонку «прочитал, что свободно,
/// и пишу» — второй писатель ждёт на `busy_timeout`, а не проходит на устаревшем чтении.
pub fn write_tx(conn: &mut Connection) -> Result<rusqlite::Transaction<'_>> {
    Ok(conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?)
}

/// Открывает базу и проверяет, что версия схемы не новее поддерживаемой.
pub fn open_checked(path: &Path) -> Result<Connection> {
    let conn = open(path)?;
    let version = schema_version(&conn)?;
    if version > SUPPORTED_SCHEMA_VERSION {
        return Err(Error::SchemaTooNew {
            found: version,
            supported: SUPPORTED_SCHEMA_VERSION,
        });
    }
    Ok(conn)
}

fn schema_version(conn: &Connection) -> Result<i64> {
    // Свежая база: таблицы `meta` ещё нет — значит миграций не было.
    let has_meta: bool = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
        [],
        |row| row.get::<_, i64>(0),
    )? > 0;
    if !has_meta {
        return Ok(0);
    }
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value.and_then(|v| v.parse().ok()).unwrap_or(0))
}

/// Итог `tasq init`.
#[derive(Debug, Serialize)]
pub struct InitOutcome {
    pub db_path: String,
    pub schema_version: i64,
    pub project_key: String,
    pub project_created: bool,
    pub migrated: bool,
}

/// Ключ проекта: `[A-Z0-9]{2,8}`.
pub fn validate_key(key: &str) -> Result<()> {
    let ok = (2..=8).contains(&key.len())
        && key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidKey(key.to_string()))
    }
}

/// Создаёт базу в `dir` (или достраивает недостающие миграции) и проект.
///
/// Существующая база не перезаписывается: проект создаётся только если в базе их нет.
pub fn init(dir: &Path, key: &str, name: &str) -> Result<InitOutcome> {
    init_at(&db_path_in(dir), key, name)
}

/// Инициализация по полному пути к файлу базы: нужна для `--db` и `TASQ_DB`,
/// где путь задан явно и каталог `.tasq` вычислять не из чего.
pub fn init_at(db_path: &Path, key: &str, name: &str) -> Result<InitOutcome> {
    validate_key(key)?;
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let existed = db_path.is_file();

    let mut conn = open(db_path)?;
    let migrated = apply_migrations(&mut conn)?;
    let version = schema_version(&conn)?;

    let existing_key: Option<String> = conn
        .query_row("SELECT key FROM projects ORDER BY id LIMIT 1", [], |row| {
            row.get(0)
        })
        .optional()?;

    let (project_key, project_created) = match existing_key {
        Some(key) => (key, false),
        None => {
            let now = now_iso();
            let tx = write_tx(&mut conn)?;
            tx.execute(
                "INSERT INTO projects (key, name, status, timezone, tags, metadata, created_at, updated_at)
                 VALUES (?1, ?2, 'active', 'UTC', '[]', '{}', ?3, ?3)",
                rusqlite::params![key, name, now],
            )?;
            tx.commit()?;
            (key.to_string(), true)
        }
    };

    Ok(InitOutcome {
        db_path: db_path.display().to_string(),
        schema_version: version,
        project_key,
        project_created,
        migrated: migrated || !existed,
    })
}

/// Применяет неприменённые миграции; возвращает `true`, если что-то применилось.
fn apply_migrations(conn: &mut Connection) -> Result<bool> {
    let current = schema_version(conn)?;
    let mut applied = false;
    for (version, sql) in MIGRATIONS {
        if *version <= current {
            continue;
        }
        let tx = write_tx(conn)?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![version.to_string()],
        )?;
        tx.pragma_update(None, "user_version", *version)?;
        tx.commit()?;
        applied = true;
    }
    Ok(applied)
}

/// Итог `tasq doctor`.
#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub db_path: String,
    pub schema_version: i64,
    pub supported_version: i64,
    pub integrity: String,
    pub foreign_key_violations: i64,
    pub projects: i64,
    pub nodes: i64,
    pub live_attempts: i64,
    pub expired_attempts: i64,
    pub orphan_reservations: i64,
    pub ok: bool,
}

/// Проверка целостности базы; ничего не меняет.
pub fn doctor(path: &Path) -> Result<DoctorReport> {
    let conn = open_checked(path)?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    let foreign_key_violations: i64 = {
        let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
        stmt.query_map([], |_| Ok(()))?.count() as i64
    };
    let scalar =
        |sql: &str| -> Result<i64> { Ok(conn.query_row(sql, [], |row| row.get::<_, i64>(0))?) };
    let now = now_iso();
    let schema_version = schema_version(&conn)?;
    let expired_attempts = conn.query_row(
        "SELECT COUNT(*) FROM attempts WHERE ended_at IS NULL AND expires_at <= ?1",
        rusqlite::params![now],
        |row| row.get::<_, i64>(0),
    )?;
    let orphan_reservations = conn.query_row(
        "SELECT COUNT(*) FROM reservations r
         LEFT JOIN attempts a ON a.id = r.attempt_id
         WHERE r.released_at IS NULL
           AND (a.id IS NULL OR a.ended_at IS NOT NULL OR a.expires_at <= ?1)",
        rusqlite::params![now],
        |row| row.get::<_, i64>(0),
    )?;

    let report = DoctorReport {
        db_path: path.display().to_string(),
        schema_version,
        supported_version: SUPPORTED_SCHEMA_VERSION,
        integrity: integrity.clone(),
        foreign_key_violations,
        projects: scalar("SELECT COUNT(*) FROM projects")?,
        nodes: scalar("SELECT COUNT(*) FROM nodes WHERE deleted_at IS NULL")?,
        live_attempts: scalar("SELECT COUNT(*) FROM attempts WHERE ended_at IS NULL")?,
        expired_attempts,
        orphan_reservations,
        ok: integrity == "ok"
            && foreign_key_violations == 0
            && schema_version <= SUPPORTED_SCHEMA_VERSION,
    };
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Пока не сделан checkpoint, кадры лежат в журнале; после — журнал пуст,
    /// и данные уже в основном файле. Это и есть окно потерь при отключении питания.
    #[test]
    fn checkpoint_moves_frames_into_the_database() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-checkpoint-{nanos}"));
        std::fs::create_dir_all(&dir).expect("dir");
        let outcome = init(&dir, "CHECK", "checkpoint").expect("init");
        let db_path = std::path::PathBuf::from(&outcome.db_path);

        let conn = open(&db_path).expect("open");
        // Пишем так же, как это делает любая команда: одной транзакцией.
        let mut writer = open(&db_path).expect("writer");
        let tx = write_tx(&mut writer).expect("tx");
        tx.execute(
            "INSERT INTO activity_log (project_id, entity_type, entity_id, action, created_at)
             VALUES (1, 'probe', 1, 'test', '2026-01-01T00:00:00.000Z')",
            [],
        )
        .expect("insert");
        tx.commit().expect("commit");

        let wal = |path: &std::path::Path| {
            std::fs::metadata(path.with_extension("db-wal"))
                .map(|meta| meta.len())
                .unwrap_or(0)
        };
        assert!(wal(&db_path) > 0, "после коммита кадры в журнале");

        checkpoint(&conn).expect("checkpoint");
        assert_eq!(wal(&db_path), 0, "после checkpoint журнал усечён");

        // Данные читаются из основного файла новым соединением.
        let fresh = open(&db_path).expect("open fresh");
        let count: i64 = fresh
            .query_row(
                "SELECT COUNT(*) FROM activity_log WHERE action = 'test'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 1);
    }
}
