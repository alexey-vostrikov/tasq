//! `tasq ls` в базе с несколькими проектами: по умолчанию видны все, `--project` сужает,
//! `--except-project` исключает, `-n` гасит закрытое. Плюс режим «все» и его ограничения.
//!
//! Проверяется наблюдаемое поведение: какие dot-ID попали в вывод, а не какие фильтры
//! переданы внутрь.

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
    let dir = std::env::temp_dir().join(format!("tasq-lsall-{tag}-{nanos}"));
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

/// Два проекта с узлами: RRL (эпик + задача, задача закрыта) и DEV (эпик + задача в очереди).
fn seeded(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    assert_eq!(run(&dir, &["init", "--key", "RRL"]).0, 0);
    assert_eq!(run(&dir, &["--project", "RRL", "add", "Эпик RRL"]).0, 0);
    assert_eq!(
        run(
            &dir,
            &["--project", "RRL", "add", "Задача RRL", "--parent", "RRL-1"]
        )
        .0,
        0
    );
    assert_eq!(run(&dir, &["create", "project", "DEV"]).0, 0);
    assert_eq!(run(&dir, &["--project", "DEV", "add", "Эпик DEV"]).0, 0);
    assert_eq!(
        run(
            &dir,
            &["--project", "DEV", "add", "Задача DEV", "--parent", "DEV-1"]
        )
        .0,
        0
    );

    // RRL-1.1: idea -> backlog -> todo -> закрыта через аренду.
    assert_eq!(
        run(
            &dir,
            &["--project", "RRL", "set", "RRL-1.1", "--status", "backlog"]
        )
        .0,
        0
    );
    assert_eq!(
        run(
            &dir,
            &["--project", "RRL", "set", "RRL-1.1", "--status", "todo"]
        )
        .0,
        0
    );
    let (code, claim, stderr) = run(
        &dir,
        &[
            "--project",
            "RRL",
            "claim",
            "RRL-1.1",
            "--holder",
            "t",
            "--json",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    let token = claim
        .lines()
        .find_map(|line| line.split("\"token\": \"").nth(1))
        .and_then(|rest| rest.split('"').next())
        .expect("token")
        .to_string();
    assert_eq!(
        run(
            &dir,
            &[
                "--project",
                "RRL",
                "release",
                &token,
                "--outcome",
                "completed"
            ]
        )
        .0,
        0
    );

    // DEV-1.1: idea -> backlog -> todo (остаётся открытой).
    assert_eq!(
        run(
            &dir,
            &["--project", "DEV", "set", "DEV-1.1", "--status", "backlog"]
        )
        .0,
        0
    );
    assert_eq!(
        run(
            &dir,
            &["--project", "DEV", "set", "DEV-1.1", "--status", "todo"]
        )
        .0,
        0
    );
    dir
}

fn dots(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|token| token.contains('-'))
        .map(str::to_string)
        .collect()
}

/// Без селектора в базе с несколькими проектами `ls` показывает ВСЕ проекты: это ответ на
/// «что у меня есть», а не выбор за пользователя. Адреса узлов самодостаточны — с ключом.
#[test]
fn without_a_selector_every_project_is_listed() {
    let dir = seeded("all");
    let (code, stdout, stderr) = run(&dir, &["ls"]);
    assert_eq!(code, 0, "{stderr}");
    let listed = dots(&stdout);
    for expected in ["RRL-1", "RRL-1.1", "DEV-1", "DEV-1.1"] {
        assert!(
            listed.contains(&expected.to_string()),
            "{expected} не показан при обзоре:\n{stdout}"
        );
    }
}

/// `--project` по-прежнему сужает до одного проекта: обзор по умолчанию его не отменяет.
#[test]
fn an_explicit_project_still_narrows_the_list() {
    let dir = seeded("one");
    let (code, stdout, stderr) = run(&dir, &["--project", "DEV", "ls"]);
    assert_eq!(code, 0, "{stderr}");
    let listed = dots(&stdout);
    assert!(listed.contains(&"DEV-1".to_string()), "{stdout}");
    assert!(
        !listed.iter().any(|dot| dot.starts_with("RRL-")),
        "чужой проект попал в список с селектором:\n{stdout}"
    );

    // `all` — то же, что отсутствие селектора, но сказанное явно.
    let (code, all, stderr) = run(&dir, &["--project", "all", "ls"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        dots(&all).iter().any(|dot| dot.starts_with("RRL-")),
        "{all}"
    );
    assert!(
        dots(&all).iter().any(|dot| dot.starts_with("DEV-")),
        "{all}"
    );
}

/// `--except-project` исключает проект из обзора; флаг повторяемый.
#[test]
fn except_project_hides_the_named_projects() {
    let dir = seeded("except");
    let (code, stdout, stderr) = run(&dir, &["ls", "--except-project", "RRL"]);
    assert_eq!(code, 0, "{stderr}");
    let listed = dots(&stdout);
    assert!(
        !listed.iter().any(|dot| dot.starts_with("RRL-")),
        "RRL не исключён:\n{stdout}"
    );
    assert!(
        listed.contains(&"DEV-1".to_string()),
        "DEV пропал:\n{stdout}"
    );

    let (code, both, stderr) = run(
        &dir,
        &["ls", "--except-project", "RRL", "--except-project", "DEV"],
    );
    assert_eq!(code, 0, "{stderr}");
    assert!(
        dots(&both).is_empty() || !dots(&both).iter().any(|dot| dot.contains('-')),
        "исключили оба проекта, а список не пуст:\n{both}"
    );
}

/// Неизвестный ключ в исключении — отказ, а не молча проигнорированный флаг: опечатка в
/// ключе иначе выглядит как «проект и так не показывался».
#[test]
fn an_unknown_excluded_key_is_refused() {
    let dir = seeded("except-bad");
    let (code, stdout, stderr) = run(&dir, &["ls", "--except-project", "NOPE"]);
    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("PROJECT_NOT_FOUND"), "{stderr}");
    assert!(stdout.is_empty(), "список выдан при отказе: {stdout}");
}

/// `-n` и исключение работают вместе: закрытое гасится, чужой проект не показывается.
#[test]
fn no_done_and_except_compose() {
    let dir = seeded("compose");
    let (code, stdout, stderr) = run(&dir, &["ls", "-n", "--except-project", "DEV"]);
    assert_eq!(code, 0, "{stderr}");
    let listed = dots(&stdout);
    assert!(
        !listed.contains(&"RRL-1.1".to_string()),
        "done показан:\n{stdout}"
    );
    assert!(
        !listed.iter().any(|dot| dot.starts_with("DEV-")),
        "{stdout}"
    );
    assert!(
        listed.contains(&"RRL-1".to_string()),
        "живой узел пропал:\n{stdout}"
    );
}

/// `--parent` в режиме всех проектов отвергается: родитель живёт внутри проекта, и молчаливое
/// игнорирование выдало бы не тот список, который просили.
#[test]
fn parent_is_refused_when_listing_all_projects() {
    let dir = seeded("parent-all");
    let (code, stdout, stderr) = run(&dir, &["ls", "--parent", "RRL-1"]);
    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("INVALID_VALUE"), "{stderr}");
    assert!(
        stderr.contains("parent"),
        "отказ не называет поле: {stderr}"
    );

    // В режиме одного проекта `--parent` работает как раньше.
    let (code, stdout, stderr) = run(&dir, &["--project", "RRL", "ls", "--parent", "RRL-1"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(dots(&stdout).contains(&"RRL-1.1".to_string()), "{stdout}");
}

/// `--json` в обзоре отдаёт узлы всех проектов и не теряет ключ в dot-ID.
#[test]
fn json_across_projects_keeps_project_keys() {
    let dir = seeded("json");
    let (code, stdout, stderr) = run(&dir, &["ls", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let nodes: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    let list = nodes.as_array().expect("array");
    let dot_ids: Vec<&str> = list
        .iter()
        .filter_map(|node| node["dot_id"].as_str())
        .collect();
    assert!(dot_ids.contains(&"RRL-1"), "{dot_ids:?}");
    assert!(dot_ids.contains(&"DEV-1.1"), "{dot_ids:?}");
    // Проект узла различим и отдельным полем — по нему удобно группировать.
    let dev = list
        .iter()
        .find(|node| node["dot_id"] == "DEV-1.1")
        .expect("узел DEV");
    assert_eq!(dev["project_id"], 2);
}
