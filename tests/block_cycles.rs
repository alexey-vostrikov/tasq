//! Замкнутые цепочки блокировок запрещены — любой длины.
//!
//! `blocks` означает «этот ждёт того». Замкнутая цепочка — это работа, которая не начнётся
//! никогда: каждый узел ждёт следующего, и ни один не может стать первым. Поэтому круг
//! отбивается до записи, а не диагностируется потом.
//!
//! Проверяются три случая, которые ломаются по-разному:
//!   * пара из двух задач (A ждёт B, B ждёт A) — самый частый и самый незаметный;
//!   * длинная цепочка (A→B→C→D→A) — видно только транзитивным поиском;
//!   * две одновременные заявки, каждая из которых по отдельности законна.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};

use rusqlite::Connection;

use tasq::db;
use tasq::edge::{self, Kind};
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-block-{tag}-{nanos}"));
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

fn fixture(tag: &str) -> (PathBuf, Connection, Project) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "BLK", "blocks").expect("init");
    let path = PathBuf::from(&outcome.db_path);
    let conn = db::open(&path).expect("open");
    let project = project::first_project(&conn).expect("project");
    (path, conn, project)
}

/// Пара взаимных блокировок отвергается: второе ребро замкнуло бы круг.
#[test]
fn two_tasks_cannot_block_each_other() {
    let (_path, mut conn, project) = fixture("pair");
    let a = add(&mut conn, &project, None, "A");
    let b = add(&mut conn, &project, None, "B");

    edge::link(
        &mut conn,
        &project,
        &a.dot_id,
        &b.dot_id,
        Kind::Blocks,
        "agent",
    )
    .expect("A ждёт B");

    let error = edge::link(
        &mut conn,
        &project,
        &b.dot_id,
        &a.dot_id,
        Kind::Blocks,
        "agent",
    )
    .expect_err("встречное ребро прошло");
    assert_eq!(error.code(), "CYCLE_DETECTED", "{error}");
    // В сообщении виден весь круг, а не только пара: иначе непонятно, что размыкать.
    let text = error.to_string();
    assert!(text.contains("A") || text.contains("B"), "{text}");

    // Граф цел: осталось ровно одно ребро.
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE kind = 'blocks'",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(count, 1);
}

/// Круг из трёх и из пяти узлов ловится так же, как пара: поиск транзитивный.
#[test]
fn longer_chains_are_detected() {
    let (_path, mut conn, project) = fixture("chain");
    let a = add(&mut conn, &project, None, "A");
    let b = add(&mut conn, &project, None, "B");
    let c = add(&mut conn, &project, None, "C");
    let d = add(&mut conn, &project, None, "D");
    let e = add(&mut conn, &project, None, "E");

    // A ждёт B, B ждёт C, C ждёт D, D ждёт E.
    for (from, to) in [(&a, &b), (&b, &c), (&c, &d), (&d, &e)] {
        edge::link(
            &mut conn,
            &project,
            &from.dot_id,
            &to.dot_id,
            Kind::Blocks,
            "agent",
        )
        .unwrap_or_else(|err| panic!("{} ждёт {}: {err}", from.dot_id, to.dot_id));
    }

    // E ждёт A замкнуло бы всю цепочку — отказ.
    let error = edge::link(
        &mut conn,
        &project,
        &e.dot_id,
        &a.dot_id,
        Kind::Blocks,
        "agent",
    )
    .expect_err("цепочка из пяти замкнулась");
    assert_eq!(error.code(), "CYCLE_DETECTED", "{error}");
    let text = error.to_string();
    // Круг показан целиком: от замыкающего ребра до него самого.
    for dot in [&a.dot_id, &b.dot_id, &c.dot_id, &d.dot_id, &e.dot_id] {
        assert!(text.contains(dot.as_str()), "в круге нет {dot}: {text}");
    }

    // А ребро через узел, который не замыкает круг, проходит: запрещать лишнее нельзя.
    edge::link(
        &mut conn,
        &project,
        &a.dot_id,
        &e.dot_id,
        Kind::Blocks,
        "agent",
    )
    .expect("второе ребро A ждёт E не замыкает круг");
}

