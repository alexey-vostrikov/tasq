//! Каталог MCP целиком: каждый из 45 инструментов вызывается через настоящий сервер.
//!
//! Смысл не в проверке форм ответов (это делают тесты ядра), а в том, что **каждый
//! инструмент каталога действительно подключён к обработчику**: забыть одну ветку `match`
//! при добавлении инструмента очень легко, а профиль покажет его как доступный.

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
    let dir = std::env::temp_dir().join(format!("tasq-tools-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn cli(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new(bin())
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run tasq");
    assert!(
        out.status.success(),
        "tasq {args:?} упал: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Запуск CLI без требования успеха: нужен, когда проверяется именно отказ.
fn run_cli(dir: &std::path::Path, args: &[&str]) -> (i32, String, String) {
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

/// Один сеанс с сервером: все вызовы подряд, ответы в том же порядке.
///
/// Ошибкой домена (`isError: true`) считается провал теста — если только тест не проверяет
/// отказ специально: тогда он берёт `session_lenient` и смотрит на код сам.
fn session(dir: &std::path::Path, calls: &[(&str, Value)]) -> Vec<Value> {
    session_inner(dir, calls, true)
}

fn session_lenient(dir: &std::path::Path, calls: &[(&str, Value)]) -> Vec<Value> {
    session_inner(dir, calls, false)
}

fn session_inner(dir: &std::path::Path, calls: &[(&str, Value)], expect_ok: bool) -> Vec<Value> {
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
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                   "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                              "clientInfo": {"name": "tools-test", "version": "1"}}})
        )
        .expect("initialize");
        for (index, (name, arguments)) in calls.iter().enumerate() {
            writeln!(
                stdin,
                "{}",
                json!({"jsonrpc": "2.0", "id": index + 1, "method": "tools/call",
                       "params": {"name": name, "arguments": arguments}})
            )
            .expect("call");
        }
    }

    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let responses: Vec<Value> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("json response"))
        .collect();
    assert_eq!(
        responses.len(),
        calls.len() + 1,
        "ответ на рукопожатие + вызовы"
    );

    // Первый ответ — initialize; дальше по одному на вызов.
    if expect_ok {
        for (index, response) in responses.iter().enumerate().skip(1) {
            let (name, _) = &calls[index - 1];
            let result = &response["result"];
            assert_eq!(
                result["isError"],
                json!(false),
                "инструмент {name} вернул ошибку: {}",
                serde_json::to_string_pretty(result).unwrap_or_default()
            );
        }
    }

    responses[1..]
        .iter()
        .map(|response| {
            let text = response["result"]["content"][0]["text"]
                .as_str()
                .expect("text content");
            serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
        })
        .collect()
}

#[test]
fn project_and_init_tools() {
    let dir = temp_dir("project");
    cli(&dir, &["init", "--key", "TASQ"]);
    let out = session(
        &dir,
        &[
            ("tasq_init", json!({"key": "TASQ", "name": "Демо"})),
            ("project_list", json!({})),
            ("project_create", json!({"key": "OTHER", "name": "Второй"})),
            (
                "project_update",
                json!({"name": "Демо-2", "status": "active"}),
            ),
        ],
    );

    assert_eq!(out[0]["project_key"], json!("TASQ"));
    let projects = out[1].as_array().expect("array");
    assert!(
        projects
            .iter()
            .any(|project| project["key"] == json!("TASQ"))
    );

    // Создание второго проекта контекст сеанса не переключает: все инструменты работают
    // с проектом сеанса, иначе `node_create` после `project_create` уходил бы в чужой проект.
    assert_eq!(out[2]["project"]["key"], json!("OTHER"));
    assert_eq!(
        out[2]["active_project"],
        json!("TASQ"),
        "сеанс остался в своём проекте"
    );
    assert_eq!(
        out[3]["project"]["key"],
        json!("TASQ"),
        "правка ушла в проект сеанса"
    );
    assert_eq!(
        out[3]["project"]["name"],
        json!("Демо-2"),
        "project_update вернул обновлённый проект"
    );

    // Переименован проект сеанса, второй проект не тронут.
    let listed = session(&dir, &[("project_list", json!({}))]);
    let projects = listed[0].as_array().expect("array");
    let first = projects
        .iter()
        .find(|project| project["key"] == json!("TASQ"))
        .expect("проект TASQ");
    assert_eq!(first["name"], json!("Демо-2"));
    let other = projects
        .iter()
        .find(|project| project["key"] == json!("OTHER"))
        .expect("проект OTHER");
    assert_eq!(other["name"], json!("Второй"), "чужой проект не изменён");
}

#[test]
fn node_lifecycle_tools() {
    let dir = temp_dir("nodes");
    cli(&dir, &["init", "--key", "TASQ"]);

    let out = session(
        &dir,
        &[
            ("node_create", json!({"title": "Эпик", "kind": "epic"})),
            (
                "node_create",
                json!({"title": "Задача", "parent": "TASQ-1"}),
            ),
            (
                "node_create",
                json!({"title": "Вторая", "parent": "TASQ-1"}),
            ),
            ("node_get", json!({"ref": "TASQ-1.1"})),
            (
                "node_update",
                json!({"ref": "TASQ-1.1", "priority": "high", "description": "текст"}),
            ),
            ("node_list", json!({"parent": "TASQ-1"})),
            (
                "node_move",
                json!({"ref": "TASQ-1.2", "parent": "TASQ-1.1"}),
            ),
            ("node_reorder", json!({"refs": ["TASQ-1.1", "TASQ-1"]})),
            ("node_archive", json!({"ref": "TASQ-1.1.2"})),
            ("node_archive", json!({"ref": "TASQ-1.1.2", "undo": true})),
            ("node_delete", json!({"ref": "TASQ-1.1.2"})),
            ("node_restore", json!({"ref": "TASQ-1.1.2"})),
            ("tasq_context", json!({"ref": "TASQ-1.1"})),
            ("tasq_next", json!({"limit": 3})),
        ],
    );

    assert_eq!(out[0]["dot_id"], json!("TASQ-1"));
    assert_eq!(out[1]["dot_id"], json!("TASQ-1.1"));
    assert_eq!(out[3]["node"]["title"], json!("Задача"));
    assert!(out[3]["children"].is_array(), "дети узла в карточке");
    assert_eq!(out[4]["priority"], json!("high"));
    let children: Vec<&str> = out[5]
        .as_array()
        .expect("children")
        .iter()
        .filter_map(|child| child["dot_id"].as_str())
        .collect();
    assert!(children.contains(&"TASQ-1.1"), "{children:?}");
    assert!(children.contains(&"TASQ-1.2"), "{children:?}");
    assert_eq!(
        out[6]["dot_id"],
        json!("TASQ-1.1.2"),
        "перенос меняет dot-ID"
    );
    assert_eq!(out[7]["reordered"], json!(2));
    assert_eq!(out[8]["affected"], json!(1), "архив поддерева");
    assert_eq!(out[9]["affected"], json!(1), "разархив");
    assert_eq!(out[10]["deleted"], json!(true));
    assert_eq!(out[11]["dot_id"], json!("TASQ-1.1.2"));
    // Цепочка контекста: от корня до узла.
    let chain = out[12].as_array().expect("chain");
    assert_eq!(chain.first().expect("root")["dot_id"], json!("TASQ-1"));
    assert!(
        !out[13].as_array().map(Vec::is_empty).unwrap_or(true),
        "{:?}",
        out[13]
    );
}

#[test]
fn lease_and_reservation_tools() {
    let dir = temp_dir("leases");
    cli(&dir, &["init", "--key", "TASQ"]);
    cli(&dir, &["add", "Эпик"]);

    let out = session(
        &dir,
        &[(
            "tasq_claim",
            json!({"ref": "TASQ-1", "holder": "agent-a", "ttl": 600,
                                  "reserve": ["path:src/api"]}),
        )],
    );
    let token = out[0]["token"].as_str().expect("token").to_string();

    let out = session(
        &dir,
        &[
            ("tasq_holdings", json!({})),
            ("tasq_renew", json!({"token": token, "ttl": 900})),
            (
                "tasq_reserve",
                json!({"token": token, "reserve": ["path:src/web"]}),
            ),
            (
                "tasq_reserve",
                json!({"token": token, "remove": ["path:src/web"]}),
            ),
            (
                "tasq_release",
                json!({"token": token, "outcome": "released"}),
            ),
        ],
    );

    let holdings = out[0].as_array().expect("holdings");
    assert_eq!(holdings[0]["holder"], json!("agent-a"));
    assert!(out[1]["expires_at"].is_string(), "продление вернуло срок");
    assert_eq!(
        out[2],
        json!(["path:src/api", "path:src/web"]),
        "резерв добавлен"
    );
    assert_eq!(out[3]["released"], json!(1), "резерв снят");
    assert_eq!(out[4]["status"], json!("todo"));
}

#[test]
fn edge_tools() {
    let dir = temp_dir("edges");
    cli(&dir, &["init", "--key", "TASQ"]);
    cli(&dir, &["add", "Эпик"]);
    cli(&dir, &["add", "Первая", "--parent", "TASQ-1"]);
    cli(&dir, &["add", "Вторая", "--parent", "TASQ-1"]);

    let out = session(
        &dir,
        &[
            (
                "tasq_link",
                json!({"from": "TASQ-1.2", "to": "TASQ-1.1", "kind": "blocks"}),
            ),
            ("tasq_graph", json!({"ref": "TASQ-1", "depth": 2})),
            ("tasq_graph", json!({"ref": "TASQ-1", "mermaid": true})),
            (
                "tasq_unlink",
                json!({"from": "TASQ-1.2", "to": "TASQ-1.1", "kind": "blocks"}),
            ),
        ],
    );

    assert_eq!(out[0]["from"], json!("TASQ-1.2"));
    assert_eq!(out[0]["kind"], json!("blocks"));
    assert_eq!(out[1]["node"], json!("TASQ-1"), "граф от корня: {out:?}");
    assert!(out[1]["blocks"].is_array());
    assert!(
        out[2]["mermaid"]
            .as_str()
            .unwrap_or_default()
            .starts_with("graph LR"),
        "mermaid-вывод: {:?}",
        out[2]
    );
    assert_eq!(out[3]["removed"], json!(true));
}

#[test]
fn comment_and_note_tools() {
    let dir = temp_dir("notes");
    cli(&dir, &["init", "--key", "TASQ"]);
    cli(&dir, &["add", "Эпик"]);

    let out = session(
        &dir,
        &[
            (
                "comment_add",
                json!({"ref": "TASQ-1", "content": "первый", "holder": "agent-a"}),
            ),
            ("comment_list", json!({"ref": "TASQ-1"})),
            (
                "note_save",
                json!({"title": "Решение", "content": "включаем WAL",
                                 "note_type": "decision", "ref": "TASQ-1"}),
            ),
            ("note_list", json!({"ref": "TASQ-1"})),
            ("note_search", json!({"query": "WAL"})),
        ],
    );

    let comment_id = out[0]["id"].as_i64().expect("comment id");
    assert_eq!(out[0]["content"], json!("первый"));
    assert_eq!(out[1].as_array().map(Vec::len), Some(1));
    let note_id = out[2]["id"].as_i64().expect("note id");
    assert_eq!(out[2]["note_type"], json!("decision"));
    assert_eq!(out[3].as_array().map(Vec::len), Some(1));
    assert!(
        out[4]
            .as_array()
            .map(|list| !list.is_empty())
            .unwrap_or(false),
        "поиск нашёл заметку: {:?}",
        out[4]
    );

    let out = session(
        &dir,
        &[
            (
                "comment_delete",
                json!({"id": comment_id, "reason": "устарело"}),
            ),
            (
                "comment_list",
                json!({"ref": "TASQ-1", "include_deleted": true}),
            ),
            ("note_delete", json!({"id": note_id})),
        ],
    );
    assert_eq!(out[0]["is_deleted"], json!(true));
    assert_eq!(out[0]["delete_reason"], json!("устарело"));
    assert_eq!(
        out[1].as_array().map(Vec::len),
        Some(1),
        "удалённый виден по флагу"
    );
    assert_eq!(out[2]["removed"], json!(true));
}

#[test]
fn template_tools() {
    let dir = temp_dir("templates");
    cli(&dir, &["init", "--key", "TASQ"]);

    let out = session(
        &dir,
        &[
            (
                "template_save",
                json!({
                    "name": "api",
                    "description": "Каркас API",
                    "tree": {"nodes": [
                        {"title": "{resource}: контроллер", "children": [
                            {"title": "{resource}: тесты"}
                        ]}
                    ]}
                }),
            ),
            ("template_list", json!({})),
            (
                "template_apply",
                json!({"name": "api", "values": {"resource": "orders"}}),
            ),
            ("template_delete", json!({"name": "api"})),
        ],
    );

    assert_eq!(out[0]["name"], json!("api"));
    assert_eq!(out[1].as_array().map(Vec::len), Some(1));
    let created = out[2]["created"].as_array().expect("created");
    assert_eq!(created.len(), 2, "шаблон создал дерево: {:?}", out[2]);
    assert_eq!(
        out[2]["parent"],
        json!("TASQ"),
        "без parent узлы в корне проекта"
    );
    assert_eq!(out[3]["removed"], json!(true));

    // Переменные без значения — отказ до первой записи, а не «{resource}» в заголовке.
    let mut child_out = String::new();
    let child = Command::new(bin())
        .current_dir(&dir)
        .args(["template", "apply", "api"])
        .output()
        .expect("run");
    child_out.push_str(&String::from_utf8_lossy(&child.stderr));
    assert!(!child.status.success());
    assert!(
        child_out.contains("not found") || child_out.contains("INVALID_VALUE"),
        "{child_out}"
    );
}

#[test]
fn observability_tools() {
    let dir = temp_dir("observability");
    cli(&dir, &["init", "--key", "TASQ"]);
    cli(&dir, &["add", "Эпик про миграции"]);
    cli(&dir, &["add", "Задача", "--parent", "TASQ-1"]);
    cli(&dir, &["add", "Ещё", "--parent", "TASQ-1"]);

    let out = session(
        &dir,
        &[
            ("activity_log", json!({"limit": 10})),
            ("session_diff", json!({"since": "2000-01-01T00:00:00.000Z"})),
            (
                "node_batch_update",
                json!({"refs": ["TASQ-1.1", "TASQ-1.2"], "status": "todo"}),
            ),
            ("tasq_search", json!({"query": "миграции"})),
            ("tasq_dashboard", json!({})),
            ("tasq_burnup", json!({"metric": "count", "ascii": true})),
            ("tasq_risk", json!({"threshold": 0.2})),
        ],
    );

    assert!(
        !out[0].as_array().map(Vec::is_empty).unwrap_or(true),
        "лог не пуст"
    );
    assert_eq!(out[1]["since"], json!("2000-01-01T00:00:00.000Z"));
    assert!(out[1]["summary"].is_array());
    let batch = out[2]["updated"].as_array().expect("updated");
    assert_eq!(batch.len(), 2, "отчёт по каждому узлу: {:?}", out[2]);
    assert_eq!(out[2]["failed"].as_array().map(Vec::len), Some(0));
    assert!(out[3].as_array().map(|l| !l.is_empty()).unwrap_or(false));
    assert!(out[4]["board"].is_object());
    assert!(out[5]["chart"].is_string(), "ASCII-график: {:?}", out[5]);
    assert!(
        out[5]["series"]["points"]
            .as_array()
            .map(|p| !p.is_empty())
            .unwrap_or(false),
        "ряд дней: {:?}",
        out[5]
    );
    assert!(out[6].is_array(), "risk отдаёт список узлов: {:?}", out[6]);
}

#[test]
fn transfer_tools() {
    let dir = temp_dir("transfer");
    cli(&dir, &["init", "--key", "SRC"]);
    cli(&dir, &["add", "Эпик"]);

    let out = session(&dir, &[("tasq_export", json!({}))]);
    let document = out[0].clone();
    assert_eq!(document["schema"], json!("tasq.export.v1"));
    assert!(document["sha256"].is_string());

    let target = temp_dir("transfer-target");
    cli(&target, &["init", "--key", "DST"]);
    let out = session(
        &target,
        &[
            (
                "tasq_import",
                json!({"document": document, "mode": "merge"}),
            ),
            ("tasq_burnup", json!({})),
        ],
    );
    assert_eq!(out[0]["nodes_created"], json!(1));
    assert_eq!(out[0]["checksum_verified"], json!(true));

    // Повторный импорт того же документа ничего не дублирует.
    let out = session(
        &target,
        &[(
            "tasq_import",
            json!({"document": document, "mode": "merge"}),
        )],
    );
    assert_eq!(out[0]["nodes_created"], json!(0));
    assert_eq!(out[0]["nodes_skipped"], json!(1));
}

/// Ни один инструмент каталога не остаётся без обработчика: вызов с пустыми аргументами
/// обязан дать ошибку домена (`isError: true`), но не `UNKNOWN_TOOL` и не падение.
#[test]
fn every_catalogue_entry_has_a_handler() {
    let dir = temp_dir("catalogue");
    cli(&dir, &["init", "--key", "TASQ"]);

    // Каталог берём у самого сервера, чтобы список не разъезжался с кодом.
    let names: Vec<String> = {
        let mut child = Command::new(bin())
            .current_dir(&dir)
            .args(["mcp", "--profile", "full"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        {
            let stdin = child.stdin.as_mut().expect("stdin");
            writeln!(
                stdin,
                "{}",
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
            )
            .expect("init");
            writeln!(
                stdin,
                "{}",
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
            )
            .expect("list");
        }
        let output = child.wait_with_output().expect("wait");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let list: Value = serde_json::from_str(stdout.lines().nth(1).expect("tools/list response"))
            .expect("json");
        list["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect()
    };
    assert_eq!(names.len(), 45, "каталог на 45 инструментов");

    for name in &names {
        let out = Command::new(bin())
            .current_dir(&dir)
            .args(["mcp", "--profile", "full"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write as _;
                {
                    let stdin = child.stdin.as_mut().expect("stdin");
                    writeln!(
                        stdin,
                        "{}",
                        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
                    )?;
                    writeln!(
                        stdin,
                        "{}",
                        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                                                 "params": {"name": name, "arguments": {}}})
                    )?;
                }
                child.wait_with_output()
            })
            .expect("run");

        let stdout = String::from_utf8_lossy(&out.stdout);
        let response: Value =
            serde_json::from_str(stdout.lines().nth(1).expect("call response")).expect("json");
        let result = &response["result"];
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        assert!(
            !text.contains("UNKNOWN_TOOL"),
            "у инструмента {name} нет обработчика"
        );
        assert!(
            result["isError"] == json!(true) || result["isError"] == json!(false),
            "ответ {name} не является результатом вызова: {result}"
        );
    }
}

/// Проекты в MCP: создание не меняет контекст молча, `project_select` меняет явно,
/// и после него работа идёт в выбранном проекте — в том же сеансе.
#[test]
fn projects_are_created_and_selected_explicitly() {
    let dir = temp_dir("projects");
    cli(&dir, &["init", "--key", "ALPHA", "--name", "Первый"]);

    let out = session(
        &dir,
        &[
            ("project_create", json!({"key": "prj", "name": "Новый"})),
            // До выбора узлы идут в проект по умолчанию: это видно в ответе, а не угадывается.
            ("node_create", json!({"title": "До выбора", "kind": "epic"})),
            ("project_select", json!({"key": "PRJ"})),
            (
                "node_create",
                json!({"title": "После выбора", "kind": "epic"}),
            ),
            ("project_list", json!({})),
        ],
    );

    let created = &out[0];
    assert_eq!(created["project"]["key"], json!("PRJ"), "ключ нормализован");
    assert_eq!(created["switched"], json!(false), "создание не переключает");
    assert_eq!(created["active_project"], json!("ALPHA"));
    assert!(
        created["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("project_select"),
        "подсказка называет способ переключиться: {created}"
    );

    assert_eq!(out[1]["dot_id"], json!("ALPHA-1"));

    let selected = &out[2];
    assert_eq!(selected["active_project"], json!("PRJ"));
    assert_eq!(selected["previous"], json!("ALPHA"));
    assert_eq!(selected["switched"], json!(true));

    assert_eq!(
        out[3]["dot_id"],
        json!("PRJ-1"),
        "узел ушёл в выбранный проект"
    );

    let projects = out[4].as_array().expect("projects");
    let alpha = projects
        .iter()
        .find(|project| project["key"] == json!("ALPHA"))
        .expect("ALPHA");
    let prj = projects
        .iter()
        .find(|project| project["key"] == json!("PRJ"))
        .expect("PRJ");
    assert_eq!(alpha["nodes"], json!(1));
    assert_eq!(prj["nodes"], json!(1));
}

#[test]
fn selecting_an_unknown_project_is_a_domain_error() {
    let dir = temp_dir("projects-unknown");
    cli(&dir, &["init", "--key", "ALPHA"]);

    let mut child = Command::new(bin())
        .current_dir(&dir)
        .args(["mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    {
        use std::io::Write as _;
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
        )
        .expect("init");
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                                     "params": {"name": "project_select", "arguments": {"key": "NOPE"}}})).expect("call");
    }
    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let response: Value =
        serde_json::from_str(stdout.lines().nth(1).expect("response")).expect("json");
    assert_eq!(response["result"]["isError"], json!(true), "{response}");
    let payload: Value = serde_json::from_str(
        response["result"]["content"][0]["text"]
            .as_str()
            .expect("text"),
    )
    .expect("json");
    assert_eq!(payload["code"], json!("PROJECT_NOT_FOUND"));
    assert!(
        payload["message"]
            .as_str()
            .unwrap_or_default()
            .contains("ALPHA")
    );
}

