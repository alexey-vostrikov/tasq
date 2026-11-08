//! Зависимости: сквозные рёбра, циклы, запрет «предок ↔ потомок», разблокировка, `next`.

use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;

use tasq::db;
use tasq::edge::{self, Kind};
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};
use tasq::status::Status;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-edge-{tag}-{nanos}"));
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

/// Два эпика, в каждом задача с подзадачей: нужны ветви, между которыми строятся рёбра.
struct Tree {
    epic_a: node::Node,
    task_a: node::Node,
    sub_a: node::Node,
    epic_b: node::Node,
    task_b: node::Node,
}

fn tree(conn: &mut Connection, project: &Project) -> Tree {
    let epic_a = add(conn, project, None, "Epic A");
    let task_a = add(conn, project, Some(&epic_a.dot_id), "Task A");
    let sub_a = add(conn, project, Some(&task_a.dot_id), "Sub A");
    let epic_b = add(conn, project, None, "Epic B");
    let task_b = add(conn, project, Some(&epic_b.dot_id), "Task B");
    Tree {
        epic_a,
        task_a,
        sub_a,
        epic_b,
        task_b,
    }
}

#[test]
fn links_cross_branch_boundaries() {
    let (mut conn, project) = fixture("cross");
    let t = tree(&mut conn, &project);

    // подзадача в ветви A ждёт задачу из ветви B — это и есть сквозная связь
    let link = edge::link(
        &mut conn,
        &project,
        &t.sub_a.dot_id,
        &t.task_b.dot_id,
        Kind::Blocks,
        "agent",
    )
    .expect("cross-branch link");
    assert_eq!(link.from, t.sub_a.dot_id);
    assert_eq!(link.to, t.task_b.dot_id);

    let blocked = node::resolve(&conn, &project, &t.sub_a.dot_id).expect("node");
    assert_eq!(
        blocked.effective_status, "blocked",
        "ожидание видно в статусе"
    );

    let links = edge::list(&conn, &project, Some(Kind::Blocks)).expect("list");
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].from, t.sub_a.dot_id);
}

#[test]
fn duplicate_link_is_idempotent() {
    let (mut conn, project) = fixture("dup");
    let t = tree(&mut conn, &project);

    edge::link(
        &mut conn,
        &project,
        &t.task_b.dot_id,
        &t.sub_a.dot_id,
        Kind::Blocks,
        "-",
    )
    .expect("first");
    edge::link(
        &mut conn,
        &project,
        &t.task_b.dot_id,
        &t.sub_a.dot_id,
        Kind::Blocks,
        "-",
    )
    .expect("second is not an error");

    let links = edge::list(&conn, &project, Some(Kind::Blocks)).expect("list");
    assert_eq!(links.len(), 1, "ребро не дублируется");
}

#[test]
fn self_and_same_branch_edges_are_refused() {
    let (mut conn, project) = fixture("refuse");
    let t = tree(&mut conn, &project);

    let err = edge::link(
        &mut conn,
        &project,
        &t.task_a.dot_id,
        &t.task_a.dot_id,
        Kind::Blocks,
        "-",
    )
    .unwrap_err();
    assert_eq!(err.code(), "SELF_EDGE");

    // предок не может ждать собственного потомка
    let err = edge::link(
        &mut conn,
        &project,
        &t.epic_a.dot_id,
        &t.sub_a.dot_id,
        Kind::Blocks,
        "-",
    )
    .unwrap_err();
    assert_eq!(err.code(), "ANCESTOR_BLOCK_EDGE");

    let err = edge::link(
        &mut conn,
        &project,
        &t.sub_a.dot_id,
        &t.epic_a.dot_id,
        Kind::Blocks,
        "-",
    )
    .unwrap_err();
    assert_eq!(err.code(), "ANCESTOR_BLOCK_EDGE");

    // а «related» между ними — можно: это пометка, а не ожидание
    edge::link(
        &mut conn,
        &project,
        &t.epic_a.dot_id,
        &t.sub_a.dot_id,
        Kind::Related,
        "-",
    )
    .expect("related внутри ветви допустим");
}

