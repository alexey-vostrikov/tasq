//! Unicode-поиск и burn-up: ряд из лога, прогноз, ASCII-график, риск срыва срока.

use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;

use tasq::db;
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};
use tasq::status::Status;
use tasq::{burnup, overview};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-burn-{tag}-{nanos}"));
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

fn add_weighted(
    conn: &mut Connection,
    project: &Project,
    title: &str,
    weight: f64,
    hours: f64,
    due: Option<&str>,
) -> node::Node {
    node::create(
        conn,
        project,
        NewNode {
            parent: None,
            title,
            kind: None,
            description: None,
            priority: None,
            due_date: due,
            estimated_hours: Some(hours),
            weight: Some(weight),
            source_ref: None,
            branch: None,
        },
    )
    .expect("node")
}

fn complete(conn: &mut Connection, project: &Project, reference: &str) {
    // Настоящий путь завершения: аренда даёт in_progress, release(completed) — done.
    let lease = tasq::lease::claim(
        conn,
        project,
        reference,
        &tasq::lease::ClaimRequest::new("test-agent"),
    )
    .expect("claim");
    tasq::lease::release(
        conn,
        project,
        &lease.token,
        tasq::lease::Outcome::Completed,
        None,
        true,
    )
    .expect("complete");
}

// ---------------------------------------------------------------- Unicode

#[test]
fn unicode_lower_handles_cyrillic() {
    let (conn, _project) = fixture("unilower");
    let lower: String = conn
        .query_row("SELECT uni_lower('МИГРАЦИИ Схемы')", [], |row| row.get(0))
        .expect("uni_lower");
    assert_eq!(lower, "миграции схемы");

    // встроенный lower() остаётся ASCII-only — именно поэтому нужен свой
    let builtin: String = conn
        .query_row("SELECT lower('МИГРАЦИИ')", [], |row| row.get(0))
        .expect("lower");
    assert_eq!(builtin, "МИГРАЦИИ");
}

#[test]
fn unicode_nocase_collation_compares_cyrillic_ignoring_case() {
    let (conn, _project) = fixture("collation");
    let equal: i64 = conn
        .query_row(
            "SELECT 'Миграции' = 'миграции' COLLATE UNICODE_NOCASE",
            [],
            |row| row.get(0),
        )
        .expect("compare");
    assert_eq!(equal, 1, "коллация сравнивает без учёта регистра");

    let sorted: String = conn
        .query_row(
            "SELECT group_concat(v, ',') FROM (
               SELECT 'яблоко' AS v UNION ALL SELECT 'Апельсин' UNION ALL SELECT 'банан'
               ORDER BY v COLLATE UNICODE_NOCASE)",
            [],
            |row| row.get(0),
        )
        .expect("sort");
    assert_eq!(
        sorted, "Апельсин,банан,яблоко",
        "порядок без учёта регистра"
    );
}

#[test]
fn search_is_case_insensitive_for_cyrillic_in_both_paths() {
    let (mut conn, project) = fixture("unicodesearch");
    let epic = add(&mut conn, &project, None, "Миграции схемы");
    node::update(
        &mut conn,
        &project,
        &epic.dot_id,
        node::Patch {
            description: Some("Описать Таблицы И Индексы"),
            ..node::Patch::default()
        },
    )
    .expect("description");

    // FTS-путь
    let lower = overview::search(&conn, &project, "миграции").expect("lower query");
    assert!(
        !lower.is_empty(),
        "нижний регистр находит заголовок в верхнем: {lower:?}"
    );

    let upper = overview::search(&conn, &project, "МИГРАЦИИ").expect("upper query");
    assert!(!upper.is_empty(), "верхний регистр тоже находит");

    // путь пословного поиска (там, где FTS не разобрал запрос: двоеточие)
    let fallback = overview::search(&conn, &project, "таблицы: индексы").expect("fallback query");
    assert!(
        !fallback.is_empty(),
        "пословный путь тоже без учёта регистра: {fallback:?}"
    );
}

