//! Экспорт и импорт проекта вместе с историей переходов.
//!
//! Зачем это в отдельном файле: до сих пор `export` нёс состав проекта, но не историю, и
//! `import` в чистую базу давал `done 0` при закрытом узле на месте — burn-up терял всю
//! закрытую работу. Здесь проверяется, что история едет, а когда её в файле нет — что импорт
//! честно говорит «истории не было», а не выдумывает её.

use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;
use serde_json::json;

use tasq::db;
use tasq::lease::{self, ClaimRequest, Outcome};
use tasq::node::{self, NewNode, Patch};
use tasq::project::{self, Project};
use tasq::status::Status;
use tasq::transfer;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-exphist-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn fixture(tag: &str) -> (Connection, Project, PathBuf) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "ALPHA", "alpha").expect("init");
    let mut conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");
    let epic = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: None,
            title: "Эпик",
            kind: None,
            description: None,
            priority: None,
            due_date: None,
            estimated_hours: None,
            weight: None,
            source_ref: None,
            branch: None,
        },
    )
    .expect("epic");
    let task = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: Some(&epic.dot_id),
            title: "Закрытая",
            kind: None,
            description: None,
            priority: None,
            due_date: None,
            estimated_hours: None,
            weight: None,
            source_ref: None,
            branch: None,
        },
    )
    .expect("task");
    node::update(
        &mut conn,
        &project,
        &task.dot_id,
        Patch {
            status: Some(Status::Todo),
            actor: Some("человек"),
            ..Patch::default()
        },
    )
    .expect("todo");
    let held = lease::claim(
        &mut conn,
        &project,
        &task.dot_id,
        &ClaimRequest {
            holder: "agent",
            ttl_seconds: 600,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
    )
    .expect("claim");
    lease::release(
        &mut conn,
        &project,
        &held.token,
        Outcome::Completed,
        None,
        false,
    )
    .expect("complete");
    let db_path = PathBuf::from(&outcome.db_path);
    (conn, project, db_path)
}

fn history(conn: &Connection) -> Vec<(Option<String>, String, String, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT from_status, to_status, cause, at FROM node_status_history
             ORDER BY node_id, id",
        )
        .expect("prepare");
    stmt.query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    })
    .expect("query")
    .collect::<Result<Vec<_>, _>>()
    .expect("rows")
}

