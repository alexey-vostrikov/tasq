//! `incomplete` — вычисляемое состояние, а не запись в базе.
//!
//! Хранится только решение: узел закрыт — `done`. «Недоведённость» выводится из состояния
//! детей, поэтому она не может разойтись с фактом: закрыли эпик с открытыми задачами — видно
//! `incomplete`; доделали последнюю — эпик сам становится `done`; открыли новую — снова
//! `incomplete`. Ни одной команды «перевести» для этого не нужно, и схемы это не касается.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tasq")
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-incomplete-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn run(dir: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(bin())
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run tasq");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn show(dir: &std::path::Path, reference: &str) -> Value {
    let (code, stdout, stderr) = run(dir, &["show", reference, "--json"]);
    assert_eq!(code, 0, "{stderr}");
    serde_json::from_str(&stdout).expect("json")
}

/// Доводит задачу до `done` обычным путём: очередь, аренда, завершение.
fn finish(dir: &std::path::Path, reference: &str) {
    assert_eq!(run(dir, &["set", reference, "--status", "backlog"]).0, 0);
    let (code, claim, stderr) = run(dir, &["claim", reference, "--holder", "t", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let lease: Value = serde_json::from_str(&claim).expect("json");
    let token = lease["token"].as_str().expect("token").to_string();
    assert_eq!(
        run(dir, &["release", &token, "--outcome", "completed"]).0,
        0
    );
}

/// Переводит узел в `planned` обычным путём: из `idea` матрица закрывать не даёт.
fn plan(dir: &std::path::Path, reference: &str) {
    assert_eq!(run(dir, &["set", reference, "--status", "backlog"]).0, 0);
    assert_eq!(run(dir, &["set", reference, "--status", "planned"]).0, 0);
}

/// Эпик в `planned` с одним открытым и одним закрытым ребёнком.
fn seeded(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);
    assert_eq!(run(&dir, &["add", "Эпик"]).0, 0);
    assert_eq!(run(&dir, &["add", "Открытая", "--parent", "TASQ-1"]).0, 0);
    assert_eq!(run(&dir, &["add", "Закрытая", "--parent", "TASQ-1"]).0, 0);
    finish(&dir, "TASQ-1.2");
    plan(&dir, "TASQ-1");
    dir
}

/// Главное свойство: закрытие с хвостами хранится как `done`, а читается как `incomplete`.
#[test]
fn closing_with_open_children_stores_done_and_reads_incomplete() {
    let dir = seeded("close");
    let (code, stdout, stderr) = run(&dir, &["set", "TASQ-1", "--status", "done", "--json"]);
    assert_eq!(code, 0, "отказа быть не должно: {stderr}");
    let node: Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(node["status"], "done", "хранится решение, не вывод");
    assert_eq!(node["effective_status"], "incomplete", "читается состояние");

    // Прогресс не 100 %: закрыто не значит выполнено.
    let listed = show(&dir, "TASQ-1");
    let done = listed["progress_done"].as_f64().expect("done");
    let total = listed["progress_total"].as_f64().expect("total");
    assert!(done < total, "эпик показал {done}/{total}");
}

/// Хвосты убрали — состояние меняется само, без команды.
#[test]
fn closing_the_last_child_makes_the_epic_done_by_itself() {
    let dir = seeded("heal");
    assert_eq!(run(&dir, &["set", "TASQ-1", "--status", "done"]).0, 0);
    assert_eq!(show(&dir, "TASQ-1")["effective_status"], "incomplete");

    finish(&dir, "TASQ-1.1");

    let node = show(&dir, "TASQ-1");
    assert_eq!(node["status"], "done");
    assert_eq!(
        node["effective_status"], "done",
        "хвостов нет — недоведённость должна исчезнуть сама"
    );
}

/// Открыли новую задачу под закрытым эпиком — он снова недоведён.
#[test]
fn a_new_open_child_makes_the_epic_incomplete_again() {
    let dir = temp_dir("again");
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);
    assert_eq!(run(&dir, &["add", "Эпик"]).0, 0);
    plan(&dir, "TASQ-1");
    assert_eq!(run(&dir, &["set", "TASQ-1", "--status", "done"]).0, 0);
    assert_eq!(
        show(&dir, "TASQ-1")["effective_status"],
        "done",
        "пустой эпик выполнен"
    );

    assert_eq!(run(&dir, &["add", "Позже", "--parent", "TASQ-1"]).0, 0);
    assert_eq!(
        show(&dir, "TASQ-1")["effective_status"],
        "incomplete",
        "появился хвост — эпик снова недоведён"
    );
}

