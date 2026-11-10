//! Хранилище: поиск файла базы, миграции, инициализация, проверка целостности.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use time::OffsetDateTime;

use crate::error::{Error, Result};

pub const SUPPORTED_SCHEMA_VERSION: i64 = 10;
pub const DB_DIR: &str = ".tasq";
pub const DB_FILE: &str = "tasq.db";

/// Миграции применяются по возрастанию версии, каждая — один `execute_batch`.
/// Все миграции по порядку.
///
/// Не `const`: миграция 0010 содержит заглушку `__MAX_DEPTH__`, и число в неё подставляется
/// из `node::MAX_DEPTH` при каждом получении списка. Держать предел глубины вторым литералом в
/// SQL значило бы иметь два числа на одно правило — ровно так и было: в v8 стояло 64, а
/// правило гласит 16, и заметить это можно было только чтением файла.
fn migrations() -> Vec<(i64, String)> {
    let fixed: &[(i64, &str)] = &[
        (1, include_str!("../migrations/0001_init.sql")),
        (2, include_str!("../migrations/0002_search_and_time.sql")),
        (3, include_str!("../migrations/0003_status_funnel.sql")),
        (4, include_str!("../migrations/0004_status_registry.sql")),
        (5, include_str!("../migrations/0005_expired_lease.sql")),
        (6, include_str!("../migrations/0006_history_cause.sql")),
        (7, include_str!("../migrations/0007_history_created.sql")),
        (
            8,
            include_str!("../migrations/0008_progress_cycle_guard.sql"),
        ),
        (9, include_str!("../migrations/0009_sibling_uniqueness.sql")),
    ];
    let mut all: Vec<(i64, String)> = fixed
        .iter()
        .map(|(version, sql)| (*version, (*sql).to_string()))
        .collect();
    all.push((
        10,
        include_str!("../migrations/0010_progress_budget.sql")
            .replace("__MAX_DEPTH__", &crate::node::MAX_DEPTH.to_string()),
    ));
    all
}

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

/// Сбрасывает журнал на диск и усекает его; возвращает число неперелитых кадров.
///
/// Зачем это нужно при `synchronous = NORMAL`: в WAL-режиме коммит не делает fsync, а лишь
/// дописывает кадры в журнал. Пока не случится checkpoint, данные живут в кэше страниц и
/// пропадут при отключении питания — сколько бы времени ни прошло. Checkpoint копирует кадры
/// в основной файл, делает fsync и (в режиме TRUNCATE) обнуляет журнал: окно потерь
/// ограничивается интервалом между вызовами этой функции, то есть временем, а не объёмом
/// записи.
///
/// Возвращаемое значение — размер окна потерь после вызова: `0` значит «всё в базе».
/// Успешный TRUNCATE рапортует о себе нулями (журнал уже усечён), поэтому судить по числу
/// скопированных кадров нельзя — только по остатку.
/// Размер окна потерь: сколько кадров журнала осталось неперелитыми в базу.
///
/// Отдельная функция, потому что это единственное арифметическое место в сбросе: тест на
/// паре чисел проверяет его точно, тогда как проверка «остаток больше нуля» не отличает
/// вычитание от сложения — на реальных значениях обе дают положительное число.
fn loss_window(log: i64, copied: i64) -> i64 {
    log - copied
}

