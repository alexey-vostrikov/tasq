//! Burn-up читает историю переходов, а не журнал, и различает «не закрыто» от «неизвестно».
//!
//! До этого дата закрытия бралась из `activity_log` по паре
//! `(field_name='status', new_value='done')`, то есть по совпадению текста в шести местах записи.
//! Шестой и седьмой пути (`import`, `restore`) в эту договорённость не входили, и импорт проекта
//! обнулял ряд: `done 0` при закрытом узле на месте.
//!
//! Здесь проверяется обе половины: с историей ряд правильный, без истории — отчёт говорит
//! «неизвестно», а не показывает ноль.

use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;

use tasq::burnup::{self, Metric};
use tasq::db;
use tasq::history::Cause;
use tasq::lease::{self, ClaimRequest, Outcome};
use tasq::node::{self, NewNode, Patch};
use tasq::project::{self, Project};
use tasq::status::Status;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-burn-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn fixture(tag: &str) -> (Connection, Project) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "BURN", "burn").expect("init");
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
    .expect("node")
}

fn series(conn: &Connection, project: &Project) -> burnup::Series {
    burnup::series(conn, project, Metric::Count, None, None).expect("series")
}

/// Эпик с историей: без перехода у него самого он попал бы в «неизвестно», и проверка
/// закрытой работы смешалась бы с проверкой счётчика неизвестного.
fn epic_with_history(conn: &mut Connection, project: &Project) -> node::Node {
    let epic = add(conn, project, None, "Эпик");
    node::update(
        conn,
        project,
        &epic.dot_id,
        Patch {
            status: Some(Status::Planned),
            actor: Some("человек"),
            ..Patch::default()
        },
    )
    .expect("epic planned");
    node::resolve(conn, project, &epic.dot_id).expect("epic")
}

/// Узел, закрытый через аренду, считается готовым: история даёт дату закрытия.
#[test]
fn a_completed_node_counts_as_done() {
    let (mut conn, project) = fixture("done");
    let epic = epic_with_history(&mut conn, &project);
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Задача");
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

    let report = series(&conn, &project);
    assert_eq!(report.scope_total, 2.0);
    assert_eq!(report.scope_done, 1.0, "закрытая работа не учтена");
    assert_eq!(
        report.scope_unknown, 0.0,
        "история есть — неизвестного быть не должно"
    );
}

/// Узел, закрытый РУКАМИ (без аренды), тоже считается: история пишется всеми путями.
#[test]
fn a_hand_closed_node_counts_as_done() {
    let (mut conn, project) = fixture("hand");
    let epic = epic_with_history(&mut conn, &project);
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Задача");
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
    // Матрица не пускает `todo → done` напрямую: закрытие руками идёт из работы, поэтому
    // сначала аренда, а статус ставится вручную — это и есть ручной переход.
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
    node::update(
        &mut conn,
        &project,
        &task.dot_id,
        Patch {
            status: Some(Status::Done),
            lease: Some(task.id),
            actor: Some("человек"),
            ..Patch::default()
        },
    )
    .expect("done");
    let _ = held;

    let report = series(&conn, &project);
    assert_eq!(report.scope_done, 1.0, "ручное закрытие не засчитано");
}

/// Узел без истории — «неизвестно», а не «не закрыто»: отчёт обязан это называть.
///
/// Так выглядит база, пришедшая из файла без истории, и любая база старше v4.
#[test]
fn a_node_without_history_is_unknown_not_zero() {
    let (mut conn, project) = fixture("unknown");
    let _ = add(&mut conn, &project, None, "Эпик");

    // Стираем историю, как если бы её не было вовсе (до v4 её не существовало).
    conn.execute("DELETE FROM node_status_history", [])
        .expect("wipe");

    let report = series(&conn, &project);
    assert_eq!(
        report.scope_unknown, 1.0,
        "узел без истории должен попасть в unknown"
    );
    assert_eq!(report.scope_done, 0.0, "неизвестное выдано за готовое");
    // Ноль в отчёте обычный, а не отрицательный: `-0.0` в печати читается как значащее число.
    assert!(
        !report.scope_done.is_sign_negative(),
        "0.0 отдаётся как -0.0: отчёт напечатает «done -0.0»"
    );
}

/// Узел с историей, но без перехода в `done`, — «не закрыто» (0), а не «неизвестно».
#[test]
fn a_node_with_history_but_no_completion_is_not_unknown() {
    let (mut conn, project) = fixture("notyet");
    let epic = epic_with_history(&mut conn, &project);
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Задача");
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

    let report = series(&conn, &project);
    assert_eq!(
        report.scope_unknown, 0.0,
        "открытый узел назван неизвестным"
    );
    assert_eq!(report.scope_done, 0.0);
}

/// Импорт истории (как это делает `load`/`import`) даёт правильный ряд даже в базе, где журнал
/// пуст: burn-up больше не зависит от журнала.
#[test]
fn the_series_is_built_from_history_even_when_the_log_is_empty() {
    let (mut conn, project) = fixture("nolog");
    let task = add(&mut conn, &project, None, "Задача");
    let created = "2026-01-05T00:00:00.000Z";
    conn.execute(
        "INSERT INTO node_status_history (node_id, from_status, to_status, cause, at, actor)
         VALUES (?1, NULL, 'done', 'import', ?2, 'importer')",
        rusqlite::params![task.id, created],
    )
    .expect("history");
    conn.execute("DELETE FROM activity_log", [])
        .expect("wipe log");

    let report = series(&conn, &project);
    assert_eq!(report.scope_done, 1.0, "ряд не строится без журнала");
    assert_eq!(report.scope_unknown, 0.0);
    // Ряд начинается с создания узла (журнал тут ни при чём), а закрытие учтено по своей дате:
    // точка на конец периода показывает готовое.
    assert_eq!(
        report.points.last().map(|point| point.done),
        Some(1.0),
        "закрытие из истории не попало в последнюю точку"
    );
}

/// Причина перехода не влияет на подсчёт: важно, что переход был. Иначе рядом причин
/// (`lease_expired`, `unblock`, `gc`) пришлось бы править burn-up при каждом добавлении.
#[test]
fn any_cause_of_completion_counts() {
    let (mut conn, project) = fixture("cause");
    let task = add(&mut conn, &project, None, "Задача");
    for cause in [Cause::Manual, Cause::Restore, Cause::Import] {
        conn.execute("DELETE FROM node_status_history", [])
            .expect("wipe");
        conn.execute(
            "INSERT INTO node_status_history (node_id, from_status, to_status, cause, at)
             VALUES (?1, NULL, 'done', ?2, '2026-02-01T00:00:00.000Z')",
            rusqlite::params![task.id, cause.as_str()],
        )
        .expect("history");
        let report = series(&conn, &project);
        assert_eq!(
            report.scope_done,
            1.0,
            "причина {} не засчитана",
            cause.as_str()
        );
    }
}