/// Отменённая задача — законный исход, а не хвост: эпик с ней доведён.
#[test]
fn a_cancelled_child_does_not_make_the_epic_incomplete() {
    let dir = temp_dir("cancelled");
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);
    assert_eq!(run(&dir, &["add", "Эпик"]).0, 0);
    assert_eq!(run(&dir, &["add", "Отменим", "--parent", "TASQ-1"]).0, 0);
    assert_eq!(
        run(&dir, &["set", "TASQ-1.1", "--status", "cancelled"]).0,
        0
    );
    plan(&dir, "TASQ-1");
    assert_eq!(run(&dir, &["set", "TASQ-1", "--status", "done"]).0, 0);
    assert_eq!(show(&dir, "TASQ-1")["effective_status"], "done");
}

/// `ls -n` (скрыть выполненное) не прячет недоведённое: это единственный сигнал о хвостах.
#[test]
fn no_done_keeps_incomplete_visible() {
    let dir = seeded("visible");
    assert_eq!(run(&dir, &["set", "TASQ-1", "--status", "done"]).0, 0);

    let (code, short, stderr) = run(&dir, &["ls", "-n", "--fields", "dot_id"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        short.contains("TASQ-1 "),
        "-n спрятал эпик с открытыми задачами:\n{short}"
    );
    assert!(
        !short.contains("TASQ-1.2"),
        "-n должен скрывать выполненную задачу:\n{short}"
    );
}

/// Отбор по вычисляемому состоянию работает так же, как по хранимому.
#[test]
fn status_filter_sees_the_derived_state() {
    let dir = seeded("filter");
    assert_eq!(run(&dir, &["set", "TASQ-1", "--status", "done"]).0, 0);

    let (code, stdout, stderr) = run(
        &dir,
        &[
            "ls",
            "--status",
            "incomplete",
            "--json",
            "--fields",
            "dot_id",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    let rows: Vec<Value> = serde_json::from_str(&stdout).expect("json");
    let ids: Vec<&str> = rows
        .iter()
        .map(|row| row["dot_id"].as_str().expect("dot_id"))
        .collect();
    assert_eq!(ids, vec!["TASQ-1"], "{stdout}");
}

/// Закрытый узел в работу не берётся — недоведённость закрытости не отменяет.
#[test]
fn a_closed_node_cannot_be_claimed() {
    let dir = seeded("claim");
    assert_eq!(run(&dir, &["set", "TASQ-1", "--status", "done"]).0, 0);

    let (code, stdout, stderr) = run(&dir, &["claim", "TASQ-1", "--holder", "other", "--json"]);
    assert_ne!(code, 0, "закрытый узел не берут в работу: {stdout}");
    let text = format!("{stdout}{stderr}");
    assert!(text.contains("NODE_CLOSED"), "{text}");
}

/// Доска подчиняется тем же двум переключателям, что и `ls`: `-n` прячет `done`, но не
/// `incomplete`, `-a` добавляет архив. Проверяется на живой базе, а не на кадре: правило
/// принадлежит доске, а не отрисовке.
#[test]
fn the_board_hides_done_but_keeps_incomplete() {
    use tasq::db;
    use tasq::overview::{BoardFilter, board_with};

    let dir = seeded("board");
    assert_eq!(run(&dir, &["set", "TASQ-1", "--status", "done"]).0, 0);

    let path = dir.join(".tasq/tasq.db");
    let conn = db::open(&path).expect("open");
    let project = tasq::project::first_project(&conn).expect("project");

    let full = board_with(&conn, &project, BoardFilter::default()).expect("board");
    let column = |board: &tasq::overview::Board, name: &str| {
        board
            .columns
            .iter()
            .find(|column| column.status == name)
            .map(|column| column.count)
            .unwrap_or(0)
    };
    assert_eq!(
        column(&full, "done"),
        1,
        "выполненная задача видна по умолчанию"
    );
    assert_eq!(column(&full, "incomplete"), 1, "недоведённый эпик виден");

    let hidden = board_with(
        &conn,
        &project,
        BoardFilter {
            hide_done: true,
            show_archived: false,
            branch: None,
        },
    )
    .expect("board");
    assert_eq!(column(&hidden, "done"), 0, "-n не спрятал done");
    assert_eq!(
        column(&hidden, "incomplete"),
        1,
        "-n спрятал недоведённое — пропал сигнал о хвостах"
    );
    assert_eq!(hidden.incomplete, 1);
    assert_eq!(hidden.done, 0, "счётчик done обязан следовать фильтру");
}
