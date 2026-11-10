//! `tasq load`: восстановление базы из дампа.
//!
//! Проверяется то, ради чего команда существует: состояние переносится целиком (все проекты,
//! аренды, резервы, журнал), восстановленная база работает как обычная, а данные, которые
//! текущая схема не принимает, попадают в отчёт с адресом узла, а не теряются молча.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tasq")
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-load-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn run_in(dir: &Path, db: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(bin())
        .current_dir(dir)
        .env("TASQ_DB", db)
        .args(args)
        .output()
        .expect("run tasq");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// Живая база с двумя проектами и всеми видами записей: узлы, аренда с резервом, ребро,
/// комментарий, журнал.
fn seeded(tag: &str) -> (PathBuf, PathBuf) {
    let dir = temp_dir(tag);
    let db = dir.join("source.db");
    let steps: &[&[&str]] = &[
        &["init", "--key", "ALPHA"],
        &["add", "Эпик ALPHA"],
        &["add", "Задача ALPHA", "--parent", "ALPHA-1"],
        // Ребро — только между соседями по ветке (родитель не ждёт своего ребёнка).
        &["add", "Вторая задача ALPHA", "--parent", "ALPHA-1"],
        &["dep", "add", "ALPHA-1.2", "ALPHA-1.1", "--kind", "blocks"],
        &["comment", "add", "ALPHA-1.1", "первый комментарий"],
        &[
            "claim",
            "ALPHA-1.1",
            "--holder",
            "vostrikov",
            "--reserve",
            "src",
        ],
        &["create", "project", "DEV"],
        &["--project", "DEV", "add", "Эпик DEV"],
    ];
    for args in steps {
        let (code, stdout, stderr) = run_in(&dir, &db, args);
        assert_eq!(code, 0, "{args:?}: {stdout}{stderr}");
    }
    (dir, db)
}

fn dump_to(dir: &Path, db: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let (code, stdout, stderr) = run_in(
        dir,
        db,
        &["export", "--all", "--out", path.to_str().expect("path")],
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    path
}

fn counts(db: &Path) -> Vec<(String, i64)> {
    let conn = rusqlite::Connection::open(db).expect("open");
    tasq::transfer::DUMP_TABLES
        .iter()
        .map(|table| {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap_or_else(|err| panic!("{table}: {err}"));
            ((*table).to_string(), count)
        })
        .collect()
}

/// Главное свойство: дамп → новая база = те же счётчики по всем таблицам, и восстановленная
/// база обслуживает обычные команды (поиск через FTS, доска, время).
#[test]
fn a_dump_restores_every_table_and_the_result_works() {
    let (dir, source) = seeded("full");
    let dump = dump_to(&dir, &source, "dump.json");
    let target = dir.join("restored.db");

    let (code, stdout, stderr) = run_in(&dir, &target, &["load", dump.to_str().expect("p")]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("committed"), "{stdout}");

    assert_eq!(counts(&source), counts(&target), "счётчики разошлись");

    // Найденное по FTS: индексы перестраиваются триггерами при вставке, а не переносятся.
    // В восстановленной базе два проекта: чтение требует выбор, это не регресс. Ищем по
    // заголовку узла — именно он попадает в FTS, а не текст комментария.
    let (code, found, _) = run_in(&dir, &target, &["--project", "ALPHA", "find", "Вторая"]);
    assert_eq!(code, 0, "{found}");
    assert!(found.contains("ALPHA-1.2"), "поиск не работает: {found}");

    // Аренда жива: она восстановлена как состояние, а не как действие.
    let (code, shown, _) = run_in(&dir, &target, &["show", "DEV-1"]);
    assert_eq!(code, 0);
    assert!(shown.contains("Эпик DEV"), "{shown}");
    let (code, _, _) = run_in(&dir, &target, &["board", "--project", "ALPHA"]);
    assert_eq!(code, 0, "доска не открылась на восстановленной базе");
}

/// YAML-дамп восстанавливается так же, как JSON: формат обмена не влияет на результат.
#[test]
fn a_yaml_dump_restores_the_same_state() {
    let (dir, source) = seeded("yaml");
    let dump = dump_to(&dir, &source, "dump.yaml");
    let target = dir.join("from-yaml.db");
    let (code, stdout, stderr) = run_in(&dir, &target, &["load", dump.to_str().expect("p")]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert_eq!(counts(&source), counts(&target));
}

/// Узел со статусом, которого текущая схема не знает, — потеря с адресом узла, код 3 и откат:
/// половинчатая база не остаётся.
#[test]
fn an_unknown_status_is_reported_with_the_node_address() {
    let (dir, source) = seeded("badstatus");
    let dump_path = dump_to(&dir, &source, "dump.json");
    let mut document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&dump_path).expect("read")).expect("json");

    for row in document["tables"]["nodes"].as_array_mut().expect("nodes") {
        if row["path"] == "1.1" {
            row["status"] = serde_json::Value::from("delayed");
        }
    }
    let broken = dir.join("broken.json");
    fs::write(&broken, serde_json::to_string(&document).expect("ser")).expect("write");

    let target = dir.join("lossy.db");
    let (code, stdout, stderr) = run_in(&dir, &target, &["load", broken.to_str().expect("p")]);
    assert_eq!(
        code, 3,
        "потери должны давать код 3\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("rolled back"), "{stdout}");
    assert!(
        stdout.contains("ALPHA-1.1"),
        "адрес узла не назван: {stdout}"
    );
    assert!(
        stdout.contains("CHECK") || stdout.contains("status") || stdout.contains("статус"),
        "причина не названа: {stdout}"
    );

    // Откат: база пуста, а не половинчата.
    let nodes: i64 = rusqlite::Connection::open(&target)
        .expect("open")
        .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
        .expect("count");
    assert_eq!(nodes, 0, "после отчёта в базе остались строки");
}

/// Поле, которого больше нет в схеме, — потеря явная и не блокирует остальное: остальные
/// строки и поля переносятся, код 3 сообщает, что не всё.
#[test]
fn a_dropped_column_is_named_in_the_report() {
    let (dir, source) = seeded("dropped");
    let dump_path = dump_to(&dir, &source, "dump.json");
    let mut document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&dump_path).expect("read")).expect("json");

    // Поле из будущего: так выглядит дамп, снятый на схеме новее текущей.
    for row in document["tables"]["nodes"].as_array_mut().expect("nodes") {
        row["assignee_email"] = serde_json::Value::from("nobody@example.com");
    }
    let future = dir.join("future.json");
    fs::write(&future, serde_json::to_string(&document).expect("ser")).expect("write");

    let target = dir.join("dropped.db");
    let (code, stdout, stderr) = run_in(&dir, &target, &["load", future.to_str().expect("p")]);
    assert_eq!(code, 3, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("assignee_email"),
        "поле не названо: {stdout}"
    );
}