pub fn checkpoint(conn: &Connection) -> Result<i64> {
    // TRUNCATE переливает кадры и, когда читателей нет, обнуляет журнал. При занятом
    // читателе он переливает ровно столько же, сколько PASSIVE (замер: обе — 3 из 7
    // кадров), но файл не усекает; поэтому отдельная запасная ветка ничего не добавляет,
    // и остаток считается по числам этого же вызова.
    let (_, log, copied): (i64, i64, i64) =
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    Ok(loss_window(log, copied))
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

/// Версия схемы в соединении: публичная обёртка для дампа и внешних проверок.
pub fn schema_version_of(conn: &Connection) -> Result<i64> {
    schema_version(conn)
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

/// Открывает (создавая при необходимости) базу и доводит её до текущей версии схемы.
///
/// Нужна восстановлению из дампа: там база создаётся с нуля, и проект в неё не заводится —
/// он приходит из самого дампа. `init_at` для этого не годится: он создаёт проект, которого
/// в дампе может не быть, и мешал бы вставке строк с их собственными ключами.
pub fn create_current(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut conn = open(path)?;
    apply_migrations(&mut conn)?;
    Ok(conn)
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
    let migrations = migrations();
    if migrations.iter().all(|(version, _)| *version <= current) {
        return Ok(false);
    }
    // Внешние ключи выключаются ДО транзакции: внутри транзакции `PRAGMA foreign_keys`
    // молча игнорируется. Пересборка таблицы (v3) удаляет `nodes`, а с включёнными
    // ключами `DROP TABLE` выполняет неявное удаление строк и каскадом сносит
    // attempts, edges, comments и notes. Целостность проверяется до коммита.
    conn.pragma_update(None, "foreign_keys", false)?;
    let result = run_migrations(conn, current);
    // Восстанавливать режим обязательно и при ошибке: соединение остаётся живым,
    // и молча выключенные ключи исказили бы всю дальнейшую работу.
    conn.pragma_update(None, "foreign_keys", true)?;
    result
}

fn run_migrations(conn: &mut Connection, current: i64) -> Result<bool> {
    let mut applied = false;
    let migrations = migrations();
    for (version, sql) in &migrations {
        if *version <= current {
            continue;
        }
        let tx = write_tx(conn)?;
        tx.execute_batch(sql)?;
        let violations = foreign_key_violations(&tx)?;
        if violations > 0 {
            return Err(Error::MigrationBroken {
                version: *version,
                violations,
            });
        }
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

/// Число нарушений внешних ключей в текущем состоянии соединения (внутри транзакции).
fn foreign_key_violations(conn: &Connection) -> Result<i64> {
    let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
    let mut rows = stmt.query([])?;
    let mut count = 0;
    while rows.next()?.is_some() {
        count += 1;
    }
    Ok(count)
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
    /// Узлы глубже предела (`node::MAX_DEPTH`).
    ///
    /// Не ошибка и не повод чинить: такие узлы могли остаться от схем до v4. Но прогресс по ним
    /// считается только до предела, и молчать об этом нельзя — иначе сумма по поддереву
    /// выглядит неполной без причины.
    pub nodes_beyond_depth: i64,
    /// Верхняя граница строк обхода прогресса: `узлы × (предел + 1)`.
    ///
    /// Это и есть ответ на вопрос «сколько может стоить обход»: у узла не больше одного
    /// родителя, поэтому каждая строка представления — пара (предок, потомок), и их не может
    /// быть больше, чем узлов, умноженных на длину цепочки предков. Величина линейна по числу
    /// узлов и не зависит от ветвления — в отличие от прежнего обхода вниз, где она равнялась
    /// сумме размеров поддеревьев.
    pub progress_scan_bound: i64,
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
        // Живая аренда — та, что ещё действует по времени, а не любая незакрытая попытка.
        // Раньше считались все `ended_at IS NULL`, и отчёт называл живыми ровно те аренды,
        // которые `doctor --gc` в следующей строке закрывал как просроченные: два числа
        // одного отчёта противоречили друг другу.
        live_attempts: conn.query_row(
            "SELECT COUNT(*) FROM attempts WHERE ended_at IS NULL AND expires_at > ?1",
            rusqlite::params![now],
            |row| row.get::<_, i64>(0),
        )?,
        expired_attempts,
        orphan_reservations,
        // Узлы глубже предела считаются по тому же числу, что и правило глубины: если предел
        // когда-нибудь изменится, отчёт обязан измениться вместе с ним.
        nodes_beyond_depth: conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE deleted_at IS NULL AND depth > ?1",
            [crate::node::MAX_DEPTH],
            |row| row.get::<_, i64>(0),
        )?,
        // Верхняя граница объёма обхода прогресса: пара (предок, потомок) на каждый шаг вверх,
        // у узла не больше одного родителя — значит строк не больше, чем узлов на длину цепочки.
        progress_scan_bound: scalar("SELECT COUNT(*) FROM nodes WHERE deleted_at IS NULL")?
            .saturating_mul(crate::node::MAX_DEPTH + 1),
        // Условия «схема не новее поддерживаемой» здесь нет намеренно: `doctor` открывает
        // базу через `open_checked`, который на новой схеме уже отказал, поэтому такое
        // слагаемое было бы всегда истинным (мёртвым), и его подмена не меняла бы отчёт.
        ok: integrity == "ok" && foreign_key_violations == 0,
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
             VALUES (1, 'project', 1, 'test', '2026-01-01T00:00:00.000Z')",
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

        let pending = checkpoint(&conn).expect("checkpoint");
        assert_eq!(wal(&db_path), 0, "после checkpoint журнал усечён");
        assert_eq!(pending, 0, "неперелитых кадров не осталось");

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

    /// Арифметика окна потерь проверяется ТОЧНЫМ значением: на реальных числах и
    /// вычитание, и сложение дают положительный остаток, поэтому «больше нуля» их не различает.
    #[test]
    fn loss_window_is_the_difference_of_frames() {
        assert_eq!(loss_window(7, 3), 4);
        assert_eq!(
            loss_window(11, 0),
            11,
            "ничего не перелито — окно на все кадры"
        );
        assert_eq!(loss_window(5, 5), 0, "перелито всё — окна нет");
        assert_eq!(loss_window(0, 0), 0);
    }

    /// Свежая база: версия схемы читается как ноль, а не падает на отсутствии `meta`.
    #[test]
    fn a_fresh_database_reports_schema_version_zero() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-fresh-{nanos}"));
        std::fs::create_dir_all(dir.join(".tasq")).expect("dir");
        let path = db_path_in(&dir);
        let conn = Connection::open(&path).expect("open");
        assert_eq!(schema_version(&conn).expect("version"), 0);
    }

    /// Первый init создаёт проект и применяет миграции; второй — не делает ни того, ни другого.
    /// Без этой пары флаги `project_created` и `migrated` можно вывернуть незаметно.
    #[test]
    fn init_reports_what_it_actually_did() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-init-flags-{nanos}"));
        std::fs::create_dir_all(&dir).expect("dir");

        let first = init(&dir, "FLAGS", "flags").expect("first init");
        assert!(first.project_created, "первый init создаёт проект");
        assert!(first.migrated, "первый init применяет миграции");
        assert!(first.schema_version > 0, "схема накатана");

        let second = init(&dir, "FLAGS", "flags").expect("second init");
        assert!(!second.project_created, "второй init проект не создаёт");
        assert!(!second.migrated, "второй init миграции не применяет");
        assert_eq!(
            second.schema_version, first.schema_version,
            "версия схемы не сдвигается"
        );
    }

    /// Каждое из трёх условий `ok` проверяется своим кейсом: сломанное целое и нулевые
    /// нарушения ловят `&&` → `||` только в одном слагаемом из трёх.
    #[test]
    fn doctor_flags_each_broken_condition_separately() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();

        // 1. Нарушение внешнего ключа при целой схеме: ok обязан стать ложным.
        let dir = std::env::temp_dir().join(format!("tasq-doctor-fk-{nanos}"));
        std::fs::create_dir_all(&dir).expect("dir");
        let outcome = init(&dir, "DOCFK", "doctor").expect("init");
        let path = std::path::PathBuf::from(&outcome.db_path);
        {
            // Строка, ссылающаяся на несуществующий проект: проверка внешних ключей её увидит.
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch("PRAGMA foreign_keys = OFF;")
                .expect("pragma");
            conn.execute(
                "INSERT INTO nodes (project_id, depth, seq, path, kind, title, status, priority,
                                    weight, created_at, updated_at)
                 VALUES (9999, 1, 1, '1', 'epic', 'висячий', 'planned', 'medium', 1.0,
                         '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z')",
                [],
            )
            .expect("insert orphan");
        }
        let clean = doctor(&path).expect("doctor");
        assert!(
            !clean.ok,
            "висячая ссылка делает отчёт неуспешным: {clean:?}"
        );
        assert_eq!(clean.foreign_key_violations, 1);
        assert_eq!(clean.integrity, "ok", "целостность при этом цела");
        assert_eq!(
            clean.schema_version, SUPPORTED_SCHEMA_VERSION,
            "схема в порядке — значит судит именно нарушение ссылки"
        );

        // 2. Схема новее поддерживаемой: `doctor` не отчёт, а отказ — база открывается
        //    через `open_checked`, и работать с незнакомой схемой он не берётся.
        let dir2 = std::env::temp_dir().join(format!("tasq-doctor-schema-{nanos}"));
        std::fs::create_dir_all(&dir2).expect("dir");
        let outcome2 = init(&dir2, "DOCSC", "doctor").expect("init");
        let path2 = std::path::PathBuf::from(&outcome2.db_path);
        {
            // Версия схемы живёт в `meta`, а не в `user_version`: поднимаем именно её.
            let conn = Connection::open(&path2).expect("open");
            conn.execute(
                "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
                [&(SUPPORTED_SCHEMA_VERSION + 1).to_string()],
            )
            .expect("bump");
        }
        let error = doctor(&path2).expect_err("новая схема не открывается");
        assert_eq!(error.code(), "SCHEMA_TOO_NEW", "{error}");
    }

    /// Миграции догоняют существующую базу: файл есть, версия в `meta` отстаёт — `init`
    /// обязан применить недостающее и сказать об этом. Это единственный случай, где
    /// `migrated` истинно при существующем файле, и без него флаг неотличим от «файла не было».
    #[test]
    fn init_finishes_pending_migrations_on_an_existing_database() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-catchup-{nanos}"));
        std::fs::create_dir_all(&dir).expect("dir");

        // Настоящая старая база: схема v1, отметка версии v1 и существующий проект.
        //
        // Раньше здесь база создавалась текущей и версия откатывалась в `meta` вручную. Такой
        // сценарий не соответствует ни одному реальному состоянию, и миграция v4 его вскрыла:
        // на уже пересобранной таблице `CREATE TABLE statuses` падает. Догон миграций нужно
        // проверять на базе, где их действительно ещё нет.
        let path = db_path_in(&dir);
        std::fs::create_dir_all(path.parent().expect("db dir")).expect("db dir");
        {
            let conn = Connection::open(&path).expect("v1 db");
            conn.execute_batch(include_str!("../migrations/0001_init.sql"))
                .expect("схема v1");
            conn.execute_batch(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '1');
                 INSERT INTO projects (id, key, name, status, timezone, tags, metadata,
                                       created_at, updated_at)
                   VALUES (1, 'CATCH', 'catchup', 'active', 'UTC', '[]', '{}',
                           '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z');",
            )
            .expect("данные v1");
        }

        let second = init(&dir, "CATCH", "catchup").expect("second init");
        assert!(
            second.migrated,
            "недостающая миграция применена, значит migrated истинно"
        );
        assert!(!second.project_created, "проект уже был");
        assert_eq!(
            second.schema_version, SUPPORTED_SCHEMA_VERSION,
            "версия поднята до поддерживаемой"
        );
    }

    /// `meta` есть, а значения версии в ней нет (или оно испорчено): читается как 0,
    /// а не как «одна миграция уже накатана».
    #[test]
    fn a_meta_table_without_a_version_reads_as_zero() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-meta-empty-{nanos}"));
        std::fs::create_dir_all(dir.join(".tasq")).expect("dir");
        let path = db_path_in(&dir);
        let conn = Connection::open(&path).expect("open");
        conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .expect("meta");
        assert_eq!(schema_version(&conn).expect("version"), 0);

        // Испорченное значение — тоже ноль: разбирать его как версию нельзя.
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', 'not-a-number')",
            [],
        )
        .expect("insert");
        assert_eq!(schema_version(&conn).expect("version"), 0);
    }

    /// Поиск базы идёт ВВЕРХ от текущего каталога: из вложенного каталога база находится,
    /// а когда её нет нигде — отказ с названным стартовым каталогом.
    #[test]
    fn discovery_walks_up_and_reports_a_missing_database() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("tasq-discover-{nanos}"));
        let nested = root.join("a/b/c");
        std::fs::create_dir_all(&nested).expect("dirs");
        let outcome = init(&root, "WALK", "walk").expect("init");
        let expected = std::path::PathBuf::from(&outcome.db_path);

        assert_eq!(discover(&nested, None).expect("up"), expected);
        assert_eq!(discover(&root, None).expect("root"), expected);

        // Каталог без базы: подъём до корня файловой системы и внятный отказ.
        let empty = std::env::temp_dir().join(format!("tasq-discover-empty-{nanos}"));
        std::fs::create_dir_all(&empty).expect("dir");
        let error = discover(&empty, None).expect_err("базы нет");
        assert_eq!(error.code(), "DB_NOT_FOUND");
        assert!(
            error.to_string().contains(&empty.display().to_string()),
            "отказ называет стартовый каталог: {error}"
        );
    }

    /// Читатель, удержавший транзакцию, не даёт усечь журнал: TRUNCATE возвращает busy,
    /// и функция обязана отрапортовать остаток кадров, а не ноль. Именно по этому числу
    /// поток сброса решает предупредить, что окно потерь растёт.
    #[test]
    fn a_held_reader_leaves_pending_frames() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-checkpoint-held-{nanos}"));
        std::fs::create_dir_all(&dir).expect("dir");
        let outcome = init(&dir, "HELD", "checkpoint").expect("init");
        let db_path = std::path::PathBuf::from(&outcome.db_path);

        let flusher = open_light(&db_path).expect("flusher");
        let mut writer = open(&db_path).expect("writer");

        // Читатель начинает и не заканчивает: транзакция держится до конца теста.
        let mut reader = open(&db_path).expect("reader");
        let tx = reader.transaction().expect("read tx");
        let before: i64 = tx
            .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
            .expect("count");

        let txw = write_tx(&mut writer).expect("write tx");
        txw.execute(
            "INSERT INTO activity_log (project_id, entity_type, entity_id, action, created_at)
             VALUES (1, 'project', 1, 'held', '2026-01-01T00:00:00.000Z')",
            [],
        )
        .expect("insert");
        txw.commit().expect("commit");

        let pending = checkpoint(&flusher).expect("checkpoint");
        assert!(
            pending > 0,
            "при занятом читателе кадры остаются неперелитыми: {pending}"
        );

        // Отпустили читателя — тот же вызов переливает всё.
        drop(tx);
        drop(reader);
        let after = checkpoint(&flusher).expect("checkpoint after release");
        assert_eq!(after, 0, "после освобождения остатка нет");
        assert_eq!(before, 0, "узлов не было — фикстура чистая");
    }
}
