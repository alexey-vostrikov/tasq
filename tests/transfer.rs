//! Экспорт и импорт: контрольная сумма, merge/replace, импорт saga, отказы до записи.

use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;
use serde_json::json;

use tasq::db;
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};
use tasq::transfer::{self, Mode};
use tasq::{comment, edge, note, template};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-xfer-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn fixture(tag: &str) -> (Connection, Project) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "TASQ", "demo").expect("init");
    let conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = project::default_project(&conn).expect("project");
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
    .expect("node")
}

/// Проект с деревом, ребром, комментарием, заметкой и шаблоном.
fn seeded(tag: &str) -> (Connection, Project) {
    let (mut conn, project) = fixture(tag);
    let epic = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: None,
            title: "Эпик",
            kind: None,
            description: Some("зачем"),
            priority: Some("high"),
            due_date: Some("2099-01-01"),
            estimated_hours: Some(8.0),
            weight: Some(5.0),
            source_ref: Some("src/main.rs:1"),
            branch: Some("main"),
        },
    )
    .expect("epic");
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Задача");
    let sub = add(&mut conn, &project, Some(&task.dot_id), "Подзадача");
    // Ребро между разными ветвями: внутри одной ветви «предок ↔ потомок» запрещено
    // (тупик по построению), это отдельное правило домена.
    let other_epic = add(&mut conn, &project, None, "Второй эпик");
    edge::link(
        &mut conn,
        &project,
        &task.dot_id,
        &other_epic.dot_id,
        edge::Kind::Blocks,
        "-",
    )
    .expect("edge");
    comment::add(&mut conn, &project, &sub.dot_id, "нить", "agent-a").expect("comment");
    note::save(
        &mut conn,
        &project,
        note::SaveNote {
            id: None,
            title: Some("Решение"),
            content: Some("WAL"),
            note_type: Some("decision"),
            node: Some(&epic.dot_id),
            actor: None,
        },
    )
    .expect("note");
    template::save(
        &mut conn,
        &project,
        "api",
        None,
        r#"[{"title":"Ручка {res}","kind":"task"}]"#,
        "-",
    )
    .expect("template");
    (conn, project)
}

#[test]
fn export_is_deterministic_and_checksummed() {
    let (conn, project) = seeded("export");
    let first = transfer::export(&conn, &project, false).expect("export");
    let second = transfer::export(&conn, &project, false).expect("export");

    // Одинаковые данные дают одинаковую сумму (время экспорта в сумму не входит).
    assert_eq!(
        first["sha256"], second["sha256"],
        "сумма не зависит от момента экспорта"
    );
    let sum = first["sha256"].as_str().expect("checksum");
    assert_eq!(sum.len(), 64, "sha256 в hex");

    let summary = transfer::describe(&first);
    assert_eq!(summary["nodes"], 4, "эпик, задача, подзадача, второй эпик");
    assert_eq!(summary["edges"], 1);
    assert_eq!(summary["comments"], 1);
    assert_eq!(summary["notes"], 1);
    assert_eq!(summary["templates"], 1);
}

