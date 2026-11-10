//! Бюджет обхода: отказ до начала чтения, а не заполненный временный файл.
//!
//! Отдельный бинарь тестов, потому что здесь проверяется и путь через окружение: `TASQ_SCAN_BUDGET`
//! задаётся дочернему процессу, а не текущему (в Rust 2024 переменные процесса меняются только
//! `unsafe`-вызовом, и полагаться на это в тестах нечего).
//!
//! Речь о том самом случае: два прогона тестов висели семь часов и держали по 97 ГБ удалённого
//! временного файла каждый. Сторож шагов убрал бесконечность, но объём остаётся производным от
//! данных — поэтому он считается и сравнивается с пределом ДО обхода.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use rusqlite::Connection;

use tasq::db;
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};
use tasq::tree;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tasq")
}

struct Fixture {
    dir: PathBuf,
    db: PathBuf,
}

fn fixture(tag: &str) -> (Fixture, Connection, Project) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-budget-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    let outcome = db::init(&dir, "BGT", "бюджет").expect("init");
    let db_path = PathBuf::from(&outcome.db_path);
    let mut conn = db::open(std::path::Path::new(&db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");
    // Одиннадцать узлов: бюджет считается по числу узлов, поэтому фикстура может быть маленькой —
    // предел для проверки задаётся явно.
    for index in 0..11 {
        node::create(
            &mut conn,
            &project,
            NewNode {
                parent: None,
                title: &format!("Узел {index}"),
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
    }
    (Fixture { dir, db: db_path }, conn, project)
}

/// Превышение бюджета — доменный отказ с числами, а не общий `STORAGE_ERROR`.
#[test]
fn an_over_budget_scan_is_refused_with_numbers() {
    let (_fixture, conn, project) = fixture("refused");
    // 11 узлов × (16 + 1) = 187 пар; предел 100 — заведомо ниже.
    let error = tree::check_scan_budget_with(&conn, Some(&project), 100).expect_err("превышен");

    assert_eq!(error.code(), "TRAVERSAL_BUDGET");
    let text = error.to_string();
    assert!(text.contains("100"), "в отказе нет предела: {text}");
    assert!(text.contains("11 node"), "в отказе нет числа узлов: {text}");
    assert!(
        text.contains(&tasq::node::MAX_DEPTH.to_string()),
        "в отказе нет предела глубины: {text}"
    );
    assert!(
        error.hint().contains("--project") && error.hint().contains("TASQ_SCAN_BUDGET"),
        "подсказка не подсказывает: {}",
        error.hint()
    );
}

/// Тот же отказ приходит из самого чтения списка: иначе проверку можно забыть вызвать, и защита
/// останется обещанием в комментарии.
#[test]
fn the_listing_is_guarded_by_the_same_budget() {
    let (_fixture, conn, project) = fixture("listing");
    // Проверка внутри `node::list` берёт бюджет из окружения; здесь оно обычное, поэтому
    // убеждаемся, что при штатном бюджете чтение проходит.
    node::list(&conn, &project, &node::Filter::default()).expect("обычное чтение проходит");
    assert!(
        tree::check_scan_budget(&conn, Some(&project)).is_ok(),
        "штатный бюджет не должен мешать одиннадцати узлам"
    );
}

/// Бюджет по умолчанию пропускает обычную базу: на живом размере (сотни узлов) отказов нет.
#[test]
fn a_normal_database_is_under_the_default_budget() {
    let (_fixture, conn, project) = fixture("normal");
    tree::check_scan_budget(&conn, Some(&project)).expect("обычная база проходит");
    // Через `parse_budget`, а не через константу напрямую: у константы значение известно
    // компилятору, и `assert!` на ней clippy считает проверкой самого себя.
    let default = tree::parse_budget(None);
    assert!(
        default >= 1_000_000,
        "бюджет по умолчанию {default} слишком мал для рабочей базы"
    );
}

/// Мусор в переменной не подменяет бюджет нулём: «0», отрицательное и текст читаются как
/// «не задано», иначе одна опечатка в окружении запретила бы любое чтение.
#[test]
fn a_broken_budget_value_falls_back_to_the_default() {
    for value in [Some("0"), Some("-5"), Some("много"), Some(""), None] {
        assert_eq!(
            tree::parse_budget(value),
            tree::MAX_SCAN_PAIRS,
            "значение {value:?} не должно менять бюджет"
        );
    }
    assert_eq!(tree::parse_budget(Some("500")), 500);
    assert_eq!(tree::parse_budget(Some(" 500 ")), 500);
}

/// Сквозная проверка через окружение: переменная доходит до бинаря и меняет ответ команды.
///
/// Это единственный способ проверить чтение переменной, не меняя окружение собственного
/// процесса: у дочернего оно своё.
#[test]
fn the_environment_variable_reaches_the_binary() {
    let (fixture, _conn, _project) = fixture("env");
    let out = Command::new(bin())
        .current_dir(&fixture.dir)
        .env("TASQ_SCAN_BUDGET", "50")
        .args([
            "--db",
            fixture.db.to_str().expect("path"),
            "--project",
            "BGT",
            "ls",
            "--json",
        ])
        .output()
        .expect("run tasq");
    assert!(!out.status.success(), "чтение прошло вопреки бюджету");
    let text =
        String::from_utf8_lossy(&out.stderr).to_string() + &String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("TRAVERSAL_BUDGET"),
        "ожидался отказ бюджета, получено: {text}"
    );
    assert!(text.contains("50"), "в отказе нет предела: {text}");
}
