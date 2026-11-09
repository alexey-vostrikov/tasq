//! Поиск и очередь «что дальше»: два пути (FTS и обычное сравнение) и причины выбора.
//!
//! FTS5 не разбирает произвольный текст: кавычка, двоеточие или дефис ломают выражение.
//! Тогда поиск обязан отступить к сравнению подстрок и всё равно найти то же самое —
//! иначе пользователь получает пустоту на ровном месте.

use std::path::PathBuf;

use rusqlite::Connection;

use tasq::db;
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};
use tasq::{edge, note, overview};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-search-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn fixture(tag: &str) -> (Connection, Project, PathBuf) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "TASQ", "demo").expect("init");
    let conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");
    (conn, project, dir)
}

fn add(conn: &mut Connection, project: &Project, title: &str) -> node::Node {
    node::create(
        conn,
        project,
        NewNode {
            parent: None,
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

fn dot_of(hits: &[overview::Hit], entity: &str) -> Vec<String> {
    hits.iter()
        .filter(|hit| hit.entity == entity)
        .map(|hit| hit.dot_id.clone().unwrap_or_default())
        .collect()
}

/// Поиск находит и узел, и заметку; заметка приходит со ссылкой на узел.
#[test]
fn search_reaches_nodes_and_notes() {
    let (mut conn, project, _dir) = fixture("both");
    let node = add(&mut conn, &project, "Миграция на WAL");
    note::save(
        &mut conn,
        &project,
        note::SaveNote {
            id: None,
            node: Some(&node.dot_id),
            title: Some("Решение про WAL"),
            content: Some("включаем WAL и контрольные точки"),
            note_type: Some("decision"),
            actor: Some("-"),
        },
    )
    .expect("note");

    let hits = overview::search(&conn, &project, "WAL").expect("search");
    assert!(dot_of(&hits, "node").contains(&node.dot_id), "{hits:?}");
    let notes: Vec<&overview::Hit> = hits.iter().filter(|hit| hit.entity == "note").collect();
    assert_eq!(notes.len(), 1, "{hits:?}");
    assert_eq!(notes[0].dot_id.as_deref(), Some(node.dot_id.as_str()));
    assert!(notes[0].title.contains("WAL"), "{:?}", notes[0].title);
}

/// Кавычка в запросе ломает выражение FTS: поиск обязан отступить к подстрокам
/// и найти то же слово.
#[test]
fn a_broken_fts_query_falls_back_to_substrings() {
    let (mut conn, project, _dir) = fixture("fallback");
    let node = add(&mut conn, &project, "Контрольная точка");
    note::save(
        &mut conn,
        &project,
        note::SaveNote {
            id: None,
            node: Some(&node.dot_id),
            title: Some("Заметка о точке"),
            content: Some("проверяем контрольная точка в деле"),
            note_type: None,
            actor: Some("-"),
        },
    )
    .expect("note");

    // Незакрытая кавычка: FTS5 такое выражение не разбирает.
    let hits = overview::search(&conn, &project, "\"контрольная").expect("search");
    assert!(
        dot_of(&hits, "node").contains(&node.dot_id),
        "узел найден сравнением подстрок: {hits:?}"
    );
    assert_eq!(
        hits.iter().filter(|hit| hit.entity == "note").count(),
        1,
        "заметка тоже: {hits:?}"
    );
}

/// Двоеточие и дефис — обычные символы для пользователя, но не для FTS.
#[test]
fn punctuation_does_not_break_the_search() {
    let (mut conn, project, _dir) = fixture("punctuation");
    let node = add(&mut conn, &project, "Задача: починить - и проверить");

    for query in ["Задача:", "- проверить", "починить -"] {
        let hits = overview::search(&conn, &project, query).expect(query);
        assert!(
            dot_of(&hits, "node").contains(&node.dot_id),
            "запрос {query:?} ничего не нашёл"
        );
    }
}

/// Слово, которого нет, даёт пустой список без ошибки; пустой запрос — отказ.
#[test]
fn empty_and_missing_queries_differ() {
    let (mut conn, project, _dir) = fixture("empty");
    add(&mut conn, &project, "Эпик");

    let hits = overview::search(&conn, &project, "нетакогослова").expect("поиск без совпадений");
    assert!(hits.is_empty(), "{hits:?}");

    for query in ["", "   "] {
        let error = overview::search(&conn, &project, query).expect_err("пустой запрос");
        assert_eq!(error.code(), "INVALID_VALUE");
    }
}

/// Очередь «что дальше» объясняет выбор: просрочка, срок и «разблокирует» видны в причине.
#[test]
fn next_explains_why_a_node_is_first() {
    let (mut conn, project, _dir) = fixture("next");
    let epic = add(&mut conn, &project, "Эпик");
    let overdue = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: Some(&epic.dot_id),
            title: "Просроченная",
            kind: None,
            description: None,
            priority: Some("high"),
            due_date: Some("2001-01-01"),
            estimated_hours: None,
            weight: None,
            source_ref: None,
            branch: None,
        },
    )
    .expect("overdue");
    let scheduled = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: Some(&epic.dot_id),
            title: "Со сроком",
            kind: None,
            description: None,
            priority: None,
            due_date: Some("2099-01-01"),
            estimated_hours: None,
            weight: None,
            source_ref: None,
            branch: None,
        },
    )
    .expect("scheduled");
    let dependent = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: Some(&epic.dot_id),
            title: "Ждёт просрочку",
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
    .expect("dependent");
    // Очередь «что дальше» работает по готовым к работе узлам: воронка отдаёт их в `todo`.
    for accepted in [&overdue, &scheduled, &dependent] {
        node::update(
            &mut conn,
            &project,
            &accepted.dot_id,
            node::Patch {
                status: Some(tasq::status::Status::Todo),
                ..node::Patch::default()
            },
        )
        .expect("принять в очередь");
    }
    edge::link(
        &mut conn,
        &project,
        &dependent.dot_id,
        &overdue.dot_id,
        edge::Kind::Blocks,
        "-",
    )
    .expect("link");

    let candidates = edge::next_candidates(&conn, &project, 10).expect("next");
    let first = candidates.first().expect("есть что брать");
    assert_eq!(first.dot_id, overdue.dot_id, "просрочка важнее срока");
    assert!(first.reason.contains("overdue by"), "{}", first.reason);
    assert!(first.reason.contains("unblocks 1"), "{}", first.reason);
    assert!(first.days_overdue > 0);

    let later = candidates
        .iter()
        .find(|candidate| candidate.dot_id == scheduled.dot_id)
        .expect("в списке");
    assert!(later.reason.contains("due 2099-01-01"), "{}", later.reason);

    // Блокированный узел в очередь не попадает вовсе.
    assert!(
        !candidates
            .iter()
            .any(|candidate| candidate.dot_id == dependent.dot_id),
        "{candidates:?}"
    );
}

