//! Проверка аргументов MCP: каждый инструмент отвечает кодом, а не паникой.
//!
//! Клиент посылает то, что посылает: пустой объект, строку вместо числа, массив вместо
//! ресурса. Сервер обязан вернуть `isError` с кодом и остаться живым — иначе агент видит
//! оборванное соединение и не понимает, что исправить в вызове.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
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
    let dir = std::env::temp_dir().join(format!("tasq-args-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn init(dir: &Path) {
    let out = Command::new(bin())
        .current_dir(dir)
        .args(["init", "--key", "TASQ"])
        .output()
        .expect("init");
    assert!(out.status.success(), "init: {out:?}");
}

fn initialize() -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
           "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                      "clientInfo": {"name": "args", "version": "1"}}})
}

fn call(id: i64, name: &str, arguments: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
           "params": {"name": name, "arguments": arguments}})
}

fn session(dir: &Path, requests: &[Value]) -> Vec<Value> {
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|err| panic!("bad line {line}: {err}"))
        })
        .collect()
}

/// Тело ответа инструмента: `content[0].text` как JSON-документ.
fn body(response: &Value) -> Value {
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("нет содержимого: {response}"));
    serde_json::from_str(text).unwrap_or_else(|err| panic!("не JSON ({err}): {text}"))
}

fn is_error(response: &Value) -> bool {
    response["result"]["isError"] == Value::Bool(true)
}

