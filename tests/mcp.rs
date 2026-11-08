//! MCP-сервер: рукопожатие, каталог по профилям, вызовы через stdin/stdout настоящего бинаря.

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
    let dir = std::env::temp_dir().join(format!("tasq-mcp-{tag}-{nanos}"));
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

/// Один сеанс: отправляет строки, возвращает ответы (по одному на строку с `id`).
fn session(dir: &std::path::Path, profile: &str, requests: &[Value]) -> Vec<Value> {
    let mut child = Command::new(bin())
        .current_dir(dir)
        .args(["mcp", "--profile", profile])
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|err| panic!("bad line {line}: {err}"))
        })
        .collect()
}

fn initialize() -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
           "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                      "clientInfo": {"name": "test", "version": "1"}}})
}

#[test]
fn handshake_and_tools_list_over_stdio() {
    let dir = temp_dir("handshake");
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);

    let responses = session(
        &dir,
        "full",
        &[
            initialize(),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}),
        ],
    );

    // уведомление без id ответа не получает: три ответа на четыре сообщения
    assert_eq!(responses.len(), 3, "{responses:?}");

    let init = &responses[0];
    assert_eq!(init["result"]["protocolVersion"], json!("2024-11-05"));
    assert_eq!(init["result"]["serverInfo"]["name"], json!("tasq"));
    assert_eq!(init["result"]["serverInfo"]["profile"], json!("full"));

    let list = &responses[1];
    let tools = list["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 44, "полный профиль отдаёт весь каталог");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"tasq_claim"), "{names:?}");
    assert!(names.contains(&"node_create"));
    // у каждого инструмента есть схема и аннотации
    for tool in tools {
        assert!(tool["inputSchema"]["type"] == json!("object"), "{tool}");
        assert!(tool["annotations"]["readOnlyHint"].is_boolean(), "{tool}");
    }
    assert_eq!(responses[2]["result"], json!({}));
}

#[test]
fn profile_narrows_the_catalogue_and_blocks_calls() {
    let dir = temp_dir("profiles");
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);
    assert_eq!(run(&dir, &["add", "Эпик"]).0, 0);

    // read-only: каталог уже, мутирующий вызов отвергнут
    let responses = session(
        &dir,
        "read-only",
        &[
            initialize(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                   "params": {"name": "node_create", "arguments": {"title": "Нельзя"}}}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
                   "params": {"name": "node_list", "arguments": {}}}),
        ],
    );

    let tools = responses[1]["result"]["tools"]
        .as_array()
        .expect("tools array");
    assert!(
        tools.len() < 20,
        "read-only заметно уже полного: {}",
        tools.len()
    );
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(!names.contains(&"node_create"), "{names:?}");
    assert!(names.contains(&"node_list"));

    let refused = &responses[2];
    assert_eq!(refused["result"]["isError"], json!(true), "{refused}");
    let payload: Value = serde_json::from_str(
        refused["result"]["content"][0]["text"]
            .as_str()
            .expect("text"),
    )
    .expect("json");
    assert_eq!(payload["code"], json!("TOOL_NOT_IN_PROFILE"));

    let allowed = &responses[3];
    assert_eq!(allowed["result"]["isError"], json!(false), "{allowed}");
    let nodes: Value = serde_json::from_str(
        allowed["result"]["content"][0]["text"]
            .as_str()
            .expect("text"),
    )
    .expect("json");
    assert_eq!(nodes[0]["title"], json!("Эпик"));
}

