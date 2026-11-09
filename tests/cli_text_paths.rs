//! Человекочитаемый вывод: строки, флаги и подсказки, которые видит человек в терминале.
//!
//! Машиночитаемые ветки закреплены отдельно; здесь проверяется то, что печатается без
//! `--json`: доска с флагами просрочки и аренды, отчёт по времени, список связей,
//! риск, дедлайн в будущем, восстановление комментария и расхождение документации.

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
    let dir = std::env::temp_dir().join(format!("tasq-text-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn run(dir: &Path, args: &[&str]) -> (i32, String, String) {
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

fn ok(dir: &Path, args: &[&str]) -> String {
    let (code, stdout, stderr) = run(dir, args);
    assert_eq!(code, 0, "tasq {args:?}: {stderr}{stdout}");
    stdout
}

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
        "tasq --json {args:?}: {stderr}{stdout}"
    );
    serde_json::from_str(&stdout).unwrap_or_else(|err| panic!("не JSON ({err}): {stdout}"))
}

fn fixture(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    ok(&dir, &["init", "--key", "TASQ"]);
    dir
}

/// Доска помечает просрочку и держателя аренды прямо в строке.
#[test]
fn board_flags_overdue_and_lease() {
    let dir = fixture("board");
    json(&dir, &["add", "Эпик"]);
    json(
        &dir,
        &[
            "add",
            "Просроченная",
            "--parent",
            "TASQ-1",
            "--due",
            "2001-01-01",
        ],
    );
    json(&dir, &["add", "В работе", "--parent", "TASQ-1"]);
    json(&dir, &["claim", "TASQ-1.2", "--holder", "agent-a"]);

    let text = ok(&dir, &["board"]);
    assert!(text.contains("OVERDUE"), "{text}");
    assert!(text.contains("[agent-a]"), "{text}");
    assert!(text.contains("leased 1"), "{text}");
}

/// Отчёт по времени: строка работы с оценкой и итог по проекту.
#[test]
fn time_report_prints_rows_and_totals() {
    let dir = fixture("time");
    json(
        &dir,
        &["add", "Задача", "--estimate", "3.5", "--weight", "2"],
    );
    let lease = json(&dir, &["claim", "TASQ-1", "--holder", "agent-a"]);
    let token = lease["token"].as_str().expect("token").to_string();
    json(
        &dir,
        &["release", &token, "--outcome", "completed", "--force"],
    );

    let text = ok(&dir, &["time"]);
    assert!(text.contains("TASQ-1"), "{text}");
    assert!(text.contains("est"), "{text}");
    assert!(text.contains("attempt(s)"), "{text}");
    assert!(text.contains("total:"), "{text}");
    assert!(text.contains("estimated"), "{text}");
}

/// Список связей печатает вид и оба конца.
#[test]
fn dependency_list_prints_links() {
    let dir = fixture("deps");
    json(&dir, &["add", "Эпик"]);
    json(&dir, &["add", "A", "--parent", "TASQ-1"]);
    json(&dir, &["add", "B", "--parent", "TASQ-1"]);
    json(&dir, &["dep", "add", "TASQ-1.2", "TASQ-1.1"]);

    let text = ok(&dir, &["dep", "ls"]);
    assert!(text.contains("blocks"), "{text}");
    assert!(text.contains("TASQ-1.2 -> TASQ-1.1"), "{text}");
}

/// Риск печатает строки с остатком в днях. Чтобы разрыв был виден, узел должен быть
/// создан давно: `created_at` задаётся только через импорт, API его не выставляет.
#[test]
fn risk_prints_rows() {
    let dir = fixture("risk");

    let data = serde_json::json!({
        "project": { "key": "TASQ", "name": "демо", "status": "active" },
        "nodes": [
            { "path": "1", "kind": "epic", "title": "Эпик", "status": "planned",
              "due_date": "2021-01-01", "created_at": "2020-01-01T00:00:00.000Z" },
            { "path": "2", "kind": "epic", "title": "Позже", "status": "planned",
              "due_date": "2022-01-01", "created_at": "2021-12-01T00:00:00.000Z" }
        ]
    });
    let document = serde_json::json!({
        "schema": tasq::transfer::SCHEMA,
        "version": "test",
        "exported_at": "2026-11-09T00:00:00.000Z",
        "sha256": tasq::transfer::payload_checksum(&data),
        "data": data,
    });
    let path = dir.join("backdated.json");
    fs::write(&path, serde_json::to_string(&document).expect("json")).expect("write");
    ok(&dir, &["import", path.to_str().expect("path")]);

    let text = ok(&dir, &["risk", "--threshold", "0"]);
    assert!(text.contains("TASQ-1"), "{text}");
    assert!(text.contains("overdue"), "{text}");
    assert!(text.contains("gap"), "{text}");
    // Порядок по разрыву: первый узел сильнее отстаёт.
    let first = text.find("TASQ-1").expect("первый узел");
    let second = text.find("TASQ-2").expect("второй узел");
    assert!(first < second, "первым идёт самый отстающий: {text}");
}

