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

/// Волна команд на базе старой схемы: все отказывают одинаково и ничего не пишут.
///
/// Это проверка главной гарантии TASQDEV-20.2: открытие не обновляет схему. Раньше здесь
/// проверялось обратное — что волна мигрирует ровно один раз и никто не падает на «база
/// занята». Оба требования несовместимы: авто-миграция при открытии и есть та запись, за
/// которую волна борется. Выбрано «не писать»: обновление — решение, у него своя команда.
///
/// Замер до правки на этой же волне: 12 процессов, 2 отказа через «database is locked»,
/// три круга подряд. После: 12 отказов `SCHEMA_OUTDATED`, файл базы не изменён.
#[test]
fn a_wave_on_a_stale_schema_refuses_and_writes_nothing() {
    let dir = temp_dir("wave");
    let outcome = db::init(&dir, "WAVE", "волна").expect("init");
    let path = PathBuf::from(&outcome.db_path);
    roll_back_to_v10(&path);

    // Размер файла и журнала до волны: любая запись их изменит или создаст `-wal`.
    let before = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    let wal_before = fs::metadata(format!("{}-wal", path.display()))
        .map(|meta| meta.len())
        .unwrap_or(0);

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

    let mut codes = Vec::new();
    for out in &children {
        assert!(
            !out.status.success(),
            "команда на старой схеме обязана отказать, а не работать"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("SCHEMA_OUTDATED"),
            "ожидался отказ с кодом SCHEMA_OUTDATED, получено: {stderr}"
        );
        assert!(
            stderr.contains("tasq migrate"),
            "отказ обязан называть команду: {stderr}"
        );
        codes.push(stderr);
    }

    assert_eq!(
        version_of(&path),
        10,
        "волна подняла схему — значит кто-то всё-таки писал"
    );
    let after = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    let wal_after = fs::metadata(format!("{}-wal", path.display()))
        .map(|meta| meta.len())
        .unwrap_or(0);
    assert_eq!(before, after, "размер базы изменился — открытие писало");
    assert_eq!(wal_before, wal_after, "появился журнал — открытие писало");

    // Отказов «занято» быть не должно вовсе: никто не просил запись.
    let busy = codes
        .iter()
        .filter(|text| text.contains("database is locked") || text.contains("MIGRATION_BUSY"))
        .count();
    assert_eq!(
        busy, 0,
        "{busy} команд упёрлись в блокировку вместо отказа по схеме"
    );

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

/// Читающее открытие на старой схеме отказывает, а не мигрирует: чтение не пишет никогда.
///
/// Отказ здесь — часть той же гарантии, что и у обычного открытия, но проверять его надо
/// отдельно: `open_reading_checked` умеет отступать к обычному соединению, и однажды в этом
/// отступлении может появиться ветка миграции — тогда чтение снова начнёт брать запись
/// незаметно. Тест ловит именно это.
#[test]
fn a_reading_open_refuses_a_stale_schema_without_writing() {
    let dir = temp_dir("readonly-stale");
    let outcome = db::init(&dir, "STAL", "старая").expect("init");
    let path = PathBuf::from(&outcome.db_path);
    roll_back_to_v10(&path);

    let err = db::open_reading_checked(&path).expect_err("чтение старой схемы должно отказать");
    assert_eq!(err.code(), "SCHEMA_OUTDATED", "{err}");
    assert_eq!(
        version_of(&path),
        10,
        "читающее открытие подняло схему — значит писало"
    );

    // И после явной миграции то же открытие работает и видит индекс.
    db::migrate(&path).expect("migrate");
    let conn = db::open_reading_checked(&path).expect("чтение после миграции");
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
    let result = db::migrate(&path);
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
    let conn = db::open_checked(&path).expect_err("схема всё ещё старая, миграции не было");
    assert_eq!(conn.code(), "SCHEMA_OUTDATED");
    db::migrate(&path).expect("миграция после снятия блокировки");
    let conn = db::open_checked(&path).expect("открытие после миграции");
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

/// Сброс журнала при занятом читателе не ждёт его, а честно сообщает остаток.
///
/// Раньше здесь стоял `wal_checkpoint(TRUNCATE)`: он не усекает файл, пока читатель держит
/// снимок, и ждёт освобождения до `busy_timeout`. При сотне агентов это значит «фоновая задача
/// стоит в ожидании каждый тик и не сбрасывает ничего» — то есть окно потерь не закрывается
/// именно тогда, когда оно растёт. PASSIVE переливает столько, сколько можно, и сразу
/// возвращает остаток.
#[test]
fn a_checkpoint_does_not_wait_for_a_reader() {
    let dir = temp_dir("checkpoint-reader");
    let outcome = db::init(&dir, "FLSH", "сброс").expect("init");
    let path = PathBuf::from(&outcome.db_path);

    let mut writer = db::open(&path).expect("writer");
    let tx = db::write_tx(&mut writer).expect("tx");
    tx.execute(
        "INSERT INTO activity_log (project_id, entity_type, entity_id, action, created_at)
         VALUES (1, 'project', 1, 'probe', '2026-01-01T00:00:00.000Z')",
        [],
    )
    .expect("insert");
    tx.commit().expect("commit");

    // Читатель держит снимок до конца теста: кадры новее его — недоступны для переливки.
    let reader = db::open(&path).expect("reader");
    reader.execute_batch("BEGIN").expect("read tx");
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
        .expect("read");
    let mut writer = db::open(&path).expect("writer второй");
    let tx = db::write_tx(&mut writer).expect("tx второй");
    tx.execute(
        "INSERT INTO activity_log (project_id, entity_type, entity_id, action, created_at)
         VALUES (1, 'project', 1, 'probe2', '2026-01-01T00:00:00.000Z')",
        [],
    )
    .expect("insert второй");
    tx.commit().expect("commit второй");

    let flusher = db::open_light(&path).expect("flusher");
    let started = std::time::Instant::now();
    let pending = db::checkpoint(&flusher).expect("checkpoint");
    let elapsed_ms = started.elapsed().as_millis();

    assert!(
        elapsed_ms < db::busy_timeout_ms() as u128,
        "сброс ждал читателя {elapsed_ms} мс — значит усечение всё ещё впереди переливки"
    );
    assert!(
        pending > 0,
        "при занятом читателе должен остаться неперелитый остаток, получено {pending}"
    );

    reader.execute_batch("COMMIT").expect("release");
    let after = db::checkpoint(&flusher).expect("checkpoint после читателя");
    assert_eq!(after, 0, "после освобождения остатка нет");

    let _ = fs::remove_dir_all(&dir);
}

/// Файл журнала усекается только когда дорос до потолка, а не на каждом тике.
///
/// PASSIVE не уменьшает WAL никогда (замер: 6,8 МБ до и после, при любом
/// `journal_size_limit`), поэтому усечение — отдельный шаг. Делать его каждый тик значило бы
/// платить ожиданием читателя постоянно; делать никогда — значит держать журнал на максимуме
/// вечно. Порог и есть компромисс, и он проверяется с обеих сторон.
#[test]
fn the_journal_is_truncated_only_above_the_limit() {
    let dir = temp_dir("journal-limit");
    let outcome = db::init(&dir, "WALS", "журнал").expect("init");
    let path = PathBuf::from(&outcome.db_path);
    let conn = db::open(&path).expect("open");
    // Авто-переливка выключена: иначе SQLite сам уменьшит журнал и замер потеряет смысл.
    conn.pragma_update(None, "wal_autocheckpoint", 0)
        .expect("pragma");

    let mut writer = db::open(&path).expect("writer");
    let tx = db::write_tx(&mut writer).expect("tx");
    let blob = "ж".repeat(4000);
    for _ in 0..120 {
        tx.execute(
            "INSERT INTO activity_log (project_id, entity_type, entity_id, action, created_at)
             VALUES (1, 'project', 1, ?1, '2026-01-01T00:00:00.000Z')",
            rusqlite::params![blob],
        )
        .expect("insert");
    }
    tx.commit().expect("commit");

    let wal = |path: &Path| {
        fs::metadata(format!("{}-wal", path.display()))
            .map(|meta| meta.len())
            .unwrap_or(0)
    };
    let grown = wal(&path);
    assert!(grown > 0, "после записи журнал должен быть непустым");

    // Потолок выше файла — журнал остаётся: усечение не самоцель.
    let pending =
        db::checkpoint_with_limit(&conn, grown as i64 * 4).expect("checkpoint ниже порога");
    assert_eq!(pending, 0, "кадры перелиты");
    assert_eq!(wal(&path), grown, "файл не должен усекаться ниже порога");

    // Потолок ниже файла — усекается до нуля.
    let pending = db::checkpoint_with_limit(&conn, 0).expect("checkpoint выше порога");
    assert_eq!(pending, 0, "кадры перелиты");
    assert_eq!(wal(&path), 0, "выше потолка журнал усечён");

    let _ = fs::remove_dir_all(&dir);
}