/// Каждый из инструментов вызывается с пустым набором аргументов. Ответ обязан быть:
/// либо результат, либо `isError` с кодом. Ни паники, ни обрыва соединения.
#[test]
fn empty_arguments_never_break_the_server() {
    let dir = temp_dir("empty");
    init(&dir);

    let mut requests = vec![initialize()];
    for (index, name) in tasq::mcp::profile::ALL_TOOLS.iter().enumerate() {
        requests.push(call(index as i64 + 10, name, json!({})));
    }
    // Последний вызов — заведомо рабочий: он доказывает, что сервер жив после всех отказов.
    requests.push(call(999, "tasq_dashboard", json!({})));

    let responses = session(&dir, &requests);
    assert_eq!(
        responses.len(),
        requests.len(),
        "на каждый запрос по ответу: {responses:?}"
    );

    for response in &responses[1..] {
        let result = &response["result"];
        assert!(
            result.is_object(),
            "инструмент не ответил результатом: {response}"
        );
        if is_error(response) {
            let code = body(response)["code"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            assert!(!code.is_empty(), "отказ без кода: {response}");
            assert_ne!(code, "STORAGE_ERROR", "отказ ушёл в хранилище: {response}");
        }
    }

    // Рабочий вызов в конце прошёл.
    let last = responses.last().expect("последний ответ");
    assert!(!is_error(last), "сервер не выжил: {last}");
}

/// Тип аргумента проверяется, а не приводится молча.
#[test]
fn wrong_argument_types_are_refused() {
    let dir = temp_dir("types");
    init(&dir);

    let responses = session(
        &dir,
        &[
            initialize(),
            call(
                10,
                "node_create",
                json!({ "title": "Задача", "weight": "много" }),
            ),
            call(
                11,
                "node_create",
                json!({ "title": "Задача", "estimated_hours": "полдня" }),
            ),
            call(12, "node_create", json!({ "title": 42 })),
            call(
                13,
                "tasq_claim",
                json!({ "ref": "TASQ-1", "ttl": "быстро" }),
            ),
            call(14, "node_list", json!({ "limit": "все" })),
            call(
                15,
                "tasq_reserve",
                json!({ "token": "x", "resources": "src/api" }),
            ),
            call(16, "tasq_burnup", json!({ "metric": "очки" })),
            call(
                17,
                "tasq_release",
                json!({ "token": "x", "outcome": "как-нибудь" }),
            ),
            call(
                18,
                "tasq_link",
                json!({ "from": "TASQ-1", "to": "TASQ-2", "kind": "мешает" }),
            ),
            call(
                19,
                "tasq_import",
                json!({ "document": {}, "mode": "влить" }),
            ),
            call(
                20,
                "node_update",
                json!({ "ref": "TASQ-1", "description_locked": "может быть" }),
            ),
        ],
    );

    assert_eq!(responses.len(), 12, "{responses:?}");

    // Первые два: строку вместо числа сервер не принимает.
    for response in &responses[1..] {
        assert!(is_error(response), "ожидался отказ: {response}");
        let code = body(response)["code"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            matches!(
                code.as_str(),
                "INVALID_VALUE" | "NODE_NOT_FOUND" | "NOT_YOUR_ATTEMPT" | "INVALID_REF"
            ),
            "неожиданный код {code}: {response}"
        );
    }
}

/// Неизвестное имя инструмента — отдельный код, а не общая ошибка разбора.
#[test]
fn unknown_tool_has_its_own_code() {
    let dir = temp_dir("unknown");
    init(&dir);

    let responses = session(
        &dir,
        &[
            initialize(),
            call(10, "tasq_нет_такого", json!({})),
            call(11, "tasq_dashboard", json!({})),
        ],
    );
    assert_eq!(responses.len(), 3, "{responses:?}");
    assert!(is_error(&responses[1]));
    // Код приходит из диспетчера RPC: имя инструмента не из каталога.
    assert_eq!(body(&responses[1])["code"], Value::from("UNKNOWN_TOOL"));
    assert!(
        !is_error(&responses[2]),
        "сервер жив после неизвестного имени"
    );
}

/// Ресурсы: логическое имя (`port:8080`) держится и освобождается тем же вызовом.
#[test]
fn logical_resources_are_reserved_and_released() {
    let dir = temp_dir("resources");
    init(&dir);

    let responses = session(
        &dir,
        &[
            initialize(),
            call(10, "node_create", json!({ "title": "Эпик" })),
            call(
                11,
                "tasq_claim",
                json!({ "ref": "TASQ-1", "holder": "agent-a" }),
            ),
        ],
    );
    assert_eq!(responses.len(), 3, "{responses:?}");
    let token = body(&responses[2])["token"]
        .as_str()
        .expect("токен")
        .to_string();

    let responses = session(
        &dir,
        &[
            initialize(),
            call(
                10,
                "tasq_reserve",
                json!({ "token": token, "reserve": ["port:8080"] }),
            ),
            call(11, "tasq_holdings", json!({})),
            call(
                12,
                "tasq_reserve",
                json!({ "token": token, "remove": ["port:8080"] }),
            ),
            call(13, "tasq_holdings", json!({})),
        ],
    );
    assert_eq!(responses.len(), 5, "{responses:?}");

    let held = body(&responses[1]);
    assert!(
        serde_json::to_string(&held)
            .expect("json")
            .contains("port:8080"),
        "логический ресурс в списке: {held}"
    );

    let released = body(&responses[3]);
    assert_eq!(released["released"], Value::from(1), "{released}");

    let empty = body(&responses[4]);
    assert!(
        !serde_json::to_string(&empty)
            .expect("json")
            .contains("port:8080"),
        "ресурс освобождён: {empty}"
    );
}

/// `remove` обязан быть массивом: одна строка — отказ, а не «удалить всё».
#[test]
fn remove_must_be_an_array() {
    let dir = temp_dir("remove");
    init(&dir);

    let responses = session(
        &dir,
        &[
            initialize(),
            call(10, "node_create", json!({ "title": "Эпик" })),
            call(
                11,
                "tasq_reserve",
                json!({ "token": "x", "remove": "port:8080" }),
            ),
        ],
    );
    assert_eq!(responses.len(), 3, "{responses:?}");
    assert!(is_error(&responses[2]), "{:?}", responses[2]);
    assert_eq!(body(&responses[2])["code"], Value::from("INVALID_VALUE"));
}

/// Состояние описания принимает только `true` и `false` — строка «может быть» отбивается.
#[test]
fn description_lock_takes_only_booleans() {
    let dir = temp_dir("locked");
    init(&dir);

    let responses = session(
        &dir,
        &[
            initialize(),
            call(10, "node_create", json!({ "title": "Эпик" })),
            call(
                11,
                "node_update",
                json!({ "ref": "TASQ-1", "description_locked": "true" }),
            ),
            call(
                12,
                "node_update",
                json!({ "ref": "TASQ-1", "description_locked": "false" }),
            ),
            call(
                13,
                "node_update",
                json!({ "ref": "TASQ-1", "description_locked": "может быть" }),
            ),
        ],
    );
    assert_eq!(responses.len(), 5, "{responses:?}");
    assert!(
        !is_error(&responses[2]),
        "строка true допустима: {:?}",
        responses[2]
    );
    assert!(
        !is_error(&responses[3]),
        "строка false допустима: {:?}",
        responses[3]
    );
    assert!(
        is_error(&responses[4]),
        "третье значение — отказ: {:?}",
        responses[4]
    );
    assert_eq!(body(&responses[4])["code"], Value::from("INVALID_VALUE"));
}