/// Дедлайн в будущем считается вперёд, а не «прошёл».
#[test]
fn burnup_reports_a_future_deadline() {
    let dir = fixture("burnup");
    json(&dir, &["add", "Эпик", "--due", "2099-01-01"]);

    let text = ok(&dir, &["burnup"]);
    assert!(text.contains("deadline in"), "{text}");
    assert!(!text.contains("deadline passed"), "{text}");
}

/// Удаление заметки и восстановление комментария говорят об этом словами.
#[test]
fn memory_commands_speak_plainly() {
    let dir = fixture("memory");
    json(&dir, &["add", "Эпик"]);
    let note = json(&dir, &["note", "save", "--title", "Т", "--content", "C"]);
    let note_id = note["id"].to_string();

    let text = ok(&dir, &["note", "rm", &note_id]);
    assert!(text.contains("removed note #"), "{text}");

    let comment = json(&dir, &["comment", "add", "TASQ-1", "замечание"]);
    let comment_id = comment["id"].to_string();
    ok(
        &dir,
        &["comment", "rm", &comment_id, "--reason", "устарело"],
    );
    let text = ok(&dir, &["comment", "restore", &comment_id]);
    assert!(text.contains("restored comment #"), "{text}");
}

/// Живые аренды печатаются с держателем и сроком.
#[test]
fn holders_lists_leases_in_text() {
    let dir = fixture("holders");
    json(&dir, &["add", "Задача"]);
    json(
        &dir,
        &[
            "claim",
            "TASQ-1",
            "--holder",
            "agent-a",
            "--checkpoint",
            "работаю",
        ],
    );

    let text = ok(&dir, &["holders"]);
    assert!(text.contains("agent-a"), "{text}");
    assert!(text.contains("TASQ-1"), "{text}");
}

/// `docs --check` на изменённом файле: код 3, перечень расхождений и подсказка.
#[test]
fn docs_check_reports_drift() {
    let dir = fixture("docs");
    let out = temp_dir("docs-out");
    ok(&dir, &["docs", "--out", out.to_str().expect("path")]);

    // Сначала всё совпадает.
    let (code, _, stderr) = run(
        &dir,
        &["docs", "--out", out.to_str().expect("path"), "--check"],
    );
    assert_eq!(code, 0, "{stderr}");

    // Правим сгенерированный файл руками — это и есть расхождение.
    let target = out.join("cli.md");
    let text = fs::read_to_string(&target).expect("read");
    fs::write(&target, format!("{text}\nручная правка\n")).expect("write");

    let (code, _, stderr) = run(
        &dir,
        &["docs", "--out", out.to_str().expect("path"), "--check"],
    );
    assert_eq!(code, 3, "{stderr}");
    assert!(stderr.contains("cli.md"), "{stderr}");
    assert!(stderr.contains("tasq docs"), "подсказка: {stderr}");

    // В машиночитаемом режиме — документ с признаком и перечнем.
    let out_json = Command::new(bin())
        .current_dir(&dir)
        .args([
            "--json",
            "docs",
            "--out",
            out.to_str().expect("path"),
            "--check",
        ])
        .output()
        .expect("run");
    assert_eq!(out_json.status.code(), Some(3));
    let payload: Value = serde_json::from_slice(&out_json.stdout).expect("json при расхождении");
    assert_eq!(payload["ok"], Value::from(false));
    assert!(
        payload["drift"]
            .as_array()
            .is_some_and(|list| !list.is_empty())
    );
}

/// Относительный путь в `--db` считается от текущего каталога.
#[test]
fn relative_database_path_lands_in_the_cwd() {
    let dir = temp_dir("relative-db");
    let (code, stdout, stderr) = run(&dir, &["init", "--key", "TASQ", "--db", "nested/base.db"]);
    assert_eq!(code, 0, "{stderr}{stdout}");
    assert!(dir.join("nested/base.db").is_file(), "{stdout}");

    // И команда находит эту базу по тому же относительному пути.
    let text = ok(&dir, &["--db", "nested/base.db", "ls"]);
    assert!(text.contains("no nodes"), "{text}");
}