#[test]
fn round_trip_into_a_clean_database_keeps_everything() {
    let (conn, project) = seeded("roundtrip");
    let document = transfer::export(&conn, &project, false).expect("export");
    drop(conn);

    // Чистая база с пустым проектом
    let dir = temp_dir("roundtrip-target");
    let outcome = db::init(&dir, "TASQ", "target").expect("init");
    let mut target = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let target_project = project::default_project(&target).expect("project");

    let result = transfer::import(&mut target, &target_project, &document, Mode::Replace, "-")
        .expect("import");
    assert_eq!(result.nodes_created, 4);
    assert!(result.checksum_verified);
    assert_eq!(result.edges, 1);
    assert_eq!(result.comments, 1);
    assert_eq!(result.notes, 1);
    assert_eq!(result.templates, 1);

    // Сверяем содержимое по существу
    let nodes = node::list(&target, &target_project, &node::Filter::default()).expect("list");
    let titles: Vec<&str> = nodes.iter().map(|n| n.title.as_str()).collect();
    assert_eq!(titles, vec!["Эпик", "Задача", "Подзадача", "Второй эпик"]);
    let epic = nodes.first().expect("epic");
    assert_eq!(epic.description.as_deref(), Some("зачем"));
    assert_eq!(epic.weight, 5.0);
    assert_eq!(epic.estimated_hours, Some(8.0));
    assert_eq!(epic.due_date.as_deref(), Some("2099-01-01"));
    assert_eq!(epic.branch.as_deref(), Some("main"));
    assert_eq!(epic.source_ref.as_deref(), Some("src/main.rs:1"));

    let links = edge::list(&target, &target_project, None).expect("edges");
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].from, "TASQ-1.1");
    assert_eq!(links[0].to, "TASQ-2");

    let comments = comment::list(&target, &target_project, "TASQ-1.1.1", false).expect("comments");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].content, "нить");

    let notes = note::list(&target, &target_project, &note::Filter::default()).expect("notes");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].title, "Решение");
    assert_eq!(notes[0].dot_id.as_deref(), Some("TASQ-1"));

    let templates = template::list(&target, &target_project).expect("templates");
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].variables, vec!["res".to_string()]);
}

#[test]
fn edited_payload_is_rejected_by_checksum() {
    let (conn, project) = seeded("tamper");
    let mut document = transfer::export(&conn, &project, false).expect("export");
    document["data"]["nodes"][0]["title"] = json!("Подменённый заголовок");

    let dir = temp_dir("tamper-target");
    let outcome = db::init(&dir, "TASQ", "target").expect("init");
    let mut target = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let target_project = project::default_project(&target).expect("project");

    let err =
        transfer::import(&mut target, &target_project, &document, Mode::Replace, "-").unwrap_err();
    assert_eq!(err.code(), "EXPORT_CHECKSUM_MISMATCH");
    let message = err.to_string();
    assert!(message.contains("declares"), "{message}");

    // ничего не записалось
    let nodes = node::list(&target, &target_project, &node::Filter::default()).expect("list");
    assert!(nodes.is_empty(), "при ошибке суммы база не тронута");
}

#[test]
fn foreign_schema_is_rejected() {
    let (mut conn, project) = fixture("schema");
    let document = json!({"schema": "something.else", "nodes": []});
    let err = transfer::import(&mut conn, &project, &document, Mode::Replace, "-").unwrap_err();
    assert_eq!(err.code(), "INVALID_VALUE");
    assert!(err.to_string().contains("tasq.export.v1"), "{err}");
}

#[test]
fn merge_skips_existing_paths_and_adds_new_ones() {
    let (mut conn, project) = fixture("merge");
    add(&mut conn, &project, None, "Первый");
    let document = {
        let snapshot = transfer::export(&conn, &project, false).expect("export");
        // добавим ещё один узел в документ и пересчитаем сумму через повторный экспорт
        // (проще: построим второй проект, где два узла, и импортируем его в первый)
        let (mut other, other_project) = fixture("merge-source");
        add(&mut other, &other_project, None, "Первый");
        add(&mut other, &other_project, None, "Второй");
        let _ = snapshot;
        transfer::export(&other, &other_project, false).expect("export")
    };

    let result =
        transfer::import(&mut conn, &project, &document, Mode::Merge, "-").expect("import");
    assert_eq!(result.nodes_created, 1, "добавился только отсутствующий");
    assert_eq!(result.nodes_skipped, 1, "существующий не дублировался");

    let nodes = node::list(&conn, &project, &node::Filter::default()).expect("list");
    let titles: Vec<&str> = nodes.iter().map(|n| n.title.as_str()).collect();
    assert_eq!(titles, vec!["Первый", "Второй"]);
}

#[test]
fn replace_wipes_previous_content() {
    let (mut conn, project) = seeded("replace");
    add(&mut conn, &project, None, "Лишний узел");

    let source = {
        let (mut other, other_project) = fixture("replace-source");
        add(&mut other, &other_project, None, "Только этот");
        transfer::export(&other, &other_project, false).expect("export")
    };

    let result =
        transfer::import(&mut conn, &project, &source, Mode::Replace, "-").expect("import");
    assert_eq!(result.nodes_created, 1);

    let nodes = node::list(&conn, &project, &node::Filter::default()).expect("list");
    assert_eq!(nodes.len(), 1, "старое содержимое удалено");
    assert_eq!(nodes[0].title, "Только этот");
}

