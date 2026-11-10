//! Строгость реоткрытия закрытого узла — политика проекта, а не код.
//!
//! Матрица переходов отвечает на вопрос «куда можно». Отдельный вопрос — «нужен ли явный флаг,
//! чтобы вернуть `done`/`cancelled` в работу». Это настройка: в проекте, где задачи закрывают по
//! ошибке часто, `--force` на каждое реоткрытие только мешает; в проекте с аудитом — обязателен.
//!
//! Политика живёт в `projects.metadata.reopen` и читается один раз при выборе проекта, поэтому
//! `check_transition` не ходит в базу на каждом переходе.

use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;

use tasq::db;
use tasq::node::{self, NewNode, Patch};
use tasq::project::{self, Project};
use tasq::status::{Kind, Reopen, Status, check_transition, check_with_policy};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-reopen-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn fixture(tag: &str) -> (PathBuf, Connection, Project) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "REO", "reopen").expect("init");
    let path = PathBuf::from(&outcome.db_path);
    let conn = db::open(&path).expect("open");
    let project = project::first_project(&conn).expect("project");
    (path, conn, project)
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

/// По умолчанию политика строгая: без `--force` закрытый узел в работу не вернуть.
#[test]
fn the_default_policy_needs_force() {
    let (_path, mut conn, project) = fixture("default");
    assert_eq!(project.reopen, Reopen::ForceRequired);

    // Задача, а не эпик: у эпика нет `todo`, и отказ пришёл бы про матрицу, а не про политику.
    let epic = add(&mut conn, &project, None, "Эпик");
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Задача");
    set_status(&conn, &task, "done");

    let error = node::update(
        &mut conn,
        &project,
        &task.dot_id,
        Patch {
            status: Some(Status::Todo),
            actor: Some("человек"),
            ..Patch::default()
        },
    )
    .expect_err("реоткрытие без флага прошло");
    assert_eq!(error.code(), "FORCE_REQUIRED", "{error}");

    // С флагом — проходит.
    node::update(
        &mut conn,
        &project,
        &task.dot_id,
        Patch {
            status: Some(Status::Todo),
            force: true,
            actor: Some("человек"),
            ..Patch::default()
        },
    )
    .expect("с --force реоткрытие разрешено");
}

/// Проект с `metadata.reopen = "allow"` разрешает реоткрытие без флага.
#[test]
fn the_project_may_allow_reopening() {
    let (path, conn, _project) = fixture("allow");
    drop(conn);
    set_metadata(&path, r#"{"reopen": "allow"}"#);

    let mut conn = db::open(&path).expect("reopen db");
    let project = project::first_project(&conn).expect("project");
    assert_eq!(
        project.reopen,
        Reopen::Allowed,
        "политика проекта не прочитана из metadata"
    );

    let epic = add(&mut conn, &project, None, "Эпик");
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Задача");
    set_status(&conn, &task, "done");

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
    .expect("политика allow должна разрешать реоткрытие без флага");
}

/// Неизвестное значение политики читается как строгий режим, а не как «разрешить».
///
/// Молча ослабить правило из-за опечатки в JSON нельзя: это ошибка в сторону безопасности.
#[test]
fn an_unknown_policy_falls_back_to_strict() {
    let (path, conn, _project) = fixture("unknown");
    drop(conn);
    set_metadata(&path, r#"{"reopen": "разрешить"}"#);

    let conn = db::open(&path).expect("reopen db");
    let project = project::first_project(&conn).expect("project");
    assert_eq!(project.reopen, Reopen::ForceRequired);
}

/// Испорченный JSON в метаданных не роняет выбор проекта: политика берётся по умолчанию.
#[test]
fn broken_metadata_does_not_break_the_project() {
    let (path, conn, _project) = fixture("broken");
    drop(conn);
    set_metadata(&path, "{это не json");

    let conn = db::open(&path).expect("reopen db");
    let project = project::first_project(&conn).expect("проект должен открываться");
    assert_eq!(project.reopen, Reopen::ForceRequired);
}

/// Политика меняет только строгость, но не матрицу: запрещённый переход остаётся запрещённым
/// даже при `allow`.
#[test]
fn the_policy_never_widens_the_matrix() {
    // `review` из `todo` недостижим в матрице; `allow` не должен это изменить.
    let error = check_with_policy(
        Status::Todo,
        Status::Review,
        Kind::Task,
        false,
        Reopen::Allowed,
    )
    .expect_err("политика разрешила переход, которого нет в матрице");
    assert_eq!(error.code(), "INVALID_TRANSITION", "{error}");

    // И наоборот: пара из матрицы проходит по строгой политике с флагом.
    assert!(
        check_with_policy(
            Status::Done,
            Status::Todo,
            Kind::Task,
            true,
            Reopen::ForceRequired
        )
        .is_ok()
    );
}

/// `check_transition` без политики ведёт себя строго — публичный контракт не изменился.
#[test]
fn the_plain_checker_stays_strict() {
    let error = check_transition(Status::Done, Status::Todo, Kind::Task, false)
        .expect_err("старая функция ослабла");
    assert_eq!(error.code(), "FORCE_REQUIRED", "{error}");
    assert!(check_transition(Status::Done, Status::Todo, Kind::Task, true).is_ok());
}

/// Разбор значений политики: оба написания каждого режима и отказ на прочем.
#[test]
fn policy_values_parse_both_ways() {
    assert_eq!(Reopen::parse("allow"), Some(Reopen::Allowed));
    assert_eq!(Reopen::parse("allowed"), Some(Reopen::Allowed));
    assert_eq!(Reopen::parse("force"), Some(Reopen::ForceRequired));
    assert_eq!(Reopen::parse("force_required"), Some(Reopen::ForceRequired));
    assert_eq!(Reopen::parse("maybe"), None);
    // В обратную сторону — только канонические имена.
    assert_eq!(Reopen::Allowed.as_str(), "allow");
    assert_eq!(Reopen::ForceRequired.as_str(), "force_required");
}

/// Предрабочий статус ставится SQL-ом: тест про политику реоткрытия, а не про переходы в работу.
fn set_status(conn: &Connection, node: &node::Node, status: &str) {
    conn.execute(
        "UPDATE nodes SET status = ?1 WHERE id = ?2",
        rusqlite::params![status, node.id],
    )
    .expect("set status");
}

fn set_metadata(path: &std::path::Path, metadata: &str) {
    let conn = Connection::open(path).expect("open raw");
    conn.execute(
        "UPDATE projects SET metadata = ?1",
        rusqlite::params![metadata],
    )
    .expect("set metadata");
}
