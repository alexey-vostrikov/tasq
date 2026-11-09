//! Необязательные аргументы инструментов MCP: то, что клиент передаёт реже, но что
//! меняет поведение — вид связи, переименование номеров, снятие архива, восстановление
//! комментария, снятие резерва вместе с добавлением, импорт saga, снимок прогноза.
//!
//! Один сеанс на весь сценарий: так проверяется и то, что состояние между вызовами
//! сохраняется — сервер не пересоздаёт проект на каждом запросе.

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
    let dir = std::env::temp_dir().join(format!("tasq-options-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn initialize() -> Value {
    json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
           "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                      "clientInfo": {"name": "options", "version": "1"}}})
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
            writeln!(stdin, "{request}").expect("write");
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

fn body(response: &Value) -> Value {
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("нет содержимого: {response}"));
    serde_json::from_str(text).unwrap_or_else(|err| panic!("не JSON ({err}): {text}"))
}

/// Вызов обязан пройти: индексы ниже считаются по порядку запросов.
fn check(response: &Value, label: &str) -> Value {
    assert!(
        response["result"]["isError"] != Value::Bool(true),
        "{label}: {response}"
    );
    body(response)
}

#[test]
fn optional_arguments_change_behaviour() {
    let dir = temp_dir("flow");
    let init = Command::new(bin())
        .current_dir(&dir)
        .args(["init", "--key", "TASQ"])
        .output()
        .expect("init");
    assert!(init.status.success());

    let requests = vec![
        initialize(),
        // 1..3: дерево
        call(1, "node_create", json!({ "title": "Эпик" })),
        call(
            2,
            "node_create",
            json!({ "title": "A", "parent": "TASQ-1" }),
        ),
        call(
            3,
            "node_create",
            json!({ "title": "B", "parent": "TASQ-1" }),
        ),
        // 4..6: связь неблокирующего вида, граф в Mermaid, снятие связи
        call(
            4,
            "tasq_link",
            json!({ "from": "TASQ-1.1", "to": "TASQ-1.2", "kind": "related" }),
        ),
        call(
            5,
            "tasq_graph",
            json!({ "ref": "TASQ-1.2", "mermaid": true }),
        ),
        call(
            6,
            "tasq_unlink",
            json!({ "from": "TASQ-1.1", "to": "TASQ-1.2", "kind": "related" }),
        ),
        // 7: перестановка номеров при переносе в корень
        call(
            7,
            "node_move",
            json!({ "ref": "TASQ-1.2", "renumber": true }),
        ),
        // 8: перестановка соседей
        call(8, "node_reorder", json!({ "refs": ["TASQ-1.1"] })),
    ];

    let responses = session(&dir, &requests);
    assert_eq!(responses.len(), requests.len(), "{responses:?}");

    let moved = check(&responses[7], "node_move");
    assert_eq!(moved["depth"], Value::from(1), "{moved}");

    let graph = check(&responses[5], "tasq_graph");
    assert!(
        graph["mermaid"]
            .as_str()
            .unwrap_or_default()
            .contains("graph"),
        "{graph}"
    );

    let linked = check(&responses[4], "tasq_link");
    assert_eq!(linked["kind"], Value::from("related"), "{linked}");

    let unlinked = check(&responses[6], "tasq_unlink");
    assert_eq!(unlinked["removed"], Value::from(true), "{unlinked}");

    let reordered = check(&responses[8], "node_reorder");
    assert_eq!(reordered["reordered"], Value::from(1), "{reordered}");
}

#[test]
fn lease_release_archive_and_memory_round_trip() {
    let dir = temp_dir("state");
    let init = Command::new(bin())
        .current_dir(&dir)
        .args(["init", "--key", "TASQ"])
        .output()
        .expect("init");
    assert!(init.status.success());

    // Аренда выдаётся отдельно: она нужна для статуса и для резерва.
    let first = session(
        &dir,
        &[
            initialize(),
            call(1, "node_create", json!({ "title": "Задача" })),
            call(2, "node_create", json!({ "title": "Вторая" })),
            call(
                3,
                "tasq_claim",
                json!({ "ref": "TASQ-1", "holder": "agent-a" }),
            ),
        ],
    );
    let token = check(&first[3], "claim")["token"]
        .as_str()
        .expect("token")
        .to_string();

    let requests = vec![
        initialize(),
        // 1: работа ставится только на свой узел и только живым токеном
        call(
            1,
            "node_update",
            json!({ "ref": "TASQ-2", "status": "in_progress", "token": token }),
        ),
        // 2: поддельный токен тоже не даёт права
        call(
            2,
            "node_update",
            json!({ "ref": "TASQ-2", "status": "in_progress", "token": "поддельный" }),
        ),
        // 3: переход в review под живой арендой
        call(
            3,
            "node_update",
            json!({ "ref": "TASQ-1", "status": "review", "token": token }),
        ),
        // 4..5: резерв добавляется и снимается одним вызовом
        call(
            4,
            "tasq_reserve",
            json!({ "token": token, "reserve": ["glob:src/**/*.rs"], "remove": ["port:8080"] }),
        ),
        call(5, "tasq_holdings", json!({})),
        // 6..7: архив и его снятие
        call(6, "node_archive", json!({ "ref": "TASQ-1" })),
        call(7, "node_archive", json!({ "ref": "TASQ-1", "undo": true })),
        // 8..11: комментарий, удаление с причиной и восстановление
        call(
            8,
            "comment_add",
            json!({ "ref": "TASQ-1", "content": "замечание" }),
        ),
        call(
            9,
            "comment_delete",
            json!({ "id": 1, "reason": "устарело" }),
        ),
        call(
            10,
            "comment_list",
            json!({ "ref": "TASQ-1", "include_deleted": true }),
        ),
        call(11, "comment_delete", json!({ "id": 1, "restore": true })),
        // 12..15: заметка с привязкой к узлу
        call(
            12,
            "note_save",
            json!({ "title": "Решение", "content": "включаем WAL", "ref": "TASQ-1",
                    "note_type": "decision" }),
        ),
        call(
            13,
            "note_list",
            json!({ "ref": "TASQ-1", "note_type": "decision" }),
        ),
        call(14, "note_search", json!({ "query": "WAL" })),
        call(15, "note_delete", json!({ "id": 1 })),
        // 16: выпуск аренды
        call(
            16,
            "tasq_release",
            json!({ "token": token, "outcome": "released" }),
        ),
    ];

    let responses = session(&dir, &requests);
    assert_eq!(responses.len(), requests.len(), "{responses:?}");

    // Чужой узел и поддельный токен: право аренды проверяется, а не принимается на веру.
    for index in [1usize, 2] {
        assert!(
            responses[index]["result"]["isError"] == Value::Bool(true),
            "статус поставлен без права: {:?}",
            responses[index]
        );
        assert_eq!(
            body(&responses[index])["code"],
            Value::from("LEASE_REQUIRED")
        );
    }

    let updated = check(&responses[3], "node_update");
    assert_eq!(updated["status"], Value::from("review"), "{updated}");

    let reserved = check(&responses[4], "tasq_reserve");
    assert_eq!(reserved["released"], Value::from(0), "{reserved}");
    assert!(
        serde_json::to_string(&reserved)
            .expect("json")
            .contains("src/**/*.rs"),
        "резерв добавлен: {reserved}"
    );

    let archived = check(&responses[6], "node_archive");
    assert_eq!(archived["affected"], Value::from(1), "{archived}");

    let restored_comment = check(&responses[11], "comment_delete restore");
    assert_eq!(
        restored_comment["is_deleted"],
        Value::from(false),
        "{restored_comment}"
    );

    let listed = check(&responses[10], "comment_list");
    assert!(listed.is_array(), "{listed}");

    let notes = check(&responses[13], "note_list");
    assert_eq!(notes.as_array().map(Vec::len), Some(1), "{notes}");

    let found = check(&responses[14], "note_search");
    assert!(!found.as_array().expect("array").is_empty(), "{found}");

    // Аренда снята: узел больше не в работе.
    let released = check(&responses[16], "tasq_release");
    assert_ne!(released["status"], Value::from("in_progress"), "{released}");
    assert_eq!(released["lease_holder"], Value::Null, "{released}");
}

/// Импорт saga через MCP: документ приходит объектом, а не файлом.
#[test]
fn saga_import_over_mcp() {
    let dir = temp_dir("saga");
    let init = Command::new(bin())
        .current_dir(&dir)
        .args(["init", "--key", "TASQ"])
        .output()
        .expect("init");
    assert!(init.status.success());

    let document = json!({
        "format_version": "1.3",
        "project": {
            "name": "Из saga",
            "epics": [{
                "_original_id": 1,
                "name": "Эпик",
                "tasks": [{ "_original_id": 2, "title": "Задача", "status": "todo" }]
            }]
        }
    });

    let responses = session(
        &dir,
        &[
            initialize(),
            call(
                1,
                "tasq_import",
                json!({ "document": document, "from_saga": true }),
            ),
            call(2, "node_list", json!({})),
            call(3, "tasq_export", json!({ "include_deleted": true })),
        ],
    );
    assert_eq!(responses.len(), 4, "{responses:?}");

    let imported = check(&responses[1], "tasq_import");
    assert_eq!(imported["mode"], Value::from("saga merge"), "{imported}");
    assert_eq!(imported["nodes_created"], Value::from(2), "{imported}");

    let nodes = check(&responses[2], "node_list");
    assert_eq!(nodes.as_array().map(Vec::len), Some(2), "{nodes}");

    let exported = check(&responses[3], "tasq_export");
    assert_eq!(exported["schema"], Value::from("tasq.export.v1"));
}

/// Снимок прогноза через MCP пишется и попадает в серию.
#[test]
fn burnup_snapshot_over_mcp() {
    let dir = temp_dir("burnup");
    let init = Command::new(bin())
        .current_dir(&dir)
        .args(["init", "--key", "TASQ"])
        .output()
        .expect("init");
    assert!(init.status.success());

    let responses = session(
        &dir,
        &[
            initialize(),
            call(
                1,
                "node_create",
                json!({ "title": "Эпик", "due_date": "2099-01-01" }),
            ),
            call(
                2,
                "tasq_burnup",
                json!({ "metric": "points", "ascii": true, "snapshot": true }),
            ),
            call(3, "tasq_risk", json!({ "threshold": 1.0 })),
        ],
    );
    assert_eq!(responses.len(), 4, "{responses:?}");

    // С `ascii` ответ — конверт: диаграмма и серия.
    let envelope = check(&responses[2], "tasq_burnup");
    assert_eq!(
        envelope["series"]["metric"],
        Value::from("points"),
        "{envelope}"
    );
    assert!(
        envelope["chart"].as_str().unwrap_or_default().contains('|'),
        "диаграмма есть: {envelope}"
    );

    let risk = check(&responses[3], "tasq_risk");
    assert!(risk.is_array(), "{risk}");
}
