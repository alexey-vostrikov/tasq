//! Обзор без выбора проекта: доска, поиск и список не требуют одного проекта.
//!
//! Дефект, который здесь закреплён: команды обзора работали только внутри проекта. В базе с
//! несколькими проектами `find` отвечал `PROJECT_REQUIRED`, а `board` показывал один проект —
//! то есть на вопрос «что у меня вообще есть» ответа не было, и «не найдено» означало не
//! «нет», а «не посмотрел».
//!
//! Главное свойство, которое проверяется: счётчики не перемешиваются. Сложить два проекта в
//! одну сводку — значит показать число, которое не относится ни к одному из них.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use rusqlite::Connection;
use serde_json::Value;

use tasq::db;
use tasq::lease;
use tasq::node::{self, NewNode};
use tasq::overview;
use tasq::project::{self, Project};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tasq")
}

struct Fixture {
    dir: PathBuf,
    db: PathBuf,
}

fn fixture(tag: &str) -> (Fixture, Connection, Project, Project) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-all-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    let outcome = db::init(&dir, "AAA", "первый").expect("init");
    let db_path = PathBuf::from(&outcome.db_path);
    let mut conn = db::open(std::path::Path::new(&db_path)).expect("open");
    let first = project::first_project(&conn).expect("first");
    let second = project::create(&mut conn, "BBB", Some("второй")).expect("second");
    (Fixture { dir, db: db_path }, conn, first, second)
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
    .expect("node")
}