/// Контекст — цепочка предков сверху вниз плюс сам узел (README §6): агент получает
/// «зачем» от эпика и «что именно» от своего уровня. Дети в контекст не входят.
#[test]
fn context_walks_the_chain() {
    let (mut conn, project, _dir) = fixture("context");
    let epic = add(&mut conn, &project, "Эпик");
    let task = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: Some(&epic.dot_id),
            title: "Задача",
            kind: None,
            description: Some("описание задачи"),
            priority: None,
            due_date: None,
            estimated_hours: None,
            weight: None,
            source_ref: None,
            branch: None,
        },
    )
    .expect("task");
    let subtask = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: Some(&task.dot_id),
            title: "Подзадача",
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

    let entries = overview::context(&conn, &project, &subtask.dot_id).expect("context");
    let chain: Vec<&str> = entries.iter().map(|entry| entry.dot_id.as_str()).collect();
    assert_eq!(
        chain,
        vec![
            epic.dot_id.as_str(),
            task.dot_id.as_str(),
            subtask.dot_id.as_str()
        ],
        "от эпика к самому узлу"
    );
    // Описание задачи видно из её подзадачи: это и есть «зачем».
    let task_entry = entries
        .iter()
        .find(|entry| entry.dot_id == task.dot_id)
        .expect("задача в цепочке");
    assert_eq!(task_entry.description.as_deref(), Some("описание задачи"));

    // Доска считает открытую работу и пустые колонки перечисляет.
    let board = overview::board(&conn, &project).expect("board");
    assert_eq!(board.project, "TASQ");
    assert!(board.open >= 2, "эпик и задача открыты: {board:?}");
    assert_eq!(board.done, 0, "закрытых нет");
}
