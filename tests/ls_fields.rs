//! `ls --fields` / `--offset` и то же в MCP `node_list`.
//!
//! Проверяется не флаг, а ответ: список из 151 узла на живой базе весит 292 КБ, из которых
//! 197 КБ — описания. Если поля не выбираются, агент платит за них контекстом ради адреса и
//! статуса; если выбираются молча неверно — платит доверием к ответу.

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
    let dir = std::env::temp_dir().join(format!("tasq-fields-{tag}-{nanos}"));
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

/// Эпик с тремя детьми, у каждого — описание на пару килобайт: именно оно и есть предмет выбора
/// полей, поэтому фикстура обязана его содержать.
fn seeded(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);
    assert_eq!(run(&dir, &["add", "Эпик"]).0, 0);
    let long = "описание ".repeat(200);
    for title in ["Первая", "Вторая", "Третья"] {
        assert_eq!(
            run(&dir, &["add", title, "--parent", "TASQ-1", "--desc", &long]).0,
            0
        );
    }
    dir
}

fn session(dir: &std::path::Path, requests: &[Value]) -> Vec<Value> {
    let mut child = Command::new(bin())
        .current_dir(dir)
        .args(["mcp", "--profile", "full"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcp");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for request in requests {
            writeln!(stdin, "{request}").expect("write request");
        }
    }
    let output = child.wait_with_output().expect("wait");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("json line"))
        .collect()
}

#[test]
fn fields_return_only_what_was_asked() {
    let dir = seeded("only");
    let (code, stdout, stderr) = run(&dir, &["ls", "--json", "--fields", "dot_id,title,status"]);
    assert_eq!(code, 0, "{stderr}");
    let rows: Vec<Value> = serde_json::from_str(&stdout).expect("json");
    assert_eq!(rows.len(), 4, "эпик и трое детей: {rows:?}");
    for row in &rows {
        let object = row.as_object().expect("object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["dot_id", "status", "title"],
            "лишние или пропавшие поля: {object:?}"
        );
    }
}

/// Порядок полей в ответе — канонический (`FIELDS`), а не порядок в аргументе: два вызова с теми
/// же полями обязаны дать одинаковый JSON, иначе сравнение ответов глазами превращается в
/// угадывание.
#[test]
fn field_order_does_not_depend_on_the_argument() {
    let dir = seeded("order");
    let (_, first, _) = run(&dir, &["ls", "--json", "--fields", "status,dot_id"]);
    let (_, second, _) = run(&dir, &["ls", "--json", "--fields", "dot_id,status"]);
    assert_eq!(first, second);
}

/// Главный замер задачи: выбор полей обязан снимать описание.
#[test]
fn descriptions_stop_being_paid_for() {
    let dir = seeded("size");
    let (_, full, _) = run(&dir, &["ls", "--json"]);
    let (_, compact, _) = run(
        &dir,
        &["ls", "--json", "--fields", "dot_id,title,status,priority"],
    );
    assert!(
        compact.len() * 5 < full.len(),
        "экономия меньше пяти раз: {} байт против {}",
        compact.len(),
        full.len()
    );
    assert!(full.contains("описание"), "фикстура обязана нести описание");
    assert!(
        !compact.contains("описание"),
        "в выбранных полях не должно быть описаний"
    );
}

#[test]
fn unknown_field_is_refused_with_the_list() {
    let dir = seeded("typo");
    let (code, stdout, stderr) = run(&dir, &["ls", "--json", "--fields", "dot_id,descr"]);
    assert_eq!(code, 1, "опечатка не должна молча давать пустой столбец");
    let text = format!("{stdout}{stderr}");
    assert!(text.contains("INVALID_VALUE"), "{text}");
    assert!(text.contains("descr"), "{text}");
    assert!(
        text.contains("description"),
        "в отказе нет списка допустимых: {text}"
    );
}