fn add_child(conn: &mut Connection, project: &Project, parent: &str, title: &str) -> node::Node {
    node::create(
        conn,
        project,
        NewNode {
            parent: Some(parent),
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
    .expect("child")
}

/// Закрывает узел так, как это делает работа: аренда и исход «готово».
///
/// Прямой перевод `idea → done` матрица запрещает (закрывать можно только начатое), и это
/// правильно: «закрыто» без попытки — это не выполненная работа, а запись в отчёт.
fn close(conn: &mut Connection, project: &Project, node_ref: &str) {
    let held = lease::claim(
        conn,
        project,
        node_ref,
        &lease::ClaimRequest {
            holder: "agent",
            ttl_seconds: lease::DEFAULT_TTL,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
    )
    .expect("claim");
    lease::release(
        conn,
        project,
        &held.token,
        lease::Outcome::Completed,
        None,
        false,
    )
    .expect("release");
}

fn cli(fixture: &Fixture, args: &[&str]) -> String {
    let mut full = vec!["--db", fixture.db.to_str().expect("path")];
    full.extend_from_slice(args);
    let out = Command::new(bin())
        .current_dir(&fixture.dir)
        .args(&full)
        .output()
        .expect("run tasq");
    assert!(
        out.status.success(),
        "tasq {args:?} упал: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Сводка по всем проектам содержит каждый проект отдельной секцией, и счётчики не сложены.
///
/// Корень проекта — эпик, а у эпика нет `done` (правило вида узла), поэтому закрытые задачи
/// создаются под эпиком. Числа в проектах выбраны разными намеренно: при одинаковых «не
/// перемешаны» доказывается и сложением — то есть ничем.
#[test]
fn the_board_covers_every_project_without_mixing_counts() {
    let (_fixture, mut conn, first, second) = fixture("board");
    // AAA: эпик и две закрытые задачи под ним → open 1 (эпик), done 2.
    let epic_a = add(&mut conn, &first, "Эпик AAA");
    for title in ["Закрытая-1 в AAA", "Закрытая-2 в AAA"] {
        let task = add_child(&mut conn, &first, &epic_a.dot_id, title);
        close(&mut conn, &first, &task.dot_id);
    }
    // BBB: эпик и одна открытая задача → open 2, done 0.
    let epic_b = add(&mut conn, &second, "Эпик BBB");
    let _open_b = add_child(&mut conn, &second, &epic_b.dot_id, "Открытая в BBB");

    let boards = overview::board_all(&conn).expect("board_all");
    assert_eq!(boards.len(), 2, "ожидались обе секции: {boards:?}");

    let aaa = boards
        .iter()
        .find(|board| board.project == "AAA")
        .expect("секция AAA");
    let bbb = boards
        .iter()
        .find(|board| board.project == "BBB")
        .expect("секция BBB");

    assert_eq!(aaa.open, 1, "в AAA открыт только эпик: {aaa:?}");
    assert_eq!(aaa.done, 2, "в AAA две закрытые задачи: {aaa:?}");
    assert_eq!(bbb.open, 2, "в BBB открыты эпик и задача: {bbb:?}");
    assert_eq!(bbb.done, 0, "в BBB закрытых нет: {bbb:?}");

    // Адреса в секциях — со своими ключами: иначе строка «1.1» не открывается ни в одном проекте.
    let aaa_rows: Vec<&str> = aaa
        .columns
        .iter()
        .flat_map(|column| column.rows.iter())
        .map(|row| row.dot_id.as_str())
        .collect();
    assert!(
        aaa_rows.iter().all(|dot| dot.starts_with("AAA-")),
        "в секции AAA чужие адреса: {aaa_rows:?}"
    );
}

/// Поиск без проекта идёт по всей базе; с проектом — только по нему.
#[test]
fn search_without_a_project_reaches_every_project() {
    let (_fixture, mut conn, first, second) = fixture("search");
    let _task_a = add(&mut conn, &first, "Обычная задача");
    let found_b = add(&mut conn, &second, "Уникальноеслово в BBB");

    let all = overview::search_all(&conn, "Уникальноеслово").expect("search_all");
    assert_eq!(all.len(), 1, "ожидалась одна находка: {all:?}");
    assert_eq!(
        all[0].dot_id.as_deref(),
        Some(found_b.dot_id.as_str()),
        "адрес найденного должен нести ключ его проекта"
    );

    let scoped = overview::search(&conn, &first, "Уникальноеслово").expect("search AAA");
    assert!(
        scoped.is_empty(),
        "в AAA этого слова нет, а поиск его нашёл: {scoped:?}"
    );

    // Поиск по второму проекту находит то же самое — оба режима согласованы.
    let in_bbb = overview::search(&conn, &second, "Уникальноеслово").expect("search BBB");
    assert_eq!(in_bbb.len(), 1, "{in_bbb:?}");
}

/// `tasq board --all` в терминале: обе секции, каждая со своим именем.
#[test]
fn board_all_prints_every_project() {
    let (fixture, mut conn, first, second) = fixture("cli-board");
    add(&mut conn, &first, "Задача AAA");
    add(&mut conn, &second, "Задача BBB");

    let text = cli(&fixture, &["board", "--all"]);
    assert!(text.contains("AAA  open"), "нет секции AAA: {text}");
    assert!(text.contains("BBB  open"), "нет секции BBB: {text}");
    assert!(
        text.contains("Задача AAA") && text.contains("Задача BBB"),
        "{text}"
    );

    // Машиночитаемый вид: массив досок, а не одна.
    let json: Value =
        serde_json::from_str(&cli(&fixture, &["board", "--all", "--json"])).expect("json");
    let boards = json.as_array().expect("массив досок");
    assert_eq!(boards.len(), 2, "{json}");
}

/// `tasq find` без `--project` ищет по всей базе; с `--project` — только в нём.
#[test]
fn find_without_a_project_searches_the_whole_database() {
    let (fixture, mut conn, first, second) = fixture("cli-find");
    add(&mut conn, &first, "Обычная задача");
    add(&mut conn, &second, "Редкоеслово в BBB");

    let everywhere = cli(&fixture, &["find", "Редкоеслово"]);
    assert!(
        everywhere.contains("BBB-"),
        "без проекта находка из BBB обязана быть видна: {everywhere}"
    );

    let only_aaa = cli(&fixture, &["--project", "AAA", "find", "Редкоеслово"]);
    assert!(
        only_aaa.contains("nothing found"),
        "с проектом AAA искать вне его не должны: {only_aaa}"
    );
}

/// Проект без узлов не попадает в сводку: пустая секция — не сведения.
#[test]
fn an_empty_project_is_not_a_section() {
    let (_fixture, mut conn, first, second) = fixture("empty");
    add(&mut conn, &first, "Единственная задача");

    let boards = overview::board_all(&conn).expect("board_all");
    assert_eq!(boards.len(), 1, "ожидалась одна секция: {boards:?}");
    assert_eq!(boards[0].project, "AAA");
    let _ = second;
}