#[test]
fn cycle_is_detected_and_reported_with_the_path() {
    let (mut conn, project) = fixture("cycle");
    let t = tree(&mut conn, &project);
    let third = add(&mut conn, &project, None, "Epic C");

    edge::link(
        &mut conn,
        &project,
        &t.task_b.dot_id,
        &t.task_a.dot_id,
        Kind::Blocks,
        "-",
    )
    .expect("B blocks A");
    edge::link(
        &mut conn,
        &project,
        &third.dot_id,
        &t.task_b.dot_id,
        Kind::Blocks,
        "-",
    )
    .expect("C blocks B");

    // A -> C замкнул бы круг A -> C -> B -> A
    let err = edge::link(
        &mut conn,
        &project,
        &t.task_a.dot_id,
        &third.dot_id,
        Kind::Blocks,
        "-",
    )
    .unwrap_err();
    assert_eq!(err.code(), "CYCLE_DETECTED");
    let message = err.to_string();
    assert!(
        message.contains("TASQ-"),
        "в сообщении есть путь: {message}"
    );
    assert!(message.contains("->"), "путь цикла читается: {message}");

    // рёбер не прибавилось
    let links = edge::list(&conn, &project, Some(Kind::Blocks)).expect("list");
    assert_eq!(links.len(), 2);
}

#[test]
fn removing_the_last_blocker_unblocks_the_node() {
    let (mut conn, project) = fixture("unblock");
    let t = tree(&mut conn, &project);

    edge::link(
        &mut conn,
        &project,
        &t.task_a.dot_id,
        &t.task_b.dot_id,
        Kind::Blocks,
        "-",
    )
    .expect("link");
    node::update(
        &mut conn,
        &project,
        &t.task_a.dot_id,
        node::Patch {
            status: Some(Status::Blocked),
            ..node::Patch::default()
        },
    )
    .expect("mark blocked");
    let blocked = node::resolve(&conn, &project, &t.task_a.dot_id).expect("node");
    assert_eq!(blocked.status, "blocked");

    let removed = edge::unlink(
        &mut conn,
        &project,
        &t.task_a.dot_id,
        &t.task_b.dot_id,
        Kind::Blocks,
        "-",
    )
    .expect("unlink");
    assert!(removed);

    let after = node::resolve(&conn, &project, &t.task_a.dot_id).expect("node");
    assert_eq!(
        after.status, "todo",
        "хранимый blocked снят вместе с последним блокером"
    );
    assert_eq!(after.effective_status, "todo");
}

#[test]
fn blockers_still_count_while_they_are_open() {
    let (mut conn, project) = fixture("still");
    let t = tree(&mut conn, &project);

    edge::link(
        &mut conn,
        &project,
        &t.task_a.dot_id,
        &t.task_b.dot_id,
        Kind::Blocks,
        "-",
    )
    .expect("link");
    edge::link(
        &mut conn,
        &project,
        &t.task_a.dot_id,
        &t.epic_b.dot_id,
        Kind::Blocks,
        "-",
    )
    .expect("second blocker");

    edge::unlink(
        &mut conn,
        &project,
        &t.task_a.dot_id,
        &t.task_b.dot_id,
        Kind::Blocks,
        "-",
    )
    .expect("unlink one");
    let still = node::resolve(&conn, &project, &t.task_a.dot_id).expect("node");
    assert_eq!(
        still.effective_status, "blocked",
        "второй блокер ещё держит"
    );
}

#[test]
fn graph_shows_both_directions_and_transitive_blockers() {
    let (mut conn, project) = fixture("graph");
    let t = tree(&mut conn, &project);
    let third = add(&mut conn, &project, None, "Epic C");

    // A ждёт B, B ждёт C: цепочка блокеров уходит вверх
    edge::link(
        &mut conn,
        &project,
        &t.task_b.dot_id,
        &third.dot_id,
        Kind::Blocks,
        "-",
    )
    .expect("B waits C");
    edge::link(
        &mut conn,
        &project,
        &t.task_a.dot_id,
        &t.task_b.dot_id,
        Kind::Blocks,
        "-",
    )
    .expect("A waits B");

    let graph = edge::graph(&conn, &project, &t.task_a.dot_id, 5).expect("graph");
    assert_eq!(graph.node, t.task_a.dot_id);
    assert!(graph.blocked_by.iter().any(|n| n.dot_id == t.task_b.dot_id));
    assert!(
        graph
            .transitive_blockers
            .iter()
            .any(|n| n.dot_id == third.dot_id),
        "транзитивный блокер виден"
    );
    assert!(graph.blocks.is_empty(), "задачу A никто не ждёт");

    // смотрим со стороны C: её завершение разблокирует и B, и A
    let from_c = edge::graph(&conn, &project, &third.dot_id, 5).expect("graph");
    assert!(from_c.blocks.iter().any(|n| n.dot_id == t.task_b.dot_id));
    assert_eq!(from_c.unblocks_total, 2);
    assert!(
        from_c.critical_chain.len() >= 3,
        "цепочка C -> B -> A: {:?}",
        from_c.critical_chain
    );

    let mermaid = from_c.to_mermaid();
    assert!(mermaid.starts_with("graph LR"), "{mermaid}");
    assert!(mermaid.contains("blocks"), "{mermaid}");
}