#[test]
fn cycle_in_the_file_is_refused_before_writing() {
    let (conn, project) = seeded("cyclefile");
    let mut document = transfer::export(&conn, &project, false).expect("export");
    // Добавим ребро в обратную сторону и пересчитаем сумму «как будто это законный файл»:
    // проверка цикла должна сработать даже при валидной сумме.
    // обратное ребро к существующему 1.1 -> 2 даёт цикл 1.1 -> 2 -> 1.1
    document["data"]["edges"]
        .as_array_mut()
        .expect("edges")
        .push(json!({"from": "2", "to": "1.1", "kind": "blocks"}));
    let payload = document["data"].clone();
    let sum = {
        // тот же алгоритм, что в модуле: компактный JSON и sha256
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(serde_json::to_string(&payload).expect("json").as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    document["sha256"] = json!(sum);

    let (mut target, target_project) = fixture("cyclefile-target");
    let err =
        transfer::import(&mut target, &target_project, &document, Mode::Replace, "-").unwrap_err();
    assert_eq!(err.code(), "CYCLE_DETECTED");
    let nodes = node::list(&target, &target_project, &node::Filter::default()).expect("list");
    assert!(nodes.is_empty(), "до записи не дошло");
}

#[test]
fn dry_run_reports_without_writing() {
    let (conn, project) = seeded("dry");
    let document = transfer::export(&conn, &project, false).expect("export");

    let (mut target, target_project) = fixture("dry-target");
    let result = transfer::dry_run(&target, &target_project, &document).expect("dry run");
    assert_eq!(result.nodes_created, 4);
    assert_eq!(result.mode, "dry-run");
    let nodes = node::list(&target, &target_project, &node::Filter::default()).expect("list");
    assert!(nodes.is_empty(), "пробный прогон ничего не пишет");
    let _ = &mut target;
}

#[test]
fn saga_export_is_imported_with_mapping() {
    let dir = temp_dir("saga");
    let outcome = db::init(&dir, "TASQ", "saga-target").expect("init");
    let mut conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = project::default_project(&conn).expect("project");

    // Реальная форма tracker_export 1.3 из saga-mcp@1.19.0 — снята с dist/tools/export-import.js.
    // Лежит отдельным файлом: у json! слишком длинный литерал упирается в recursion limit.
    let document: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/saga-export-v1.3.json")).expect("fixture");

    let result = transfer::import_saga(&mut conn, &project, &document, transfer::Mode::Merge, "-")
        .expect("import saga");
    assert_eq!(result.nodes_created, 5, "эпик + 2 задачи + 2 подзадачи");
    assert_eq!(result.edges, 1, "вторая задача ждёт первую");
    assert_eq!(result.comments, 1);
    assert_eq!(result.notes, 1);

    let nodes = node::list(&conn, &project, &node::Filter::default()).expect("list");
    let by_title = |title: &str| {
        nodes
            .iter()
            .find(|node| node.title == title)
            .unwrap_or_else(|| panic!("нет узла {title}"))
    };

    let epic = by_title("Эпик из saga");
    assert_eq!(epic.kind, "epic");
    assert_eq!(epic.status, "in_progress");
    assert_eq!(epic.branch.as_deref(), Some("feature/x"));

    let task = by_title("Задача из saga");
    assert_eq!(task.kind, "task");
    assert_eq!(task.status, "done", "completed маппится в done");
    assert_eq!(task.priority, "critical");
    assert_eq!(task.assigned_to.as_deref(), Some("agent"));
    assert_eq!(task.estimated_hours, Some(3.5));
    assert_eq!(task.due_date.as_deref(), Some("2026-11-20"));
    assert!(task.description_locked, "блокировка описания переехала");

    let subs: Vec<&node::Node> = nodes.iter().filter(|n| n.kind == "subtask").collect();
    assert_eq!(subs.len(), 2);
    assert_eq!(subs[0].status, "done");

    // Рёбра: вторая задача ждёт первую
    let links = edge::list(&conn, &project, Some(edge::Kind::Blocks)).expect("edges");
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].from, by_title("Вторая задача").dot_id);
    assert_eq!(links[0].to, task.dot_id);

    // Заметка привязана к импортированной задаче, а не потеряна
    let notes = note::list(&conn, &project, &note::Filter::default()).expect("notes");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].dot_id.as_deref(), Some(task.dot_id.as_str()));

    // saga_id сохранён для трассировки
    let metadata: String = conn
        .query_row(
            "SELECT metadata FROM nodes WHERE id = ?1",
            rusqlite::params![task.id],
            |row| row.get(0),
        )
        .expect("metadata");
    assert!(metadata.contains("\"saga_id\":21"), "{metadata}");
}

