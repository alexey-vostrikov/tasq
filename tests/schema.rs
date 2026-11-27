//! Проверки схемы: представления должны быть не только объявлены, но и работать на данных.

use std::fs;
use std::path::PathBuf;

use rusqlite::params;

use tasq::db;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-schema-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Готовит базу с проектом, эпиком, задачей и подзадачей; возвращает соединение и id эпика.
fn seeded() -> (rusqlite::Connection, i64, i64) {
    let dir = temp_dir("seed");
    let outcome = db::init(&dir, "TASQ", "demo").expect("init");
    let conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let now = db::now_iso();

    let epic: i64 = {
        conn.execute(
            "INSERT INTO nodes (project_id, parent_id, depth, seq, path, kind, title, status,
                                weight, created_at, updated_at)
             VALUES (1, NULL, 1, 12, '12', 'epic', 'Epic', 'todo', 5, ?1, ?1)",
            params![now],
        )
        .expect("insert epic");
        conn.last_insert_rowid()
    };
    let task: i64 = {
        conn.execute(
            "INSERT INTO nodes (project_id, parent_id, depth, seq, path, kind, title, status,
                                weight, created_at, updated_at)
             VALUES (1, ?1, 2, 3, '12.3', 'task', 'Task', 'done', 2, ?2, ?2)",
            params![epic, now],
        )
        .expect("insert task");
        conn.last_insert_rowid()
    };
    (conn, epic, task)
}

/// Досыпает в базу ребро и закрытую попытку: так все представления получают по строке.
fn add_edge_and_finished_attempt(conn: &rusqlite::Connection, epic: i64, task: i64) {
    let now = db::now_iso();
    conn.execute(
        "INSERT INTO edges (from_node, to_node, kind, created_at) VALUES (?1, ?2, 'blocks', ?3)",
        params![task, epic, now],
    )
    .expect("insert edge");
    conn.execute(
        "INSERT INTO attempts (node_id, attempt_no, holder, token, ttl_seconds,
                               started_at, expires_at, ended_at, end_reason)
         VALUES (?1, 1, 'codex', 'tok-ended', 900, ?2, ?2, ?2, 'released')",
        params![task, now],
    )
    .expect("insert finished attempt");
    // просроченный узел, чтобы v_overdue тоже отдавал строку
    conn.execute(
        "UPDATE nodes SET due_date = '2000-01-01' WHERE id = ?1",
        params![epic],
    )
    .expect("set due date");
}

#[test]
fn every_view_is_queryable() {
    let (conn, epic, task) = seeded();
    add_edge_and_finished_attempt(&conn, epic, task);
    for view in [
        "v_node_effective",
        "v_node_progress",
        "v_blockers",
        "v_overdue",
        "v_attempt_hours",
    ] {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {view}"))
            .unwrap_or_else(|e| panic!("view {view} does not compile: {e}"));
        let rows = stmt
            .query_map([], |_| Ok(()))
            .unwrap_or_else(|e| panic!("view {view} does not run: {e}"))
            .count();
        assert!(rows > 0, "view {view} returned no rows on seeded data");
    }
}

#[test]
fn effective_status_derives_from_live_lease() {
    let (conn, epic, _) = seeded();
    let now = db::now_iso();
    let status_of = |id: i64| -> String {
        conn.query_row(
            "SELECT effective_status FROM v_node_effective WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .expect("effective status")
    };
    assert_eq!(status_of(epic), "todo");

    conn.execute(
        "INSERT INTO attempts (node_id, attempt_no, holder, token, ttl_seconds,
                               started_at, expires_at)
         VALUES (?1, 1, 'claude-code', 'tok-1', 900, ?2, '2999-01-01T00:00:00.000Z')",
        params![epic, now],
    )
    .expect("insert attempt");
    assert_eq!(
        status_of(epic),
        "in_progress",
        "живая аренда делает статус производным"
    );

    // истёкшая аренда статус не держит
    conn.execute(
        "UPDATE attempts SET expires_at = '2000-01-01T00:00:00.000Z' WHERE node_id = ?1",
        params![epic],
    )
    .expect("expire attempt");
    assert_eq!(
        status_of(epic),
        "todo",
        "истёкшая аренда не держит in_progress"
    );
}

#[test]
fn only_one_live_attempt_per_node() {
    let (conn, epic, _) = seeded();
    let now = db::now_iso();
    let insert = |no: i64, token: &str| {
        conn.execute(
            "INSERT INTO attempts (node_id, attempt_no, holder, token, ttl_seconds,
                                   started_at, expires_at)
             VALUES (?1, ?2, 'agent', ?3, 900, ?4, '2999-01-01T00:00:00.000Z')",
            params![epic, no, token, now],
        )
    };
    insert(1, "tok-a").expect("first attempt");
    let second = insert(2, "tok-b");
    assert!(
        second.is_err(),
        "вторая живая попытка должна быть отвергнута индексом"
    );
}

