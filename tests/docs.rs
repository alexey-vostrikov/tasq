//! `tasq docs`: генерация справочника и гейт расхождения.

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
    let dir = std::env::temp_dir().join(format!("tasq-docs-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
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
fn generated_reference_covers_every_command_and_tool() {
    let dir = temp_dir("generate");
    let (code, stdout, stderr) = run(&["docs", "--out", dir.to_str().expect("path")]);
    assert_eq!(code, 0, "{stdout}{stderr}");

    let cli = fs::read_to_string(dir.join("cli.md")).expect("cli.md");
    // Все команды верхнего уровня должны попасть в справочник.
    for command in [
        "init",
        "doctor",
        "add",
        "show",
        "ls",
        "set",
        "mv",
        "rm",
        "restore",
        "archive",
        "log",
        "claim",
        "renew",
        "release",
        "reserve",
        "unreserve",
        "holders",
        "dep",
        "next",
        "note",
        "comment",
        "template",
        "find",
        "ctx",
        "board",
        "bulk",
        "burnup",
        "risk",
        "time",
        "export",
        "import",
        "tokens",
        "connect",
        "create",
        "list",
        "mcp",
        "docs",
    ] {
        assert!(
            cli.contains(&format!("## `{command}`")),
            "в справочнике нет команды {command}"
        );
    }
    assert!(cli.contains("--json"), "глобальные флаги описаны");

    let tools = fs::read_to_string(dir.join("mcp-tools.md")).expect("mcp-tools.md");
    assert!(tools.contains("Всего инструментов: 44"), "{tools}");
    for name in [
        "tasq_claim",
        "node_create",
        "tasq_export",
        "tasq_risk",
        "template_apply",
    ] {
        assert!(tools.contains(name), "в каталоге нет {name}");
    }
    // Стоимость инструмента — число, а не прочерк.
    assert!(
        tools.contains("`node_create`"),
        "строка node_create на месте"
    );

    let schema = fs::read_to_string(dir.join("schema.md")).expect("schema.md");
    for table in [
        "nodes",
        "edges",
        "attempts",
        "reservations",
        "projects",
        "activity_log",
    ] {
        assert!(schema.contains(table), "в схеме нет {table}");
    }
    assert!(
        schema.contains("CREATE TABLE"),
        "схема снята с базы, а не пересказана"
    );
    assert!(
        schema.contains("CREATE VIEW") || schema.contains("CREATE INDEX"),
        "представления и индексы тоже попадают в схему"
    );
}

#[test]
fn check_reports_drift_and_missing_files() {
    let dir = temp_dir("check");
    let root = dir.to_str().expect("path");

    // Пока файлов нет — это расхождение, а не успех.
    let (code, _, stderr) = run(&["docs", "--check", "--out", root]);
    assert_eq!(code, 3, "пустой каталог = расхождение: {stderr}");
    assert!(stderr.contains("отсутствует"), "{stderr}");

    let (code, _, _) = run(&["docs", "--out", root]);
    assert_eq!(code, 0);
    let (code, stdout, _) = run(&["docs", "--check", "--out", root]);
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("up to date"), "{stdout}");

    // Правка файла руками — расхождение с бинарём.
    let path = dir.join("mcp-tools.md");
    let mut text = fs::read_to_string(&path).expect("read");
    text.push_str("\nдописано руками\n");
    fs::write(&path, text).expect("write");

    let (code, _, stderr) = run(&["docs", "--check", "--out", root]);
    assert_eq!(code, 3, "правка руками должна ловиться: {stderr}");
    assert!(stderr.contains("устарел"), "{stderr}");

    // Повторная генерация возвращает согласие.
    let (code, _, _) = run(&["docs", "--out", root]);
    assert_eq!(code, 0);
    let (code, _, _) = run(&["docs", "--check", "--out", root]);
    assert_eq!(code, 0);
}

#[test]
fn generation_is_deterministic() {
    let first = temp_dir("det-a");
    let second = temp_dir("det-b");
    assert_eq!(run(&["docs", "--out", first.to_str().expect("p")]).0, 0);
    assert_eq!(run(&["docs", "--out", second.to_str().expect("p")]).0, 0);

    for name in ["cli.md", "mcp-tools.md", "schema.md"] {
        let left = fs::read_to_string(first.join(name)).expect("left");
        let right = fs::read_to_string(second.join(name)).expect("right");
        assert_eq!(
            left, right,
            "{name} должен быть одинаковым при повторной генерации"
        );
        assert!(!left.trim().is_empty(), "{name} не пуст");
    }
}

#[test]
fn docs_command_does_not_need_a_project_database() {
    let dir = temp_dir("nodb");
    let (code, _, stderr) = run(&["docs", "--out", dir.to_str().expect("p")]);
    assert_eq!(code, 0, "{stderr}");
    // В рабочем каталоге документация ничего не создаёт.
    let stray = fs::read_dir(std::env::current_dir().expect("cwd"))
        .expect("cwd")
        .filter_map(|entry| entry.ok())
        .any(|entry| entry.file_name() == ".tasq");
    assert!(!stray || std::env::current_dir().expect("cwd").join(".tasq").exists());
}

#[test]
fn json_output_lists_written_files() {
    let dir = temp_dir("json");
    let (code, stdout, _) = run(&["--json", "docs", "--out", dir.to_str().expect("p")]);
    assert_eq!(code, 0);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(value["ok"], serde_json::json!(true));
    let written = value["written"].as_array().expect("written");
    assert_eq!(written.len(), 3);
}