#[test]
fn saga_version_is_checked() {
    let (mut conn, project) = fixture("sagaversion");
    let document = json!({"format_version": "9.9", "project": {"name": "x", "epics": []}});
    let err = transfer::import_saga(&mut conn, &project, &document, transfer::Mode::Merge, "-")
        .unwrap_err();
    assert_eq!(err.code(), "INVALID_VALUE");
    assert!(err.to_string().contains("1.3"), "{err}");
}

#[test]
fn saga_epic_without_tasks_still_imports() {
    let (mut conn, project) = fixture("sagaepic");
    let document = json!({
        "format_version": "1.3",
        "project": {"name": "empty", "epics": [
            {"_original_id": 1, "name": "Одинокий эпик", "status": "planned", "priority": "low"}
        ]}
    });
    let result = transfer::import_saga(&mut conn, &project, &document, transfer::Mode::Merge, "-")
        .expect("import");
    assert_eq!(result.nodes_created, 1);
    let nodes = node::list(&conn, &project, &node::Filter::default()).expect("list");
    assert_eq!(nodes[0].kind, "epic");
    assert_eq!(nodes[0].status, "planned");
}

#[test]
fn export_of_empty_project_is_still_valid() {
    let (conn, project) = fixture("empty");
    let document = transfer::export(&conn, &project, false).expect("export");
    let summary = transfer::describe(&document);
    assert_eq!(summary["nodes"], 0);

    let (mut target, target_project) = fixture("empty-target");
    let result = transfer::import(&mut target, &target_project, &document, Mode::Replace, "-")
        .expect("import empty");
    assert_eq!(result.nodes_created, 0);
}

#[test]
fn deleted_nodes_are_excluded_by_default() {
    let (mut conn, project) = fixture("deleted");
    let epic = add(&mut conn, &project, None, "Остаётся");
    // удалять можно только `todo`: эпик начинается как `planned`, поэтому берём задачу
    let dropped = add(&mut conn, &project, Some(&epic.dot_id), "Удаляется");
    node::delete(&mut conn, &project, &dropped.dot_id).expect("delete");

    let without = transfer::export(&conn, &project, false).expect("export");
    assert_eq!(transfer::describe(&without)["nodes"], 1);

    let with = transfer::export(&conn, &project, true).expect("export");
    assert_eq!(transfer::describe(&with)["nodes"], 2);
}

#[test]
fn imported_status_is_kept_even_if_unreachable_by_transitions() {
    let (mut conn, project) = fixture("statuses");
    add(&mut conn, &project, None, "Эпик");
    // Готовим документ с состоянием, которое через матрицу не получить (blocked у эпика)
    let document = transfer::export(&conn, &project, false).expect("export");
    let mut document = document;
    document["data"]["nodes"][0]["status"] = json!("blocked");
    let payload = document["data"].clone();
    let sum = {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(serde_json::to_string(&payload).expect("json").as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    document["sha256"] = json!(sum);

    let (mut target, target_project) = fixture("statuses-target");
    transfer::import(&mut target, &target_project, &document, Mode::Replace, "-")
        .expect("import keeps the stored state");
    let nodes = node::list(&target, &target_project, &node::Filter::default()).expect("list");
    assert_eq!(nodes[0].status, "blocked");
}
