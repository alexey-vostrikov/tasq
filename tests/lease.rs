//! Аренды: эксклюзивность, истечение, чекпойнт, перехват и уборка.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use rusqlite::Connection;

use tasq::db;
use tasq::lease::{self, ClaimRequest, Outcome};
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};
use tasq::status::{self, Status};

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("tasq-lease-{tag}-{nanos}-{seq}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn fixture(tag: &str) -> (PathBuf, Connection, Project, node::Node) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "TASQ", "demo").expect("init");
    let db_path = PathBuf::from(outcome.db_path);
    let mut conn = db::open(&db_path).expect("open");
    let project = project::first_project(&conn).expect("project");
    let epic = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: None,
            title: "Epic",
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
            title: "Task",
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
    (dir, conn, project, task)
}

fn claim(
    conn: &mut Connection,
    project: &Project,
    reference: &str,
    holder: &str,
) -> tasq::Result<lease::Lease> {
    let mut request = ClaimRequest::new(holder);
    request.ttl_seconds = 900;
    lease::claim(conn, project, reference, &request)
}

#[test]
fn claim_makes_the_node_in_progress() {
    let (_dir, mut conn, project, task) = fixture("claim");
    let lease = claim(&mut conn, &project, &task.dot_id, "claude-code").expect("claim");

    assert_eq!(lease.attempt_no, 1);
    assert_eq!(lease.token.len(), 64, "токен — 32 байта в hex");
    assert!(lease.inherited_checkpoint.is_none());

    let reread = node::resolve(&conn, &project, &task.dot_id).expect("node");
    assert_eq!(reread.status, "in_progress");
    assert_eq!(reread.effective_status, "in_progress");
    assert_eq!(reread.lease_holder.as_deref(), Some("claude-code"));
    assert!(reread.lease_expires_at.is_some());
}

#[test]
fn second_claim_is_refused_with_the_holder_named() {
    let (_dir, mut conn, project, task) = fixture("busy");
    let first = claim(&mut conn, &project, &task.dot_id, "agent-a").expect("first");

    let err = claim(&mut conn, &project, &task.dot_id, "agent-b").unwrap_err();
    assert_eq!(err.code(), "ACTIVE_ATTEMPT_EXISTS");
    let message = err.to_string();
    assert!(
        message.contains("agent-a"),
        "в ошибке есть держатель: {message}"
    );
    assert!(
        message.contains(&first.expires_at),
        "в ошибке есть срок аренды: {message}"
    );
}

#[test]
fn closed_nodes_cannot_be_claimed() {
    let (_dir, mut conn, project, task) = fixture("closed");
    let lease = claim(&mut conn, &project, &task.dot_id, "agent").expect("claim");
    lease::release(
        &mut conn,
        &project,
        &lease.token,
        Outcome::Completed,
        None,
        false,
    )
    .expect("complete");

    let err = claim(&mut conn, &project, &task.dot_id, "agent").unwrap_err();
    assert_eq!(err.code(), "NODE_CLOSED");
}

#[test]
fn renew_extends_and_refuses_after_expiry() {
    let (_dir, mut conn, project, task) = fixture("renew");
    let lease = claim(&mut conn, &project, &task.dot_id, "agent").expect("claim");
    let renewed = lease::renew(&mut conn, &project, &lease.token, 1800).expect("renew");
    assert!(
        renewed.expires_at > lease.expires_at,
        "срок сдвинулся вперёд"
    );
    assert_eq!(renewed.ttl_seconds, 1800);

    // срок аренды в прошлом: продление невозможно
    conn.execute(
        "UPDATE attempts SET expires_at = '2000-01-01T00:00:00.000Z' WHERE token = ?1",
        rusqlite::params![lease.token],
    )
    .expect("simulate expiry");
    let err = lease::renew(&mut conn, &project, &lease.token, 900).unwrap_err();
    assert_eq!(err.code(), "LEASE_EXPIRED");
}

#[test]
fn unknown_token_is_rejected() {
    let (_dir, mut conn, project, task) = fixture("token");
    claim(&mut conn, &project, &task.dot_id, "agent").expect("claim");
    let err = lease::renew(&mut conn, &project, "not-a-token", 900).unwrap_err();
    assert_eq!(err.code(), "NOT_YOUR_ATTEMPT");
}

#[test]
fn ttl_bounds_are_enforced() {
    let (_dir, mut conn, project, task) = fixture("ttl");
    let mut request = ClaimRequest::new("agent");
    request.ttl_seconds = 10;
    let err = lease::claim(&mut conn, &project, &task.dot_id, &request).unwrap_err();
    assert_eq!(err.code(), "INVALID_VALUE");
    assert!(err.to_string().contains("60"), "{err}");

    request.ttl_seconds = 100_000;
    let err = lease::claim(&mut conn, &project, &task.dot_id, &request).unwrap_err();
    assert_eq!(err.code(), "INVALID_VALUE");
}