/// Проект можно задать при старте сервера: так его прописывают в конфиге клиента.
#[test]
fn the_server_can_start_in_a_named_project() {
    let dir = temp_dir("projects-start");
    cli(&dir, &["init", "--key", "ALPHA"]);
    cli(&dir, &["create", "project", "PRJ"]);
    cli(&dir, &["--project", "PRJ", "add", "Узел в PRJ"]);

    let mut child = Command::new(bin())
        .current_dir(&dir)
        .args(["mcp", "--project", "PRJ"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    {
        use std::io::Write as _;
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
        )
        .expect("init");
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                                     "params": {"name": "node_list", "arguments": {}}})
        )
        .expect("call");
    }
    let output = child.wait_with_output().expect("wait");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("project PRJ"),
        "сервер сообщает проект: {stderr}"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let response: Value =
        serde_json::from_str(stdout.lines().nth(1).expect("response")).expect("json");
    let nodes: Value = serde_json::from_str(
        response["result"]["content"][0]["text"]
            .as_str()
            .expect("text"),
    )
    .expect("json");
    let ids: Vec<&str> = nodes
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|node| node["dot_id"].as_str())
        .collect();
    assert_eq!(ids, vec!["PRJ-1"], "старт сразу в выбранном проекте");
}

/// Переименование в MCP: ключ проекта сессии меняется вместе с контекстом, иначе
/// следующий вызов ушёл бы в первый проект по порядку.
#[test]
fn a_project_is_renamed_over_mcp_without_losing_the_session() {
    let dir = temp_dir("project-rename");
    cli(&dir, &["init", "--key", "ALPHA", "--name", "Первый"]);
    cli(&dir, &["create", "project", "PRJ", "--name", "Второй"]);
    cli(&dir, &["--project", "PRJ", "add", "Задача в PRJ"]);

    let out = session(
        &dir,
        &[
            ("project_select", json!({"key": "PRJ"})),
            ("project_update", json!({"name": "Переименован"})),
            ("project_update", json!({"new_key": "BETA"})),
            ("node_list", json!({})),
            ("project_list", json!({})),
        ],
    );

    assert_eq!(out[0]["active_project"], json!("PRJ"));
    assert_eq!(out[1]["project"]["name"], json!("Переименован"));
    assert_eq!(
        out[1]["active_project"],
        json!("PRJ"),
        "имя сменилось, проект тот же"
    );

    // Смена ключа: адреса узлов переехали, сессия осталась в том же проекте.
    assert_eq!(out[2]["project"]["key"], json!("BETA"));
    assert_eq!(out[2]["previous_key"], json!("PRJ"));
    assert_eq!(
        out[2]["active_project"],
        json!("BETA"),
        "контекст переехал вместе с ключом"
    );
    assert_eq!(out[2]["nodes"], json!(1));

    let ids: Vec<&str> = out[3]
        .as_array()
        .expect("nodes")
        .iter()
        .filter_map(|node| node["dot_id"].as_str())
        .collect();
    assert_eq!(ids, vec!["BETA-1"], "dot-ID собраны из нового ключа");

    let projects = out[4].as_array().expect("projects");
    assert!(
        projects
            .iter()
            .any(|project| project["key"] == json!("BETA")),
        "{projects:?}"
    );
    assert!(
        !projects
            .iter()
            .any(|project| project["key"] == json!("PRJ")),
        "старого ключа нет: {projects:?}"
    );
}

