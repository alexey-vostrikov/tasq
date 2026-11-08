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
