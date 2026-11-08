//! Гейт бюджета схем: `tasq tokens --max` в CI и в Makefile.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tasq")
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(bin()).args(args).output().expect("run tasq");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn every_profile_fits_the_declared_budget() {
    let (code, stdout, stderr) = run(&["tokens"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        stdout.matches(" ok").count(),
        4,
        "четыре профиля в бюджете:\n{stdout}"
    );
    assert!(!stdout.contains("OVER"), "{stdout}");
}

#[test]
fn a_tight_limit_fails_the_gate() {
    // Нижняя граница заведомо ниже любого профиля.
    let (code, _, stderr) = run(&["tokens", "--max", "100"]);
    assert_eq!(code, 1, "гейт должен упасть");
    assert!(stderr.contains("budget exceeded"), "{stderr}");

    // Щедрая граница проходит.
    let (code, _, _) = run(&["tokens", "--max", "20000"]);
    assert_eq!(code, 0);
}

#[test]
fn single_profile_report_is_json_readable() {
    let (code, stdout, _) = run(&["tokens", "--profile", "migration", "--json"]);
    assert_eq!(code, 0);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let report = &value[0];
    assert_eq!(report["profile"], "migration");
    assert_eq!(report["tools"], 6);
    assert_eq!(report["within_budget"], true);
    assert!(report["tokens"].as_u64().unwrap_or(0) > 0);
    assert!(report["per_tool"].as_array().map(|a| a.len()).unwrap_or(0) == 6);
}

#[test]
fn unknown_profile_is_rejected() {
    let (code, _, stderr) = run(&["tokens", "--profile", "nonsense"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("INVALID_VALUE") || stderr.contains("allowed"),
        "{stderr}"
    );
}

#[test]
fn the_cheapest_profile_is_much_smaller_than_the_full_one() {
    let (_, full, _) = run(&["tokens", "--profile", "full", "--json"]);
    let (_, migration, _) = run(&["tokens", "--profile", "migration", "--json"]);
    let full: serde_json::Value = serde_json::from_str(&full).expect("json");
    let migration: serde_json::Value = serde_json::from_str(&migration).expect("json");
    let full_tokens = full[0]["tokens"].as_u64().expect("tokens");
    let migration_tokens = migration[0]["tokens"].as_u64().expect("tokens");
    assert!(
        migration_tokens * 3 < full_tokens,
        "migration {migration_tokens} против full {full_tokens}"
    );
}
