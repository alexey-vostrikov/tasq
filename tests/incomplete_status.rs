//! Закрытие с хвостами: `incomplete` — закрыт, но не выполнен (схема v11).
//!
//! Проверяется поведение, а не поле: эпик с открытыми задачами закрывается без отказа и без
//! ложного `done`; он остаётся видимым там, где видно закрытое, и не считается выполненным в
//! прогрессе. `--force` по-прежнему пишет именно `done` — это осознанное «я знаю, что это ложь».

use std::fs;
use std::path::PathBuf;
use std::process::Command;

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

/// Эпик с двумя детьми: один в `todo` (открыт), второй закрыт как `done`.
///
/// Эпик переводится в `in_progress` через захват, чтобы переход в закрытое состояние был
/// допустим матрицей, а не требовал `--force`.
fn seeded(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);
    assert_eq!(run(&dir, &["add", "Эпик"]).0, 0);
    assert_eq!(run(&dir, &["add", "Открытая", "--parent", "TASQ-1"]).0, 0);
    assert_eq!(run(&dir, &["add", "Закрытая", "--parent", "TASQ-1"]).0, 0);

    // TASQ-1.1 остаётся открытой (idea), TASQ-1.2 доводится до done через аренду.
    for step in [
        vec!["set", "TASQ-1.2", "--status", "backlog"],
        vec!["set", "TASQ-1.2", "--status", "todo"],
    ] {
        assert_eq!(run(&dir, &step).0, 0);
    }
    let (code, claim, stderr) = run(&dir, &["claim", "TASQ-1.2", "--holder", "t", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let token = claim
        .lines()
        .find_map(|line| line.split("\"token\": \"").nth(1))
        .and_then(|rest| rest.split('"').next())
        .expect("token")
        .to_string();
    assert_eq!(
        run(&dir, &["release", &token, "--outcome", "completed"]).0,
        0
    );

    // Эпик — в работу, чтобы закрытие шло по матрице (in_progress -> done/cancelled разрешены).
    assert_eq!(run(&dir, &["claim", "TASQ-1", "--holder", "t"]).0, 0);
    dir
}

#[test]
fn closing_with_open_children_gives_incomplete_not_done() {
    let dir = seeded("close");

    let (code, stdout, stderr) = run(&dir, &["set", "TASQ-1", "--status", "done", "--json"]);
    assert_eq!(code, 0, "отказа быть не должно: {stderr}");
    let node: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(
        node["status"], "incomplete",
        "эпик с открытыми задачами не может быть done"
    );
    assert_eq!(node["effective_status"], "incomplete");

    // Прогресс не 100 %: закрыто не значит выполнено.
    let (code, stdout, stderr) = run(&dir, &["ls", "--parent", "TASQ-1", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let nodes: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("valid json");
    let epic = nodes
        .iter()
        .find(|node| node["dot_id"] == "TASQ-1")
        .expect("эпик в списке");
    let done = epic["progress_done"].as_f64().expect("progress_done");
    let total = epic["progress_total"].as_f64().expect("progress_total");
    assert!(
        done < total,
        "эпик с открытым ребёнком показал {done}/{total} — это ложь"
    );
}

#[test]
fn no_done_still_shows_incomplete() {
    let dir = seeded("visible");
    assert_eq!(run(&dir, &["set", "TASQ-1", "--status", "done"]).0, 0);

    let (code, full, stderr) = run(&dir, &["ls"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(full.contains("TASQ-1 "), "эпик виден:\n{full}");

    let (code, short, stderr) = run(&dir, &["ls", "-n"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        short.contains("TASQ-1 "),
        "-n спрятал эпик с открытыми задачами — пропал сигнал о хвостах:\n{short}"
    );
    // Закрытая задача под ним скрыта: `-n` гасит ровно `done`.
    assert!(
        !short.contains("TASQ-1.2"),
        "-n должен скрывать закрытую задачу:\n{short}"
    );
}

#[test]
fn force_still_writes_done() {
    let dir = seeded("force");
    let (code, stdout, stderr) = run(
        &dir,
        &["set", "TASQ-1", "--status", "done", "--force", "--json"],
    );
    assert_eq!(code, 0, "{stderr}");
    let node: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(
        node["status"], "done",
        "--force означает «запиши done, я знаю, что остались хвосты»"
    );
}

#[test]
fn epic_without_open_children_closes_as_done() {
    let dir = temp_dir("clean");
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);
    assert_eq!(run(&dir, &["add", "Эпик"]).0, 0);
    assert_eq!(run(&dir, &["claim", "TASQ-1", "--holder", "t"]).0, 0);

    let (code, stdout, stderr) = run(&dir, &["set", "TASQ-1", "--status", "done", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let node: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(node["status"], "done", "пустой эпик закрывается как done");
}

#[test]
fn incomplete_reopens_without_force() {
    let dir = seeded("reopen");
    assert_eq!(run(&dir, &["set", "TASQ-1", "--status", "done"]).0, 0);

    // Возврат в работу: «закрыто, но не закончено» открывается без флага.
    let (code, stdout, stderr) = run(&dir, &["set", "TASQ-1", "--status", "planned", "--json"]);
    assert_eq!(code, 0, "--force не должен требоваться: {stderr}");
    let node: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(node["status"], "planned");
}

#[test]
fn closed_node_cannot_be_claimed() {
    let dir = seeded("claim");
    assert_eq!(run(&dir, &["set", "TASQ-1", "--status", "done"]).0, 0);

    let (code, stdout, stderr) = run(&dir, &["claim", "TASQ-1", "--holder", "other", "--json"]);
    assert_ne!(code, 0, "закрытый узел не берут в работу: {stdout}");
    assert!(
        stderr.contains("NODE_CLOSED") || stdout.contains("NODE_CLOSED"),
        "ожидался NODE_CLOSED: {stderr}"
    );
}
