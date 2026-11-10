//! Ссылки самодостаточны: `DEV-1.1` находит узел проекта DEV из любой сессии.
//!
//! Адрес узла начинается с ключа его проекта, поэтому «работаю в ALPHA» не должно мешать
//! прочитать DEV-1.1 — иначе обзор по всем проектам (`ls` по умолчанию) выдаёт адреса,
//! которые нельзя открыть. Проверяется наблюдаемое поведение: что вернула команда и какой
//! код ошибки у несуществующего ключа.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::json;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tasq")
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-xref-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn run(dir: &std::path::Path, env: &[(&str, &str)], args: &[&str]) -> (i32, String, String) {
    let mut command = Command::new(bin());
    command.current_dir(dir).args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let out = command.output().expect("run tasq");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// Два проекта: ALPHA с одной задачей и DEV с одной задачей.
fn seeded(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    assert_eq!(run(&dir, &[], &["init", "--key", "ALPHA"]).0, 0);
    assert_eq!(run(&dir, &[], &["add", "Эпик ALPHA"]).0, 0);
    assert_eq!(
        run(&dir, &[], &["add", "Задача ALPHA", "--parent", "ALPHA-1"]).0,
        0
    );
    assert_eq!(run(&dir, &[], &["create", "project", "DEV"]).0, 0);
    assert_eq!(
        run(&dir, &[], &["--project", "DEV", "add", "Эпик DEV"]).0,
        0
    );
    assert_eq!(
        run(
            &dir,
            &[],
            &["--project", "DEV", "add", "Задача DEV", "--parent", "DEV-1"]
        )
        .0,
        0
    );
    dir
}

/// Сессия в ALPHA читает узел DEV по ссылке с ключом.
#[test]
fn a_reference_with_a_key_reaches_another_project() {
    let dir = seeded("read");
    let env = [("TASQ_PROJECT", "ALPHA")];

    let (code, stdout, stderr) = run(&dir, &env, &["show", "DEV-1.1"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("Задача DEV"),
        "прочитан не тот узел:\n{stdout}"
    );
    assert!(stdout.contains("DEV-1.1"), "{stdout}");

    // Свой узел по-прежнему читается.
    let (code, stdout, _) = run(&dir, &env, &["show", "ALPHA-1.1"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("Задача ALPHA"), "{stdout}");
}

/// Селектора нет вовсе — ссылка всё равно самодостаточна: проект берётся из её ключа.
/// Без этого обзор `ls` выдавал бы адреса, которые нельзя открыть без предварительного
/// выбора проекта.
#[test]
fn a_reference_with_a_key_needs_no_selector() {
    let dir = seeded("noselector");

    let (code, stdout, stderr) = run(&dir, &[], &["show", "DEV-1"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("Эпик DEV"), "{stdout}");
}

/// Ссылка без ключа в базе с несколькими проектами — по-прежнему отказ: выбрать за
/// пользователя нечего.
#[test]
fn a_reference_without_a_key_still_needs_a_project() {
    let dir = seeded("nokey");

    let (code, stdout, stderr) = run(&dir, &[], &["show", "1.1"]);
    assert_eq!(code, 1, "stdout: {stdout}");
    assert!(stderr.contains("PROJECT_REQUIRED"), "{stderr}");
}

/// Неизвестный ключ — `PROJECT_NOT_FOUND` со списком, а не «битая ссылка»: пользователь
/// опечатался в проекте, и подсказка должна называть существующие.
#[test]
fn an_unknown_key_is_reported_as_a_missing_project() {
    let dir = seeded("unknown");
    let (code, stdout, stderr) = run(&dir, &[], &["show", "NOPE-1"]);
    assert_eq!(code, 1, "stdout: {stdout}");
    assert!(stderr.contains("PROJECT_NOT_FOUND"), "{stderr}");
    assert!(
        stderr.contains("ALPHA") && stderr.contains("DEV"),
        "отказ не перечисляет ключи: {stderr}"
    );
}

/// Запись по ссылке тоже работает: правка уходит в проект из ключа, а не в сессию.
#[test]
fn a_write_through_a_keyed_reference_lands_in_its_project() {
    let dir = seeded("write");
    let env = [("TASQ_PROJECT", "ALPHA")];

    let (code, _, stderr) = run(&dir, &env, &["set", "DEV-1.1", "--priority", "critical"]);
    assert_eq!(code, 0, "{stderr}");

    // Изменился узел DEV, а узел ALPHA остался прежним.
    let (_, dev, _) = run(&dir, &[], &["--project", "DEV", "show", "DEV-1.1"]);
    assert!(dev.contains("critical"), "правка не дошла: {dev}");
    let (_, alpha, _) = run(&dir, &[], &["--project", "ALPHA", "show", "ALPHA-1.1"]);
    assert!(
        !alpha.contains("critical"),
        "правка ушла чужому узлу: {alpha}"
    );
}

/// MCP: сессия в ALPHA читает узел DEV по ссылке с ключом, а сессия без проекта — тоже.
#[test]
fn mcp_resolves_keyed_references_in_both_session_states() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = seeded("mcp");

    let session = |project: Option<&str>, ref_key: &str| -> serde_json::Value {
        let mut command = Command::new(bin());
        command
            .current_dir(&dir)
            .args(["mcp", "--profile", "full"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(project) = project {
            command.env("TASQ_PROJECT", project);
        } else {
            command.env_remove("TASQ_PROJECT");
        }
        let mut child = command.spawn().expect("spawn mcp");
        {
            let stdin = child.stdin.as_mut().expect("stdin");
            for message in [
                json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                       "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                                  "clientInfo": {"name": "xref", "version": "1"}}}),
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                       "params": {"name": "node_get", "arguments": {"ref": ref_key}}}),
            ] {
                writeln!(stdin, "{message}").expect("write");
            }
        }
        let out = child.wait_with_output().expect("wait");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let response: serde_json::Value = stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("json"))
            .find(|value: &serde_json::Value| value["id"] == json!(1))
            .expect("ответ на вызов");
        let body = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        serde_json::from_str(&body).unwrap_or(json!({ "raw": body }))
    };

    // Сессия в ALPHA: ссылка уводит в DEV.
    let from_alpha = session(Some("ALPHA"), "DEV-1.1");
    assert_eq!(
        from_alpha["node"]["title"],
        json!("Задача DEV"),
        "сессия в ALPHA не прочитала DEV: {from_alpha}"
    );

    // Сессии проекта нет: ссылка всё равно разрешается.
    let no_project = session(None, "ALPHA-1");
    assert_eq!(
        no_project["node"]["title"],
        json!("Эпик ALPHA"),
        "без проекта ссылка с ключом не разрешилась: {no_project}"
    );
}
