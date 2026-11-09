//! Дерево узлов: dot-ID, матрица переходов, перенос, архив, мягкое удаление.

use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;

use tasq::db;
use tasq::error::Error;
use tasq::node::{self, Filter, NewNode, Patch};
use tasq::project::{self, Project};
use tasq::status::{Kind, Status};
use tasq::{activity, status};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-tree-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("create temp dir");
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

fn set_status(
    conn: &mut Connection,
    project: &Project,
    reference: &str,
    status: Status,
    force: bool,
) -> tasq::Result<node::Node> {
    node::update(
        conn,
        project,
        reference,
        Patch {
            status: Some(status),
            force,
            ..Patch::default()
        },
    )
}

#[test]
fn dot_ids_follow_the_tree() {
    let (mut conn, project) = fixture("dots");
    let epic = add(&mut conn, &project, None, "Epic");
    assert_eq!(epic.dot_id, "TASQ-1");
    assert_eq!(epic.kind, "epic");
    assert_eq!(epic.status, "idea", "воронка: новый эпик — идея");

    let task = add(&mut conn, &project, Some("TASQ-1"), "Task");
    assert_eq!(task.dot_id, "TASQ-1.1");
    assert_eq!(task.kind, "task");
    assert_eq!(task.status, "idea", "воронка: новая задача — идея");

    let sub = add(&mut conn, &project, Some("1.1"), "Subtask");
    assert_eq!(sub.dot_id, "TASQ-1.1.1");
    assert_eq!(sub.kind, "subtask");
}

#[test]
fn sequence_numbers_are_never_reused() {
    let (mut conn, project) = fixture("seq");
    let epic = add(&mut conn, &project, None, "Epic");
    let first = add(&mut conn, &project, Some(&epic.dot_id), "First");
    let second = add(&mut conn, &project, Some(&epic.dot_id), "Second");
    assert_eq!(first.path, "1.1");
    assert_eq!(second.path, "1.2");

    node::delete(&mut conn, &project, &second.dot_id).expect("delete todo node");
    let third = add(&mut conn, &project, Some(&epic.dot_id), "Third");
    assert_eq!(
        third.path, "1.3",
        "номер удалённого узла не переиспользуется"
    );
}

#[test]
fn reference_forms_resolve_to_the_same_node() {
    let (mut conn, project) = fixture("refs");
    let epic = add(&mut conn, &project, None, "Epic");
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Task");

    for reference in [
        task.dot_id.clone(),
        task.path.clone(),
        format!("#{}", task.id),
    ] {
        let resolved = node::resolve(&conn, &project, &reference).expect("resolve");
        assert_eq!(resolved.id, task.id, "форма {reference}");
    }

    let err = node::resolve(&conn, &project, "OTHER-1").unwrap_err();
    assert_eq!(err.code(), "INVALID_REF");
    let err = node::resolve(&conn, &project, "TASQ-99").unwrap_err();
    assert_eq!(err.code(), "NODE_NOT_FOUND");
}

#[test]
fn in_progress_needs_a_lease() {
    let (mut conn, project) = fixture("lease");
    let epic = add(&mut conn, &project, None, "Epic");
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Task");

    let err = set_status(&mut conn, &project, &task.dot_id, Status::InProgress, false).unwrap_err();
    assert_eq!(err.code(), "LEASE_REQUIRED");
    assert!(matches!(err, Error::LeaseRequired));
    // статус не изменился
    let reread = node::resolve(&conn, &project, &task.dot_id).expect("node");
    assert_eq!(reread.status, "idea", "статус не изменился отказом");
}

#[test]
fn transition_matrix_is_enforced() {
    let (mut conn, project) = fixture("matrix");
    let epic = add(&mut conn, &project, None, "Epic");
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Task");

    let err = set_status(&mut conn, &project, &task.dot_id, Status::Done, false).unwrap_err();
    assert_eq!(err.code(), "INVALID_TRANSITION", "todo -> done запрещён");

    // todo -> blocked разрешён, blocked -> todo тоже
    set_status(&mut conn, &project, &task.dot_id, Status::Blocked, false).expect("block");
    let blocked = node::resolve(&conn, &project, &task.dot_id).expect("node");
    assert_eq!(blocked.effective_status, "blocked");
    set_status(&mut conn, &project, &task.dot_id, Status::Todo, false).expect("unblock");

    // epic не может быть todo
    let err = set_status(&mut conn, &project, &epic.dot_id, Status::Todo, false).unwrap_err();
    assert_eq!(err.code(), "INVALID_TRANSITION");
}

/// Правка с правом аренды: `lease` — идентификатор узла, покрытого живой арендой.
/// В этом тесте матрицы аренда подразумевается, настоящая проверка — в `tests/lease.rs`.
fn set_status_leased(
    conn: &mut Connection,
    project: &Project,
    reference: &str,
    status: Status,
    force: bool,
) -> tasq::Result<node::Node> {
    let node = node::resolve(conn, project, reference)?;
    node::update(
        conn,
        project,
        reference,
        Patch {
            status: Some(status),
            force,
            lease: Some(node.id),
            ..Patch::default()
        },
    )
}

#[test]
fn children_guard_fires_with_forced_transition_path() {
    let (mut conn, project) = fixture("guard");
    let epic = add(&mut conn, &project, None, "Epic");
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Task");
    add(&mut conn, &project, Some(&task.dot_id), "Sub");

    // todo -> in_progress (право даёт аренда) -> review -> done: матрица пропускает,
    // но дети не закрыты — гвард срабатывает
    set_status_leased(&mut conn, &project, &task.dot_id, Status::InProgress, false)
        .expect("claim-like transition");
    set_status(&mut conn, &project, &task.dot_id, Status::Review, false).expect("review");
    let err = set_status(&mut conn, &project, &task.dot_id, Status::Done, false).unwrap_err();
    assert_eq!(err.code(), "CHILDREN_UNFINISHED");

    let forced = set_status(&mut conn, &project, &task.dot_id, Status::Done, true).expect("forced");
    assert_eq!(forced.status, "done");
}

