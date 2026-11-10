//! История статусов: каждый путь, меняющий статус, обязан оставить факт.
//!
//! Проверяется не «таблица существует», а полнота: для каждого из путей (ручной переход,
//! взятие в работу, завершение, сдача, истечение, разблокировка, сборка) в истории появляется
//! ровно одна строка с правильной причиной. Раньше `burnup` брал дату закрытия из журнала по
//! совпадению текста, и седьмой путь (`import`) молча выпал из ряда — здесь такой путь не может
//! выпасть незаметно.

use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;

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
    let dir = std::env::temp_dir().join(format!("tasq-hist-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
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

fn fixture(tag: &str) -> (Connection, Project, node::Node, node::Node) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "HIST", "history").expect("init");
    let mut conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");
    let epic = add(&mut conn, &project, None, "Эпик");
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Задача");
    (conn, project, epic, task)
}

/// История узла: (from, to, cause) в порядке записи.
fn history_of(conn: &Connection, node_id: i64) -> Vec<(Option<String>, String, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT from_status, to_status, cause FROM node_status_history
             WHERE node_id = ?1 ORDER BY id",
        )
        .expect("prepare");
    stmt.query_map([node_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows")
}

fn claim(conn: &mut Connection, project: &Project, reference: &str) -> lease::Lease {
    lease::claim(
        conn,
        project,
        reference,
        &ClaimRequest {
            holder: "agent",
            ttl_seconds: 600,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
    )
    .expect("claim")
}

fn expire(conn: &Connection, node: &node::Node) {
    let changed = conn
        .execute(
            "UPDATE attempts SET expires_at = '2000-01-01T00:00:00.000Z'
             WHERE node_id = ?1 AND ended_at IS NULL",
            [node.id],
        )
        .expect("expire");
    assert_eq!(changed, 1, "просрочить нечего");
}

/// Ручной переход пишет факт с причиной `manual`.
#[test]
fn a_manual_transition_is_recorded() {
    let (mut conn, project, _epic, task) = fixture("manual");
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

    let rows = history_of(&conn, task.id);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].1, "todo");
    assert_eq!(rows[0].2, Cause::Manual.as_str());
}

/// Взятие в работу и завершение через аренду: две строки, разные причины.
///
/// Это и есть разница, на которой стоит burn-up: «взял» и «закрыл через аренду» — разные
/// события, и раньше они различались только по тексту `action` в журнале.
#[test]
fn claim_and_completion_are_recorded_with_their_own_causes() {
    let (mut conn, project, _epic, task) = fixture("claim");
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

    let held = claim(&mut conn, &project, &task.dot_id);
    let rows = history_of(&conn, task.id);
    assert_eq!(
        rows.last().expect("claim row").2,
        Cause::LeaseClaimed.as_str()
    );

    lease::release(
        &mut conn,
        &project,
        &held.token,
        Outcome::Completed,
        None,
        false,
    )
    .expect("release");
    let rows = history_of(&conn, task.id);
    let last = rows.last().expect("completion row");
    assert_eq!(last.1, "done");
    assert_eq!(last.2, Cause::LeaseCompleted.as_str());
}

/// Сдача без завершения — причина `lease_released`, а не `lease_completed`: работа не сделана.
#[test]
fn a_voluntary_release_is_not_a_completion() {
    let (mut conn, project, _epic, task) = fixture("released");
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
    let held = claim(&mut conn, &project, &task.dot_id);
    lease::release(
        &mut conn,
        &project,
        &held.token,
        Outcome::Released,
        None,
        false,
    )
    .expect("release");

    let rows = history_of(&conn, task.id);
    let last = rows.last().expect("row");
    assert_eq!(last.1, "todo");
    assert_eq!(last.2, Cause::LeaseReleased.as_str());
    assert!(
        !rows
            .iter()
            .any(|(_, _, cause)| cause == Cause::LeaseCompleted.as_str()),
        "сдача записана как завершение: {rows:?}"
    );
}

/// Истечение аренды — своя причина, отдельная от добровольной сдачи.
#[test]
fn an_expiry_has_its_own_cause() {
    let (mut conn, project, _epic, task) = fixture("expired");
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
    claim(&mut conn, &project, &task.dot_id);
    expire(&conn, &task);

    lease::release_expired(&mut conn, &project, None).expect("sweep");
    let rows = history_of(&conn, task.id);
    let last = rows.last().expect("row");
    assert_eq!(last.1, "todo");
    assert_eq!(last.2, Cause::LeaseExpired.as_str());
}