#[test]
fn next_prefers_priority_due_date_and_unblocking() {
    let (mut conn, project) = fixture("next");
    let epic = add(&mut conn, &project, None, "Epic");
    let low = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: Some(&epic.dot_id),
            title: "Low work",
            kind: None,
            description: None,
            priority: Some("low"),
            due_date: None,
            estimated_hours: None,
            weight: None,
            source_ref: None,
            branch: None,
        },
    )
    .expect("low");
    let high = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: Some(&epic.dot_id),
            title: "High work",
            kind: None,
            description: None,
            priority: Some("high"),
            due_date: None,
            estimated_hours: None,
            weight: None,
            source_ref: None,
            branch: None,
        },
    )
    .expect("high");
    let blocked = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: Some(&epic.dot_id),
            title: "Waiting work",
            kind: None,
            description: None,
            priority: Some("critical"),
            due_date: None,
            estimated_hours: None,
            weight: None,
            source_ref: None,
            branch: None,
        },
    )
    .expect("blocked");

    // «ждёт» идёт от blocked к high: критика ждёт, пока сделают высокий приоритет
    edge::link(
        &mut conn,
        &project,
        &blocked.dot_id,
        &high.dot_id,
        Kind::Blocks,
        "-",
    )
    .expect("blocked waits for high");

    let candidates = edge::next_candidates(&conn, &project, 5).expect("next");
    let order: Vec<&str> = candidates.iter().map(|c| c.dot_id.as_str()).collect();
    assert_eq!(
        order.first(),
        Some(&high.dot_id.as_str()),
        "высокий приоритет первым: {order:?}"
    );
    assert!(
        !order.contains(&blocked.dot_id.as_str()),
        "заблокированный узел не предлагается: {order:?}"
    );
    assert!(order.contains(&low.dot_id.as_str()));

    let chosen = candidates
        .iter()
        .find(|c| c.dot_id == high.dot_id)
        .expect("high in list");
    assert!(
        chosen.reason.contains("unblocks 1"),
        "причина называет разблокировку: {}",
        chosen.reason
    );
    assert!(chosen.reason.contains("priority high"), "{}", chosen.reason);
}

#[test]
fn gc_clears_blocked_without_open_blockers() {
    let (mut conn, project) = fixture("gcblocked");
    let t = tree(&mut conn, &project);

    // статус выставлен руками, рёбер нет — это и есть устаревший blocked
    node::update(
        &mut conn,
        &project,
        &t.task_a.dot_id,
        node::Patch {
            status: Some(Status::Blocked),
            ..node::Patch::default()
        },
    )
    .expect("blocked");

    let cleared = edge::clear_stale_blocked(&mut conn, &project).expect("clear");
    assert_eq!(cleared, 1);
    let node = node::resolve(&conn, &project, &t.task_a.dot_id).expect("node");
    assert_eq!(node.status, "todo");
}

#[test]
fn deleted_blocker_does_not_hold_the_node() {
    let (mut conn, project) = fixture("deleted");
    let t = tree(&mut conn, &project);

    // задача A ждёт удалённую задачу B: после удаления B ожидание не держит
    edge::link(
        &mut conn,
        &project,
        &t.task_a.dot_id,
        &t.task_b.dot_id,
        Kind::Blocks,
        "-",
    )
    .expect("link");
    node::delete(&mut conn, &project, &t.task_b.dot_id).expect("delete todo blocker");

    let node = node::resolve(&conn, &project, &t.task_a.dot_id).expect("node");
    assert_ne!(
        node.effective_status, "blocked",
        "удалённый узел не блокирует"
    );
}

#[test]
fn every_kind_round_trips_through_the_database() {
    let (mut conn, project) = fixture("kinds");
    let t = tree(&mut conn, &project);

    for kind in [
        Kind::Related,
        Kind::Duplicates,
        Kind::DiscoveredFrom,
        Kind::SplitFrom,
    ] {
        edge::link(
            &mut conn,
            &project,
            &t.task_b.dot_id,
            &t.task_a.dot_id,
            kind,
            "-",
        )
        .expect("link");
        let links = edge::list(&conn, &project, Some(kind)).expect("list");
        assert_eq!(links.len(), 1, "вид {} сохраняется", kind.as_str());
    }
}