#[test]
fn status_is_validated_over_mcp_too() {
    let dir = temp_dir("project-status");
    cli(&dir, &["init", "--key", "ALPHA"]);

    let mut child = Command::new(bin())
        .current_dir(&dir)
        .args(["mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    {
        use std::io::Write as _;
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
        )
        .expect("init");
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                                     "params": {"name": "project_update", "arguments": {"status": "nonsense"}}})).expect("call");
    }
    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let response: Value =
        serde_json::from_str(stdout.lines().nth(1).expect("response")).expect("json");
    assert_eq!(response["result"]["isError"], json!(true), "{response}");
    let payload: Value = serde_json::from_str(
        response["result"]["content"][0]["text"]
            .as_str()
            .expect("text"),
    )
    .expect("json");
    // Доменная ошибка, а не STORAGE_ERROR от CHECK-ограничения базы.
    assert_eq!(payload["code"], json!("INVALID_VALUE"), "{payload}");

    // И в базе ничего не изменилось.
    let (_, list, _) = run_cli(&dir, &["--json", "list", "projects"]);
    let projects: Value = serde_json::from_str(&list).expect("json");
    assert_eq!(projects[0]["status"], json!("active"));
}

#[test]
fn deleting_the_session_project_moves_the_session_to_another_one() {
    let dir = temp_dir("project-delete");
    cli(&dir, &["init", "--key", "ALPHA"]);
    cli(&dir, &["create", "project", "PRJ"]);
    cli(&dir, &["--project", "PRJ", "add", "Задача"]);

    let out = session_lenient(
        &dir,
        &[
            ("project_select", json!({"key": "PRJ"})),
            // Без force удаление непустого проекта — отказ, и содержимое цело.
            ("project_delete", json!({})),
            ("node_list", json!({})),
            ("project_delete", json!({"force": true})),
            ("project_list", json!({})),
        ],
    );

    let refused = &out[1];
    assert_eq!(refused["code"], json!("PROJECT_NOT_EMPTY"), "{refused}");
    assert_eq!(
        out[2].as_array().map(Vec::len),
        Some(1),
        "после отказа узел на месте"
    );

    let deleted = &out[3];
    assert_eq!(deleted["deleted"], json!("PRJ"));
    assert_eq!(deleted["nodes"], json!(1));
    assert_eq!(
        deleted["active_project"],
        json!("ALPHA"),
        "сессия переехала"
    );
    assert_eq!(deleted["switched_to"], json!("ALPHA"));

    let projects = out[4].as_array().expect("projects");
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["key"], json!("ALPHA"));
}

