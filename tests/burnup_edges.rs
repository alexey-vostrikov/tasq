//! Прогноз и оценка риска: границы аргументов, пустые данные и сортировка.
//!
//! Эти ветки видны только на живых данных: серия без завершённой работы, отменённый
//! узел вне области, дедлайн в прошлом. Ошибка здесь не роняет трекер, но врёт в
//! отчёте — поэтому каждое решение закреплено тестом.

use std::path::PathBuf;

use rusqlite::Connection;

use tasq::burnup::{self, Metric};
use tasq::db;
use tasq::node::{self, NewNode, Patch};
use tasq::project::{self, Project};
use tasq::status::Status;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-burnup-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn fixture(tag: &str) -> (Connection, Project) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "TASQ", "demo").expect("init");
    let conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");
    (conn, project)
}

fn add(
    conn: &mut Connection,
    project: &Project,
    parent: Option<&str>,
    title: &str,
    due: Option<&str>,
) -> node::Node {
    node::create(
        conn,
        project,
        NewNode {
            parent,
            title,
            kind: None,
            description: None,
            priority: None,
            due_date: due,
            estimated_hours: None,
            weight: None,
            source_ref: None,
            branch: None,
        },
    )
    .expect("create node")
}

/// Даты приходят строками: битая дата — отказ с именем поля, а не «сегодня по умолчанию».
#[test]
fn bad_dates_are_refused() {
    let (conn, project) = fixture("dates");

    for (since, until, field) in [
        (Some("2026-13-45"), Some("2026-11-09"), "since"),
        (Some("2026-11-01"), Some("завтра"), "until"),
        (Some("01.11.2026"), None, "since"),
    ] {
        let error =
            burnup::series(&conn, &project, Metric::Count, since, until).expect_err("битая дата");
        assert_eq!(error.code(), "INVALID_VALUE", "{since:?}..{until:?}");
        assert!(error.to_string().contains(field), "{error}");
    }
}

/// Серия ограничена 900 днями: длинный запрос отбивается с указанием предела.
#[test]
fn long_series_is_refused() {
    let (conn, project) = fixture("span");
    let error = burnup::series(
        &conn,
        &project,
        Metric::Count,
        Some("2000-01-01"),
        Some("2026-11-09"),
    )
    .expect_err("слишком длинная серия");
    assert_eq!(error.code(), "INVALID_VALUE");
    assert!(error.to_string().contains("900"), "{error}");
}

/// Снимок за конкретный день: битый день — отказ, пустой — отказ с тем же полем.
#[test]
fn snapshot_day_is_checked() {
    let (mut conn, project) = fixture("snapshot-day");

    let error =
        burnup::snapshot(&mut conn, &project, Metric::Count, "вчера").expect_err("битый день");
    assert_eq!(error.code(), "INVALID_VALUE");
    assert!(error.to_string().contains("day"), "{error}");

    // Пустой проект в этот день даёт точку с нулями, а не ошибку.
    let point =
        burnup::snapshot(&mut conn, &project, Metric::Count, "2026-11-09").expect("пустой снимок");
    assert_eq!(point.total, 0.0);
    assert_eq!(point.done, 0.0);
}

/// Диаграмма без данных говорит словами, а с данными рисует и линию цели.
#[test]
fn ascii_chart_with_and_without_data() {
    let (mut conn, project) = fixture("ascii");
    let epic = add(&mut conn, &project, None, "Эпик", Some("2099-01-01"));

    let empty = burnup::series(&conn, &project, Metric::Count, None, None).expect("series");
    let text = burnup::ascii(&empty, 40);
    assert!(!text.is_empty(), "диаграмма печатается и на одной точке");

    // Завершённая работа даёт непустой график с линией идеального хода.
    let held = tasq::lease::claim(
        &mut conn,
        &project,
        &epic.dot_id,
        &tasq::lease::ClaimRequest {
            holder: "agent-a",
            ttl_seconds: tasq::lease::DEFAULT_TTL,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
    )
    .expect("claim");
    tasq::lease::release(
        &mut conn,
        &project,
        &held.token,
        tasq::lease::Outcome::Completed,
        None,
        true,
    )
    .expect("release");

    let series = burnup::series(&conn, &project, Metric::Count, None, None).expect("series");
    let text = burnup::ascii(&series, 40);
    assert!(text.contains('|'), "линия цели нарисована: {text}");
}

/// Дедлайн проекта — самый ранний срок среди открытых узлов; прошедший виден отдельно.
#[test]
fn project_deadline_comes_from_open_nodes() {
    let (mut conn, project) = fixture("deadline");
    assert!(
        !burnup::deadline_in_past(&conn, &project).expect("без узлов"),
        "без узлов дедлайна нет"
    );

    let epic = add(&mut conn, &project, None, "Эпик", Some("2001-01-01"));
    add(
        &mut conn,
        &project,
        Some(&epic.dot_id),
        "Позже",
        Some("2099-01-01"),
    );

    assert!(
        burnup::deadline_in_past(&conn, &project).expect("просрочен"),
        "самый ранний срок в прошлом"
    );

    // Закрытая работа в дедлайн не входит.
    let held = tasq::lease::claim(
        &mut conn,
        &project,
        &epic.dot_id,
        &tasq::lease::ClaimRequest {
            holder: "agent-a",
            ttl_seconds: tasq::lease::DEFAULT_TTL,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
    )
    .expect("claim");
    tasq::lease::release(
        &mut conn,
        &project,
        &held.token,
        tasq::lease::Outcome::Completed,
        None,
        true,
    )
    .expect("release");
    assert!(
        !burnup::deadline_in_past(&conn, &project).expect("после закрытия"),
        "у закрытого узла срок больше не считается"
    );

    // Сегодняшняя дата в формате календаря.
    let today = burnup::utc_today();
    assert_eq!(today.len(), 10, "{today}");
    assert_eq!(today.matches('-').count(), 2, "{today}");
}

/// Отменённая работа выходит из области прогноза: её не доделывают, и в проценте она
/// не должна тянуть вниз.
#[test]
fn cancelled_work_leaves_the_scope() {
    let (mut conn, project) = fixture("cancelled");
    let epic = add(&mut conn, &project, None, "Эпик", None);
    let keep = add(&mut conn, &project, Some(&epic.dot_id), "Оставить", None);
    let drop = add(&mut conn, &project, Some(&epic.dot_id), "Отменить", None);

    node::update(
        &mut conn,
        &project,
        &drop.dot_id,
        Patch {
            status: Some(Status::Cancelled),
            ..Patch::default()
        },
    )
    .expect("cancel");

    let series = burnup::series(&conn, &project, Metric::Count, None, None).expect("series");
    assert!(
        series.scope_total < 3.0,
        "отменённый узел вне области: {}",
        series.scope_total
    );

    // Просроченные с разным отставанием сортируются по величине разрыва.
    node::update(
        &mut conn,
        &project,
        &keep.dot_id,
        Patch {
            due_date: Some("2001-01-01"),
            ..Patch::default()
        },
    )
    .expect("due");
    let risky = burnup::at_risk(&conn, &project, 0.0).expect("risk");
    assert!(
        risky.windows(2).all(|pair| pair[0].gap >= pair[1].gap),
        "порядок по разрыву: {risky:?}"
    );
}
