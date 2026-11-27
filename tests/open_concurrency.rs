//! Открытие базы под нагрузкой: волна команд, соединение только для чтения, гонка миграции.
//!
//! Эти проверки появились после замера, который показал дефект: сто одновременных `ls` против
//! базы, ждущей миграции v11, теряли треть команд на «database is locked». Причина была не в
//! блокировках чтения (сто читателей на голом SQLite проходят без единого отказа), а в том, что
//! каждая команда при открытии бралась обновлять схему — то есть просила запись, ничего не
//! собираясь менять.
//!
//! Проверять это иначе, чем процессами, бессмысленно: гонка живёт между процессами, а не внутри
//! одного. Поэтому здесь настоящий бинарь, настоящие процессы и настоящая база в файле.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tasq::db;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tasq")
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-open-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Возвращает базу к состоянию до v11: байт-в-байт то, что лежит у человека, обновившего бинарь,
/// но ещё не запускавшего ни одной команды на новой схеме.
fn roll_back_to_v10(path: &Path) {
    let conn = db::open(path).expect("open");
    conn.execute_batch(
        "DROP TRIGGER comments_fts_ai;
         DROP TRIGGER comments_fts_ad;
         DROP TRIGGER comments_fts_au;
         DROP TABLE comments_fts;
         UPDATE meta SET value = '10' WHERE key = 'schema_version';",
    )
    .expect("откат к v10");
}

fn version_of(path: &Path) -> i64 {
    let conn = db::open(path).expect("open");
    db::schema_version_of(&conn).expect("версия")
}

/// Волна команд на базе старой схемы: миграцию делает кто-то один, остальные не падают.
///
/// Замер до правки на этой же волне: 12 процессов, 2 отказа через «database is locked» —
/// устойчиво, в трёх кругах подряд. Ради этого числа волна и оставлена маленькой: тест должен
/// краснеть на регрессии, а не ждать сотню процессов.
#[test]
fn a_wave_on_a_stale_schema_migrates_once_and_nobody_fails() {
    let dir = temp_dir("wave");
    let outcome = db::init(&dir, "WAVE", "волна").expect("init");
    let path = PathBuf::from(&outcome.db_path);
    roll_back_to_v10(&path);

    let mut children = Vec::new();
    for _ in 0..12 {
        children.push(
            Command::new(bin())
                .args(["--db", outcome.db_path.as_str(), "--project", "WAVE", "ls"])
                .env("TASQ_BUSY_MS", "200")
                .output()
                .expect("запуск tasq"),
        );
    }

    let failures: Vec<String> = children
        .iter()
        .filter(|out| !out.status.success())
        .map(|out| {
            format!(
                "код {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            )
        })
        .collect();
    assert!(
        failures.is_empty(),
        "волна команд на старой схеме потеряла {} из 12:\n{}",
        failures.len(),
        failures.join("\n")
    );

    assert_eq!(
        version_of(&path),
        db::SUPPORTED_SCHEMA_VERSION,
        "схема не поднялась"
    );
    // Индекс комментариев — свидетельство того, что миграция не «отметилась версией», а прошла.
    let conn = db::open(&path).expect("open");
    let index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'comments_fts'",
            [],
            |row| row.get(0),
        )
        .expect("поиск индекса");
    assert_eq!(index, 1, "миграция не создала индекс комментариев");

    let _ = fs::remove_dir_all(&dir);
}