#[test]
fn deleting_the_last_project_over_mcp_reports_no_active_project() {
    let dir = temp_dir("project-delete-last");
    cli(&dir, &["init", "--key", "ALPHA"]);

    let out = session(&dir, &[("project_delete", json!({"force": true}))]);
    assert_eq!(out[0]["deleted"], json!("ALPHA"));
    assert_eq!(out[0]["active_project"], Value::Null);
    assert!(
        out[0]["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("project_create"),
        "подсказка называет способ вернуться: {}",
        out[0]
    );
}

#[test]
fn project_delete_is_not_in_the_agent_profile() {
    let dir = temp_dir("project-delete-profile");
    cli(&dir, &["init", "--key", "ALPHA"]);
    cli(&dir, &["create", "project", "PRJ"]);

    // Агентский профиль не удаляет проекты: это операция уровня full.
    let mut child = Command::new(bin())
        .current_dir(&dir)
        .args(["mcp", "--profile", "agent"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    {
        use std::io::Write as _;
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
        )
        .expect("init");
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                                     "params": {"name": "project_delete", "arguments": {"key": "PRJ", "force": true}}})).expect("call");
    }
    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let response: Value =
        serde_json::from_str(stdout.lines().nth(1).expect("response")).expect("json");
    let payload: Value = serde_json::from_str(
        response["result"]["content"][0]["text"]
            .as_str()
            .expect("text"),
    )
    .expect("json");
    assert_eq!(payload["code"], json!("TOOL_NOT_IN_PROFILE"));

    // Проект не удалён.
    let (_, list, _) = run_cli(&dir, &["list", "projects"]);
    assert!(list.contains("PRJ"), "{list}");
}