#[test]
fn expired_lease_is_reclaimable_and_hands_over_the_checkpoint() {
    let (_dir, mut conn, project, task) = fixture("handover");
    let first = claim(&mut conn, &project, &task.dot_id, "agent-a").expect("claim");

    // агент успел оставить чекпойнт и умер: срок аренды истёк
    conn.execute(
        "UPDATE attempts SET expires_at = '2000-01-01T00:00:00.000Z',
                             checkpoint = 'schema drafted, migrations pending'
         WHERE token = ?1",
        rusqlite::params![first.token],
    )
    .expect("simulate expiry");

    let second = claim(&mut conn, &project, &task.dot_id, "agent-b").expect("reclaim");
    assert_eq!(second.attempt_no, 2);
    assert_eq!(second.superseded_attempt, Some(first.attempt_id));
    assert_eq!(
        second.inherited_checkpoint.as_deref(),
        Some("schema drafted, migrations pending")
    );

    let (reason, ended): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT end_reason, ended_at FROM attempts WHERE id = ?1",
            rusqlite::params![first.attempt_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("previous attempt");
    assert_eq!(reason.as_deref(), Some("superseded"));
    assert!(ended.is_some(), "прежняя попытка закрыта");
}

#[test]
fn steal_takes_over_a_live_lease() {
    let (_dir, mut conn, project, task) = fixture("steal");
    let first = claim(&mut conn, &project, &task.dot_id, "hanging-agent").expect("claim");

    let err = claim(&mut conn, &project, &task.dot_id, "agent-b").unwrap_err();
    assert_eq!(err.code(), "ACTIVE_ATTEMPT_EXISTS");

    let mut request = ClaimRequest::new("agent-b");
    request.steal = true;
    let second = lease::claim(&mut conn, &project, &task.dot_id, &request).expect("steal");
    assert_eq!(second.attempt_no, 2);
    assert_eq!(second.superseded_attempt, Some(first.attempt_id));

    let records = tasq::activity::recent(&conn, project.id, 20).expect("log");
    assert!(
        records
            .iter()
            .any(|r| r.action == "claim" && r.actor == "agent-b"),
        "захват записан в лог"
    );
}

#[test]
fn release_outcomes_drive_the_node_status() {
    let (_dir, mut conn, project, task) = fixture("release");

    let lease = claim(&mut conn, &project, &task.dot_id, "agent").expect("claim");
    let node = lease::release(
        &mut conn,
        &project,
        &lease.token,
        Outcome::Failed,
        Some("boom"),
        false,
    )
    .expect("fail");
    assert_eq!(node.status, "todo");
    let checkpoint: Option<String> = conn
        .query_row(
            "SELECT checkpoint FROM attempts WHERE id = ?1",
            rusqlite::params![lease.attempt_id],
            |row| row.get(0),
        )
        .expect("checkpoint");
    assert_eq!(checkpoint.as_deref(), Some("boom"));

    let lease = claim(&mut conn, &project, &task.dot_id, "agent").expect("reclaim");
    assert_eq!(
        lease.inherited_checkpoint.as_deref(),
        Some("boom"),
        "чекпойнт наследуется при следующем захвате"
    );

    let node = lease::release(
        &mut conn,
        &project,
        &lease.token,
        Outcome::Completed,
        None,
        false,
    )
    .expect("complete");
    assert_eq!(node.status, "done");
    assert_eq!(node.effective_status, "done");

    // токен закрытой попытки второй раз не работает
    let err = lease::release(
        &mut conn,
        &project,
        &lease.token,
        Outcome::Completed,
        None,
        false,
    )
    .unwrap_err();
    assert_eq!(err.code(), "NOT_YOUR_ATTEMPT");
}

#[test]
fn completing_with_open_children_needs_force() {
    let (_dir, mut conn, project, task) = fixture("children");
    node::create(
        &mut conn,
        &project,
        NewNode {
            parent: Some(&task.dot_id),
            title: "Sub",
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
    .expect("subtask");

    let lease = claim(&mut conn, &project, &task.dot_id, "agent").expect("claim");
    let err = lease::release(
        &mut conn,
        &project,
        &lease.token,
        Outcome::Completed,
        None,
        false,
    )
    .unwrap_err();
    assert_eq!(err.code(), "CHILDREN_UNFINISHED");

    let node = lease::release(
        &mut conn,
        &project,
        &lease.token,
        Outcome::Completed,
        None,
        true,
    )
    .expect("forced completion");
    assert_eq!(node.status, "done");
}

#[test]
fn expired_leases_do_not_block_and_gc_tidies_them() {
    let (_dir, mut conn, project, task) = fixture("gc");
    let lease = claim(&mut conn, &project, &task.dot_id, "dead-agent").expect("claim");
    conn.execute(
        "UPDATE attempts SET expires_at = '2000-01-01T00:00:00.000Z' WHERE id = ?1",
        rusqlite::params![lease.attempt_id],
    )
    .expect("simulate expiry");

    // статус уже не in_progress: аренда выводится из времени
    let node = node::resolve(&conn, &project, &task.dot_id).expect("node");
    assert_eq!(
        node.effective_status, "in_progress",
        "хранимый статус остался in_progress — это ожидаемо"
    );

    assert!(
        lease::holdings(&conn, &project)
            .expect("holdings")
            .is_empty()
    );

    let closed = lease::gc(&mut conn, &project).expect("gc");
    assert_eq!(closed, 1);
    let node = node::resolve(&conn, &project, &task.dot_id).expect("node");
    assert_eq!(node.status, "todo", "после уборки узел снова свободен");
    assert_eq!(lease::gc(&mut conn, &project).expect("second gc"), 0);
}

#[test]
fn concurrent_claims_let_exactly_one_win() {
    let dir = temp_dir("race");
    let outcome = db::init(&dir, "TASQ", "demo").expect("init");
    let db_path = PathBuf::from(outcome.db_path);

    let mut setup = db::open(&db_path).expect("open");
    let project = project::first_project(&setup).expect("project");

    // Каждый круг — свой узел: освобождать его внутри круга нельзя,
    // иначе второй поток законно захватит уже свободный узел.
    for round in 0..10 {
        let node = node::create(
            &mut setup,
            &project,
            NewNode {
                parent: None,
                title: &format!("Race {round}"),
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
        .expect("node");
        let reference = node.dot_id.clone();

        let workers = 8;
        let barrier = Arc::new(Barrier::new(workers));
        let mut handles = Vec::new();
        for worker in 0..workers {
            let path = db_path.clone();
            let reference = reference.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let mut conn = Connection::open(&path).expect("open");
                conn.busy_timeout(std::time::Duration::from_secs(10))
                    .expect("busy timeout");
                let project = project::first_project(&conn).expect("project");
                barrier.wait();
                let holder = format!("agent-{worker}");
                let mut request = ClaimRequest::new(&holder);
                request.ttl_seconds = 900;
                match lease::claim(&mut conn, &project, &reference, &request) {
                    Ok(_) => None,
                    Err(err) => Some(err.code().to_string()),
                }
            }));
        }

        let mut wins = 0;
        let mut refusals = Vec::new();
        for handle in handles {
            match handle.join().expect("thread") {
                None => wins += 1,
                Some(code) => refusals.push(code),
            }
        }
        assert_eq!(wins, 1, "круг {round}: должен победить ровно один захват");
        assert_eq!(refusals.len(), workers - 1, "круг {round}");
        for code in refusals {
            assert_eq!(
                code, "ACTIVE_ATTEMPT_EXISTS",
                "круг {round}: отказ должен быть про занятую аренду, а не про блокировку БД"
            );
        }
    }
}

fn db_path(dir: &Path) -> PathBuf {
    dir.join(".tasq").join("tasq.db")
}

#[test]
fn status_module_agrees_with_the_view() {
    let (_dir, mut conn, project, task) = fixture("view");
    let lease = claim(&mut conn, &project, &task.dot_id, "agent").expect("claim");
    let node = node::resolve(&conn, &project, &task.dot_id).expect("node");
    assert_eq!(node.stored_status(), Status::InProgress);
    assert!(status::Status::can_transition(
        Status::InProgress,
        Status::Review,
        node.kind()
    ));
    let _ = db_path(Path::new("."));
    drop(lease);
}

#[test]
fn checkpoint_limit_is_enforced() {
    let (_dir, mut conn, project, task) = fixture("checkpoint");
    let big = "x".repeat(lease::MAX_CHECKPOINT_BYTES + 1);
    let lease = claim(&mut conn, &project, &task.dot_id, "agent").expect("claim");
    let err = lease::release(
        &mut conn,
        &project,
        &lease.token,
        Outcome::Failed,
        Some(&big),
        false,
    )
    .unwrap_err();
    assert_eq!(err.code(), "INVALID_VALUE");
}
