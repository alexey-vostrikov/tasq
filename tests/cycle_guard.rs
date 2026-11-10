//! Перенос узла не может замкнуть дерево на себя.
//!
//! Проверка нужна не для красоты иерархии: `path` считается обходом дерева, и цикл делает
//! чтение бесконечным. Два сценария, где защита обязана сработать:
//!
//!   * прямой перенос в собственного потомка — обычная ошибка пользователя;
//!   * одновременные переносы A→B и B→A из двух процессов — гонка, в которой проверка «до
//!     транзакции» проходила у обоих, и цикл создавался уже после проверки.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};

use rusqlite::Connection;

use tasq::db;
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-cycle-{tag}-{nanos}"));
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
    let outcome = db::init(&dir, "CYC", "cycle").expect("init");
    let path = PathBuf::from(&outcome.db_path);
    let conn = db::open(&path).expect("open");
    let project = project::first_project(&conn).expect("project");
    (path, conn, project)
}

/// Перенос в собственного потомка отвергается, дерево остаётся целым.
#[test]
fn moving_into_a_descendant_is_refused() {
    let (_path, mut conn, project) = fixture("descendant");
    let root = add(&mut conn, &project, None, "Корень");
    let task = add(&mut conn, &project, Some(&root.dot_id), "Задача");
    let leaf = add(&mut conn, &project, Some(&task.dot_id), "Лист");

    let error = node::move_node(&mut conn, &project, &root.dot_id, Some(&leaf.dot_id), false)
        .expect_err("перенос в собственного потомка прошёл");
    assert_eq!(error.code(), "MOVE_INTO_SELF", "{error}");

    // Дерево не тронуто: корень на месте, лист под задачей.
    assert_eq!(
        node::resolve(&conn, &project, &root.dot_id)
            .expect("root")
            .depth,
        1
    );
    assert_eq!(
        node::resolve(&conn, &project, &leaf.dot_id)
            .expect("leaf")
            .depth,
        3
    );
}

/// Перенос в себя отвергается.
#[test]
fn moving_into_itself_is_refused() {
    let (_path, mut conn, project) = fixture("self");
    let root = add(&mut conn, &project, None, "Корень");
    let error = node::move_node(&mut conn, &project, &root.dot_id, Some(&root.dot_id), false)
        .expect_err("перенос в себя прошёл");
    assert_eq!(error.code(), "MOVE_INTO_SELF", "{error}");
}

/// Одновременные переносы A→B и B→A: ровно один проходит, цикл не образуется.
///
/// Проверка «до транзакции» в одиночку этот случай не закрывает: два процесса проходят её
/// одновременно, видя ещё старое дерево, и оба пишут. Внутри транзакции второй видит уже
/// перенесённое дерево и отказывает.
#[test]
fn two_opposite_moves_at_once_never_both_succeed() {
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
            let outcome = node::move_node(&mut conn, &project, &from, Some(&to), false);
            (
                from,
                outcome.map(|node| node.dot_id).map_err(|err| err.code()),
            )
        }));
    }

    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread"))
        .collect();
    let failures: Vec<_> = results
        .iter()
        .filter(|(_, outcome)| outcome.is_err())
        .collect();
    assert_eq!(
        results.len() - failures.len(),
        1,
        "прошло не ровно одно перемещение: {results:?}"
    );

    // Дерево обязано остаться деревом: подъём от каждого узла к корню конечен.
    let conn = db::open(&path).expect("reopen");
    // По внутреннему id: узел, который переехал, сменил адрес, и это ожидаемо.
    for id in [a.id, b.id] {
        let mut current = Some(id);
        let mut steps = 0;
        while let Some(id) = current {
            steps += 1;
            assert!(steps <= 16, "дерево замкнулось в цикл: #{id}");
            current = conn
                .query_row("SELECT parent_id FROM nodes WHERE id = ?1", [id], |row| {
                    row.get::<_, Option<i64>>(0)
                })
                .expect("parent");
        }
    }
}

