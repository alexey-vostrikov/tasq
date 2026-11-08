//! `tasq connect`: правка конфигов клиентов на настоящих файлах во временном `HOME`.
//!
//! Ни один тест не трогает реальные конфиги: `HOME` подменяется на временный каталог,
//! и только там создаются файлы.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tasq")
}

struct Sandbox {
    home: PathBuf,
    project: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("tasq-connect-{tag}-{nanos}"));
        let home = root.join("home");
        let project = root.join("project");
        fs::create_dir_all(&home).expect("home");
        fs::create_dir_all(&project).expect("project");
        Sandbox { home, project }
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = Command::new(bin())
            .current_dir(&self.project)
            .env("HOME", &self.home)
            .args(args)
            .output()
            .expect("run tasq");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.home.join(relative)).expect("config file")
    }
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).expect("json")
}

#[test]
fn listing_touches_nothing() {
    let sandbox = Sandbox::new("list");
    let (code, stdout, _) = sandbox.run(&["connect"]);
    assert_eq!(code, 0);
    for client in [
        "claude-code",
        "claude-desktop",
        "opencode",
        "zed",
        "cursor",
        "vscode",
        "gemini",
        "codex",
        "goose",
        "crush",
        "windsurf",
    ] {
        assert!(stdout.contains(client), "нет клиента {client}:\n{stdout}");
    }
    assert!(stdout.contains("ничего не записано"), "{stdout}");

    // Ни одного файла не создано.
    let created = fs::read_dir(sandbox.home)
        .expect("home")
        .filter_map(|entry| entry.ok())
        .count();
    assert_eq!(created, 0, "список клиентов не должен создавать конфиги");
}

#[test]
fn write_then_repeat_is_idempotent_and_backed_up() {
    let sandbox = Sandbox::new("idempotent");
    let (code, stdout, _) = sandbox.run(&["connect", "claude-code", "--scope", "user"]);
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("written"), "{stdout}");

    let document = json(&sandbox.read(".claude.json"));
    let server = &document["mcpServers"]["tasq"];
    assert!(
        server["command"]
            .as_str()
            .unwrap_or_default()
            .ends_with("tasq"),
        "{server}"
    );
    assert_eq!(server["args"], serde_json::json!(["mcp"]));

    // Второй запуск ничего не меняет и не плодит копии.
    let stamp = fs::metadata(sandbox.home.join(".claude.json"))
        .expect("metadata")
        .modified()
        .expect("mtime");
    let (code, stdout, _) = sandbox.run(&["connect", "claude-code", "--scope", "user"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("already configured"), "{stdout}");
    let again = fs::metadata(sandbox.home.join(".claude.json"))
        .expect("metadata")
        .modified()
        .expect("mtime");
    assert_eq!(stamp, again, "файл не должен перезаписываться");
    let backups = fs::read_dir(&sandbox.home)
        .expect("home")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains(".bak-"))
        .count();
    assert_eq!(backups, 0, "при повторном запуске копий быть не должно");
}

#[test]
fn existing_keys_survive_the_edit() {
    let sandbox = Sandbox::new("preserve");
    fs::write(
        sandbox.home.join(".claude.json"),
        r#"{"theme":"dark","mcpServers":{"other":{"command":"x"}}}"#,
    )
    .expect("write");

    let (code, _, stderr) = sandbox.run(&["connect", "claude-code", "--scope", "user"]);
    assert_eq!(code, 0, "{stderr}");

    let document = json(&sandbox.read(".claude.json"));
    assert_eq!(
        document["theme"],
        serde_json::json!("dark"),
        "чужой ключ на месте"
    );
    assert_eq!(
        document["mcpServers"]["other"]["command"],
        serde_json::json!("x"),
        "чужой сервер на месте"
    );
    assert!(document["mcpServers"]["tasq"].is_object());

    let backups: Vec<String> = fs::read_dir(&sandbox.home)
        .expect("home")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.contains(".bak-"))
        .collect();
    assert_eq!(
        backups.len(),
        1,
        "перед правкой сделана одна копия: {backups:?}"
    );
    let backup = sandbox.home.join(&backups[0]);
    assert_eq!(
        json(&fs::read_to_string(backup).expect("backup"))["theme"],
        serde_json::json!("dark")
    );
}

#[test]
fn a_config_with_comments_is_left_alone() {
    let sandbox = Sandbox::new("comments");
    let path = sandbox.home.join(".config/zed/settings.json");
    fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    let original = "// Zed settings\n{\n  \"agent_servers\": {}\n}\n";
    fs::write(&path, original).expect("write");

    let (code, stdout, _) = sandbox.run(&["connect", "zed"]);
    assert_eq!(
        code, 3,
        "код 3 = править нельзя, нужен ручной шаг:\n{stdout}"
    );
    assert_eq!(
        fs::read_to_string(&path).expect("read"),
        original,
        "файл с комментариями не перезаписывается"
    );
    assert!(stdout.contains("not touched"), "{stdout}");
    assert!(
        stdout.contains("agent_servers"),
        "подсказка называет правильный ключ:\n{stdout}"
    );
}