/// Экспорт несёт историю вместе с её причинами и временами, и говорит об этом маркером.
#[test]
fn the_export_carries_the_history() {
    let (conn, project, _path) = fixture("export");
    let document = transfer::export(&conn, &project, false).expect("export");
    let data = &document["data"];

    assert_eq!(data["history"], "present", "маркер наличия истории");
    let rows = data["status_history"].as_array().expect("status_history");

    // Пять фактов на два узла: создание каждого и три перехода задачи. Создание — тоже факт:
    // без него у нового узла истории нет, и он выглядит «неизвестным».
    let causes: Vec<&str> = rows
        .iter()
        .map(|row| row["cause"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        causes,
        vec![
            "created",
            "created",
            "manual",
            "lease_claimed",
            "lease_completed"
        ]
    );
    // Адресация путём, а не id: путь переносим между базами.
    assert_eq!(rows[0]["node"], "1");
    assert_eq!(rows[1]["node"], "1.1");
    assert!(rows[0]["at"].as_str().is_some_and(|at| !at.is_empty()));
}

/// Импорт переносит историю как есть: причины и времена не переписываются.
///
/// Иначе ряд burn-up съехал бы на день импорта — то есть отчёт врал бы ровно там, где его читают.
#[test]
fn the_import_keeps_the_original_causes_and_times() {
    let (conn, project, _path) = fixture("import");
    let document = transfer::export(&conn, &project, false).expect("export");
    let source = history(&conn);

    let dir = temp_dir("import-target");
    let target = db::init(&dir, "ALPHA", "alpha").expect("init").db_path;
    let mut target_conn = db::open(std::path::Path::new(&target)).expect("open");
    let target_project = project::first_project(&target_conn).expect("project");

    let outcome = transfer::import(
        &mut target_conn,
        &target_project,
        &document,
        transfer::Mode::Merge,
        "importer",
    )
    .expect("import");
    assert_eq!(
        outcome.history,
        Some(source.len()),
        "перенесено не столько строк, сколько было"
    );

    let restored = history(&target_conn);
    assert_eq!(
        restored.len(),
        source.len(),
        "число строк истории разошлось"
    );
    for (index, (from, to, cause, at)) in restored.iter().enumerate() {
        let (source_from, source_to, source_cause, source_at) = &source[index];
        assert_eq!(from, source_from);
        assert_eq!(to, source_to);
        assert_eq!(cause, source_cause, "причина перехода переписана");
        assert_eq!(at, source_at, "время перехода переписано");
    }
}

/// Файл без истории (старый формат или чужая база): на каждый узел одна запись `import`, с
/// временем создания узла, а не моментом импорта.
#[test]
fn a_file_without_history_gets_one_import_fact_per_node() {
    let (conn, project, _path) = fixture("synthesize");
    let mut document = transfer::export(&conn, &project, false).expect("export");
    // Так выглядит файл, снятый до появления истории: маркера нет, массива нет.
    let data = document["data"].as_object_mut().expect("data");
    data.remove("history");
    data.remove("status_history");
    let checksum = transfer::payload_checksum(&serde_json::Value::Object(data.clone()));
    document["sha256"] = json!(checksum);

    let dir = temp_dir("synthesize-target");
    let target = db::init(&dir, "ALPHA", "alpha").expect("init").db_path;
    let mut target_conn = db::open(std::path::Path::new(&target)).expect("open");
    let target_project = project::first_project(&target_conn).expect("project");
    let outcome = transfer::import(
        &mut target_conn,
        &target_project,
        &document,
        transfer::Mode::Merge,
        "importer",
    )
    .expect("import");
    assert_eq!(
        outcome.history,
        Some(2),
        "по записи на каждый узел: {outcome:?}"
    );

    let rows = history(&target_conn);
    assert_eq!(rows.len(), 2, "{rows:?}");
    for (from, _to, cause, _at) in &rows {
        assert_eq!(
            from, &None,
            "у синтетической записи нет предыдущего статуса"
        );
        assert_eq!(cause, "import");
    }

    // Время — создания узла, а не импорта: узел не появился сейчас, он сейчас доехал.
    let node_created: String = target_conn
        .query_row(
            "SELECT created_at FROM nodes ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("created_at");
    assert_eq!(rows[0].3, node_created);
}

/// Неизвестная причина в файле — отказ до записи: молчаливая подмена на `manual` исказила бы
/// отчёты, ради которых таблица и заведена.
#[test]
fn an_unknown_cause_is_refused_before_writing() {
    let (conn, project, _path) = fixture("badcause");
    let mut document = transfer::export(&conn, &project, false).expect("export");
    let data = document["data"].as_object_mut().expect("data");
    let rows = data["status_history"].as_array_mut().expect("rows");
    rows[0]["cause"] = json!("взял и бросил");
    let checksum = transfer::payload_checksum(&serde_json::Value::Object(data.clone()));
    document["sha256"] = json!(checksum);

    let dir = temp_dir("badcause-target");
    let target = db::init(&dir, "ALPHA", "alpha").expect("init").db_path;
    let mut target_conn = db::open(std::path::Path::new(&target)).expect("open");
    let target_project = project::first_project(&target_conn).expect("project");

    let error = transfer::import(
        &mut target_conn,
        &target_project,
        &document,
        transfer::Mode::Merge,
        "importer",
    )
    .expect_err("неизвестная причина принята");
    assert_eq!(error.code(), "INVALID_VALUE", "{error}");

    let nodes: i64 = target_conn
        .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
        .expect("count");
    assert_eq!(nodes, 0, "отказ пришёл после записи");
}