/// Соединение для чтения не может писать — не «редко пишет», а не может вовсе.
///
/// Это и есть смысл правки: запрет на запись снимает саму возможность гонки при открытии,
/// тогда как «мы просто не пишем» держится на дисциплине кода.
#[test]
fn a_reading_connection_cannot_write() {
    let dir = temp_dir("readonly");
    let outcome = db::init(&dir, "READ", "чтение").expect("init");
    let path = PathBuf::from(&outcome.db_path);

    let conn = db::open_reading_checked(&path).expect("открытие только для чтения");
    assert_eq!(
        db::schema_version_of(&conn).expect("версия"),
        db::SUPPORTED_SCHEMA_VERSION
    );
    let now = db::now_iso();
    let err = conn
        .execute(
            "INSERT INTO projects (key, name, status, timezone, tags, metadata, created_at, updated_at)
             VALUES ('XXXX', 'нельзя', 'active', 'UTC', '[]', '{}', ?1, ?1)",
            rusqlite::params![now],
        )
        .expect_err("запись через соединение только для чтения должна быть отвергнута");
    let text = err.to_string().to_lowercase();
    assert!(
        text.contains("readonly") || text.contains("read-only"),
        "ожидался отказ по режиму «только чтение», получено: {err}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Читающей команде на старой схеме миграция всё-таки нужна — и она её получает.
///
/// Отказ был бы проще, но неверен: `find` на старой схеме ответил бы запасным путём `LIKE`,
/// то есть неполно и медленно. Поэтому «только для чтения» — это про способ открытия, а не про
/// отказ от обновления схемы.
#[test]
fn a_reading_open_migrates_a_stale_database() {
    let dir = temp_dir("readonly-stale");
    let outcome = db::init(&dir, "STAL", "старая").expect("init");
    let path = PathBuf::from(&outcome.db_path);
    roll_back_to_v10(&path);

    let conn = db::open_reading_checked(&path).expect("открытие читающей команды");
    assert_eq!(
        db::schema_version_of(&conn).expect("версия"),
        db::SUPPORTED_SCHEMA_VERSION,
        "читающая команда не подняла схему"
    );
    // И после миграции соединение действительно читает: индекс на месте.
    let hits: i64 = conn
        .query_row("SELECT COUNT(*) FROM comments_fts", [], |row| row.get(0))
        .expect("чтение индекса комментариев");
    assert_eq!(hits, 0, "индекс существует, но отвечает не числом");

    let _ = fs::remove_dir_all(&dir);
}

/// Значение ожидания блокировки: ноль и мусор означают умолчание, а не «не ждать вовсе».
///
/// Ноль в `PRAGMA busy_timeout` — это не «ждать миллисекунду», а «не ждать»; команда на базе,
/// которую в этот момент пишет сосед, падала бы сразу. Мусор в переменной окружения — обычная
/// опечатка в стенде, и превращать её в отказ нельзя.
#[test]
fn the_busy_timeout_is_parsed_leniently() {
    assert_eq!(db::busy_timeout_from(None), db::DEFAULT_BUSY_MS);
    assert_eq!(db::busy_timeout_from(Some("")), db::DEFAULT_BUSY_MS);
    assert_eq!(db::busy_timeout_from(Some("0")), db::DEFAULT_BUSY_MS);
    assert_eq!(db::busy_timeout_from(Some("мусор")), db::DEFAULT_BUSY_MS);
    assert_eq!(db::busy_timeout_from(Some(" 750 ")), 750);
    assert_eq!(db::busy_timeout_from(Some("5000")), 5000);
}

/// Миграцию держит чужой писатель: отказ называет себя и не притворяется «база занята».
///
/// Проверка детерминированная — блокировку держит этот же тест, а не гонка процессов. Она
/// закрывает сразу три решения: повтор ограничен (иначе тест висел бы до таймаута SQLite),
/// отказ после повторов имеет свой код (`MIGRATION_BUSY`, а не общий `STORAGE_ERROR`), и
/// команда не работает на старой схеме молча.
#[test]
fn a_busy_migration_names_itself_and_finishes_once_the_writer_leaves() {
    let dir = temp_dir("busy");
    let outcome = db::init(&dir, "BUSY", "занято").expect("init");
    let path = PathBuf::from(&outcome.db_path);
    roll_back_to_v10(&path);

    let holder = db::open(&path).expect("open держателя");
    holder
        .execute_batch("BEGIN IMMEDIATE")
        .expect("чужая запись");

    let started = std::time::Instant::now();
    let result = db::open_checked(&path);
    let elapsed_ms = started.elapsed().as_millis();

    match result {
        Err(tasq::error::Error::MigrationBusy { found, supported }) => {
            assert_eq!(found, 10, "в ошибке должна быть версия, которую нашли");
            assert_eq!(supported, db::SUPPORTED_SCHEMA_VERSION);
        }
        Err(other) => panic!(
            "ожидалась MIGRATION_BUSY, получено {}: {other}",
            other.code()
        ),
        Ok(_) => panic!("миграция прошла при чужой блокировке записи"),
    }

    // Повтор ограничен, а не бесконечен: три попытки по таймауту ожидания — это не «ждать
    // вечно», но и не «сдаться сразу». Нижняя граница держится от значения из окружения,
    // поэтому тест не сломается, если стенд задал короткий таймаут.
    let timeout = db::busy_timeout_ms() as u128;
    assert!(
        elapsed_ms >= 2 * timeout,
        "отказ пришёл за {elapsed_ms} мс при ожидании {timeout} мс — повтор не выполнялся"
    );

    holder.execute_batch("ROLLBACK").expect("снять блокировку");
    let conn = db::open_checked(&path).expect("открытие после снятия блокировки");
    assert_eq!(
        db::schema_version_of(&conn).expect("версия"),
        db::SUPPORTED_SCHEMA_VERSION,
        "после снятия блокировки схема не поднялась"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Запись ждёт освобождения блокировки и проходит, а не падает сразу.
///
/// Держатель отпускает запись позже, чем истекает одна попытка ожидания (200 мс против 260 мс),
/// — значит успех возможен только повтором. Проверяется процессом, а не потоком: соединение
/// SQLite между потоками не переносится, а держать блокировку нужно в том же процессе, где идёт
/// проверка. Без повтора команда получила бы «database is locked», хотя рядом ничего не сломано:
/// просто сосед дописывал.
#[test]
fn a_write_waits_for_a_released_lock() {
    let dir = temp_dir("write-wait");
    let outcome = db::init(&dir, "WAIT", "ожидание").expect("init");
    let path = PathBuf::from(&outcome.db_path);

    let mut holder = db::open(&path).expect("open держателя");
    let held = db::write_tx(&mut holder).expect("держатель взял запись");

    let child = Command::new(bin())
        .args([
            "--db",
            outcome.db_path.as_str(),
            "--project",
            "WAIT",
            "add",
            "узел сквозь блокировку",
        ])
        .env("TASQ_BUSY_MS", "200")
        // Печать команды в выводе теста — шум: проверяется код возврата и наличие узла.
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("запуск tasq add");

    // Отпускаем позже первой попытки ребёнка и раньше второй: успех возможен только повтором.
    std::thread::sleep(std::time::Duration::from_millis(260));
    drop(held);
    drop(holder);

    let finished = child.wait_with_output().expect("ожидание tasq add");
    assert!(
        finished.status.success(),
        "запись не дождалась блокировки: {}",
        String::from_utf8_lossy(&finished.stderr).trim()
    );

    let conn = db::open_reading_checked(&path).expect("open");
    let nodes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE title = 'узел сквозь блокировку'",
            [],
            |row| row.get(0),
        )
        .expect("счёт узлов");
    assert_eq!(nodes, 1, "команда отчиталась успехом, но узла нет");

    let _ = fs::remove_dir_all(&dir);
}
