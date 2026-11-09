//! Границы дерева и состояний: то, что отбивается до записи или требует явного согласия.
//!
//! Здесь собраны ветки `node.rs`, до которых не доходит удачный сценарий: предел глубины,
//! вид узла не по глубине, повторный архив, восстановление не удалённого узла, закрытие
//! задачи с открытым блокером и правка полей, которых не было.

use std::path::PathBuf;

use rusqlite::Connection;
use serde_json::Value;

use tasq::db;
use tasq::node::{self, Filter, NewNode, Patch};
use tasq::project::{self, Project};
use tasq::status::{Kind, Status};
use tasq::{activity, edge, lease};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-limits-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
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
    .expect("create node")
}

/// Предел глубины — 64 сегмента: шестьдесят пятый узел отбивается, дерево остаётся целым.
#[test]
fn depth_limit_stops_the_chain() {
    let (mut conn, project) = fixture("depth");
    let mut last = add(&mut conn, &project, None, "уровень 1");
    let mut depth = 1;

    loop {
        match node::create(
            &mut conn,
            &project,
            NewNode {
                parent: Some(&last.dot_id),
                title: "следующий уровень",
                kind: None,
                description: None,
                priority: None,
                due_date: None,
                estimated_hours: None,
                weight: None,
                source_ref: None,
                branch: None,
            },
        ) {
            Ok(node) => {
                last = node;
                depth += 1;
                assert!(depth <= 64, "предел глубины не сработал");
            }
            Err(error) => {
                assert_eq!(error.code(), "DEPTH_LIMIT", "на глубине {depth}");
                break;
            }
        }
    }

    assert_eq!(depth, 64, "предел наступает на 65-м узле");
    // Отказ не испортил дерево: последний узел на месте.
    assert_eq!(
        node::resolve(&conn, &project, &last.dot_id)
            .expect("last")
            .depth,
        64
    );
}

/// Вид узла задаётся глубиной: эпиком нельзя посадить задачу, и наоборот.
#[test]
fn kind_must_match_the_depth() {
    let (mut conn, project) = fixture("kind");
    let epic = add(&mut conn, &project, None, "Эпик");

    let error = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: Some(&epic.dot_id),
            title: "не тот вид",
            kind: Some(Kind::Epic),
            description: None,
            priority: None,
            due_date: None,
            estimated_hours: None,
            weight: None,
            source_ref: None,
            branch: None,
        },
    )
    .expect_err("эпик на второй глубине");
    assert_eq!(error.code(), "INVALID_VALUE");
    assert!(error.to_string().contains("does not fit depth"), "{error}");

    // Пустое имя тоже отбивается: безымянный узел в дереве бесполезен.
    let error = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: None,
            title: "   ",
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
    .expect_err("пустое имя");
    assert_eq!(error.code(), "INVALID_VALUE");
}

/// Повторный архив, распаковка неархивного узла и восстановление не удалённого —
/// отказы `ALREADY_IN_STATE` с названием состояния, а не молчаливый успех.
#[test]
fn repeated_state_changes_are_named() {
    let (mut conn, project) = fixture("states");
    let epic = add(&mut conn, &project, None, "Эпик");
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Задача");

    node::archive(&mut conn, &project, &task.dot_id, false).expect("archive");
    let error = node::archive(&mut conn, &project, &task.dot_id, false).expect_err("archive twice");
    assert_eq!(error.code(), "ALREADY_IN_STATE");
    assert!(error.to_string().contains("archived"), "{error}");

    node::archive(&mut conn, &project, &task.dot_id, true).expect("unarchive");
    let error =
        node::archive(&mut conn, &project, &task.dot_id, true).expect_err("unarchive twice");
    assert_eq!(error.code(), "ALREADY_IN_STATE");
    assert!(error.to_string().contains("not archived"), "{error}");

    let error = node::restore(&mut conn, &project, &task.dot_id).expect_err("restore alive");
    assert_eq!(error.code(), "ALREADY_IN_STATE");
    assert!(error.to_string().contains("present"), "{error}");

    // Архивированный узел нельзя править: сначала распакуйте.
    node::archive(&mut conn, &project, &task.dot_id, false).expect("archive again");
    let error = node::update(
        &mut conn,
        &project,
        &task.dot_id,
        Patch {
            title: Some("новое имя"),
            ..Patch::default()
        },
    )
    .expect_err("правка в архиве");
    assert_eq!(error.code(), "ALREADY_IN_STATE");
}