#[test]
fn offset_and_limit_page_without_gaps_or_repeats() {
    let dir = seeded("page");
    let (_, full, _) = run(&dir, &["ls", "--json", "--fields", "dot_id"]);
    let all: Vec<Value> = serde_json::from_str(&full).expect("json");
    assert_eq!(all.len(), 4);

    let mut paged = Vec::new();
    for offset in 0..4 {
        let (code, stdout, stderr) = run(
            &dir,
            &[
                "ls",
                "--json",
                "--fields",
                "dot_id",
                "--offset",
                &offset.to_string(),
                "--limit",
                "1",
            ],
        );
        assert_eq!(code, 0, "{stderr}");
        let rows: Vec<Value> = serde_json::from_str(&stdout).expect("json");
        assert_eq!(rows.len(), 1, "offset {offset} вернул {rows:?}");
        paged.push(rows[0].clone());
    }
    assert_eq!(paged, all, "страница за страницей не складывается в список");

    // Смещение за концом — пустой ответ, а не ошибка: так работает листание.
    let (code, stdout, stderr) = run(&dir, &["ls", "--json", "--offset", "10"]);
    assert_eq!(code, 0, "{stderr}");
    let rows: Vec<Value> = serde_json::from_str(&stdout).expect("json");
    assert!(rows.is_empty(), "{rows:?}");
}

/// Без новых флагов ответ обязан остаться прежним: список всех полей и в прежнем виде.
#[test]
fn the_default_answer_is_unchanged() {
    let dir = seeded("default");
    let (_, stdout, _) = run(&dir, &["ls", "--json"]);
    let rows: Vec<Value> = serde_json::from_str(&stdout).expect("json");
    let keys: Vec<&str> = rows[0]
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys.len(),
        26,
        "полей узла стало больше или меньше: {keys:?}"
    );
    assert!(keys.contains(&"description"));
    assert!(keys.contains(&"updated_at"));
}

/// Паритет с CLI: у MCP-инструмента те же фильтры и тот же выбор полей — иначе агент и человек
/// получают разные ответы на один вопрос.
#[test]
fn mcp_node_list_offers_the_same_fields_and_filters() {
    let dir = seeded("mcp");

    let responses = session(
        &dir,
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                              "clientInfo": {"name": "test", "version": "1"}}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                   "params": {"name": "node_list",
                              "arguments": {"fields": ["dot_id", "status"],
                                            "exclude_done": true,
                                            "offset": 1,
                                            "limit": 1}}}),
        ],
    );

    let call = responses
        .iter()
        .find(|response| response["id"] == 2)
        .expect("ответ на вызов");
    let body = call["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("нет текста: {call}"));
    let rows: Vec<Value> = serde_json::from_str(body).unwrap_or_else(|err| panic!("{body}: {err}"));
    assert_eq!(
        rows.len(),
        1,
        "offset+limit должны вернуть одну строку: {body}"
    );
    let keys: Vec<&str> = rows[0]
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, vec!["dot_id", "status"], "{body}");
    assert_eq!(
        rows[0]["dot_id"], "TASQ-1.1",
        "offset 1 — второй узел списка"
    );
}

/// Каталог `tools/list` обязан называть новые аргументы: агент узнаёт о них только оттуда.
#[test]
fn the_tool_schema_names_the_new_arguments() {
    let dir = seeded("schema");
    let responses = session(
        &dir,
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                              "clientInfo": {"name": "test", "version": "1"}}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        ],
    );
    let list = responses
        .iter()
        .find(|response| response["id"] == 2)
        .expect("ответ на tools/list");
    let tools = list["result"]["tools"].as_array().expect("tools");
    let node_list = tools
        .iter()
        .find(|tool| tool["name"] == "node_list")
        .expect("node_list в каталоге");
    let properties = node_list["inputSchema"]["properties"]
        .as_object()
        .expect("properties");
    for name in ["fields", "offset", "exclude_done", "exclude_projects"] {
        assert!(
            properties.contains_key(name),
            "в схеме нет {name}: {:?}",
            properties.keys().collect::<Vec<_>>()
        );
    }
}
