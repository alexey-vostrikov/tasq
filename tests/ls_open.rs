//! `tasq ls -n`: закрытая работа не попадает в список по умолчанию-скрытому.
//!
//! Проверяется поведение, а не флаг: `-n` и `--no-done` дают один и тот же список, из него
//! исчезают ТОЛЬКО `done`, а работающие и отменённые узлы остаются (отменённое — тоже
//! закрыто, но его видно намеренно: это разные ответы на «что осталось»).
//! Фильтр действует и в `--json`, и с `--status done`: явный статус сильнее, иначе флаг
//! молча гасил бы осмысленный запрос.

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
    let dir = std::env::temp_dir().join(format!("tasq-ls-{tag}-{nanos}"));
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

/// Каталог с одним проектом, где есть по узлу в нужных состояниях:
/// `idea`, `todo`, `in_progress` (с живой арендой), `done`, `cancelled`.
fn seeded(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);
    assert_eq!(run(&dir, &["add", "Эпик"]).0, 0);
    assert_eq!(run(&dir, &["add", "Мысль", "--parent", "TASQ-1"]).0, 0);
    assert_eq!(run(&dir, &["add", "В очереди", "--parent", "TASQ-1"]).0, 0);
    assert_eq!(run(&dir, &["add", "Готово", "--parent", "TASQ-1"]).0, 0);
    assert_eq!(run(&dir, &["add", "Отменено", "--parent", "TASQ-1"]).0, 0);
    assert_eq!(run(&dir, &["add", "В работе", "--parent", "TASQ-1"]).0, 0);

    // Дети эпика: 1.1 мысль, 1.2 очередь, 1.3 готово, 1.4 отменено, 1.5 в работе.
    // Переводы по матрице: idea -> backlog -> todo, дальше работа через аренду.
    for reference in ["TASQ-1.2", "TASQ-1.3", "TASQ-1.4", "TASQ-1.5"] {
        assert_eq!(run(&dir, &["set", reference, "--status", "backlog"]).0, 0);
        assert_eq!(run(&dir, &["set", reference, "--status", "todo"]).0, 0);
    }
    assert_eq!(run(&dir, &["claim", "TASQ-1.5", "--holder", "t"]).0, 0);
    // `done` требует in_progress/review, поэтому: claim -> release --outcome completed.
    let (code, claim, stderr) = run(&dir, &["claim", "TASQ-1.3", "--holder", "t", "--json"]);
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
    assert_eq!(
        run(&dir, &["set", "TASQ-1.5", "--status", "cancelled"]).0,
        0
    );
    dir
}

fn dots(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|token| token.starts_with("TASQ-"))
        .map(str::to_string)
        .collect()
}

#[test]
fn no_done_hides_finished_work_and_nothing_else() {
    let dir = seeded("basic");

    let (code, full, stderr) = run(&dir, &["ls"]);
    assert_eq!(code, 0, "{stderr}");
    let full_dots = dots(&full);
    assert!(
        full_dots.contains(&"TASQ-1.3".to_string()),
        "фон: done виден\n{full}"
    );
    assert!(
        full_dots.contains(&"TASQ-1.4".to_string()),
        "фон: cancelled виден\n{full}"
    );

    let (code, short, stderr) = run(&dir, &["ls", "-n"]);
    assert_eq!(code, 0, "{stderr}");
    let short_dots = dots(&short);
    assert!(
        !short_dots.contains(&"TASQ-1.3".to_string()),
        "-n не должен показывать done:\n{short}"
    );
    // Остальное на месте: флаг гасит ровно закрытое словом `done`.
    for expected in ["TASQ-1", "TASQ-1.1", "TASQ-1.2", "TASQ-1.5"] {
        assert!(
            short_dots.contains(&expected.to_string()),
            "{expected} пропал из списка:\n{short}"
        );
    }
    assert!(
        short_dots.contains(&"TASQ-1.4".to_string()),
        "-n не прячет отменённое: это другой ответ на «что осталось»\n{short}"
    );
    assert_eq!(
        short_dots.len(),
        full_dots.len() - 1,
        "-n убирает ровно один узел: {full_dots:?} -> {short_dots:?}"
    );

    // Длинная форма — тот же результат.
    let (code, long, stderr) = run(&dir, &["ls", "--no-done"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(dots(&long), short_dots, "--no-done и -n расходятся");
}

#[test]
fn the_flag_also_applies_to_json() {
    let dir = seeded("json");
    let (code, stdout, stderr) = run(&dir, &["ls", "-n", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let nodes: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    let list = nodes.as_array().expect("array");
    assert!(!list.is_empty());
    assert!(
        list.iter().all(|node| node["effective_status"] != "done"),
        "в --json просочился done: {stdout}"
    );
}

/// Явный `--status done` сильнее: просьба «покажи готовое» не должна молча давать пустоту.
#[test]
fn an_explicit_status_wins_over_the_flag() {
    let dir = seeded("status");
    let (code, stdout, stderr) = run(&dir, &["ls", "-n", "--status", "done"]);
    assert_eq!(code, 0, "{stderr}");
    let listed = dots(&stdout);
    assert_eq!(listed, vec!["TASQ-1.3".to_string()], "{stdout}");
}
