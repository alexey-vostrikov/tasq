//! Массовый архив по статусу: `tasq archive --project (PRJ|all) --status done`.
//!
//! Проверяется то, что нельзя увидеть по одному запуску: выбор САМЫХ ВЕРХНИХ узлов (вложенные не
//! берутся повторно), неприкосновенность открытых узлов, `--dry-run`, откат и работа по всем
//! проектам сразу.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use rusqlite::Connection;

use tasq::db;
use tasq::lease::{self, ClaimRequest, Outcome};
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};
use tasq::status::Status;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-arch-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn add(conn: &mut Connection, project: &Project, parent: Option<&str>, title: &str) -> String {
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
    .dot_id
}

/// Закрывает узел так, как это делает человек: берёт аренду и сдаёт работу.
///
/// Через `node::update` статус не поставить — переход требует живой аренды, и обход этого
/// правила в фикстуре означал бы, что тест проверяет состояние, которого в базе не бывает.
/// `force` — тот же `--force`: эпик закрывается при незакрытой подзадаче.
fn close(conn: &mut Connection, project: &Project, reference: &str) {
    let lease = lease::claim(
        conn,
        project,
        reference,
        &ClaimRequest {
            holder: "tester",
            ttl_seconds: 600,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
    )
    .expect("claim");
    lease::release(conn, project, &lease.token, Outcome::Completed, None, true).expect("complete");
}

/// Фикстура: закрытый эпик с закрытым и открытым ребёнком, отдельная закрытая задача и открытая.
///
/// Открытый ребёнок под закрытым родителем — не выдумка: так выглядит задача, закрытая вручную при
/// незакрытой подзадаче, и именно на ней видно, что поддерево уходит целиком.
fn fixture(tag: &str) -> (PathBuf, Connection, Project) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "ARC", "archive").expect("init");
    let mut conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");

    let epic = add(&mut conn, &project, None, "закрытый эпик");
    let closed = add(&mut conn, &project, Some(&epic), "закрытая задача");
    add(&mut conn, &project, Some(&epic), "открытая подзадача");
    close(&mut conn, &project, &closed);
    close(&mut conn, &project, &epic);

    let solo = add(&mut conn, &project, None, "отдельная закрытая");
    close(&mut conn, &project, &solo);

    add(&mut conn, &project, None, "открытая задача");

    (dir, conn, project)
}

fn visible(conn: &Connection, project: &Project) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM nodes WHERE project_id = ?1 AND archived_at IS NULL",
        [project.id],
        |row| row.get(0),
    )
    .expect("count visible")
}

fn archived(conn: &Connection, project: &Project) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM nodes WHERE project_id = ?1 AND archived_at IS NOT NULL",
        [project.id],
        |row| row.get(0),
    )
    .expect("count archived")
}

/// Верхние узлы множества: вложенный закрытый узел НЕ выбирается отдельно, его накрывает родитель.
#[test]
fn only_the_topmost_nodes_of_the_status_are_selected() {
    let (_dir, mut conn, project) = fixture("top");
    let report =
        node::archive_batch(&mut conn, &project, &Status::Done, false, true).expect("batch");
    assert_eq!(
        report.refs,
        vec!["ARC-1".to_string(), "ARC-2".to_string()],
        "выбраны не самые верхние закрытые узлы: {:?}",
        report.refs
    );
    assert!(
        !report.refs.iter().any(|reference| reference == "ARC-1.1"),
        "вложенная закрытая задача выбрана повторно"
    );
    // Поддеревья: эпик с двумя детьми (3) плюс отдельная задача (1).
    assert_eq!(report.affected, 4, "счёт не по размеру поддеревьев");
    assert!(report.dry_run);
}

/// `--dry-run` ничего не пишет, а настоящий запуск архивирует ровно то, что показал.
#[test]
fn dry_run_changes_nothing_and_predicts_the_real_run() {
    let (_dir, mut conn, project) = fixture("dry");
    let hidden_before = visible(&conn, &project);
    let predicted =
        node::archive_batch(&mut conn, &project, &Status::Done, false, true).expect("dry");
    assert_eq!(
        visible(&conn, &project),
        hidden_before,
        "dry-run изменил видимые узлы"
    );
    assert_eq!(archived(&conn, &project), 0, "dry-run что-то заархивировал");

    let done =
        node::archive_batch(&mut conn, &project, &Status::Done, false, false).expect("apply");
    assert_eq!(
        done.refs, predicted.refs,
        "настоящий запуск выбрал не то, что показал dry-run"
    );
    assert_eq!(
        done.affected, predicted.affected,
        "число узлов разошлось с предсказанием"
    );
    assert_eq!(
        archived(&conn, &project),
        4,
        "заархивировано не то число узлов"
    );
}

/// Открытые узлы остаются на месте: архив берёт только статус из запроса.
#[test]
fn open_nodes_stay_visible() {
    let (_dir, mut conn, project) = fixture("open");
    node::archive_batch(&mut conn, &project, &Status::Done, false, false).expect("batch");
    let open: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE project_id = ?1 AND archived_at IS NULL",
            [project.id],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(open, 1, "открытых узлов осталось не один: {open}");
    let title: String = conn
        .query_row(
            "SELECT title FROM nodes WHERE project_id = ?1 AND archived_at IS NULL",
            [project.id],
            |row| row.get(0),
        )
        .expect("title");
    assert_eq!(title, "открытая задача", "уцелел не тот узел");
}