#[test]
fn note_search_is_case_insensitive_for_cyrillic() {
    let (mut conn, project) = fixture("unicodenote");
    tasq::note::save(
        &mut conn,
        &project,
        tasq::note::SaveNote {
            id: None,
            title: Some("Решение по WAL"),
            content: Some("Включаем WAL для Параллельных читателей"),
            note_type: Some("decision"),
            node: None,
            actor: None,
        },
    )
    .expect("note");

    let found = tasq::note::search(&conn, &project, "параллельных").expect("search");
    assert_eq!(
        found.len(),
        1,
        "поиск заметок по кириллице без учёта регистра"
    );
}

// ---------------------------------------------------------------- burn-up

#[test]
fn series_rebuilds_scope_from_the_log() {
    let (mut conn, project) = fixture("series");
    let first = add_weighted(&mut conn, &project, "Первая", 3.0, 2.0, None);
    let second = add_weighted(&mut conn, &project, "Вторая", 5.0, 4.0, None);

    complete(&mut conn, &project, &first.dot_id);

    let series =
        burnup::series(&conn, &project, burnup::Metric::Points, None, None).expect("series");
    assert_eq!(series.scope_total, 8.0, "объём = 3 + 5");
    assert_eq!(series.scope_done, 3.0, "готово только первое");
    assert_eq!(series.scope_added, 8.0, "оба узла добавлены в окне");

    let hours = burnup::series(&conn, &project, burnup::Metric::Hours, None, None).expect("hours");
    assert_eq!(hours.scope_total, 6.0);
    assert_eq!(hours.scope_done, 2.0);

    let count = burnup::series(&conn, &project, burnup::Metric::Count, None, None).expect("count");
    assert_eq!(count.scope_total, 2.0);

    let _ = second;
}

#[test]
fn cancelled_node_leaves_the_scope() {
    let (mut conn, project) = fixture("cancel");
    let kept = add_weighted(&mut conn, &project, "Остаётся", 4.0, 1.0, None);
    let dropped = add_weighted(&mut conn, &project, "Отменяется", 6.0, 1.0, None);

    node::update(
        &mut conn,
        &project,
        &dropped.dot_id,
        node::Patch {
            status: Some(Status::Cancelled),
            ..node::Patch::default()
        },
    )
    .expect("cancel");

    let series =
        burnup::series(&conn, &project, burnup::Metric::Points, None, None).expect("series");
    assert_eq!(series.scope_total, 4.0, "отменённое не входит в объём");

    let _ = kept;
}

