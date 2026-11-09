//! Сквозная поверхность CLI: каждая команда и каждый флаг, включая ветки `--json`.
//!
//! Замер покрытия показал, что почти весь непокрытый код `main.rs` — это ветки
//! `if cli.json { print_json(...) }`: команды проверялись человекопонятным выводом, а
//! машиночитаемый оставался неисполненным. Здесь каждая команда вызывается с `--json`, и
//! ответ обязан разбираться как JSON: расхождение формы ловится сразу, а не у потребителя.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tasq")
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-surface-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Запуск с `--json`; успех обязателен, ответ обязан быть JSON.
fn json(dir: &Path, args: &[&str]) -> Value {
    let out = Command::new(bin())
        .current_dir(dir)
        .arg("--json")
        .args(args)
        .output()
        .expect("run tasq");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "tasq --json {args:?} упал: {stderr}{stdout}"
    );
    serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("tasq --json {args:?} вернул не JSON ({err}):\n{stdout}"))
}

/// Запуск с `--json`, где ожидается отказ: код возврата и разобранный код ошибки.
fn refused(dir: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(bin())
        .current_dir(dir)
        .arg("--json")
        .args(args)
        .output()
        .expect("run tasq");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let code = out.status.code().unwrap_or(-1);
    let payload: Value =
        serde_json::from_str(&stdout).unwrap_or_else(|_| Value::String(stderr.clone()));
    // Отказ печатается в конверте `{"error": {"code": ...}}`, а не плоским объектом.
    let code_text = payload["error"]["code"]
        .as_str()
        .or_else(|| payload["code"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| stderr.clone());
    (code, code_text)
}

/// Прогон без `--json`: человекочитаемые ветки тоже надо исполнять.
fn text(dir: &Path, args: &[&str]) -> (i32, String, String) {
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

/// Дата `YYYY-MM-DD`, отстоящая от сегодняшней на `days_ago` дней (UTC).
fn today_utc_minus(days_ago: u32) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
        - (days_ago as i64) * 86_400;
    let days = secs.div_euclid(86_400);
    // Обратный счёт от 1970-01-01: календарь не нужен, достаточно дней от эпохи.
    let (mut y, mut d) = (1970i64, days);
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let len = if leap { 366 } else { 365 };
        if d < len {
            break;
        }
        d -= len;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let months = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0usize;
    while d >= months[m] {
        d -= months[m];
        m += 1;
    }
    format!("{y:04}-{:02}-{:02}", m + 1, d + 1)
}

fn fixture(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    json(&dir, &["init", "--key", "TASQ", "--name", "Проверка"]);
    dir
}

#[test]
fn project_commands_answer_json_and_text() {
    let dir = fixture("projects");

    let created = json(&dir, &["create", "project", "PRJ", "--name", "Второй"]);
    assert_eq!(created["key"], Value::from("PRJ"));

    let listed = json(&dir, &["list", "projects"]);
    assert_eq!(listed.as_array().map(Vec::len), Some(2));

    let updated = json(
        &dir,
        &["update", "project", "PRJ", "--name", "Переименован"],
    );
    assert_eq!(updated["project"]["name"], Value::from("Переименован"));

    let restatus = json(&dir, &["update", "project", "PRJ", "--status", "archived"]);
    assert_eq!(restatus["project"]["status"], Value::from("archived"));

    let rekeyed = json(&dir, &["update", "project", "PRJ", "--new-key", "BETA"]);
    assert_eq!(rekeyed["project"]["key"], Value::from("BETA"));
    assert_eq!(rekeyed["previous_key"], Value::from("PRJ"));

    let deleted = json(&dir, &["delete", "project", "BETA", "--force"]);
    assert_eq!(deleted["deleted"], Value::from("BETA"));

    // Человекочитаемые ветки тех же команд.
    for args in [
        vec!["create", "project", "GAMMA"],
        vec!["list", "projects"],
        vec!["update", "project", "GAMMA", "--name", "Гамма"],
        vec!["delete", "project", "GAMMA", "--force"],
    ] {
        let (code, stdout, stderr) = text(&dir, &args);
        assert_eq!(code, 0, "{args:?}: {stderr}");
        assert!(!stdout.trim().is_empty(), "{args:?} ничего не напечатал");
    }
}

#[test]
fn node_commands_answer_json_and_text() {
    let dir = fixture("nodes");

    let epic = json(
        &dir,
        &["add", "Эпик", "--kind", "epic", "--desc", "описание эпика"],
    );
    assert_eq!(epic["dot_id"], Value::from("TASQ-1"));

    let task = json(
        &dir,
        &[
            "add",
            "Задача",
            "--parent",
            "TASQ-1",
            "--kind",
            "task",
            "--priority",
            "high",
            "--due",
            "2099-12-01",
            "--weight",
            "2",
            "--estimate",
            "3.5",
            "--source",
            "src/api.rs",
            "--branch",
            "feature/x",
        ],
    );
    assert_eq!(task["dot_id"], Value::from("TASQ-1.1"));
    assert_eq!(task["due_date"], Value::from("2099-12-01"));

    let sub = json(&dir, &["add", "Подзадача", "--parent", "TASQ-1.1"]);
    assert_eq!(sub["dot_id"], Value::from("TASQ-1.1.1"));

    let shown = json(&dir, &["show", "TASQ-1.1"]);
    assert_eq!(shown["dot_id"], Value::from("TASQ-1.1"));

    // `--tree` отдаёт всё поддерево массивом, а не карточку узла.
    let tree = json(&dir, &["show", "TASQ-1", "--tree"]);
    let subtree = tree.as_array().expect("поддерево массивом");
    assert_eq!(subtree.len(), 3, "эпик и двое детей: {tree}");
    assert_eq!(subtree[0]["dot_id"], Value::from("TASQ-1"));

    let listed = json(&dir, &["ls"]);
    assert_eq!(listed.as_array().map(Vec::len), Some(3));

    for filters in [
        vec!["ls", "--status", "todo"],
        vec!["ls", "--parent", "TASQ-1"],
        vec!["ls", "--assignee", "nobody"],
        vec!["ls", "--overdue"],
        vec!["ls", "--archived"],
    ] {
        json(&dir, &filters);
    }

    // Правки по одному полю и все сразу.
    json(
        &dir,
        &[
            "set",
            "TASQ-1.1",
            "--title",
            "Задача (уточнена)",
            "--priority",
            "critical",
            "--due",
            "2099-11-01",
            "--assignee",
            "agent-a",
            "--weight",
            "3",
            "--desc",
            "новое описание",
        ],
    );
    let locked = json(&dir, &["set", "TASQ-1.1", "--lock-desc"]);
    assert_eq!(locked["description_locked"], Value::from(true));
    json(&dir, &["set", "TASQ-1.1", "--unlock-desc"]);

    // Перенос, удаление, восстановление, архив.
    // Номер 1 у нового родителя занят, поэтому берётся следующий свободный: 2.
    let moved = json(&dir, &["mv", "TASQ-1.1.1", "--parent", "TASQ-1"]);
    assert_eq!(moved["dot_id"], Value::from("TASQ-1.2"));
    let moved = json(
        &dir,
        &["mv", "TASQ-1.2", "--parent", "TASQ-1.1", "--renumber"],
    );
    assert_eq!(moved["dot_id"], Value::from("TASQ-1.1.1"));
    let rooted = json(&dir, &["mv", "TASQ-1.1.1", "--renumber"]);
    assert_eq!(rooted["depth"], Value::from(1));

    let removed = json(&dir, &["rm", "TASQ-2"]);
    assert_eq!(removed["deleted"], Value::from(true));
    json(&dir, &["restore", "TASQ-2"]);

    let archived = json(&dir, &["archive", "TASQ-2"]);
    assert_eq!(archived["affected"], Value::from(1));
    json(&dir, &["archive", "TASQ-2", "--undo"]);

    let log = json(&dir, &["log", "--limit", "5"]);
    assert!(!log.as_array().expect("log").is_empty());

    // Человекочитаемые ветки. Ссылки берутся из текущего состояния: после переносов
    // прежние dot-ID сместились, и «архивировать TASQ-1.1» здесь не существует.
    let fresh = json(&dir, &["add", "Свежий", "--parent", "TASQ-1"]);
    let fresh_ref = fresh["dot_id"].as_str().expect("dot").to_string();
    let throwaway = json(&dir, &["add", "Временный", "--parent", "TASQ-1"]);
    let throwaway_ref = throwaway["dot_id"].as_str().expect("dot").to_string();
    for args in [
        vec!["show", "TASQ-1"],
        vec!["show", "TASQ-1.1"],
        vec!["ls"],
        vec!["set", "TASQ-1.1", "--title", "ещё раз"],
        vec!["mv", "TASQ-1.1", "--parent", "TASQ-1"],
        vec!["archive", fresh_ref.as_str()],
        vec!["archive", fresh_ref.as_str(), "--undo"],
        vec!["log"],
        vec!["add", "Корневой", "--kind", "epic"],
        vec!["rm", throwaway_ref.as_str()],
        vec!["restore", throwaway_ref.as_str()],
    ] {
        let (code, _, stderr) = text(&dir, &args);
        assert_eq!(code, 0, "{args:?}: {stderr}");
    }
}

#[test]
fn lease_commands_answer_json_and_text() {
    let dir = fixture("leases");
    json(&dir, &["add", "Эпик"]);
    json(&dir, &["add", "Ждёт", "--parent", "TASQ-1"]);
    json(&dir, &["add", "Блокер", "--parent", "TASQ-1"]);

    let lease = json(
        &dir,
        &[
            "claim",
            "TASQ-1.1",
            "--holder",
            "agent-a",
            "--ttl",
            "600",
            "--checkpoint",
            "начал с контроллера",
            "--reserve",
            "src/api",
            "--reserve-glob",
            "src/**/*.rs",
            "--reserve-logical",
            "port:8080",
            "--shared",
        ],
    );
    let token = lease["token"].as_str().expect("token").to_string();
    assert_eq!(lease["holder"], Value::from("agent-a"));

    // Захват того же узла другим агентом — отказ с кодом.
    let (code, error) = refused(&dir, &["claim", "TASQ-1.1", "--holder", "agent-b"]);
    assert_eq!(code, 1);
    assert_eq!(error, "ACTIVE_ATTEMPT_EXISTS");

    // Перехват живой аренды.
    let stolen = json(
        &dir,
        &["claim", "TASQ-1.1", "--holder", "agent-b", "--steal"],
    );
    assert_eq!(stolen["holder"], Value::from("agent-b"));
    let stolen_token = stolen["token"].as_str().expect("token").to_string();

    json(&dir, &["holders"]);
    json(&dir, &["renew", &stolen_token, "--ttl", "900"]);
    json(
        &dir,
        &["reserve", &stolen_token, "--path", "src/web", "--shared"],
    );
    json(&dir, &["unreserve", &stolen_token, "--path", "src/web"]);
    json(&dir, &["unreserve", &stolen_token]);
    json(&dir, &["release", &stolen_token, "--outcome", "released"]);

    // Возврат к первой аренде: она перехвачена, поэтому токен больше не жив.
    let (code, error) = refused(&dir, &["renew", &token]);
    assert_eq!(code, 1);
    assert_eq!(error, "NOT_YOUR_ATTEMPT");

    // Завершение с чекпойнтом и принудительное завершение контейнера с открытыми детьми.
    let second = json(&dir, &["claim", "TASQ-1", "--holder", "agent-c"]);
    let second_token = second["token"].as_str().expect("token").to_string();
    let (code, error) = refused(&dir, &["release", &second_token, "--outcome", "completed"]);
    assert_eq!(code, 1);
    assert_eq!(error, "CHILDREN_UNFINISHED");
    json(
        &dir,
        &[
            "release",
            &second_token,
            "--outcome",
            "completed",
            "--checkpoint",
            "оставил заметку",
            "--force",
        ],
    );

    // Человекочитаемые ветки.
    let third = text(&dir, &["claim", "TASQ-1.2", "--holder", "agent-d"]);
    assert_eq!(third.0, 0, "{}", third.2);
    let token = third
        .1
        .lines()
        .find_map(|line| line.split_whitespace().find(|w| w.len() == 64))
        .expect("токен в выводе")
        .to_string();
    for args in [
        vec!["holders"],
        vec!["renew", token.as_str()],
        vec!["release", token.as_str(), "--outcome", "failed"],
    ] {
        let (code, _, stderr) = text(&dir, &args);
        assert_eq!(code, 0, "{args:?}: {stderr}");
    }
}

#[test]
fn dependency_and_overview_commands_answer_json() {
    let dir = fixture("overview");
    json(&dir, &["add", "Эпик"]);
    json(&dir, &["add", "Первый", "--parent", "TASQ-1"]);
    json(&dir, &["add", "Второй", "--parent", "TASQ-1"]);

    let linked = json(&dir, &["dep", "add", "TASQ-1.2", "TASQ-1.1"]);
    assert_eq!(linked["from"], Value::from("TASQ-1.2"));
    json(&dir, &["dep", "ls"]);
    let graph = json(&dir, &["dep", "graph", "TASQ-1.2"]);
    assert_eq!(graph["node"], Value::from("TASQ-1.2"));
    let mermaid = json(&dir, &["dep", "graph", "TASQ-1.2", "--mermaid"]);
    assert!(
        mermaid["mermaid"]
            .as_str()
            .unwrap_or_default()
            .starts_with("graph LR"),
        "mermaid есть и в машиночитаемом режиме: {mermaid}"
    );
    assert!(mermaid["graph"]["node"].is_string());
    json(&dir, &["dep", "rm", "TASQ-1.2", "TASQ-1.1"]);

    json(&dir, &["next", "--limit", "2"]);
    json(&dir, &["ctx", "TASQ-1.1"]);
    json(&dir, &["board"]);
    json(&dir, &["time"]);
    json(&dir, &["time", "--parent", "TASQ-1"]);
    json(
        &dir,
        &["bulk", "set", "TASQ-1.1", "TASQ-1.2", "--status", "todo"],
    );
    json(&dir, &["find", "Первый"]);
    json(&dir, &["risk"]);
    json(&dir, &["risk", "--threshold", "0.9"]);

    for metric in ["count", "points", "hours"] {
        json(&dir, &["burnup", "--metric", metric]);
    }
    json(&dir, &["burnup", "--ascii", "--width", "40"]);
    json(
        &dir,
        &[
            "burnup",
            "--since",
            &today_utc_minus(30),
            "--until",
            &today_utc_minus(0),
            "--snapshot",
        ],
    );

    // Человекочитаемые ветки тех же команд.
    for args in [
        vec!["dep", "add", "TASQ-1.2", "TASQ-1.1"],
        vec!["dep", "ls"],
        vec!["dep", "graph", "TASQ-1.2"],
        vec!["dep", "graph", "TASQ-1.2", "--mermaid"],
        vec!["dep", "rm", "TASQ-1.2", "TASQ-1.1"],
        vec!["next"],
        vec!["ctx", "TASQ-1.1"],
        vec!["board"],
        vec!["time"],
        vec!["bulk", "set", "TASQ-1.1", "--status", "todo"],
        vec!["find", "Первый"],
        vec!["burnup", "--ascii"],
        vec!["risk"],
    ] {
        let (code, _, stderr) = text(&dir, &args);
        assert_eq!(code, 0, "{args:?}: {stderr}");
    }
}

#[test]
fn note_comment_template_commands_answer_json() {
    let dir = fixture("memory");
    json(&dir, &["add", "Эпик"]);

    let note = json(
        &dir,
        &[
            "note",
            "save",
            "--title",
            "Решение",
            "--content",
            "включаем WAL",
            "--type",
            "decision",
            "--node",
            "TASQ-1",
        ],
    );
    let note_id = note["id"].to_string();
    json(&dir, &["note", "ls"]);
    json(&dir, &["note", "ls", "--type", "decision"]);
    json(&dir, &["note", "search", "WAL"]);
    json(
        &dir,
        &[
            "note",
            "save",
            "--id",
            &note_id,
            "--title",
            "Решение (уточнено)",
            "--content",
            "включаем WAL и сброс по времени",
        ],
    );
    json(&dir, &["note", "rm", &note_id]);

    let comment = json(
        &dir,
        &["comment", "add", "TASQ-1", "первый", "--author", "agent-a"],
    );
    let comment_id = comment["id"].to_string();
    json(&dir, &["comment", "ls", "TASQ-1"]);
    json(
        &dir,
        &["comment", "rm", &comment_id, "--reason", "устарело"],
    );
    json(&dir, &["comment", "ls", "TASQ-1", "--include-deleted"]);
    json(&dir, &["comment", "restore", &comment_id]);

    let tree = r#"[{"title":"{resource}: контроллер","children":[{"title":"{resource}: тесты"}]}]"#;
    json(
        &dir,
        &[
            "template",
            "save",
            "api",
            "--tree",
            tree,
            "--description",
            "Каркас API",
        ],
    );
    json(&dir, &["template", "ls"]);
    json(&dir, &["template", "show", "api"]);
    json(
        &dir,
        &[
            "template",
            "apply",
            "api",
            "--parent",
            "TASQ-1",
            "--var",
            "resource=orders",
        ],
    );
    json(
        &dir,
        &["template", "apply", "api", "--var", "resource=sales"],
    );
    json(&dir, &["template", "rm", "api"]);

    // Человекочитаемые ветки.
    for args in [
        vec!["note", "save", "--title", "Т", "--content", "C"],
        vec!["note", "ls"],
        vec!["note", "search", "C"],
        vec!["comment", "add", "TASQ-1", "второй"],
        vec!["comment", "ls", "TASQ-1"],
        vec!["template", "save", "basic", "--tree", tree],
        vec!["template", "ls"],
        vec!["template", "show", "basic"],
        vec!["template", "apply", "basic", "--var", "resource=root"],
        vec!["template", "rm", "basic"],
    ] {
        let (code, _, stderr) = text(&dir, &args);
        assert_eq!(code, 0, "{args:?}: {stderr}");
    }
}

#[test]
fn transfer_and_tooling_commands_answer_json() {
    let dir = fixture("transfer");
    json(&dir, &["add", "Эпик"]);

    let document = json(&dir, &["export"]);
    assert_eq!(document["schema"], Value::from("tasq.export.v1"));
    // `--out` перенаправляет вывод в файл: в stdout ничего нет, поэтому проверяется файл.
    let (code, stdout, stderr) = text(&dir, &["export", "--out", "dump.json", "--include-deleted"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("exported to dump.json"),
        "сводка печатается и при --out: {stdout}"
    );
    assert!(dir.join("dump.json").is_file());

    let dry = json(&dir, &["import", "dump.json", "--dry-run"]);
    assert_eq!(dry["mode"], Value::from("dry-run"));
    json(&dir, &["import", "dump.json"]);
    json(&dir, &["import", "dump.json", "--mode", "replace"]);
    let (code, _, stderr) = text(&dir, &["export", "--out", "text.json"]);
    assert_eq!(code, 0, "{stderr}");

    json(&dir, &["tokens"]);
    json(&dir, &["tokens", "--profile", "agent", "--per-tool"]);
    json(&dir, &["tokens", "--max", "6000"]);

    let report = json(&dir, &["doctor"]);
    assert_eq!(report["ok"], Value::from(true));
    let swept = json(&dir, &["doctor", "--gc"]);
    assert_eq!(swept["report"]["ok"], Value::from(true));
    json(&dir, &["doctor", "--check"]);

    // `connect` пишет чужие конфиги, поэтому в этом тесте — только чтение и dry-run.
    json(&dir, &["connect", "--dry-run"]);
    json(&dir, &["connect", "claude-code", "--dry-run"]);
    json(&dir, &["connect", "goose", "--dry-run"]);
    let (code, error) = refused(&dir, &["connect", "claude-desktop", "--check"]);
    assert_eq!(code, 1, "незаписанный конфиг — это не успех: {error}");

    let docs = temp_dir("surface-docs");
    json(&dir, &["docs", "--out", docs.to_str().expect("path")]);
    json(
        &dir,
        &["docs", "--out", docs.to_str().expect("path"), "--check"],
    );

    // Человекочитаемые ветки.
    for args in [
        vec!["import", "text.json", "--dry-run"],
        vec!["tokens"],
        vec!["tokens", "--per-tool"],
        vec!["doctor"],
        vec!["doctor", "--gc"],
        vec!["connect"],
        vec!["connect", "claude-code", "--dry-run"],
        vec!["docs", "--out", docs.to_str().expect("path")],
        vec!["docs", "--out", docs.to_str().expect("path"), "--check"],
    ] {
        let (code, _, stderr) = text(&dir, &args);
        assert_eq!(code, 0, "{args:?}: {stderr}");
    }
}

/// Ветки разбора аргументов, до которых обычный сценарий не доходит: недопустимые значения.
/// Каждая — доменная ошибка `INVALID_VALUE`/`INVALID_KEY`, а не паника и не `STORAGE_ERROR`.
#[test]
fn argument_parsing_refuses_bad_values() {
    let dir = fixture("badargs");
    // Узел нужен, иначе отказ придёт про ненайденную ссылку, а не про значение флага.
    json(&dir, &["add", "Эпик"]);
    json(&dir, &["add", "Задача", "--parent", "TASQ-1"]);

    for (args, expected) in [
        (vec!["add", "Узел", "--kind", "nonsense"], "INVALID_VALUE"),
        (vec!["add", "Узел", "--priority", "urgent"], "INVALID_VALUE"),
        (vec!["add", "Узел", "--due", "01.12.2099"], "INVALID_VALUE"),
        // Нечисловое значение отбивает сам clap: это неверные аргументы (код 2), а не
        // доменный отказ, поэтому код возврата отличается.
        (vec!["add", "Узел", "--weight", "abc"], "CLAP"),
        (
            vec!["set", "TASQ-1", "--status", "nonsense"],
            "INVALID_VALUE",
        ),
        (
            vec!["set", "TASQ-1", "--priority", "nonsense"],
            "INVALID_VALUE",
        ),
        (vec!["ls", "--status", "nonsense"], "INVALID_VALUE"),
        (
            vec!["bulk", "set", "--status", "nonsense", "TASQ-1"],
            "INVALID_VALUE",
        ),
        (vec!["claim", "TASQ-1", "--ttl", "0"], "INVALID_VALUE"),
        (vec!["burnup", "--metric", "nonsense"], "INVALID_VALUE"),
        (vec!["tokens", "--profile", "nonsense"], "INVALID_VALUE"),
        (vec!["connect", "no-such-client"], "INVALID_VALUE"),
        (
            vec!["connect", "claude-code", "--scope", "nonsense"],
            "INVALID_VALUE",
        ),
    ] {
        let (code, error) = refused(&dir, &args);
        if expected == "CLAP" {
            assert_eq!(code, 2, "{args:?}: clap обязан отбить значение");
            continue;
        }
        assert_eq!(code, 1, "{args:?}: ожидался отказ, код {code}");
        assert_eq!(error, expected, "{args:?}");
    }

    // Выпуск с недопустимым исходом — тоже отказ разбора.
    let lease = json(&dir, &["add", "Для аренды"]);
    let dot = lease["dot_id"].as_str().expect("dot").to_string();
    let claimed = json(&dir, &["claim", &dot, "--holder", "agent-a"]);
    let token = claimed["token"].as_str().expect("token").to_string();
    let (code, _) = refused(&dir, &["release", &token, "--outcome", "nonsense"]);
    assert_eq!(code, 1);

    // Недопустимый ключ проекта — через команду создания.
    let (code, error) = refused(&dir, &["create", "project", "toolongkey"]);
    assert_eq!(code, 1);
    assert_eq!(error, "INVALID_KEY");
}
