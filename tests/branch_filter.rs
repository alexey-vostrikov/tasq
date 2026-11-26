//! Ветка как рабочий фильтр: `--branch`, автозначение из git, отбор в `ls` и `next`.
//!
//! Проверяется поведение, а не флаг: ветка — точный адрес, поэтому `feat/x` не должен задевать
//! `feat/x-fix`, а вне репозитория значения нет вовсе — и фильтр не включается молча.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tasq")
}

/// Каталог, которого git не видит: `GIT_CEILING_DIRECTORIES` обрывает поиск `.git` вверх, и
/// это единственный способ получить «вне репозитория» внутри дерева, где репозиторий есть.
fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-branch-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn run_in(
    dir: &std::path::Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> (i32, String, String) {
    let mut command = Command::new(bin());
    command.current_dir(dir).args(args);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let out = command.output().expect("run tasq");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// Вне репозитория: `git` не должен найти `.git` выше по дереву.
fn run(dir: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let ceiling = dir.to_string_lossy().to_string();
    run_in(dir, args, &[("GIT_CEILING_DIRECTORIES", ceiling.as_str())])
}

fn seeded(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);
    assert_eq!(run(&dir, &["add", "В ветке", "--branch", "feat/x"]).0, 0);
    assert_eq!(
        run(&dir, &["add", "Похожая ветка", "--branch", "feat/x-fix"]).0,
        0
    );
    assert_eq!(run(&dir, &["add", "Без ветки"]).0, 0);
    dir
}

fn dots(stdout: &str) -> Vec<String> {
    let rows: Vec<Value> = serde_json::from_str(stdout).expect("json");
    rows.iter()
        .map(|row| row["dot_id"].as_str().expect("dot_id").to_string())
        .collect()
}

/// `ls --branch NAME` — точное совпадение имени ветки.
#[test]
fn ls_filters_by_branch_exactly() {
    let dir = seeded("exact");

    let (code, stdout, stderr) = run(
        &dir,
        &["ls", "--json", "--fields", "dot_id", "--branch", "feat/x"],
    );
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        dots(&stdout),
        vec!["TASQ-1"],
        "ветка сравнивается как адрес, а не как префикс: {stdout}"
    );

    let (code, stdout, stderr) = run(
        &dir,
        &[
            "ls",
            "--json",
            "--fields",
            "dot_id",
            "--branch",
            "feat/x-fix",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(dots(&stdout), vec!["TASQ-2"]);
}

/// `set --branch` ставит ветку, `--no-branch` снимает её в NULL, а не в пустую строку.
#[test]
fn set_assigns_and_clears_the_branch() {
    let dir = seeded("set");

    let (code, stdout, stderr) = run(
        &dir,
        &["set", "TASQ-3", "--branch", "release/1.0", "--json"],
    );
    assert_eq!(code, 0, "{stderr}");
    let node: Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(node["branch"], "release/1.0");

    let (code, stdout, stderr) = run(&dir, &["set", "TASQ-3", "--no-branch", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let node: Value = serde_json::from_str(&stdout).expect("json");
    assert!(
        node["branch"].is_null(),
        "снятая ветка обязана быть NULL, а не пустой строкой: {node}"
    );

    // Фильтр по ветке после снятия пуст, но узлы возвращаются фильтром без ветки.
    let (code, stdout, _) = run(
        &dir,
        &[
            "ls",
            "--json",
            "--fields",
            "dot_id",
            "--branch",
            "release/1.0",
        ],
    );
    assert_eq!(code, 0);
    assert!(dots(&stdout).is_empty(), "{stdout}");
}

/// `next --branch` отбирает кандидатов до обрезки по `limit`: иначе чужая ветка заняла бы
/// место в ответе и была бы выброшена, и строк оказалось бы меньше запрошенного.
#[test]
fn next_filters_candidates_before_the_limit() {
    let dir = seeded("next");
    // Три узла в одной ветке и один в другой; `-l 2` обязан вернуть два из СВОЕЙ ветки.
    for title in ["Первая", "Вторая", "Третья"] {
        assert_eq!(run(&dir, &["add", title, "--branch", "work"]).0, 0);
    }
    for reference in ["TASQ-4", "TASQ-5", "TASQ-6"] {
        assert_eq!(run(&dir, &["set", reference, "--status", "backlog"]).0, 0);
        assert_eq!(run(&dir, &["set", reference, "--status", "planned"]).0, 0);
    }
    assert_eq!(run(&dir, &["set", "TASQ-1", "--status", "backlog"]).0, 0);
    assert_eq!(run(&dir, &["set", "TASQ-1", "--status", "planned"]).0, 0);

    let (code, stdout, stderr) = run(
        &dir,
        &["next", "--branch", "work", "--limit", "2", "--json"],
    );
    assert_eq!(code, 0, "{stderr}");
    let rows: Vec<Value> = serde_json::from_str(&stdout).expect("json");
    assert_eq!(
        rows.len(),
        2,
        "фильтр обязан примениться до обрезки: {stdout}"
    );
    for row in &rows {
        assert_eq!(
            row["branch"], "work",
            "в ответ попал узел чужой ветки: {row}"
        );
    }
}

/// Вне репозитория `--branch` без значения значения не даёт: фильтр не включается, и об этом
/// сказано — молча выданный полный список читался бы как «в этой ветке весь проект».
#[test]
fn a_bare_branch_outside_git_says_so() {
    let dir = seeded("nogit");

    let (code, stdout, stderr) = run(&dir, &["ls", "--json", "--fields", "dot_id", "--branch"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("filter is off"),
        "отсутствие ветки обязано быть названо: {stderr}"
    );
    assert_eq!(
        dots(&stdout).len(),
        3,
        "без значения фильтр не включается: {stdout}"
    );
}

/// Внутри репозитория `--branch` без значения берёт ветку у git — и это проверяется на
/// настоящем репозитории, а не подстановкой переменной.
#[test]
fn a_bare_branch_takes_the_git_branch() {
    let dir = seeded("git");
    // Свой репозиторий внутри временного каталога: `git init` + `checkout -b` дают ветку,
    // которую видит `rev-parse`, и стек не зависит от ветки самого tasq.
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .current_dir(&dir)
            .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["checkout", "-q", "-b", "feat/live"]);

    assert_eq!(run(&dir, &["add", "Живая ветка", "--branch"]).0, 0);
    let (code, stdout, stderr) = run(
        &dir,
        &["ls", "--json", "--fields", "dot_id,branch", "--branch"],
    );
    assert_eq!(code, 0, "{stderr}");
    let rows: Vec<Value> = serde_json::from_str(&stdout).expect("json");
    assert_eq!(rows.len(), 1, "{stdout}");
    assert_eq!(rows[0]["branch"], "feat/live", "{stdout}");
}

/// Вне git узел не получает ветку от `add --branch` — и это не ошибка, а отсутствие значения.
#[test]
fn add_with_bare_branch_outside_git_stores_nothing() {
    let dir = seeded("add-nogit");
    let (code, stdout, stderr) = run(&dir, &["add", "Без ветки снаружи", "--branch", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let node: Value = serde_json::from_str(&stdout).expect("json");
    assert!(
        node["branch"].is_null(),
        "ветка не придумывается вне репозитория: {node}"
    );
}