/// Узел, перенесённый под родителя с БОЛЬШИМ id, восстанавливается: дамп отсортирован по id,
/// и без отложенных внешних ключей вставка падала бы на строке, чей родитель идёт позже.
/// Именно так выглядит живая база после `mv` — и именно на ней это и обнаружилось.
#[test]
fn a_child_with_a_larger_parent_id_still_loads() {
    let dir = temp_dir("reorder");
    let source = dir.join("source.db");
    for args in [
        &["init", "--key", "REO"][..],
        &["add", "Эпик"][..],
        &["add", "Ребёнок", "--parent", "REO-1"][..],
        &["add", "Новый родитель", "--parent", "REO-1"][..],
        // Ребёнок получает родителя, созданного позже: id(ребёнок) < id(родитель).
        &["mv", "REO-1.1", "--parent", "REO-1.2"][..],
    ] {
        let (code, stdout, stderr) = run_in(&dir, &source, args);
        assert_eq!(code, 0, "{args:?}: {stdout}{stderr}");
    }
    let dump = dump_to(&dir, &source, "dump.json");
    let target = dir.join("restored.db");
    let (code, stdout, stderr) = run_in(&dir, &target, &["load", dump.to_str().expect("p")]);
    assert_eq!(
        code, 0,
        "порядок строк дампа не должен ломать вставку:\n{stdout}{stderr}"
    );
    assert_eq!(counts(&source), counts(&target));
}