/// Задачу нельзя закрыть, пока открыт её блокер: отказ называет, кого ждать.
#[test]
fn finishing_with_an_open_blocker_is_refused() {
    let (mut conn, project) = fixture("blockers");
    let epic = add(&mut conn, &project, None, "Эпик");
    let blocker = add(&mut conn, &project, Some(&epic.dot_id), "Блокер");
    let waiting = add(&mut conn, &project, Some(&epic.dot_id), "Ждёт");

    // Работа начата до того, как выяснилась зависимость: так и бывает на практике.
    let held = lease::claim(
        &mut conn,
        &project,
        &waiting.dot_id,
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
    node::update(
        &mut conn,
        &project,
        &waiting.dot_id,
        Patch {
            status: Some(Status::Review),
            ..Patch::default()
        },
    )
    .expect("in_progress -> review");

    // `waiting` ждёт `blocker`; блокер ещё открыт.
    edge::link(
        &mut conn,
        &project,
        &waiting.dot_id,
        &blocker.dot_id,
        edge::Kind::Blocks,
        "-",
    )
    .expect("link");

    let error = node::update(
        &mut conn,
        &project,
        &waiting.dot_id,
        Patch {
            status: Some(Status::Done),
            ..Patch::default()
        },
    )
    .expect_err("блокер открыт");
    assert_eq!(error.code(), "UNMET_PREREQUISITES");
    // Блокеры называются путями: так они и лежат в графе, без ключа проекта.
    assert!(error.to_string().contains(&blocker.path), "{error}");

    // После закрытия блокера переход проходит.
    let blocker_held = lease::claim(
        &mut conn,
        &project,
        &blocker.dot_id,
        &lease::ClaimRequest {
            holder: "agent-b",
            ttl_seconds: lease::DEFAULT_TTL,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
    )
    .expect("claim blocker");
    lease::release(
        &mut conn,
        &project,
        &blocker_held.token,
        lease::Outcome::Completed,
        None,
        false,
    )
    .expect("close blocker");
    let done = node::update(
        &mut conn,
        &project,
        &waiting.dot_id,
        Patch {
            status: Some(Status::Done),
            ..Patch::default()
        },
    )
    .expect("блокер закрыт");
    assert_eq!(done.status, "done");
    assert_eq!(held.holder, "agent-a");
}

/// Заблокированный узел нельзя взять в работу: отказ приходит на захвате,
/// а не после того, как агент начал писать код.
#[test]
fn a_blocked_node_cannot_be_claimed() {
    let (mut conn, project) = fixture("blocked-claim");
    let epic = add(&mut conn, &project, None, "Эпик");
    let blocker = add(&mut conn, &project, Some(&epic.dot_id), "Блокер");
    let waiting = add(&mut conn, &project, Some(&epic.dot_id), "Ждёт");

    edge::link(
        &mut conn,
        &project,
        &waiting.dot_id,
        &blocker.dot_id,
        edge::Kind::Blocks,
        "-",
    )
    .expect("link");

    let error = lease::claim(
        &mut conn,
        &project,
        &waiting.dot_id,
        &lease::ClaimRequest {
            holder: "agent-a",
            ttl_seconds: lease::DEFAULT_TTL,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
    )
    .expect_err("блокер открыт");
    assert_eq!(error.code(), "UNMET_PREREQUISITES");
}

/// Правка полей, которых раньше не было, попадает и в узел, и в журнал — парой «было/стало».
#[test]
fn patch_records_new_values_and_logs_them() {
    let (mut conn, project) = fixture("patch");
    let epic = add(&mut conn, &project, None, "Эпик");
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Задача");

    let updated = node::update(
        &mut conn,
        &project,
        &task.dot_id,
        Patch {
            estimated_hours: Some(6.5),
            source_ref: Some("src/api.rs:42"),
            branch: Some("feature/x"),
            ..Patch::default()
        },
    )
    .expect("patch");

    assert_eq!(updated.estimated_hours, Some(6.5));
    assert_eq!(updated.source_ref.as_deref(), Some("src/api.rs:42"));
    assert_eq!(updated.branch.as_deref(), Some("feature/x"));

    let records = activity::recent(&conn, project.id, 50).expect("log");
    let fields: Vec<String> = records
        .iter()
        .filter_map(|record| record.field_name.clone())
        .collect();
    for field in ["estimated_hours", "source_ref", "branch"] {
        assert!(fields.contains(&field.to_string()), "в журнале нет {field}");
    }
    let hours = records
        .iter()
        .find(|record| record.field_name.as_deref() == Some("estimated_hours"))
        .expect("запись о часах");
    assert_eq!(hours.new_value.as_deref(), Some("6.5"));
    assert_eq!(hours.old_value, None, "прежнего значения не было");

    // Повторная правка тем же значением не пишет пустую запись.
    let before = activity::recent(&conn, project.id, 200).expect("log").len();
    node::update(
        &mut conn,
        &project,
        &task.dot_id,
        Patch {
            estimated_hours: Some(6.5),
            ..Patch::default()
        },
    )
    .expect("same value");
    let after = activity::recent(&conn, project.id, 200).expect("log").len();
    assert_eq!(
        after, before,
        "значение не изменилось — записи быть не должно"
    );
}

/// Предел выдачи: `limit` обрезает список, отрицательный — отдаёт пустой.
#[test]
fn list_limit_is_applied_after_sorting() {
    let (mut conn, project) = fixture("limit");
    let epic = add(&mut conn, &project, None, "Эпик");
    for index in 1..=5 {
        add(
            &mut conn,
            &project,
            Some(&epic.dot_id),
            &format!("Задача {index}"),
        );
    }

    let one = node::list(
        &conn,
        &project,
        &Filter {
            limit: Some(1),
            ..Filter::default()
        },
    )
    .expect("limit 1");
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].dot_id, "TASQ-1");

    let none = node::list(
        &conn,
        &project,
        &Filter {
            limit: Some(-3),
            ..Filter::default()
        },
    )
    .expect("negative limit");
    assert!(none.is_empty(), "отрицательный предел — пустая выдача");

    // Без предела — всё дерево, включая эпик.
    let all = node::list(&conn, &project, &Filter::default()).expect("all");
    assert_eq!(all.len(), 6);
}

/// Ссылка по внутреннему id и по пути дают один узел; чужая ссылка — отказ.
#[test]
fn internal_id_reference_matches_the_path() {
    let (mut conn, project) = fixture("internal-id");
    let epic = add(&mut conn, &project, None, "Эпик");
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Задача");

    let by_id = node::resolve(&conn, &project, &format!("#{}", task.id)).expect("по id");
    let by_path = node::resolve(&conn, &project, &task.dot_id).expect("по пути");
    assert_eq!(by_id.id, by_path.id);

    // Мусор в ссылке — `INVALID_REF`, а не «не найдено».
    for bad in ["", "#x", "TASQ-", "TASQ-1.x", "1..2"] {
        let error = node::resolve(&conn, &project, bad).expect_err(bad);
        assert_eq!(error.code(), "INVALID_REF", "ссылка {bad:?}");
    }
}

/// Лог доступен как JSON-строки: поля называются так, как их ждёт потребитель.
#[test]
fn activity_records_serialise_with_named_fields() {
    let (mut conn, project) = fixture("activity");
    let epic = add(&mut conn, &project, None, "Эпик");
    node::update(
        &mut conn,
        &project,
        &epic.dot_id,
        Patch {
            title: Some("Эпик (уточнён)"),
            ..Patch::default()
        },
    )
    .expect("rename");

    let records = activity::recent(&conn, project.id, 10).expect("log");
    let value: Value = serde_json::to_value(&records).expect("serialise");
    let first = &value[0];
    for key in [
        "id",
        "action",
        "field_name",
        "old_value",
        "new_value",
        "created_at",
    ] {
        assert!(first.get(key).is_some(), "в записи нет поля {key}: {first}");
    }
    assert_eq!(first["action"], Value::from("update"));
    assert_eq!(first["old_value"], Value::from("Эпик"));
    assert_eq!(first["new_value"], Value::from("Эпик (уточнён)"));
}