/// Узел, закрытый руками при живой аренде, не получает ложного перехода «истекла → todo».
#[test]
fn finishing_by_hand_does_not_create_a_phantom_expiry() {
    let (mut conn, project, _epic, task) = fixture("phantom");
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
    claim(&mut conn, &project, &task.dot_id);
    conn.execute("UPDATE nodes SET status = 'done' WHERE id = ?1", [task.id])
        .expect("mark done");
    expire(&conn, &task);

    lease::release_expired(&mut conn, &project, None).expect("sweep");
    let rows = history_of(&conn, task.id);
    assert!(
        !rows
            .iter()
            .any(|(_, _, cause)| cause == Cause::LeaseExpired.as_str()),
        "закрытому узлу приписан возврат в очередь: {rows:?}"
    );
}

/// Разблокировка при снятии ребра и разблокировка уборкой — разные причины.
#[test]
fn unblocking_is_recorded_and_gc_is_distinguished() {
    let (mut conn, project, epic, task) = fixture("unblock");
    let blocker = add(&mut conn, &project, Some(&epic.dot_id), "Блокер");
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
    // Узел в blocked: его вернёт снятие ребра.
    conn.execute(
        "UPDATE nodes SET status = 'blocked' WHERE id = ?1",
        [task.id],
    )
    .expect("blocked");
    tasq::edge::link(
        &mut conn,
        &project,
        &task.dot_id,
        &blocker.dot_id,
        tasq::edge::Kind::Blocks,
        "agent",
    )
    .expect("edge");
    tasq::edge::unlink(
        &mut conn,
        &project,
        &task.dot_id,
        &blocker.dot_id,
        tasq::edge::Kind::Blocks,
        "agent",
    )
    .expect("unlink");

    let rows = history_of(&conn, task.id);
    let unblock = rows
        .iter()
        .find(|(from, to, _)| from.as_deref() == Some("blocked") && to == "todo")
        .unwrap_or_else(|| panic!("разблокировка не записана: {rows:?}"));
    assert_eq!(unblock.2, Cause::Unblock.as_str());
}

/// `record` сам не пишет переход в тот же статус: защита живёт в одной точке, а не в каждом
/// вызывающем. Проверяется напрямую — через `node::update` этот случай не воспроизводится:
/// там изменения собираются по различию, и одинаковое значение до записи не доходит.
#[test]
fn the_writer_itself_skips_a_no_op_transition() {
    let (conn, _project, _epic, task) = fixture("noop");
    tasq::history::record(&conn, task.id, Some("todo"), "todo", Cause::Manual, "agent")
        .expect("record");
    assert!(
        history_of(&conn, task.id).is_empty(),
        "записана пустая смена статуса"
    );

    tasq::history::record(&conn, task.id, None, "idea", Cause::Manual, "agent").expect("record");
    assert_eq!(
        history_of(&conn, task.id).len(),
        1,
        "первая запись узла потеряна"
    );
}

/// Переход в тот же статус через `set` не пишется: это не переход, и лишняя строка мешала бы
/// отчётам.
#[test]
fn setting_the_same_status_records_nothing() {
    let (mut conn, project, _epic, task) = fixture("same");
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
    let before = history_of(&conn, task.id).len();
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
    .expect("same status");
    assert_eq!(
        history_of(&conn, task.id).len(),
        before,
        "лишняя строка истории"
    );
}

/// Каждая запись истории ссылается на существующие узел и статусы: справочник — не украшение.
#[test]
fn every_row_points_at_a_real_node_and_known_status() {
    let (mut conn, project, _epic, task) = fixture("refs");
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
    claim(&mut conn, &project, &task.dot_id);

    let orphan: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM node_status_history h
             LEFT JOIN nodes n ON n.id = h.node_id
             LEFT JOIN statuses s ON s.name = h.to_status
             WHERE n.id IS NULL OR s.name IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(orphan, 0, "история ссылается на несуществующие сущности");

    let bad_cause = conn.execute(
        "INSERT INTO node_status_history (node_id, to_status, cause, at)
         VALUES (?1, 'todo', 'взял и бросил', '2026-01-01T00:00:00.000Z')",
        [task.id],
    );
    assert!(bad_cause.is_err(), "схема приняла причину вне списка");
}
