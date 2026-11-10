//! `tasq export --all`: дамп ВСЕЙ базы, а не одного проекта.
//!
//! Проверяется наблюдаемый результат: какие таблицы попали в документ, совпадают ли счётчики
//! с фактическими, и одинаковы ли две выгрузки одного состояния (контрольная сумма не зависит
//! от момента).

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use rusqlite::Connection;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tasq")
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-dump-{tag}-{nanos}"));
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

/// База с двумя проектами и полным набором связанных записей: узлы, аренда с резервом,
/// ребро, комментарий, запись журнала. Именно этого набора не хватало документу одного проекта.
fn seeded(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);
    assert_eq!(run(&dir, &["add", "Эпик ALPHA"]).0, 0);
    assert_eq!(
        run(&dir, &["add", "Задача ALPHA", "--parent", "ALPHA-1"]).0,
        0
    );
    assert_eq!(run(&dir, &["create", "project", "DEV"]).0, 0);
    assert_eq!(run(&dir, &["--project", "DEV", "add", "Эпик DEV"]).0, 0);

    // Аренда с резервом: в документ проекта ни то, ни другое не попадает.
    let (code, _, stderr) = run(
        &dir,
        &[
            "--project",
            "DEV",
            "claim",
            "DEV-1",
            "--holder",
            "vostrikov",
            "--reserve",
            "src",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    // Комментарий и запись журнала (журнал пишется любым действием, комментарий — своим).
    assert_eq!(
        run(
            &dir,
            &["--project", "DEV", "comment", "add", "DEV-1", "первый"]
        )
        .0,
        0
    );
    dir
}

fn db_counts(dir: &std::path::Path) -> Vec<(String, i64)> {
    let conn = Connection::open(dir.join(".tasq").join("tasq.db")).expect("open db");
    let mut counts = Vec::new();
    for table in tasq::transfer::DUMP_TABLES {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap_or_else(|err| panic!("{table}: {err}"));
        counts.push(((*table).to_string(), count));
    }
    counts
}

#[test]
fn the_dump_has_every_table_with_matching_counts() {
    let dir = seeded("counts");
    let (code, stdout, stderr) = run(&dir, &["export", "--all"]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let document: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");

    assert_eq!(document["kind"], "tasq-dump");
    assert_eq!(document["format"], 1);
    assert_eq!(
        document["schema_version"],
        tasq::db::SUPPORTED_SCHEMA_VERSION
    );

    let tables = document["tables"].as_object().expect("tables");
    for table in tasq::transfer::DUMP_TABLES {
        assert!(tables.contains_key(*table), "в дампе нет таблицы {table}");
    }
    // FTS-индексы в дамп не идут: они перестраиваются из nodes/notes.
    assert!(!tables.contains_key("nodes_fts"), "индекс попал в дамп");
    assert!(!tables.contains_key("notes_fts"), "индекс попал в дамп");
    // Справочник статусов — часть схемы, а не данных: он создаётся миграцией и обязан
    // совпадать с `Status::all()`. В дампе он был бы попыткой наложить данные на схему.
    assert!(
        !tables.contains_key("statuses"),
        "справочник статусов попал в дамп"
    );
    // История переходов — данные, и она обязана ехать вместе с узлами.
    assert!(
        tables.contains_key("node_status_history"),
        "история переходов не попала в дамп"
    );

    for (table, expected) in db_counts(&dir) {
        let in_document = tables[&table].as_array().expect("array").len() as i64;
        assert_eq!(
            in_document, expected,
            "{table}: в дампе {in_document}, в базе {expected}"
        );
        assert_eq!(
            document["counts"][&table].as_i64(),
            Some(expected),
            "{table}: счётчик в конверте расходится с таблицей"
        );
    }

    // То, из-за чего дамп и делался: аренды и журнал на месте, проектов больше одного.
    assert_eq!(document["counts"]["projects"], 2);
    assert!(
        document["counts"]["attempts"].as_i64().unwrap_or(0) >= 1,
        "аренды не попали в дамп: {stdout}"
    );
    assert!(
        document["counts"]["activity_log"].as_i64().unwrap_or(0) >= 4,
        "журнал не попал в дамп"
    );
}

/// Две выгрузки одного состояния дают одну контрольную сумму: момент выгрузки и версия
/// бинаря в неё не входят, иначе проверять целостность было бы нечем.
#[test]
fn two_dumps_of_the_same_state_are_identical() {
    let dir = seeded("stable");
    let (code, first, _) = run(&dir, &["export", "--all"]);
    assert_eq!(code, 0);
    let (code, second, _) = run(&dir, &["export", "--all"]);
    assert_eq!(code, 0);

    let first: serde_json::Value = serde_json::from_str(&first).expect("json");
    let second: serde_json::Value = serde_json::from_str(&second).expect("json");
    assert_eq!(
        first["sha256"], second["sha256"],
        "сумма зависит от чего-то, кроме состояния"
    );
    // Момент всё равно пишется — он нужен человеку, просто не входит в сумму.
    assert!(first["exported_at"].is_string());
}

/// `--out` пишет файл, а сводка называет проекты, узлы, аренды и версию схемы.
#[test]
fn out_writes_a_file_and_reports_what_it_contains() {
    let dir = seeded("out");
    let target = dir.join("dump.json");
    let (code, stdout, stderr) = run(
        &dir,
        &["export", "--all", "--out", target.to_str().expect("path")],
    );
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains(&format!("schema v{}", tasq::db::SUPPORTED_SCHEMA_VERSION)),
        "сводка без версии схемы: {stdout}"
    );
    assert!(stdout.contains("2 project(s)"), "{stdout}");
    assert!(stdout.contains("sha256"), "{stdout}");

    let written = fs::read_to_string(&target).expect("файл создан");
    let document: serde_json::Value = serde_json::from_str(&written).expect("valid json");
    assert_eq!(document["kind"], "tasq-dump");
    assert_eq!(document["counts"]["nodes"], 3);
}

/// Дамп всей базы работает без селектора проекта — в базе с двумя проектами это отдельное
/// свойство: обычный `export` без `--project` отвечает PROJECT_REQUIRED.
#[test]
fn the_dump_needs_no_project_selector() {
    let dir = seeded("noselector");
    let (code, stdout, stderr) = run(&dir, &["export", "--all"]);
    assert_eq!(code, 0, "{stdout}\n{stderr}");

    let (code, _, stderr) = run(&dir, &["export"]);
    assert_eq!(
        code, 1,
        "обычный экспорт без проекта должен требовать селектор"
    );
    assert!(stderr.contains("PROJECT_REQUIRED"), "{stderr}");
}

/// Документ одного проекта остаётся прежним: дамп — отдельный вид, а не замена.
#[test]
fn the_single_project_export_keeps_its_shape() {
    let dir = seeded("compat");
    let (code, stdout, _) = run(&dir, &["--project", "ALPHA", "export"]);
    assert_eq!(code, 0);
    let document: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert!(document.get("kind").is_none(), "экспорт проекта сменил вид");
    let data = document["data"].as_object().expect("data");
    assert!(data.contains_key("project"), "потерян проект: {data:?}");
    assert!(data.contains_key("nodes"));
    assert!(
        !data.contains_key("attempts"),
        "экспорт проекта обзавёлся арендами"
    );
}