#[test]
fn zed_key_follows_the_file() {
    let sandbox = Sandbox::new("zedkey");
    let dir = sandbox.home.join(".config/zed");
    fs::create_dir_all(&dir).expect("dir");

    // Новая версия редактора: agent_servers.
    fs::write(dir.join("settings.json"), r#"{"agent_servers":{}}"#).expect("write");
    let (code, _, stderr) = sandbox.run(&["connect", "zed"]);
    assert_eq!(code, 0, "{stderr}");
    let document = json(&sandbox.read(".config/zed/settings.json"));
    assert!(
        document["agent_servers"]["tasq"].is_object(),
        "запись ушла в agent_servers: {document}"
    );
    assert!(document.get("context_servers").is_none());

    // Старая версия: context_servers.
    fs::write(dir.join("settings.json"), r#"{"context_servers":{}}"#).expect("write");
    let (code, _, stderr) = sandbox.run(&["connect", "zed"]);
    assert_eq!(code, 0, "{stderr}");
    let document = json(&sandbox.read(".config/zed/settings.json"));
    assert!(
        document["context_servers"]["tasq"].is_object(),
        "запись ушла в context_servers: {document}"
    );
}

#[test]
fn opencode_gets_a_single_command_array() {
    let sandbox = Sandbox::new("opencode");
    fs::create_dir_all(sandbox.home.join(".config/opencode")).expect("dir");
    fs::write(
        sandbox.home.join(".config/opencode/opencode.json"),
        r#"{"$schema":"https://opencode.ai/config.json","model":"deepseek/deepseek-flash"}"#,
    )
    .expect("write");

    let (code, _, stderr) = sandbox.run(&["connect", "opencode"]);
    assert_eq!(code, 0, "{stderr}");
    let document = json(&sandbox.read(".config/opencode/opencode.json"));
    assert_eq!(
        document["model"],
        serde_json::json!("deepseek/deepseek-flash")
    );
    let server = &document["mcp"]["tasq"];
    assert_eq!(server["type"], serde_json::json!("local"));
    let command = server["command"].as_array().expect("command array");
    assert_eq!(
        command.len(),
        2,
        "команда и аргумент в одном массиве: {command:?}"
    );
    assert_eq!(command[1], serde_json::json!("mcp"));
}

#[test]
fn codex_gets_a_toml_table_and_no_empty_parent() {
    let sandbox = Sandbox::new("codex");
    fs::create_dir_all(sandbox.home.join(".codex")).expect("dir");
    fs::write(
        sandbox.home.join(".codex/config.toml"),
        "[model]\nname = \"deepseek\"\n",
    )
    .expect("write");

    let (code, _, stderr) = sandbox.run(&["connect", "codex"]);
    assert_eq!(code, 0, "{stderr}");
    let text = sandbox.read(".codex/config.toml");
    assert!(text.contains("[model]"), "{text}");
    assert!(text.contains("name = \"deepseek\""), "{text}");
    assert!(text.contains("[mcp_servers.tasq]"), "{text}");
    assert!(text.contains("args = [\"mcp\"]"), "{text}");
    // Пустая таблица-контейнер не должна появляться.
    assert!(
        !text.lines().any(|line| line.trim() == "[mcp_servers]"),
        "лишний заголовок:\n{text}"
    );

    let (code, stdout, _) = sandbox.run(&["connect", "codex"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("already configured"), "{stdout}");
}

#[test]
fn goose_prints_yaml_instead_of_json() {
    let sandbox = Sandbox::new("goose");
    let (code, stdout, _) = sandbox.run(&["connect", "goose"]);
    assert_eq!(code, 3, "YAML правится руками:\n{stdout}");
    assert!(stdout.contains("extensions:"), "{stdout}");
    assert!(stdout.contains("type: stdio"), "{stdout}");
    assert!(stdout.contains("  - mcp"), "{stdout}");
    assert!(!stdout.contains("\"args\""), "не JSON:\n{stdout}");
}

#[test]
fn remove_takes_the_entry_away_and_leaves_others() {
    let sandbox = Sandbox::new("remove");
    fs::write(
        sandbox.home.join(".claude.json"),
        r#"{"mcpServers":{"other":{"command":"x"}},"theme":"dark"}"#,
    )
    .expect("write");

    let (code, _, stderr) = sandbox.run(&["connect", "claude-code", "--scope", "user"]);
    assert_eq!(code, 0, "{stderr}");
    let (code, stdout, _) = sandbox.run(&["connect", "claude-code", "--scope", "user", "--remove"]);
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("removed"), "{stdout}");
    let document = json(&sandbox.read(".claude.json"));
    assert!(document["mcpServers"]["tasq"].is_null());
    assert_eq!(
        document["mcpServers"]["other"]["command"],
        serde_json::json!("x")
    );
    assert_eq!(document["theme"], serde_json::json!("dark"));

    // Удаление контейнера, созданного нами же.
    fs::write(sandbox.home.join(".claude.json"), "{}").expect("write");
    let (code, _, _) = sandbox.run(&["connect", "claude-code", "--scope", "user"]);
    assert_eq!(code, 0);
    let (code, _, _) = sandbox.run(&["connect", "claude-code", "--scope", "user", "--remove"]);
    assert_eq!(code, 0);
    assert_eq!(
        json(&sandbox.read(".claude.json")),
        serde_json::json!({}),
        "пустой контейнер убран"
    );
}

#[test]
fn project_scope_writes_next_to_the_repository() {
    let sandbox = Sandbox::new("project");
    let (code, stdout, _) = sandbox.run(&["connect", "claude-code"]);
    assert_eq!(code, 0, "{stdout}");
    let path = sandbox.project.join(".mcp.json");
    assert!(path.is_file(), "проектный конфиг создан рядом");
    let document = json(&fs::read_to_string(path).expect("read"));
    assert!(document["mcpServers"]["tasq"].is_object());
    // Пользовательский файл не тронут.
    assert!(!sandbox.home.join(".claude.json").exists());
}

#[test]
fn dry_run_changes_nothing() {
    let sandbox = Sandbox::new("dry");
    let (code, stdout, _) =
        sandbox.run(&["connect", "claude-code", "--scope", "user", "--dry-run"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("would write"), "{stdout}");
    assert!(
        !sandbox.home.join(".claude.json").exists(),
        "файл не создан"
    );
}

#[test]
fn check_reports_the_real_state() {
    let sandbox = Sandbox::new("check");
    let (code, stdout, _) = sandbox.run(&["connect", "claude-code", "--scope", "user", "--check"]);
    assert_eq!(code, 1, "не прописан — код 1:\n{stdout}");
    assert!(stdout.contains("missing"), "{stdout}");

    let (code, _, _) = sandbox.run(&["connect", "claude-code", "--scope", "user"]);
    assert_eq!(code, 0);
    let (code, stdout, _) = sandbox.run(&["connect", "claude-code", "--scope", "user", "--check"]);
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("is configured"), "{stdout}");
}

#[test]
fn non_loopback_http_is_refused() {
    let sandbox = Sandbox::new("http");
    let (code, _, stderr) =
        sandbox.run(&["connect", "claude-code", "--url", "http://0.0.0.0:7777/mcp"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("loopback"), "{stderr}");

    let (code, _, _) = sandbox.run(&[
        "connect",
        "claude-code",
        "--scope",
        "user",
        "--url",
        "http://127.0.0.1:7777/mcp",
    ]);
    assert_eq!(code, 0);
    let document = json(&sandbox.read(".claude.json"));
    assert_eq!(
        document["mcpServers"]["tasq"]["url"],
        serde_json::json!("http://127.0.0.1:7777/mcp")
    );
}

#[test]
fn unknown_client_is_a_clear_error() {
    let sandbox = Sandbox::new("unknown");
    let (code, _, stderr) = sandbox.run(&["connect", "editor-of-the-future"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("INVALID_VALUE"), "{stderr}");
    assert!(
        stderr.contains("claude-code"),
        "перечислены известные клиенты:\n{stderr}"
    );
}

#[test]
fn a_client_without_a_project_config_refuses_project_scope() {
    let sandbox = Sandbox::new("scopeless");
    let (code, _, stderr) = sandbox.run(&["connect", "gemini", "--scope", "project"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("no project config") || stderr.contains("INVALID_VALUE"),
        "{stderr}"
    );
}

#[test]
fn connect_works_without_a_database() {
    let sandbox = Sandbox::new("nodb");
    assert!(!sandbox.project.join(".tasq").exists());
    let (code, stdout, _) = sandbox.run(&["connect", "claude-code"]);
    assert_eq!(code, 0, "{stdout}");
    assert!(!sandbox.project.join(".tasq").exists(), "база не создаётся");
}

#[test]
fn unreadable_parent_is_reported() {
    let sandbox = Sandbox::new("dirs");
    // Каталог на месте файла: создать конфиг невозможно.
    fs::write(sandbox.home.join(".config"), "not a directory").expect("write");
    let (code, _, stderr) = sandbox.run(&["connect", "claude-desktop"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        !stderr.is_empty(),
        "ошибка должна быть внятной, а не молчаливым нулём"
    );
}

#[test]
fn every_client_can_be_proposed() {
    let sandbox = Sandbox::new("all");
    for client in [
        "claude-code",
        "claude-desktop",
        "opencode",
        "zed",
        "cursor",
        "vscode",
        "gemini",
        "codex",
        "goose",
        "crush",
        "windsurf",
    ] {
        let (code, stdout, stderr) = sandbox.run(&["connect", client, "--dry-run"]);
        assert!(code == 0, "{client}: код {code}\n{stdout}\n{stderr}");
        assert!(
            stdout.contains("would write") || stdout.contains("would remove"),
            "{client}: {stdout}"
        );
    }
}
