//! Интеграционные тесты CLI: работают с настоящим бинарём в отдельном каталоге.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tasq")
}

/// Уникальный временный каталог без внешних зависимостей.
fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-test-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn run(dir: &Path, args: &[&str]) -> (i32, String, String) {
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

#[test]
fn init_creates_database_and_project() {
    let dir = temp_dir("init");
    let (code, stdout, stderr) = run(&dir, &["init", "--key", "TASQ", "--name", "demo"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("TASQ"), "stdout: {stdout}");

    let db = dir.join(".tasq").join("tasq.db");
    assert!(db.is_file(), "database file must exist: {}", db.display());

    // повторный запуск идемпотентен и проект не дублирует
    let (code, stdout, _) = run(&dir, &["init", "--key", "OTHER"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("TASQ"),
        "existing project must be kept: {stdout}"
    );
    assert!(stdout.contains("already present"), "stdout: {stdout}");
}

#[test]
fn doctor_reports_healthy_database() {
    let dir = temp_dir("doctor");
    run(&dir, &["init", "--key", "TASQ"]);
    let (code, stdout, stderr) = run(&dir, &["doctor", "--check"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("integrity: ok"), "stdout: {stdout}");
    assert!(stdout.contains("status:    ok"), "stdout: {stdout}");
}

#[test]
fn doctor_json_is_machine_readable() {
    let dir = temp_dir("json");
    run(&dir, &["init", "--key", "API2"]);
    let (code, stdout, _) = run(&dir, &["doctor", "--json"]);
    assert_eq!(code, 0);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(value["ok"], serde_json::Value::Bool(true));
    assert_eq!(value["schema_version"], serde_json::json!(1));
    assert_eq!(value["projects"], serde_json::json!(1));
}

#[test]
fn missing_database_is_a_domain_error() {
    let dir = temp_dir("nodb");
    let (code, _, stderr) = run(&dir, &["doctor"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("DB_NOT_FOUND"), "stderr: {stderr}");
    assert!(stderr.contains("hint:"), "stderr: {stderr}");
}

#[test]
fn invalid_key_is_rejected() {
    let dir = temp_dir("badkey");
    let (code, _, stderr) = run(&dir, &["init", "--key", "tasq-lower"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("INVALID_KEY"), "stderr: {stderr}");
}

#[test]
fn tree_workflow_through_the_cli() {
    let dir = temp_dir("tree");
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);

    let (code, stdout, stderr) = run(&dir, &["add", "Epic", "--desc", "big goal"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("TASQ-1"), "stdout: {stdout}");

    let (code, _, stderr) = run(
        &dir,
        &["add", "Task", "--parent", "TASQ-1", "--due", "2026-12-01"],
    );
    assert_eq!(code, 0, "{stderr}");
    let (code, _, stderr) = run(&dir, &["add", "Sub", "--parent", "TASQ-1.1"]);
    assert_eq!(code, 0, "{stderr}");

    let (_, stdout, _) = run(&dir, &["ls"]);
    assert!(stdout.contains("TASQ-1 "), "{stdout}");
    assert!(stdout.contains("TASQ-1.1 "), "{stdout}");
    assert!(stdout.contains("TASQ-1.1.1"), "{stdout}");

    // вложенность видна отступом
    let sub_line = stdout
        .lines()
        .find(|line| line.contains("TASQ-1.1.1"))
        .expect("subtask line");
    assert!(
        sub_line.starts_with("    "),
        "ожидался отступ: {sub_line:?}"
    );

    let (code, stdout, _) = run(&dir, &["show", "TASQ-1", "--tree"]);
    assert_eq!(code, 0);
    assert_eq!(stdout.lines().count(), 3, "в поддереве три узла: {stdout}");

    let (code, stdout, _) = run(&dir, &["show", "TASQ-1.1"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("due       2026-12-01"), "{stdout}");

    // перенос: dot-ID меняется
    assert_eq!(run(&dir, &["add", "Second epic"]).0, 0);
    let (code, stdout, stderr) = run(&dir, &["mv", "TASQ-1.1", "--parent", "TASQ-2"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("TASQ-2.1"), "stdout: {stdout}");
    let (code, stdout, _) = run(&dir, &["show", "TASQ-2.1.1"]);
    assert_eq!(code, 0, "потомок следует за родителем: {stdout}");

    // лог видит перенос
    let (code, stdout, _) = run(&dir, &["log"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("move"), "{stdout}");
    assert!(stdout.contains("1.1 -> 2.1"), "{stdout}");

    // удаление и восстановление
    assert_eq!(run(&dir, &["rm", "TASQ-2.1.1"]).0, 0);
    assert!(run(&dir, &["show", "TASQ-2.1.1"]).0 != 0);
    assert_eq!(run(&dir, &["restore", "TASQ-2.1.1"]).0, 0);
    assert_eq!(run(&dir, &["show", "TASQ-2.1.1"]).0, 0);
}

#[test]
fn json_output_is_machine_readable_for_nodes() {
    let dir = temp_dir("nodejson");
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);
    assert_eq!(run(&dir, &["add", "Epic", "--weight", "3"]).0, 0);
    let (code, stdout, _) = run(&dir, &["ls", "--json"]);
    assert_eq!(code, 0);
    let nodes: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    let first = &nodes[0];
    assert_eq!(first["dot_id"], "TASQ-1");
    assert_eq!(first["kind"], "epic");
    assert_eq!(first["effective_status"], "planned");
    assert_eq!(first["weight"], 3.0);
}

#[test]
fn bad_values_are_rejected_before_writing() {
    let dir = temp_dir("badvalues");
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);

    let (code, _, stderr) = run(&dir, &["add", "Epic", "--priority", "urgent"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("INVALID_VALUE"), "{stderr}");

    let (code, _, stderr) = run(&dir, &["add", "Epic", "--due", "01.12.2026"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("INVALID_VALUE"), "{stderr}");

    // ничего не записалось
    let (_, stdout, _) = run(&dir, &["ls"]);
    assert!(stdout.contains("no nodes"), "{stdout}");
}

#[test]
fn in_progress_from_the_cli_points_at_the_lease() {
    let dir = temp_dir("lease");
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);
    assert_eq!(run(&dir, &["add", "Epic"]).0, 0);
    assert_eq!(run(&dir, &["add", "Task", "--parent", "TASQ-1"]).0, 0);
    let (code, _, stderr) = run(&dir, &["set", "TASQ-1.1", "--status", "in_progress"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("LEASE_REQUIRED"), "{stderr}");
    assert!(
        stderr.contains("tasq claim"),
        "подсказка ведёт к аренде: {stderr}"
    );
}

/// Запускает `tasq` в отдельном процессе (для тестов гонки и убийства держателя).
fn spawn(dir: &Path, args: &[&str]) -> std::process::Child {
    Command::new(bin())
        .current_dir(dir)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn tasq")
}

fn token_of(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("token      "))
        .map(|value| value.trim().to_string())
        .unwrap_or_else(|| panic!("no token in output:\n{stdout}"))
}

#[test]
fn lease_round_trip_through_the_cli() {
    let dir = temp_dir("leaseflow");
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);
    assert_eq!(run(&dir, &["add", "Epic"]).0, 0);
    assert_eq!(run(&dir, &["add", "Task", "--parent", "TASQ-1"]).0, 0);

    let (code, stdout, stderr) = run(&dir, &["claim", "TASQ-1.1", "--holder", "agent-a"]);
    assert_eq!(code, 0, "{stderr}");
    let token = token_of(&stdout);
    assert_eq!(token.len(), 64);

    // узел занят: второй захват называет держателя
    let (code, _, stderr) = run(&dir, &["claim", "TASQ-1.1", "--holder", "agent-b"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("ACTIVE_ATTEMPT_EXISTS"), "{stderr}");
    assert!(stderr.contains("agent-a"), "{stderr}");

    // holders показывает живую аренду
    let (code, stdout, _) = run(&dir, &["holders"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("agent-a"), "{stdout}");
    assert!(stdout.contains("TASQ-1.1"), "{stdout}");

    // продление и завершение
    let (code, stdout, stderr) = run(&dir, &["renew", &token, "--ttl", "1800"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("renewed until"), "{stdout}");

    let (code, stdout, stderr) = run(
        &dir,
        &[
            "release",
            &token,
            "--outcome",
            "completed",
            "--checkpoint",
            "all done",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("TASQ-1.1 -> done"), "{stdout}");

    let (_, stdout, _) = run(&dir, &["holders"]);
    assert!(stdout.contains("no live leases"), "{stdout}");

    // лог содержит захват, продление и завершение
    let (_, stdout, _) = run(&dir, &["log", "--limit", "20"]);
    for action in ["claim", "renew", "completed"] {
        assert!(stdout.contains(action), "нет действия {action}:\n{stdout}");
    }
}

#[test]
fn only_one_process_wins_the_race() {
    let dir = temp_dir("race");
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);
    assert_eq!(run(&dir, &["add", "Epic"]).0, 0);

    let children: Vec<_> = (0..8)
        .map(|worker| {
            let holder = format!("agent-{worker}");
            spawn(
                &dir,
                &["claim", "TASQ-1", "--holder", &holder, "--ttl", "60"],
            )
        })
        .collect();

    let mut winners = 0;
    for child in children {
        let output = child.wait_with_output().expect("wait");
        if output.status.success() {
            winners += 1;
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("ACTIVE_ATTEMPT_EXISTS"),
                "проигравший должен упереться в занятую аренду: {stderr}"
            );
        }
    }
    assert_eq!(winners, 1, "ровно один процесс получает аренду");

    let (_, stdout, _) = run(&dir, &["holders"]);
    assert_eq!(
        stdout.matches("attempt 1").count(),
        1,
        "живая аренда ровно одна:\n{stdout}"
    );
}

#[test]
fn killed_holder_leaves_the_lease_until_it_expires() {
    let dir = temp_dir("killed");
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);
    assert_eq!(run(&dir, &["add", "Epic"]).0, 0);

    // держатель захватывает узел; процесс завершается — аренда остаётся живой
    let holder = spawn(
        &dir,
        &[
            "claim",
            "TASQ-1",
            "--holder",
            "doomed-agent",
            "--ttl",
            "900",
        ],
    );
    let output = holder.wait_with_output().expect("claim output");
    assert!(output.status.success(), "захват должен пройти");
    let token = token_of(&String::from_utf8_lossy(&output.stdout));

    let (_, stdout, _) = run(&dir, &["holders"]);
    assert!(stdout.contains("doomed-agent"), "{stdout}");

    let (code, _, stderr) = run(&dir, &["claim", "TASQ-1", "--holder", "agent-b"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("doomed-agent"), "{stderr}");

    // восстановление: перехват живой аренды осознанным --steal
    let (code, stdout, stderr) = run(&dir, &["claim", "TASQ-1", "--holder", "agent-b", "--steal"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("superseded attempt"), "{stdout}");
    let new_token = token_of(&stdout);
    assert_ne!(new_token, token, "у новой попытки свой токен");

    // прежний токен больше не действует
    let (code, _, stderr) = run(&dir, &["renew", &token]);
    assert_eq!(code, 1);
    assert!(stderr.contains("NOT_YOUR_ATTEMPT"), "{stderr}");
}