/// Обязательное поле, которого в дампе нет, — все строки таблицы не переносятся, и об этом
/// сказано один раз, а не строкой на каждую запись.
#[test]
fn a_missing_required_column_is_one_loss_not_a_flood() {
    let (dir, source) = seeded("required");
    let dump_path = dump_to(&dir, &source, "dump.json");
    let mut document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&dump_path).expect("read")).expect("json");

    // Узлов в базе несколько: если бы отчёт строился по строке на каждую, «одна потеря»
    // превратилась бы в список одинаковых причин.
    for row in document["tables"]["nodes"].as_array_mut().expect("nodes") {
        if let Some(object) = row.as_object_mut() {
            object.remove("title");
        }
    }
    let old = dir.join("old.json");
    fs::write(&old, serde_json::to_string(&document).expect("ser")).expect("write");

    let target = dir.join("required.db");
    let (code, stdout, stderr) = run_in(&dir, &target, &["load", old.to_str().expect("p")]);
    assert_eq!(code, 3, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("title"), "поле не названо: {stdout}");
    let mentions = stdout.matches("обязательных полей").count();
    assert_eq!(
        mentions, 1,
        "одна потеря на таблицу, а не на строку: {stdout}"
    );
}

/// Чужой файл и дамп из будущего отвергаются до записи: восстановление — операция над базой
/// целиком, и «наполовину чужой» файл не должен её трогать.
#[test]
fn foreign_and_future_documents_are_refused_before_writing() {
    let dir = temp_dir("foreign");
    let target = dir.join("target.db");

    let not_a_dump = dir.join("other.json");
    fs::write(&not_a_dump, r#"{"kind":"something-else","tables":{}}"#).expect("write");
    let (code, _, stderr) = run_in(&dir, &target, &["load", not_a_dump.to_str().expect("p")]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("INVALID_VALUE"), "{stderr}");

    let future = dir.join("v9.json");
    fs::write(
        &future,
        r#"{"kind":"tasq-dump","format":1,"schema_version":9,"tables":{}}"#,
    )
    .expect("write");
    let (code, _, stderr) = run_in(&dir, &target, &["load", future.to_str().expect("p")]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("SCHEMA_TOO_NEW"), "{stderr}");

    let nodes: i64 = rusqlite::Connection::open(&target)
        .map(|conn| {
            conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
                .unwrap_or(0)
        })
        .unwrap_or(0);
    assert_eq!(nodes, 0);
}

/// В непустую базу без `--force` заливка не идёт: иначе одна опечатка в `--db` стирает
/// рабочее состояние.
#[test]
fn a_non_empty_target_needs_force() {
    let (dir, source) = seeded("force");
    let dump = dump_to(&dir, &source, "dump.json");
    let target = dir.join("busy.db");
    assert_eq!(run_in(&dir, &target, &["init", "--key", "BUSY"]).0, 0);
    assert_eq!(run_in(&dir, &target, &["add", "местный узел"]).0, 0);

    let (code, _, stderr) = run_in(&dir, &target, &["load", dump.to_str().expect("p")]);
    assert_eq!(code, 1, "без --force заливка не должна идти: {stderr}");
    assert!(stderr.contains("--force"), "{stderr}");

    // С --force — идёт, но строки дампа ключуются своими id, поэтому база смешивается;
    // это осознанное действие, и оно должно хотя бы не падать на конфликте.
    let (code, stdout, stderr) = run_in(
        &dir,
        &target,
        &["load", dump.to_str().expect("p"), "--force"],
    );
    assert!(
        code == 0 || code == 3,
        "с --force ожидается 0 (перенесено) или 3 (отчёт о потерях): {stdout}{stderr}"
    );
}
