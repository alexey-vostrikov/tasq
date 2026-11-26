//! `tasq history REF` и MCP `tasq_history`: история статусов на чтение.
//!
//! Проверяется не «команда отвечает», а что она читает ровно тот факт, который записали разные
//! пути: ручной переход, взятие в работу, завершение через аренду. Журнал мутаций для этого не
//! годится — он отвечает «что менялось», а вопрос здесь «когда и почему узел стал done».

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tasq")
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-hist-cli-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
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

fn claim_token(dir: &std::path::Path, reference: &str, holder: &str) -> String {
    let (code, stdout, stderr) = run(dir, &["claim", reference, "--holder", holder, "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let lease: Value = serde_json::from_str(&stdout).expect("json");
    lease["token"].as_str().expect("token").to_string()
}

/// Узел, прошедший двумя путями: ручной перевод и аренда до `done`.
fn seeded(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);
    assert_eq!(run(&dir, &["add", "Задача"]).0, 0);
    // Один ручной перевод и одна аренда: в истории обязаны оказаться обе причины.
    // `todo` эпику запрещён (у него `planned`), поэтому ручной переход — в `backlog`.
    assert_eq!(run(&dir, &["set", "TASQ-1", "--status", "backlog"]).0, 0);
    let token = claim_token(&dir, "TASQ-1", "agent-a");
    assert_eq!(
        run(&dir, &["release", &token, "--outcome", "completed"]).0,
        0
    );
    dir
}

#[test]
fn history_names_every_transition_and_its_cause() {
    let dir = seeded("json");
    let (code, stdout, stderr) = run(&dir, &["history", "TASQ-1", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let document: Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(document["dot_id"], "TASQ-1");
    assert_eq!(document["status"], "done");
    let rows = document["transitions"].as_array().expect("transitions");
    assert!(rows.len() >= 3, "мало строк: {rows:?}");

    // Первый факт — создание: у него нет предыдущего статуса, и это единственная такая строка.
    assert_eq!(rows[0]["cause"], "created");
    assert!(rows[0]["from"].is_null());
    assert_eq!(rows[0]["to"], "idea");

    // Ручные переводы и закрытие через аренду — разные причины, и обе названы словами.
    let causes: Vec<&str> = rows
        .iter()
        .map(|row| row["cause"].as_str().expect("cause"))
        .collect();
    assert!(causes.contains(&"manual"), "{causes:?}");
    assert!(causes.contains(&"lease_claimed"), "{causes:?}");
    assert!(causes.contains(&"lease_completed"), "{causes:?}");
    let last = rows.last().expect("last");
    assert_eq!(last["to"], "done");
    assert_eq!(last["reason"], "закрыт через аренду");
    assert_eq!(last["actor"], "agent-a");

    // Порядок — по возрастанию времени: история читается сверху вниз, как она происходила.
    let times: Vec<&str> = rows
        .iter()
        .map(|row| row["at"].as_str().expect("at"))
        .collect();
    let mut sorted = times.clone();
    sorted.sort_unstable();
    assert_eq!(times, sorted, "история не по порядку");
}

#[test]
fn text_output_is_readable_and_says_who_and_why() {
    let dir = seeded("text");
    let (code, stdout, stderr) = run(&dir, &["history", "TASQ-1"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("TASQ-1"), "{stdout}");
    assert!(stdout.contains("закрыт через аренду"), "{stdout}");
    assert!(stdout.contains("agent-a"), "{stdout}");
    // Машинный код причины живёт в `--json`: в тексте он был бы шумом для человека, а
    // разбирать текст глазами агент не должен — для этого есть JSON.
    let (code, json_out, stderr) = run(&dir, &["history", "TASQ-1", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(json_out.contains("lease_completed"), "{json_out}");
}

/// Узла нет — отказ домена, а не пустая история: «нечего показать» и «узла нет» — разные ответы.
#[test]
fn unknown_reference_is_refused() {
    let dir = seeded("missing");
    // Отказ в режиме `--json` печатается в stdout как документ ошибки: так его разбирает
    // клиент, и так же ведут себя остальные команды.
    let (code, stdout, stderr) = run(&dir, &["history", "TASQ-99", "--json"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stdout.contains("NODE_NOT_FOUND"), "{stdout}{stderr}");
}

/// Тот же вопрос через MCP: инструмент есть в профиле агента и читает то же самое.
#[test]
fn mcp_history_answers_the_same_question() {
    let dir = seeded("mcp");

    let mut child = Command::new(bin())
        .current_dir(&dir)
        .args(["mcp", "--profile", "agent"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcp");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for request in [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                   "params": {"name": "tasq_history", "arguments": {"ref": "TASQ-1"}}}),
        ] {
            writeln!(stdin, "{request}").expect("write");
        }
    }
    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let response: Value =
        serde_json::from_str(stdout.lines().nth(1).expect("response")).expect("json");
    assert!(
        response["result"]["isError"].as_bool() != Some(true),
        "вызов не должен быть ошибкой: {response}"
    );
    let body = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("нет текста: {response}"));
    let document: Value = serde_json::from_str(body).expect("json body");
    let rows = document["transitions"].as_array().expect("transitions");
    assert!(
        rows.iter().any(|row| row["cause"] == "lease_completed"),
        "{body}"
    );
    assert_eq!(document["dot_id"], "TASQ-1");
}