/// Испорченная база (цикл уже есть, записан руками или другой версией) не вешает перенос:
/// обход ограничен, а не «пока не кончится».
#[test]
fn a_corrupted_base_does_not_hang_the_move() {
    let (_path, mut conn, project) = fixture("corrupt");
    let root = add(&mut conn, &project, None, "Корень");
    let task = add(&mut conn, &project, Some(&root.dot_id), "Задача");
    let mover = add(&mut conn, &project, None, "Переносимый");

    // Портим базу руками: корень становится потомком задачи, а задача — потомком корня.
    conn.execute(
        "UPDATE nodes SET parent_id = ?1 WHERE id = ?2",
        rusqlite::params![task.id, root.id],
    )
    .expect("break the tree");

    // Команда обязана завершиться отказом или успехом, но не зависнуть.
    let _ = node::move_node(
        &mut conn,
        &project,
        &mover.dot_id,
        Some(&task.dot_id),
        false,
    );
    // Проверяем по внутреннему id: если перенос прошёл, адрес узла изменился — и это
    // правильное поведение, а не потеря.
    assert!(node::resolve(&conn, &project, &format!("#{}", mover.id)).is_ok());
}

/// Испорченная база не вешает ЧТЕНИЕ. Это важнее, чем защита переноса: цикл ломал не команду
/// переноса, а любое чтение, включая `show` и `ls`.
///
/// Найдено этим же набором тестов: `v_node_progress` обходил дерево вверх без ограничения, и
/// `select count(*) from v_node_progress` на базе с циклом не завершался вовсе.
#[test]
fn a_cycle_does_not_hang_the_progress_view() {
    let (_path, mut conn, project) = fixture("progress-cycle");
    let root = add(&mut conn, &project, None, "Корень");
    let task = add(&mut conn, &project, Some(&root.dot_id), "Задача");

    // Замыкаем дерево: корень становится потомком задачи.
    conn.execute(
        "UPDATE nodes SET parent_id = ?1 WHERE id = ?2",
        rusqlite::params![task.id, root.id],
    )
    .expect("break the tree");

    // Чтение завершается: обход ограничен числом шагов.
    let counted: i64 = conn
        .query_row("SELECT COUNT(*) FROM v_node_progress", [], |row| row.get(0))
        .expect("progress view on a cyclic base");
    assert!(counted > 0, "представление вернуло пустоту вместо строк");

    // И обход именно ОГРАНИЧЕН, а не «завершился случайно»: на цикле из двух узлов без
    // ограничения строк было бы бесконечно много, а с щедрым пределом — десятки тысяч.
    // Предел 64 шага в схеме, поэтому больше 65 строк на узел быть не может.
    let worst: i64 = conn
        .query_row("SELECT MAX(total_count) FROM v_node_progress", [], |row| {
            row.get(0)
        })
        .expect("counts");
    assert!(
        worst <= 65,
        "обход не ограничен: одна ветка насчитала {worst} строк"
    );

    // И обычные команды тоже: `show` тянет прогресс узла.
    assert!(node::resolve(&conn, &project, &root.dot_id).is_ok());
    assert!(node::list(&conn, &project, &Default::default()).is_ok());
}

/// Обычный перенос по-прежнему работает и пересчитывает пути потомков.
#[test]
fn a_legal_move_still_works() {
    let (_path, mut conn, project) = fixture("legal");
    let left = add(&mut conn, &project, None, "Левая ветка");
    let right = add(&mut conn, &project, None, "Правая ветка");
    let task = add(&mut conn, &project, Some(&left.dot_id), "Задача");
    let leaf = add(&mut conn, &project, Some(&task.dot_id), "Лист");

    let moved = node::move_node(
        &mut conn,
        &project,
        &task.dot_id,
        Some(&right.dot_id),
        false,
    )
    .expect("перенос в соседнюю ветку");
    assert_eq!(moved.path, "2.1", "узел должен переехать под правую ветку");

    // После переноса старый адрес больше не резолвится — узел переехал вместе с веткой.
    // Обращаемся по внутреннему id: адрес и есть то, что изменилось.
    let leaf_after = node::resolve(&conn, &project, &format!("#{}", leaf.id)).expect("leaf");
    assert_eq!(leaf_after.path, "2.1.1", "путь потомка не пересчитан");
    assert_eq!(leaf_after.depth, 3);
}