#[test]
fn description_lock_blocks_rewrites() {
    let (mut conn, project) = fixture("lock");
    let epic = add(&mut conn, &project, None, "Epic");

    node::update(
        &mut conn,
        &project,
        &epic.dot_id,
        Patch {
            description: Some("spec v1"),
            description_locked: Some(true),
            ..Patch::default()
        },
    )
    .expect("set description and lock");

    let err = node::update(
        &mut conn,
        &project,
        &epic.dot_id,
        Patch {
            description: Some("spec v2"),
            ..Patch::default()
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), "DESCRIPTION_LOCKED");

    // прочие поля меняются свободно
    node::update(
        &mut conn,
        &project,
        &epic.dot_id,
        Patch {
            title: Some("Epic renamed"),
            ..Patch::default()
        },
    )
    .expect("title still editable");
}

#[test]
fn move_recomputes_descendant_paths() {
    let (mut conn, project) = fixture("move");
    let first = add(&mut conn, &project, None, "First epic");
    let second = add(&mut conn, &project, None, "Second epic");
    let task = add(&mut conn, &project, Some(&first.dot_id), "Task");
    let sub = add(&mut conn, &project, Some(&task.dot_id), "Sub");
    assert_eq!(sub.dot_id, "TASQ-1.1.1");

    let moved = node::move_node(
        &mut conn,
        &project,
        &task.dot_id,
        Some(&second.dot_id),
        false,
    )
    .expect("move");
    assert_eq!(moved.dot_id, "TASQ-2.1");
    let sub_after =
        node::resolve(&conn, &project, "TASQ-2.1.1").expect("потомок резолвится по новому пути");
    assert_eq!(sub_after.dot_id, "TASQ-2.1.1");
    assert_eq!(sub_after.depth, 3);
    assert_eq!(sub_after.id, sub.id, "узел тот же, изменился только путь");

    // старые пути больше не принадлежат узлам
    assert!(node::resolve(&conn, &project, "TASQ-1.1").is_err());
    assert!(node::resolve(&conn, &project, "TASQ-1.1.1").is_err());

    // перенос в собственное поддерево запрещён
    let err = node::move_node(
        &mut conn,
        &project,
        &moved.dot_id,
        Some(&sub_after.dot_id),
        false,
    )
    .unwrap_err();
    assert_eq!(err.code(), "MOVE_INTO_SELF");
}

#[test]
fn archive_hides_the_subtree() {
    let (mut conn, project) = fixture("archive");
    let epic = add(&mut conn, &project, None, "Epic");
    add(&mut conn, &project, Some(&epic.dot_id), "Task");

    node::archive(&mut conn, &project, &epic.dot_id, false).expect("archive");
    let visible = node::list(&conn, &project, &Filter::default()).expect("list");
    assert!(visible.is_empty(), "архив скрыт по умолчанию");

    let all = node::list(
        &conn,
        &project,
        &Filter {
            include_archived: true,
            ..Filter::default()
        },
    )
    .expect("list");
    assert_eq!(all.len(), 2);

    node::archive(&mut conn, &project, &epic.dot_id, true).expect("unarchive");
    let visible = node::list(&conn, &project, &Filter::default()).expect("list");
    assert_eq!(visible.len(), 2);
}

#[test]
fn only_todo_nodes_can_be_removed() {
    let (mut conn, project) = fixture("delete");
    let epic = add(&mut conn, &project, None, "Epic");
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Task");

    set_status(&mut conn, &project, &task.dot_id, Status::Blocked, false).expect("block");
    let err = node::delete(&mut conn, &project, &task.dot_id).unwrap_err();
    assert_eq!(err.code(), "NOT_REMOVABLE");

    set_status(&mut conn, &project, &task.dot_id, Status::Todo, false).expect("unblock");
    node::delete(&mut conn, &project, &task.dot_id).expect("delete todo");
    assert!(node::resolve(&conn, &project, &task.dot_id).is_err());

    let restored = node::restore(&mut conn, &project, &task.dot_id).expect("restore");
    assert_eq!(restored.dot_id, task.dot_id);
    assert_eq!(restored.status, "todo");
}

#[test]
fn every_mutation_lands_in_the_activity_log() {
    let (mut conn, project) = fixture("activity");
    let epic = add(&mut conn, &project, None, "Epic");
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Task");
    node::update(
        &mut conn,
        &project,
        &task.dot_id,
        Patch {
            priority: Some("high"),
            ..Patch::default()
        },
    )
    .expect("update");

    let records = activity::recent(&conn, project.id, 50).expect("log");
    let actions: Vec<&str> = records.iter().map(|r| r.action.as_str()).collect();
    assert!(actions.contains(&"create"), "есть создание");
    assert!(actions.contains(&"update"), "есть изменение поля");

    let priority_change = records
        .iter()
        .find(|r| r.field_name.as_deref() == Some("priority"))
        .expect("запись об изменении приоритета");
    assert_eq!(priority_change.old_value.as_deref(), Some("medium"));
    assert_eq!(priority_change.new_value.as_deref(), Some("high"));
}

#[test]
fn status_helpers_stay_consistent() {
    assert!(status::Status::can_transition(
        Status::Todo,
        Status::Blocked,
        Kind::Task
    ));
    assert!(!status::Status::can_transition(
        Status::Blocked,
        Status::Review,
        Kind::Task
    ));
}
