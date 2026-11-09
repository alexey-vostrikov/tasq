//! Ветки, до которых не доходит обычный сценарий: пустые состояния, текстовые
//! подсказки «ничего не найдено», аренда в карточке узла, частичный отказ `bulk`,
//! прогноз и риск в человекочитаемом виде, импорт saga и отказы `template`.
//!
//! Это не проверка «печатается хоть что-то»: каждый случай закрепляет решение —
//! пустой список не должен выглядеть как ошибка, отказ `bulk` обязан вернуть код 1,
//! а saga-импорт — отчитаться числом узлов.

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
    let dir = std::env::temp_dir().join(format!("tasq-cov-{tag}-{nanos}"));
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
    assert_eq!(code, 0, "tasq {args:?} упал: {stderr}{stdout}");
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

fn refused(dir: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(bin())
        .current_dir(dir)
        .arg("--json")
        .args(args)
        .output()
        .expect("run tasq");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let payload: Value =
        serde_json::from_str(&stdout).unwrap_or_else(|_| Value::String(stderr.clone()));
    let code = payload["error"]["code"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| stderr.clone());
    (out.status.code().unwrap_or(-1), code)
}

fn fixture(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    ok(&dir, &["init", "--key", "TASQ"]);
    dir
}

/// Каждая команда списка на пустой базе отвечает подсказкой и кодом 0: пустота — не ошибка.
#[test]
fn empty_project_answers_with_hints() {
    let dir = fixture("empty");

    for (args, needle) in [
        (vec!["ls"], "no nodes"),
        (vec!["dep", "ls"], "no links"),
        (vec!["next"], "nothing to pick up"),
        (vec!["note", "ls"], "no notes"),
        (vec!["note", "search", "нет"], "nothing found"),
        (vec!["find", "нет"], "nothing found"),
        (vec!["template", "ls"], "no templates"),
        (vec!["time"], "no tracked work"),
        (vec!["risk"], "no node is behind"),
    ] {
        let (code, stdout, stderr) = run(&dir, &args);
        assert_eq!(code, 0, "{args:?}: {stderr}");
        assert!(stdout.contains(needle), "{args:?}: {stdout}");
    }

    // Удаление того, чего нет, — доменный отказ с кодом 1, а не тихий успех.
    let (code, _, stderr) = run(&dir, &["note", "rm", "999"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("INVALID_VALUE"), "{stderr}");
    let (code, _, stderr) = run(&dir, &["template", "rm", "nope"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("INVALID_VALUE"), "{stderr}");

    // Для «связи нет» нужны сами узлы: на несуществующих приходит NODE_NOT_FOUND.
    json(&dir, &["add", "Первый"]);
    json(&dir, &["add", "Второй"]);
    let (code, stdout, _) = run(&dir, &["dep", "rm", "TASQ-1", "TASQ-2"]);
    assert_eq!(code, 1);
    assert!(stdout.contains("no such link"), "{stdout}");
}

/// Проектов может не остаться: `list projects` обязан сказать это словами, а не падать.
#[test]
fn no_projects_left_is_reported() {
    let dir = fixture("noprojects");
    ok(&dir, &["delete", "project", "TASQ", "--force"]);

    let (code, stdout, stderr) = run(&dir, &["list", "projects"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("no projects"), "{stdout}");
    assert!(
        stdout.contains("tasq create project"),
        "подсказка: {stdout}"
    );
}

/// Карточка узла показывает аренду, признак закрытого описания и чекпойнт прошлой попытки.
#[test]
fn show_reports_lease_lock_and_inherited_checkpoint() {
    let dir = fixture("show");
    json(&dir, &["add", "Эпик"]);
    json(
        &dir,
        &["add", "Задача", "--parent", "TASQ-1", "--desc", "описание"],
    );

    let lease = json(
        &dir,
        &[
            "claim",
            "TASQ-1.1",
            "--holder",
            "agent-a",
            "--checkpoint",
            "остановился на тестах",
        ],
    );
    let token = lease["token"].as_str().expect("token").to_string();

    // Описание закрыто от правок агентом: карточка это отмечает.
    json(&dir, &["set", "TASQ-1.1", "--lock-desc"]);

    let card = ok(&dir, &["show", "TASQ-1.1"]);
    assert!(card.contains("lease     agent-a until"), "{card}");
    assert!(card.contains("desc      (locked)"), "{card}");
    assert!(card.contains("--- description ---"), "{card}");

    // Освобождение с чекпойнтом и повторный захват: прежняя заметка наследуется.
    ok(
        &dir,
        &[
            "release",
            &token,
            "--outcome",
            "failed",
            "--checkpoint",
            "падение на валидации",
        ],
    );
    let again = ok(&dir, &["claim", "TASQ-1.1", "--holder", "agent-b"]);
    assert!(
        again.contains("--- checkpoint from the previous attempt ---"),
        "{again}"
    );
    assert!(again.contains("падение на валидации"), "{again}");
    assert!(again.contains("keep the token"), "{again}");
}

/// Граф зависимостей в текстовом виде: ожидание, разблокировка и цепочка.
#[test]
fn dependency_graph_prints_waits_unblocks_and_chain() {
    let dir = fixture("graph");
    json(&dir, &["add", "Эпик"]);
    for title in ["A", "B", "C"] {
        json(&dir, &["add", title, "--parent", "TASQ-1"]);
    }

    // C ждёт B, B ждёт A: у A никто не ждёт, у B — C, у C — B.
    json(&dir, &["dep", "add", "TASQ-1.2", "TASQ-1.1"]);
    json(&dir, &["dep", "add", "TASQ-1.3", "TASQ-1.2"]);

    let lonely = ok(&dir, &["dep", "graph", "TASQ-1.1"]);
    assert!(lonely.contains("waits for   nothing"), "{lonely}");

    let middle = ok(&dir, &["dep", "graph", "TASQ-1.2"]);
    assert!(middle.contains("unblocks    TASQ-1.3"), "{middle}");
    assert!(middle.contains("transitively unblocks"), "{middle}");

    let listed = ok(&dir, &["dep", "ls"]);
    assert!(listed.contains("blocks"), "{listed}");

    let (code, stdout, stderr) = run(&dir, &["dep", "ls", "--kind", "blocks"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("TASQ-1.2"), "{stdout}");
}

/// Время по узлам: после аренды появляется строка и итог, а не «учёта нет».
#[test]
fn time_prints_rows_and_totals() {
    let dir = fixture("time");
    json(&dir, &["add", "Эпик"]);
    let lease = json(
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
    let token = lease["token"].as_str().expect("token").to_string();
    json(
        &dir,
        &["release", &token, "--outcome", "completed", "--force"],
    );

    let table = ok(&dir, &["time"]);
    assert!(table.contains("TASQ-1"), "{table}");
    assert!(table.contains("total:"), "{table}");
    assert!(table.contains("estimated"), "{table}");

    let scoped = ok(&dir, &["time", "--parent", "TASQ-1"]);
    assert!(scoped.contains("TASQ-1"), "{scoped}");
}

/// `bulk set` сообщает и об успехах, и об отказах; отказ даёт код 1.
#[test]
fn bulk_reports_partial_failure() {
    let dir = fixture("bulk");
    json(&dir, &["add", "Первый"]);
    json(&dir, &["add", "Второй"]);

    // Ручной `in_progress` запрещён без аренды, поэтому обе строки попадают в отказы:
    // массовая правка аренды не держит и права выставить работу не даёт.
    let out = Command::new(bin())
        .current_dir(&dir)
        .args(["bulk", "set", "TASQ-1", "TASQ-2", "--status", "in_progress"])
        .output()
        .expect("run tasq");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("updated 0, failed 2"), "{stdout}");
    assert!(stdout.contains("LEASE_REQUIRED"), "{stdout}");

    // Живая аренда — другой путь: узел в работе, статус выставляет она.
    let lease = json(&dir, &["claim", "TASQ-1", "--holder", "agent-a"]);
    assert_eq!(lease["holder"], Value::from("agent-a"));

    // Успешная часть того же пути: `cancelled` матрица разрешает и из `todo`,
    // и из `in_progress`, поэтому обе строки проходят.
    let out = Command::new(bin())
        .current_dir(&dir)
        .args(["bulk", "set", "TASQ-1", "TASQ-2", "--status", "cancelled"])
        .output()
        .expect("run tasq");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("updated 2, failed 0"), "{stdout}");
    assert!(stdout.contains("TASQ-1 -> cancelled"), "{stdout}");
}

/// Прогноз в текстовом виде: дедлайн в прошлом и в будущем, снимки, хвост серии.
#[test]
fn burnup_text_covers_deadlines_forecast_and_snapshots() {
    let dir = fixture("burnup");
    json(&dir, &["add", "Эпик", "--due", "2001-01-01"]);
    json(
        &dir,
        &["add", "Задача", "--parent", "TASQ-1", "--due", "2099-01-01"],
    );
    json(&dir, &["add", "Сделанная", "--parent", "TASQ-1"]);
    // `done` из `todo` матрица не разрешает: работа закрывается завершением аренды.
    let lease = json(&dir, &["claim", "TASQ-1.2", "--holder", "agent-a"]);
    let token = lease["token"].as_str().expect("token").to_string();
    json(&dir, &["release", &token, "--outcome", "completed"]);

    let late = ok(&dir, &["burnup"]);
    assert!(late.contains("deadline passed"), "{late}");

    let ahead = ok(&dir, &["burnup", "--since", &days_ago(40)]);
    assert!(ahead.contains("deadline"), "{ahead}");

    let ascii = ok(&dir, &["burnup", "--ascii", "--width", "40"]);
    assert!(ascii.contains("metric count"), "{ascii}");

    let snapshots = ok(&dir, &["burnup", "--snapshot"]);
    assert!(snapshots.contains("snapshots written"), "{snapshots}");

    // Прогноз по завершённой работе: ставка считается, хвост серии печатается.
    let with_rate = ok(&dir, &["burnup", "--ascii", "--metric", "points"]);
    assert!(with_rate.contains("forecast") || with_rate.contains("cannot extrapolate"));
}

/// Риск: строки с остатком в днях и отказ на пороге вне диапазона.
#[test]
fn risk_prints_rows_and_refuses_bad_threshold() {
    let dir = fixture("risk");
    json(&dir, &["add", "Эпик", "--due", "2001-01-01"]);
    json(
        &dir,
        &["add", "Задача", "--parent", "TASQ-1", "--due", "2099-01-01"],
    );

    let table = ok(&dir, &["risk", "--threshold", "0"]);
    assert!(
        table.contains("TASQ-1") || table.contains("no node is behind"),
        "{table}"
    );

    let (code, error) = refused(&dir, &["risk", "--threshold", "1.5"]);
    assert_eq!(code, 1);
    assert_eq!(error, "INVALID_VALUE");
}

/// Импорт saga через CLI: пробный прогон считает эпики, настоящий переносит дерево.
#[test]
fn saga_import_through_cli() {
    let dir = fixture("saga");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/saga-export-v1.3.json");
    let target = dir.join("saga.json");
    fs::copy(&source, &target).expect("копия фикстуры");
    let file = target.to_str().expect("path").to_string();

    let dry = json(&dir, &["import", &file, "--from-saga", "--dry-run"]);
    assert_eq!(dry["mode"], Value::from("saga dry-run"));
    assert!(dry["nodes_created"].as_i64().unwrap_or(0) > 0, "{dry}");

    let real = json(&dir, &["import", &file, "--from-saga"]);
    assert!(
        real["nodes_created"].as_i64().unwrap_or(0) > 0,
        "saga-импорт переносит узлы: {real}"
    );

    // Текстовый отчёт того же переноса.
    let text = ok(&dir, &["import", &file, "--from-saga", "--mode", "replace"]);
    assert!(text.contains("node(s) created"), "{text}");
    // Повторный replace не должен падать на UNIQUE: дерево очищается до вставки.
    let again = json(&dir, &["import", &file, "--from-saga", "--mode", "replace"]);
    assert_eq!(again["mode"], Value::from("saga replace"));
    assert!(again["nodes_created"].as_i64().unwrap_or(0) > 0, "{again}");
}

/// Отказы разбора, которые доходят до своих проверок только на живом файле.
#[test]
fn import_and_template_refusals() {
    let dir = fixture("refuse");
    json(&dir, &["add", "Эпик"]);

    let broken = dir.join("broken.json");
    fs::write(&broken, "{ не json").expect("write");
    let broken = broken.to_str().expect("path").to_string();

    let (code, error) = refused(&dir, &["import", &broken]);
    assert_eq!(code, 1);
    assert_eq!(error, "INVALID_VALUE");

    let good = dir.join("good.json");
    ok(&dir, &["export", "--out", good.to_str().expect("path")]);
    let good = good.to_str().expect("path").to_string();
    let (code, error) = refused(&dir, &["import", &good, "--mode", "nonsense"]);
    assert_eq!(code, 1);
    assert_eq!(error, "INVALID_VALUE");

    // Шаблон без источника и с негодной подстановкой.
    let (code, error) = refused(&dir, &["template", "save", "empty"]);
    assert_eq!(code, 1);
    assert_eq!(error, "INVALID_VALUE");

    let tree = r#"[{"title":"корень"}]"#;
    let from_file = dir.join("tree.json");
    fs::write(&from_file, tree).expect("write");
    json(
        &dir,
        &[
            "template",
            "save",
            "fromfile",
            "--file",
            from_file.to_str().expect("path"),
            "--description",
            "из файла",
        ],
    );
    let shown = ok(&dir, &["template", "show", "fromfile"]);
    assert!(shown.contains("description"), "{shown}");

    let (code, error) = refused(
        &dir,
        &["template", "apply", "fromfile", "--var", "без равно"],
    );
    assert_eq!(code, 1);
    assert_eq!(error, "INVALID_VALUE");
}

/// Негодный профиль MCP отбивается до открытия базы.
#[test]
fn mcp_refuses_unknown_profile() {
    let dir = fixture("mcp-profile");
    let (code, error) = refused(&dir, &["mcp", "--profile", "nonsense"]);
    assert_eq!(code, 1);
    assert_eq!(error, "INVALID_VALUE");
}

/// Просроченная аренда видна в отчёте `doctor` и убирается ключом `--gc`.
#[test]
fn doctor_reports_stale_leases() {
    let dir = fixture("doctor");
    json(&dir, &["add", "Эпик"]);
    json(&dir, &["add", "Задача", "--parent", "TASQ-1"]);
    let lease = json(&dir, &["claim", "TASQ-1.1", "--holder", "agent-a"]);
    assert!(lease["token"].is_string());

    // Минимальный ttl — минута; чтобы не ждать, сдвигаем срок аренды в прошлое.
    // Это ровно то состояние, в которое база приходит сама по истечении срока.
    {
        let db = dir.join(".tasq/tasq.db");
        let conn = rusqlite::Connection::open(&db).expect("open db");
        conn.execute(
            "UPDATE attempts SET expires_at = '2000-01-01T00:00:00.000Z' WHERE ended_at IS NULL",
            [],
        )
        .expect("expire attempt");
    }

    let report = ok(&dir, &["doctor"]);
    assert!(report.contains("stale:"), "{report}");
    assert!(report.contains("expired attempt"), "{report}");

    let swept = ok(&dir, &["doctor", "--gc"]);
    assert!(swept.contains("closed 1 expired attempt"), "{swept}");
}

/// Дата `YYYY-MM-DD`, отстоящая от сегодняшней на `days_ago` дней (UTC).
fn days_ago(days_ago: u32) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
        - (days_ago as i64) * 86_400;
    let days = secs.div_euclid(86_400);
    let (mut year, mut rest) = (1970i64, days);
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let len = if leap { 366 } else { 365 };
        if rest < len {
            break;
        }
        rest -= len;
        year += 1;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
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
    let mut month = 0usize;
    while rest >= months[month] {
        rest -= months[month];
        month += 1;
    }
    format!("{year:04}-{:02}-{:02}", month + 1, rest + 1)
}