#[test]
fn agent_can_take_and_release_a_lease_over_mcp() {
    let dir = temp_dir("workflow");
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);
    assert_eq!(run(&dir, &["add", "Эпик"]).0, 0);
    assert_eq!(run(&dir, &["add", "Задача", "--parent", "TASQ-1"]).0, 0);

    // Один сеанс — захват и освобождение: token живёт в ответе, а не в файле.
    let responses = session(
        &dir,
        "agent",
        &[
            initialize(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                   "params": {"name": "tasq_claim", "arguments": {
                       "ref": "TASQ-1.1", "holder": "claude-code", "ttl": 900,
                       "reserve": ["path:src/api"]}}}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                   "params": {"name": "tasq_claim", "arguments": {
                       "ref": "TASQ-1.1", "holder": "codex"}}}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
                   "params": {"name": "tasq_holdings", "arguments": {}}}),
            json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
                   "params": {"name": "tasq_context", "arguments": {"ref": "TASQ-1.1"}}}),
        ],
    );

    let claimed: Value = serde_json::from_str(
        responses[1]["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .expect("json");
    assert_eq!(claimed["isError"], Value::Null, "успех без isError");
    assert_eq!(claimed["holder"], json!("claude-code"));
    assert_eq!(claimed["attempt_no"], json!(1));
    let token = claimed["token"].as_str().expect("token").to_string();
    assert_eq!(token.len(), 64);

    // второй агент получает отказ с держателем
    let refused: Value = serde_json::from_str(
        responses[2]["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .expect("json");
    assert_eq!(refused["code"], json!("ACTIVE_ATTEMPT_EXISTS"), "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .unwrap_or_default()
            .contains("claude-code"),
        "{refused}"
    );

    let holdings: Value = serde_json::from_str(
        responses[3]["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .expect("json");
    assert_eq!(holdings[0]["holder"], json!("claude-code"));
    assert!(
        holdings[0]["reservations"]
            .as_array()
            .map(|list| !list.is_empty())
            .unwrap_or(false),
        "резерв виден: {holdings}"
    );

    let chain: Value = serde_json::from_str(
        responses[4]["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .expect("json");
    let dots: Vec<&str> = chain
        .as_array()
        .expect("chain")
        .iter()
        .filter_map(|entry| entry["dot_id"].as_str())
        .collect();
    assert_eq!(dots, vec!["TASQ-1", "TASQ-1.1"]);

    // освобождаем тем же токеном во втором сеансе
    let responses = session(
        &dir,
        "agent",
        &[
            initialize(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                   "params": {"name": "tasq_release", "arguments": {
                       "token": token, "outcome": "completed"}}}),
        ],
    );
    let released: Value = serde_json::from_str(
        responses[1]["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .expect("json");
    assert_eq!(released["status"], json!("done"));

    let (_, stdout, _) = run(&dir, &["ls"]);
    assert!(stdout.contains("done"), "{stdout}");
}

#[test]
fn errors_come_back_as_content_with_a_code() {
    let dir = temp_dir("errors");
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);

    let responses = session(
        &dir,
        "full",
        &[
            initialize(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                   "params": {"name": "node_get", "arguments": {"ref": "TASQ-99"}}}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                   "params": {"name": "nope_tool", "arguments": {}}}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "unknown/method"}),
        ],
    );

    let missing = &responses[1]["result"];
    assert_eq!(missing["isError"], json!(true));
    let payload: Value =
        serde_json::from_str(missing["content"][0]["text"].as_str().unwrap()).expect("json");
    assert_eq!(payload["code"], json!("NODE_NOT_FOUND"));
    assert!(payload["hint"].as_str().unwrap_or_default().len() > 5);

    let unknown = &responses[2]["result"];
    assert_eq!(unknown["isError"], json!(true));
    let payload: Value =
        serde_json::from_str(unknown["content"][0]["text"].as_str().unwrap()).expect("json");
    assert_eq!(payload["code"], json!("UNKNOWN_TOOL"));

    // неизвестный метод — ошибка протокола, а не content
    assert_eq!(responses[3]["error"]["code"], json!(-32601));
}

#[test]
fn dashboard_and_search_answer_in_one_call() {
    let dir = temp_dir("overview");
    assert_eq!(run(&dir, &["init", "--key", "TASQ"]).0, 0);
    assert_eq!(run(&dir, &["add", "Эпик про миграции"]).0, 0);
    assert_eq!(
        run(
            &dir,
            &[
                "note",
                "save",
                "--title",
                "Решение",
                "--content",
                "WAL",
                "--type",
                "decision"
            ]
        )
        .0,
        0
    );

    let responses = session(
        &dir,
        "agent",
        &[
            initialize(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                   "params": {"name": "tasq_dashboard", "arguments": {}}}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                   "params": {"name": "tasq_search", "arguments": {"query": "миграции"}}}),
        ],
    );

    let dashboard: Value = serde_json::from_str(
        responses[1]["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .expect("json");
    assert_eq!(dashboard["project"], json!("TASQ"));
    assert!(dashboard["board"]["columns"].is_array());
    assert!(dashboard["leases"].is_array());
    assert!(dashboard["next"].is_array());

    let hits: Value = serde_json::from_str(
        responses[2]["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .expect("json");
    assert!(
        hits.as_array()
            .map(|list| !list.is_empty())
            .unwrap_or(false),
        "поиск нашёл узел: {hits}"
    );
}

#[test]
fn migration_profile_moves_data_and_hides_work_tools() {
    let dir = temp_dir("migration");
    assert_eq!(run(&dir, &["init", "--key", "SRC"]).0, 0);
    assert_eq!(run(&dir, &["add", "Эпик"]).0, 0);
    assert_eq!(run(&dir, &["export", "--out", "backup.json"]).0, 0);

    let target = temp_dir("migration-target");
    assert_eq!(run(&target, &["init", "--key", "DST"]).0, 0);

    let document: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("backup.json")).expect("file"))
            .expect("json");

    let responses = session(
        &target,
        "migration",
        &[
            initialize(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                   "params": {"name": "tasq_import", "arguments": {
                       "document": document, "mode": "replace"}}}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                   "params": {"name": "tasq_claim", "arguments": {"ref": "DST-1"}}}),
        ],
    );

    let imported: Value = serde_json::from_str(
        responses[1]["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .expect("json");
    assert_eq!(imported["nodes_created"], json!(1), "{imported}");
    assert_eq!(imported["checksum_verified"], json!(true));

    let refused = &responses[2]["result"];
    assert_eq!(refused["isError"], json!(true));
    let payload: Value =
        serde_json::from_str(refused["content"][0]["text"].as_str().unwrap()).expect("json");
    assert_eq!(payload["code"], json!("TOOL_NOT_IN_PROFILE"));

    let (_, stdout, _) = run(&target, &["ls"]);
    assert!(stdout.contains("Эпик"), "{stdout}");
}

/// Сервер без базы обязан подняться: клиент получает подключение и каталог, а внятный отказ
/// приходит на вызов. Раньше сервер молча закрывал соединение, и клиент видел
/// «connection closed» без причины — так и не подключился MCP-клиент у агента.
#[test]
fn server_starts_without_a_database_and_can_create_one() {
    let dir = temp_dir("nodb");

    let responses = session(
        &dir,
        "agent",
        &[
            initialize(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                   "params": {"name": "node_list", "arguments": {}}}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
                   "params": {"name": "tasq_init", "arguments": {"key": "LIVE", "name": "Живой"}}}),
            json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
                   "params": {"name": "node_create", "arguments": {"title": "Эпик"}}}),
        ],
    );

    // рукопожатие и каталог работают без базы
    assert_eq!(responses[0]["result"]["serverInfo"]["name"], json!("tasq"));
    let tools = responses[1]["result"]["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 29, "профиль agent целиком, без базы");

    // вызов без базы — доменный отказ с кодом и подсказкой, а не закрытие соединения
    let refused = &responses[2]["result"];
    assert_eq!(refused["isError"], json!(true), "{refused}");
    let payload: Value =
        serde_json::from_str(refused["content"][0]["text"].as_str().expect("text")).expect("json");
    assert_eq!(payload["code"], json!("DB_NOT_FOUND"));
    assert!(
        payload["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("tasq_init"),
        "подсказка называет способ создать базу: {payload}"
    );

    // tasq_init создаёт базу прямо в сессии, и дальше инструменты работают
    let created = &responses[3]["result"];
    assert_eq!(created["isError"], json!(false), "{created}");
    let init: Value =
        serde_json::from_str(created["content"][0]["text"].as_str().unwrap()).expect("json");
    assert_eq!(init["project_key"], json!("LIVE"));

    let node = &responses[4]["result"];
    assert_eq!(node["isError"], json!(false), "{node}");
    let node: Value =
        serde_json::from_str(node["content"][0]["text"].as_str().unwrap()).expect("json");
    assert_eq!(node["dot_id"], json!("LIVE-1"));

    // и база осталась на диске после выхода
    let (code, stdout, _) = run(&dir, &["ls"]);
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("LIVE-1"), "{stdout}");
}

/// `TASQ_DB` задаёт базу без флага: так её прописывают в конфиге MCP-клиента.
#[test]
fn tasq_db_environment_variable_selects_the_database() {
    let project = temp_dir("envdb");
    assert_eq!(run(&project, &["init", "--key", "ENV"]).0, 0);
    assert_eq!(run(&project, &["add", "Эпик"]).0, 0);

    // Работаем из пустого каталога: без переменной базы тут нет.
    let elsewhere = temp_dir("envdb-elsewhere");
    let mut child = Command::new(bin())
        .current_dir(&elsewhere)
        .env("TASQ_DB", project.join(".tasq/tasq.db"))
        .args(["ls"])
        .output()
        .expect("run tasq");
    assert!(
        child.status.success(),
        "TASQ_DB не подхватился: {}",
        String::from_utf8_lossy(&child.stderr)
    );
    let stdout = String::from_utf8_lossy(&child.stdout);
    assert!(stdout.contains("ENV-1"), "{stdout}");

    // И сервер тоже видит базу через переменную.
    child = Command::new(bin())
        .current_dir(&elsewhere)
        .env("TASQ_DB", project.join(".tasq/tasq.db"))
        .args(["mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            {
                let stdin = child.stdin.as_mut().expect("stdin");
                writeln!(stdin, "{}", initialize())?;
                writeln!(
                    stdin,
                    "{}",
                    json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                                             "params": {"name": "node_list", "arguments": {}}})
                )?;
            }
            child.wait_with_output()
        })
        .expect("run mcp");

    let stdout = String::from_utf8_lossy(&child.stdout);
    let response: Value =
        serde_json::from_str(stdout.lines().nth(1).expect("call response")).expect("json");
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
}