/// Замыкание через третью ветку: A ждёт B и C, B ждёт C, C ждёт A — отказ.
#[test]
fn a_cycle_through_a_side_branch_is_detected() {
    let (_path, mut conn, project) = fixture("branch");
    let a = add(&mut conn, &project, None, "A");
    let b = add(&mut conn, &project, None, "B");
    let c = add(&mut conn, &project, None, "C");

    edge::link(
        &mut conn,
        &project,
        &a.dot_id,
        &b.dot_id,
        Kind::Blocks,
        "agent",
    )
    .expect("A→B");
    edge::link(
        &mut conn,
        &project,
        &b.dot_id,
        &c.dot_id,
        Kind::Blocks,
        "agent",
    )
    .expect("B→C");

    let error = edge::link(
        &mut conn,
        &project,
        &c.dot_id,
        &a.dot_id,
        Kind::Blocks,
        "agent",
    )
    .expect_err("круг A→B→C→A прошёл");
    assert_eq!(error.code(), "CYCLE_DETECTED", "{error}");
}

/// Узел не может ждать сам себя.
#[test]
fn a_node_cannot_wait_for_itself() {
    let (_path, mut conn, project) = fixture("self");
    let a = add(&mut conn, &project, None, "A");
    let error = edge::link(
        &mut conn,
        &project,
        &a.dot_id,
        &a.dot_id,
        Kind::Blocks,
        "agent",
    )
    .expect_err("ребро в себя прошло");
    assert_eq!(error.code(), "SELF_EDGE", "{error}");
}

/// Нетранзитивные рёбра (`related`, `duplicates`) круг не образуют и не ограничиваются:
/// две задачи могут быть связаны взаимно, и это нормально.
#[test]
fn non_blocking_relations_may_be_mutual() {
    let (_path, mut conn, project) = fixture("related");
    let a = add(&mut conn, &project, None, "A");
    let b = add(&mut conn, &project, None, "B");

    edge::link(
        &mut conn,
        &project,
        &a.dot_id,
        &b.dot_id,
        Kind::Related,
        "agent",
    )
    .expect("A ~ B");
    edge::link(
        &mut conn,
        &project,
        &b.dot_id,
        &a.dot_id,
        Kind::Related,
        "agent",
    )
    .expect("B ~ A тоже допустимо: related не про порядок работ");
}

/// Две одновременные заявки, каждая из которых по отдельности законна.
///
/// Именно этот случай проверка «до транзакции» не закрывала: оба процесса видели граф без
/// встречного ребра и оба вставляли своё. Внутри `BEGIN IMMEDIATE` второй видит первое.
#[test]
fn two_simultaneous_opposite_links_never_both_succeed() {
    let (path, mut conn, project) = fixture("race");
    let a = add(&mut conn, &project, None, "A");
    let b = add(&mut conn, &project, None, "B");
    let a_id = a.dot_id.clone();
    let b_id = b.dot_id.clone();
    drop(conn);

    let barrier = Arc::new(Barrier::new(2));
    let shared: Arc<Project> = Arc::new(project);
    let mut handles = Vec::new();
    for (from, to) in [(a_id.clone(), b_id.clone()), (b_id.clone(), a_id.clone())] {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        let shared = Arc::clone(&shared);
        handles.push(std::thread::spawn(move || {
            let project = (*shared).clone();
            let mut conn = db::open(&path).expect("open");
            barrier.wait();
            edge::link(&mut conn, &project, &from, &to, Kind::Blocks, "racer")
                .map(|_| ())
                .map_err(|err| err.code())
        }));
    }
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread"))
        .collect();

    let succeeded = results.iter().filter(|result| result.is_ok()).count();
    assert_eq!(
        succeeded, 1,
        "прошло не ровно одно ребро — образовалась замкнутая пара: {results:?}"
    );

    let conn = db::open(&path).expect("reopen");
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE kind = 'blocks'",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(count, 1, "в графе больше одного ребра");
}