/// Повторный запуск ничего не находит и не падает: идемпотентность — часть контракта,
/// иначе «заархивировать всё» нельзя запускать дважды (скрипт, cron, повтор после сбоя).
#[test]
fn a_second_run_finds_nothing() {
    let (_dir, mut conn, project) = fixture("twice");
    node::archive_batch(&mut conn, &project, &Status::Done, false, false).expect("first");
    let again =
        node::archive_batch(&mut conn, &project, &Status::Done, false, false).expect("second");
    assert!(
        again.refs.is_empty(),
        "второй запуск что-то выбрал: {:?}",
        again.refs
    );
    assert_eq!(again.affected, 0);
    assert_eq!(
        archived(&conn, &project),
        4,
        "второй запуск изменил состояние"
    );
}

/// Откат возвращает ровно поддеревья, которые были заархивированы, и ничего больше.
#[test]
fn undo_restores_the_same_subtrees() {
    let (_dir, mut conn, project) = fixture("undo");
    let applied =
        node::archive_batch(&mut conn, &project, &Status::Done, false, false).expect("apply");
    let restored =
        node::archive_batch(&mut conn, &project, &Status::Done, true, false).expect("undo");
    assert_eq!(restored.refs, applied.refs, "откат выбрал другие узлы");
    assert_eq!(
        restored.affected, applied.affected,
        "откат вернул не то число узлов"
    );
    assert_eq!(
        archived(&conn, &project),
        0,
        "после отката что-то осталось в архиве"
    );
}

/// Журнал: одна запись на поддерево, а не на каждый узел. Иначе пакет из сорока узлов оставляет
/// сорок строк, в которых не видно, какой командой это сделано.
#[test]
fn the_journal_gets_one_row_per_subtree() {
    let (_dir, mut conn, project) = fixture("log");
    node::archive_batch(&mut conn, &project, &Status::Done, false, false).expect("batch");
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM activity_log WHERE action = 'archive' AND project_id = ?1",
            [project.id],
            |row| row.get(0),
        )
        .expect("count log");
    assert_eq!(
        rows, 2,
        "записей в журнале не по одной на поддерево: {rows}"
    );
}

/// `--project all` проходит по всем проектам базы: по одному проекту команда работала бы только
/// в нём, а «заархивировать всё закрытое» — это про базу целиком.
#[test]
fn all_projects_are_walked_by_the_cli() {
    let dir = temp_dir("cli");
    let outcome = db::init(&dir, "ONE", "one").expect("init");
    let db_path = PathBuf::from(&outcome.db_path);
    let mut conn = db::open(&db_path).expect("open");
    let one = project::first_project(&conn).expect("project");
    let epic = add(&mut conn, &one, None, "закрытый эпик");
    close(&mut conn, &one, &epic);
    let second = project::create(&mut conn, "TWO", Some("второй")).expect("create project");
    let other = add(&mut conn, &second, None, "второй закрытый");
    close(&mut conn, &second, &other);
    drop(conn);

    let bin = env!("CARGO_BIN_EXE_tasq");
    let output = Command::new(bin)
        .args([
            "--db",
            db_path.to_str().expect("path"),
            "--project",
            "all",
            "archive",
            "--status",
            "done",
        ])
        .output()
        .expect("run tasq");
    assert!(
        output.status.success(),
        "команда упала: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ONE"), "проект ONE не обработан: {stdout}");
    assert!(stdout.contains("TWO"), "проект TWO не обработан: {stdout}");

    let conn = db::open(&db_path).expect("reopen");
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE archived_at IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(
        total, 2,
        "заархивировано не по одному узлу в каждом проекте: {total}"
    );
}

/// Отказы: ссылка и статус вместе, ни того ни другого, неизвестный статус, dry-run со ссылкой.
#[test]
fn contradictory_arguments_are_refused() {
    let dir = temp_dir("refuse");
    let outcome = db::init(&dir, "REF", "refuse").expect("init");
    let bin = env!("CARGO_BIN_EXE_tasq");
    let cases: [(&[&str], &str); 4] = [
        (&["archive", "REF-1", "--status", "done"], "not both"),
        (&["archive"], "give a reference or --status"),
        (&["archive", "--status", "bogus"], "invalid status"),
        (
            &["archive", "REF-1", "--dry-run"],
            "only works with --status",
        ),
    ];
    for (args, expected) in cases {
        let mut full = vec!["--db", &outcome.db_path, "--project", "REF"];
        full.extend_from_slice(args);
        let output = Command::new(bin).args(&full).output().expect("run tasq");
        assert!(
            !output.status.success(),
            "{args:?} прошло, хотя обязано быть отвергнуто"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(expected),
            "{args:?}: в отказе нет {expected:?}: {stderr}"
        );
    }
}