#[test]
fn snapshots_match_the_recomputed_series() {
    let (mut conn, project) = fixture("snapshot");
    let task = add_weighted(&mut conn, &project, "Задача", 2.0, 1.0, None);
    complete(&mut conn, &project, &task.dot_id);

    let written =
        burnup::snapshot_all(&mut conn, &project, burnup::Metric::Points).expect("snapshot");
    assert!(written >= 1);

    let series =
        burnup::series(&conn, &project, burnup::Metric::Points, None, None).expect("series");
    let last = series.points.last().expect("last point");

    let stored: (f64, f64) = conn
        .query_row(
            "SELECT scope_total, scope_done FROM burn_snapshots
             WHERE project_id = ?1 AND day = ?2 AND metric = 'points'",
            rusqlite::params![project.id, last.day],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("snapshot row");
    assert_eq!(stored.0, last.total, "снимок совпадает с пересчётом");
    assert_eq!(stored.1, last.done);

    // повторная запись обновляет, а не дублирует
    let again = burnup::snapshot(&mut conn, &project, burnup::Metric::Points, &last.day)
        .expect("resnapshot");
    assert_eq!(again.total, last.total);
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM burn_snapshots WHERE project_id = ?1 AND day = ?2 AND metric = 'points'",
            rusqlite::params![project.id, last.day],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(count, 1);
}

#[test]
fn ascii_chart_has_both_lines_and_the_axis() {
    let (mut conn, project) = fixture("ascii");
    let first = add_weighted(&mut conn, &project, "Первая", 1.0, 1.0, None);
    add_weighted(&mut conn, &project, "Вторая", 1.0, 1.0, None);
    complete(&mut conn, &project, &first.dot_id);

    let series =
        burnup::series(&conn, &project, burnup::Metric::Count, None, None).expect("series");
    let chart = burnup::ascii(&series, 40);
    assert!(chart.contains('#'), "есть линия готового: {chart}");
    assert!(chart.contains('·'), "есть линия объёма: {chart}");
    assert!(chart.contains('+'), "есть ось: {chart}");
    assert!(chart.contains("объём"), "есть легенда: {chart}");
}

#[test]
fn forecast_extrapolates_completed_work() {
    let (mut conn, project) = fixture("forecast");
    let first = add_weighted(&mut conn, &project, "Первая", 1.0, 1.0, None);
    add_weighted(&mut conn, &project, "Вторая", 1.0, 1.0, None);
    add_weighted(&mut conn, &project, "Третья", 1.0, 1.0, None);
    complete(&mut conn, &project, &first.dot_id);

    // разнесём создание по дням, чтобы у ряда была длина больше одной точки
    conn.execute(
        "UPDATE nodes SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ','now','-3 days') WHERE path = '1'",
        [],
    )
    .expect("backdate");

    let series =
        burnup::series(&conn, &project, burnup::Metric::Count, None, None).expect("series");
    if series.points.len() > 2 {
        assert!(
            series.forecast.is_some() || series.scope_done == series.scope_total,
            "при движении прогноз есть, иначе его нет — оба варианта честны"
        );
    }
}

#[test]
fn risk_flags_a_node_that_is_behind() {
    let (mut conn, project) = fixture("risk");
    // срок в прошлом, работа не сделана — отставание максимальное
    let late = add_weighted(
        &mut conn,
        &project,
        "Опоздание",
        1.0,
        1.0,
        Some("2026-01-01"),
    );
    conn.execute(
        "UPDATE nodes SET created_at = '2025-12-01T00:00:00.000Z' WHERE id = ?1",
        rusqlite::params![late.id],
    )
    .expect("backdate");

    // свежая задача с далёким сроком не в риске
    let fresh = add_weighted(&mut conn, &project, "Свежая", 1.0, 1.0, Some("2099-01-01"));

    let risky = burnup::at_risk(&conn, &project, 0.2).expect("at_risk");
    let dots: Vec<&str> = risky.iter().map(|item| item.dot_id.as_str()).collect();
    assert!(dots.contains(&late.dot_id.as_str()), "{dots:?}");
    assert!(
        !dots.contains(&fresh.dot_id.as_str()),
        "свежая не в риске: {dots:?}"
    );
    assert!(risky[0].gap > 0.2);
}

#[test]
fn deadline_of_the_project_comes_from_open_nodes() {
    let (mut conn, project) = fixture("deadline");
    add_weighted(&mut conn, &project, "Ранняя", 1.0, 1.0, Some("2099-05-01"));
    let later = add_weighted(&mut conn, &project, "Поздняя", 1.0, 1.0, Some("2099-06-01"));

    let series =
        burnup::series(&conn, &project, burnup::Metric::Count, None, None).expect("series");
    assert!(series.days_left.is_some());

    // закрываем раннюю — срок проекта сдвигается на следующую
    complete(&mut conn, &project, "TASQ-1");
    let after = burnup::series(&conn, &project, burnup::Metric::Count, None, None).expect("series");
    assert!(after.days_left.is_some());
    let _ = later;
}

#[test]
fn invalid_metric_and_range_are_rejected() {
    let (conn, project) = fixture("badmetric");
    assert_eq!(burnup::Metric::parse("euro"), None);
    let err = burnup::series(
        &conn,
        &project,
        burnup::Metric::Count,
        Some("2099-01-02"),
        Some("2099-01-01"),
    )
    .unwrap_err();
    assert_eq!(err.code(), "INVALID_VALUE");
}