#[test]
fn progress_rolls_up_to_parent() {
    let (conn, epic, task) = seeded();
    let (done, total): (f64, f64) = conn
        .query_row(
            "SELECT done_weight, total_weight FROM v_node_progress WHERE node_id = ?1",
            params![epic],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("progress row");
    assert_eq!(total, 7.0, "эпик + задача по весу");
    assert_eq!(done, 2.0, "задача в done даёт 2");

    let (task_done, _): (f64, f64) = conn
        .query_row(
            "SELECT done_weight, total_weight FROM v_node_progress WHERE node_id = ?1",
            params![task],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("progress row for task");
    assert_eq!(task_done, 2.0, "свой вес считается в свой прогресс");
}

#[test]
fn blocked_status_follows_unfinished_blocker() {
    let (conn, epic, task) = seeded();
    let now = db::now_iso();
    let sub: i64 = {
        conn.execute(
            "INSERT INTO nodes (project_id, parent_id, depth, seq, path, kind, title, status,
                                weight, created_at, updated_at)
             VALUES (1, ?1, 3, 1, '12.3.1', 'subtask', 'Sub', 'todo', 1, ?2, ?2)",
            params![task, now],
        )
        .expect("insert subtask");
        conn.last_insert_rowid()
    };
    conn.execute(
        "INSERT INTO edges (from_node, to_node, kind, created_at) VALUES (?1, ?2, 'blocks', ?3)",
        params![sub, epic, now],
    )
    .expect("insert edge");

    let (effective, blockers): (String, String) = conn
        .query_row(
            "SELECT e.effective_status, b.blocker_paths
             FROM v_node_effective e JOIN v_blockers b ON b.node_id = e.id
             WHERE e.id = ?1",
            params![sub],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("effective + blockers");
    assert_eq!(effective, "blocked");
    assert_eq!(blockers, "12", "блокер отдаётся своим dot-путём");
}

#[test]
fn schema_version_is_recorded() {
    let (conn, _, _) = seeded();
    let version: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .expect("schema_version");
    assert_eq!(version, db::SUPPORTED_SCHEMA_VERSION.to_string());
}

/// Отстающая база мигрирует при обычном открытии, а не только через `init` или восстановление.
/// Открытие отстающей базы отказывает и называет команду, а `tasq migrate` поднимает схему.
///
/// Тест живёт в двух состояниях намеренно, и это история одного решения. Сначала открытие
/// мигрировало само — потому что иначе `find` по комментариям молча отвечал запасным путём
/// `LIKE`, без индекса, и «работает, но иначе» было незаметно. Потом оказалось, что платить за
/// это приходится каждой волной команд: сто процессов на файле старой схемы все брались
/// обновлять её и теряли треть вызовов на «database is locked» (TASQDEV-19). Теперь открытие
/// только читает версию и отказывает кодом `SCHEMA_OUTDATED`, а обновление — отдельная команда.
///
/// Проверяется именно пара: отказ без записи, затем миграция с работающим индексом. Одного
/// отказа мало — он был бы выполнен и «отказывать всегда», что тоже неверно.
#[test]
fn an_outdated_database_refuses_to_open_and_migrates_on_command() {
    let dir = temp_dir("migrate-on-command");
    let outcome = db::init(&dir, "MIGR", "миграция").expect("init");
    let path = std::path::Path::new(&outcome.db_path);

    // Откатываем базу к состоянию до v11: убираем индекс с триггерами и ставим версию 10.
    // Так воспроизводится ровно то, что лежит у человека, обновившего бинарь.
    {
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

    // Обычное открытие — то же, с чего начинается любая команда CLI.
    let err = db::open_checked(path).expect_err("открытие старой схемы должно отказать");
    match err {
        tasq::error::Error::SchemaOutdated { found, supported } => {
            assert_eq!(found, 10);
            assert_eq!(supported, db::SUPPORTED_SCHEMA_VERSION);
        }
        other => panic!(
            "ожидался SCHEMA_OUTDATED, получен {}: {other}",
            other.code()
        ),
    }
    assert!(err_hint_mentions_migrate(&err));

    // И база при этом не тронута: индекс так и не появился.
    {
        let conn = db::open(path).expect("open");
        let index: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'comments_fts'",
                [],
                |row| row.get(0),
            )
            .expect("поиск индекса");
        assert_eq!(index, 0, "открытие всё-таки записало схему");
    }

    // Явная миграция — и схема та, и индекс на месте.
    let migration = db::migrate(path).expect("migrate");
    assert!(migration.applied, "миграция не применилась");
    assert_eq!(migration.from, 10);
    assert_eq!(migration.to, db::SUPPORTED_SCHEMA_VERSION);

    let conn = db::open_checked(path).expect("открытие после миграции");
    let index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'comments_fts'",
            [],
            |row| row.get(0),
        )
        .expect("поиск индекса");
    assert_eq!(index, 1, "индекс комментариев не создан миграцией");

    // И база после миграции рабочая: комментарий пишется и находится.
    let project = tasq::project::first_project(&conn).expect("проект");
    let node = tasq::node::create(
        &mut db::open_checked(path).expect("open"),
        &project,
        tasq::node::NewNode {
            parent: None,
            title: "Узел",
            kind: None,
            description: None,
            priority: None,
            due_date: None,
            estimated_hours: None,
            weight: None,
            source_ref: None,
            branch: None,
            tags: None,
        },
    )
    .expect("узел");
    let mut writer = db::open_checked(path).expect("open");
    tasq::comment::add(
        &mut writer,
        &project,
        &node.dot_id,
        "проверка слова фрегат",
        "тест",
    )
    .expect("комментарий");
    let hits = tasq::overview::search(&writer, &project, "фрегат").expect("поиск");
    assert_eq!(
        hits.len(),
        1,
        "поиск по комментарию не работает после миграции"
    );

    // Повтор миграции — «делать нечего», а не ошибка: скрипт обновления запускают дважды.
    let again = db::migrate(path).expect("повторная миграция");
    assert!(!again.applied, "повторная миграция что-то применила");
    assert_eq!(again.to, db::SUPPORTED_SCHEMA_VERSION);

    let _ = fs::remove_dir_all(dir);
}

/// Подсказка отказа должна называть команду: код без действия заставляет искать причину.
fn err_hint_mentions_migrate(err: &tasq::error::Error) -> bool {
    err.hint().contains("tasq migrate")
}
