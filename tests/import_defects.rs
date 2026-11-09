//! Отказы импорта: каждый дефект документа обязан давать свой код до первой записи.
//!
//! Проверяется не «ошибка вообще», а именно тот код, который увидит вызывающий: по нему
//! скрипт миграции решает, править файл или пересчитывать сумму. Документ собирается
//! здесь же, поэтому сумма считается `payload_checksum` — та же, что и в экспорте.

use std::path::PathBuf;

use rusqlite::Connection;
use serde_json::{Value, json};

use tasq::db;
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};
use tasq::transfer::{self, Mode};
use tasq::{burnup, lease};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-defect-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn fixture(tag: &str) -> (Connection, Project) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "TASQ", "demo").expect("init");
    let conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");
    (conn, project)
}

fn add(conn: &mut Connection, project: &Project, parent: Option<&str>, title: &str) -> node::Node {
    node::create(
        conn,
        project,
        NewNode {
            parent,
            title,
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
    .expect("create node")
}

/// Собирает документ обмена с правильной суммой: иначе проверка падала бы на сумме,
/// не доходя до проверяемого дефекта.
fn sealed(data: Value) -> Value {
    json!({
        "schema": transfer::SCHEMA,
        "version": "test",
        "exported_at": "2026-11-09T00:00:00.000Z",
        "sha256": transfer::payload_checksum(&data),
        "data": data,
    })
}

fn node_json(path: &str, title: &str) -> Value {
    json!({ "path": path, "title": title, "kind": if path.contains('.') { "task" } else { "epic" } })
}

fn refusal(conn: &mut Connection, project: &Project, document: Value) -> String {
    let error = transfer::import(conn, project, &document, Mode::Merge, "-")
        .expect_err("документ должен быть отвергнут");
    error.code().to_string()
}

#[test]
fn envelope_defects_are_named() {
    let (mut conn, project) = fixture("envelope");

    let mut without_hash = sealed(json!({ "nodes": [] }));
    without_hash
        .as_object_mut()
        .expect("object")
        .remove("sha256");
    assert_eq!(refusal(&mut conn, &project, without_hash), "INVALID_VALUE");

    let mut without_data = sealed(json!({ "nodes": [] }));
    without_data.as_object_mut().expect("object").remove("data");
    assert_eq!(refusal(&mut conn, &project, without_data), "INVALID_VALUE");

    let mut foreign = sealed(json!({ "nodes": [] }));
    foreign["schema"] = Value::String("other.export.v1".to_string());
    assert_eq!(refusal(&mut conn, &project, foreign), "INVALID_VALUE");
}

#[test]
fn node_defects_are_named() {
    let (mut conn, project) = fixture("nodes");

    // Нет списка узлов вовсе и список не того типа — один и тот же отказ.
    assert_eq!(
        refusal(&mut conn, &project, sealed(json!({}))),
        "INVALID_VALUE"
    );
    assert_eq!(
        refusal(&mut conn, &project, sealed(json!({ "nodes": "1" }))),
        "INVALID_VALUE"
    );

    // Узел без пути.
    assert_eq!(
        refusal(
            &mut conn,
            &project,
            sealed(json!({ "nodes": [{ "title": "без пути" }] }))
        ),
        "INVALID_VALUE"
    );

    // Путь не из чисел: такой узел не собрать в дерево.
    assert_eq!(
        refusal(
            &mut conn,
            &project,
            sealed(json!({ "nodes": [node_json("1.a", "плохой путь")] }))
        ),
        "INVALID_VALUE"
    );

    // Один путь дважды: второе вхождение отвергается как дубль ключа.
    assert_eq!(
        refusal(
            &mut conn,
            &project,
            sealed(json!({ "nodes": [node_json("1", "A"), node_json("1", "B")] }))
        ),
        "DUPLICATE_KEY"
    );

    // Родитель, которого нет в файле.
    assert_eq!(
        refusal(
            &mut conn,
            &project,
            sealed(json!({ "nodes": [json!({
                "path": "1.1",
                "title": "сирота",
                "parent_id_path": "9"
            })] }))
        ),
        "INVALID_VALUE"
    );
}

#[test]
fn edge_defects_are_named() {
    let (mut conn, project) = fixture("edges");

    assert_eq!(
        refusal(
            &mut conn,
            &project,
            sealed(json!({
                "nodes": [node_json("1", "A")],
                "edges": [{ "from": "1", "to": "2", "kind": "blocks" }]
            }))
        ),
        "INVALID_VALUE"
    );

    assert_eq!(
        refusal(
            &mut conn,
            &project,
            sealed(json!({
                "nodes": [node_json("1", "A")],
                "edges": [{ "from": "1", "to": "1", "kind": "blocks" }]
            }))
        ),
        "SELF_EDGE"
    );
}

#[test]
fn memory_references_must_exist_in_the_file() {
    let (mut conn, project) = fixture("memory");

    assert_eq!(
        refusal(
            &mut conn,
            &project,
            sealed(json!({
                "nodes": [node_json("1", "A")],
                "notes": [{ "node": "9", "title": "заметка", "content": "c" }]
            }))
        ),
        "INVALID_VALUE"
    );

    assert_eq!(
        refusal(
            &mut conn,
            &project,
            sealed(json!({
                "nodes": [node_json("1", "A")],
                "comments": [{ "node": "9", "content": "комментарий" }]
            }))
        ),
        "INVALID_VALUE"
    );

    // Комментарий без ссылки на узел — тоже отказ: привязать его не к чему.
    assert_eq!(
        refusal(
            &mut conn,
            &project,
            sealed(json!({
                "nodes": [node_json("1", "A")],
                "comments": [{ "content": "без узла" }]
            }))
        ),
        "INVALID_VALUE"
    );
}

/// Снимок прогноза переезжает вместе с проектом: экспорт его пишет, импорт восстанавливает.
#[test]
fn snapshots_survive_the_round_trip() {
    let (mut conn, project) = fixture("snapshots");
    let epic = add(&mut conn, &project, None, "Эпик");
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Задача");

    // Завершённая работа даёт непустой снимок.
    let held = lease::claim(
        &mut conn,
        &project,
        &task.dot_id,
        &lease::ClaimRequest {
            holder: "agent-a",
            ttl_seconds: lease::DEFAULT_TTL,
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
        lease::Outcome::Completed,
        None,
        false,
    )
    .expect("release");

    let written =
        burnup::snapshot_all(&mut conn, &project, burnup::Metric::Count).expect("snapshot");
    assert!(written > 0, "снимок должен записаться");

    let document = transfer::export(&conn, &project, false).expect("export");
    let snapshots = document["data"]["snapshots"]
        .as_array()
        .expect("snapshots array");
    assert!(!snapshots.is_empty(), "экспорт пишет снимки: {document}");
    assert!(snapshots[0]["day"].is_string(), "{document}");

    // Импорт в чистую базу возвращает те же строки.
    let (mut target, target_project) = fixture("snapshots-target");
    let report = transfer::import(&mut target, &target_project, &document, Mode::Merge, "-")
        .expect("import");
    assert_eq!(report.nodes_created, 2, "{report:?}");

    let again = transfer::export(&target, &target_project, false).expect("re-export");
    assert_eq!(
        again["data"]["snapshots"], document["data"]["snapshots"],
        "снимки переехали без потерь"
    );
}

/// Импорт saga проверяет обязательные поля до записи.
#[test]
fn saga_document_defects_are_named() {
    let (mut conn, project) = fixture("saga-defects");

    let base = json!({ "format_version": "1.3" });

    let mut no_project = base.clone();
    no_project["project"] = Value::Null;
    let error = transfer::import_saga(&mut conn, &project, &no_project, Mode::Merge, "-")
        .expect_err("без project");
    assert_eq!(error.code(), "INVALID_VALUE");

    let mut no_name = base.clone();
    no_name["project"] = json!({ "epics": [] });
    let error = transfer::import_saga(&mut conn, &project, &no_name, Mode::Merge, "-")
        .expect_err("без имени проекта");
    assert_eq!(error.code(), "INVALID_VALUE");

    let mut no_epics = base.clone();
    no_epics["project"] = json!({ "name": "Пустой", "epics": [] });
    let error = transfer::import_saga(&mut conn, &project, &no_epics, Mode::Merge, "-")
        .expect_err("без эпиков");
    assert_eq!(error.code(), "INVALID_VALUE");
}

/// Статусы saga, которых нет в схеме tasq, получают разумное место в матрице:
/// эпик — `planned`, работа — `todo`. Иначе дерево импортировалось бы без статуса.
#[test]
fn unknown_saga_statuses_fall_back_by_kind() {
    let (mut conn, project) = fixture("saga-status");
    let document = json!({
        "format_version": "1.3",
        "project": {
            "name": "Странные статусы",
            "epics": [{
                "_original_id": 1,
                "name": "Эпик",
                "status": "hold-on-for-really",
                "tasks": [{
                    "_original_id": 2,
                    "title": "Задача",
                    "status": "hold-on-for-really",
                    "subtasks": [{
                        "_original_id": 3,
                        "title": "Подзадача",
                        "status": "hold-on-for-really"
                    }]
                }]
            }]
        }
    });

    let report =
        transfer::import_saga(&mut conn, &project, &document, Mode::Merge, "-").expect("import");
    assert_eq!(report.nodes_created, 3, "{report:?}");

    let nodes = node::list(&conn, &project, &node::Filter::default()).expect("list");
    let epic = nodes.iter().find(|n| n.kind == "epic").expect("эпик");
    assert_eq!(epic.status, "planned");
    let task = nodes.iter().find(|n| n.kind == "task").expect("задача");
    assert_eq!(task.status, "todo");
}

/// Ссылки saga на сущности, которых нет в файле, тихо пропускаются: связи
/// восстанавливаются по картам id, и отсутствие записи — не повод ронять импорт.
#[test]
fn dangling_saga_references_are_skipped() {
    let (mut conn, project) = fixture("saga-dangling");
    let document = json!({
        "format_version": "1.3",
        "project": {
            "name": "Висячие ссылки",
            "epics": [{
                "_original_id": 1,
                "name": "Эпик",
                "tasks": [{
                    "_original_id": 2,
                    "title": "Задача",
                    "status": "todo",
                    "depends_on": [999]
                }]
            }]
        },
        "notes": [
            { "entity_type": "task", "entity_id": 999, "title": "заметка", "content": "c" },
            { "entity_type": "epic", "entity_id": 1, "title": "заметка эпика", "content": "c" }
        ]
    });

    let report =
        transfer::import_saga(&mut conn, &project, &document, Mode::Merge, "-").expect("import");
    // Зависимость на несуществующий id не создаёт рёбер.
    assert_eq!(report.edges, 0, "{report:?}");
    // Заметка без узла сохраняется: она принадлежит проекту, а не сущности.
    assert_eq!(report.notes, 2, "{report:?}");
}

/// Задача, которая по файлу ждёт саму себя, — порча данных: отказ с кодом, а не
/// CHECK-ограничение схемы, из которого вызывающий не поймёт, что править.
#[test]
fn self_dependency_in_saga_is_refused_by_code() {
    let (mut conn, project) = fixture("saga-selfref");
    let document = json!({
        "format_version": "1.3",
        "project": {
            "name": "Самоссылка",
            "epics": [{
                "_original_id": 1,
                "name": "Эпик",
                "tasks": [{
                    "_original_id": 2,
                    "title": "Задача",
                    "status": "todo",
                    "depends_on": [2]
                }]
            }]
        }
    });

    let error = transfer::import_saga(&mut conn, &project, &document, Mode::Merge, "-")
        .expect_err("самоссылка должна быть отвергнута");
    assert_eq!(error.code(), "SELF_EDGE");

    // Отказ случился до записи: в проекте пусто.
    let nodes = node::list(&conn, &project, &node::Filter::default()).expect("list");
    assert!(
        nodes.is_empty(),
        "после отказа ничего не записано: {nodes:?}"
    );
}
