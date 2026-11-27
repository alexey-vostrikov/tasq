//! Наблюдаемость: вызовы, длительности, отказы по кодам и цена блокировок.
//!
//! Метрики живут в отдельном файле рядом с базой (`<имя>-metrics.db`), и это не деталь
//! реализации, а условие работы: чтение базы не пишет в неё ничего (TASQDEV-19), поэтому счётчик
//! в основной базе вернул бы блокировку записи каждой читающей команде. Проверяется здесь и то, и
//! другое: что отчёт отвечает, и что база остаётся нетронутой.

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
    let dir = std::env::temp_dir().join(format!("tasq-metrics-{tag}-{nanos}"));
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

fn init(dir: &Path) {
    let (code, _, stderr) = run(dir, &["init", "--key", "MET"]);
    assert_eq!(code, 0, "{stderr}");
}

fn stats(dir: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--json", "stat"];
    full.extend_from_slice(args);
    let (code, stdout, stderr) = run(dir, &full);
    assert_eq!(code, 0, "{stdout}{stderr}");
    serde_json::from_str(&stdout).expect("json")
}

/// Успехи и отказы считаются по отдельности, и отказ называет свой код.
///
/// Второй половина проверки — та, ради которой всё делалось: «сколько чего было» отвечает на
/// вопрос, на который до сих пор нечем было ответить, кроме пересказа памяти.
#[test]
fn calls_are_counted_with_their_outcome_codes() {
    let dir = temp_dir("count");
    init(&dir);
    for title in ["первый", "второй"] {
        let (code, _, stderr) = run(&dir, &["add", title]);
        assert_eq!(code, 0, "{stderr}");
    }
    // Отказ: узла нет. Код ошибки обязан попасть в отчёт именно кодом, а не словом «ошибка».
    let (code, _, _) = run(&dir, &["set", "MET-99", "--status", "done"]);
    assert_ne!(code, 0, "команда обязана была отказать");

    let report = stats(&dir, &[]);
    assert_eq!(report["calls"], serde_json::json!(4), "{report}");
    assert_eq!(report["failures"], serde_json::json!(1), "{report}");

    let outcomes = report["by_outcome"].as_array().expect("исходы");
    let codes: Vec<&str> = outcomes
        .iter()
        .map(|row| row["outcome"].as_str().expect("код"))
        .collect();
    assert!(codes.contains(&"ok"), "{codes:?}");
    assert!(codes.contains(&"NODE_NOT_FOUND"), "{codes:?}");

    let operations = report["by_operation"].as_array().expect("операции");
    let add = operations
        .iter()
        .find(|row| row["operation"] == serde_json::json!("add"))
        .expect("add в отчёте");
    assert_eq!(add["calls"], serde_json::json!(2), "{add}");
    assert_eq!(add["failures"], serde_json::json!(0), "{add}");

    let _ = fs::remove_dir_all(&dir);
}

/// Метрики не пишут в базу: после читающих команд ни таблицы, ни изменения файла.
///
/// Это и есть обещание TASQDEV-19, под которое сделана вся отдельная файловая история. Счётчик,
/// который пишет в основную базу, вернул бы каждому `ls` блокировку записи — и сто читателей снова
/// начали бы терять вызовы.
#[test]
fn metrics_never_touch_the_database() {
    let dir = temp_dir("sidecar");
    init(&dir);
    let db = dir.join(".tasq/tasq.db");
    let before = fs::metadata(&db).map(|meta| meta.len()).unwrap_or(0);

    for _ in 0..5 {
        let (code, _, stderr) = run(&dir, &["--project", "MET", "ls"]);
        assert_eq!(code, 0, "{stderr}");
    }

    let metrics = dir.join(".tasq/tasq-metrics.db");
    assert!(metrics.is_file(), "файл метрик не создан");
    let after = fs::metadata(&db).map(|meta| meta.len()).unwrap_or(0);
    assert_eq!(before, after, "основная база изменилась от читающих команд");

    // И в базе нет таблицы метрик — их там не должно быть ни при каких условиях.
    let conn = rusqlite::Connection::open(&db).expect("open");
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'calls'",
            [],
            |row| row.get(0),
        )
        .expect("счёт таблиц");
    assert_eq!(tables, 0, "таблица метрик попала в основную базу");

    let _ = fs::remove_dir_all(&dir);
}

/// Пустой отчёт — ответ, а не ошибка: команду зовут и до того, как появилась хоть одна запись.
#[test]
fn an_empty_report_answers_instead_of_failing() {
    let dir = temp_dir("empty");
    init(&dir);
    // `init` сам пишет метрику, поэтому проверяем базу, где файла метрик ещё нет.
    let metrics = dir.join(".tasq/tasq-metrics.db");
    let _ = fs::remove_file(&metrics);
    let report = stats(&dir, &[]);
    assert_eq!(report["present"], serde_json::json!(false), "{report}");
    assert_eq!(report["calls"], serde_json::json!(0), "{report}");
    assert_eq!(report["by_operation"], serde_json::json!([]), "{report}");

    let _ = fs::remove_dir_all(&dir);
}

/// Окно отчёта проверяется, а не толкуется молча: опечатка — отказ с названным форматом.
///
/// Отчёт не за тот период даёт неверный вывод о собственной работе, поэтому «не разобрал — взял
/// сутки» было бы худшим из вариантов.
#[test]
fn a_bad_window_is_refused() {
    let dir = temp_dir("window");
    init(&dir);
    let (code, _, stderr) = run(&dir, &["stat", "--since", "вчера"]);
    assert_ne!(code, 0, "опечатка в окне обязана отказать");
    assert!(
        stderr.contains("24h") && stderr.contains("RFC 3339"),
        "отказ обязан называть допустимые формы: {stderr}"
    );

    // А законные формы принимаются.
    for window in ["24h", "7d", "30d"] {
        let report = stats(&dir, &["--since", window]);
        assert_eq!(report["present"], serde_json::json!(true), "{window}");
    }

    let _ = fs::remove_dir_all(&dir);
}

/// Чистка удаляет только старое: свежие записи остаются, и отчёт об этом говорит числом.
#[test]
fn pruning_drops_only_what_is_older_than_the_horizon() {
    let dir = temp_dir("prune");
    init(&dir);
    run(&dir, &["add", "свежий узел"]);
    let metrics = dir.join(".tasq/tasq-metrics.db");

    // Состарим одну запись вручную: ждать тридцать дней в тесте нельзя, а условие чистки —
    // единственное, что здесь проверяется.
    {
        let conn = rusqlite::Connection::open(&metrics).expect("open metrics");
        conn.execute(
            "UPDATE calls SET at = '2000-01-01T00:00:00.000Z' WHERE id = (SELECT MIN(id) FROM calls)",
            [],
        )
        .expect("состарить");
    }

    let report = stats(&dir, &["--prune", "--since", "30d"]);
    assert_eq!(report["pruned"], serde_json::json!(1), "{report}");
    assert!(
        report["calls"].as_i64().unwrap_or(0) >= 1,
        "свежие записи должны остаться: {report}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Отчёт виден и агенту — тем же числом, что человеку.
///
/// Проверяется настоящим сервером: инструмент объявлен в профиле `read-only`, значит модель может
/// спросить «как дела» не заводя себе прав.
#[test]
fn the_mcp_tool_reports_the_same_numbers() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;

    let dir = temp_dir("mcp");
    init(&dir);
    run(&dir, &["add", "узел для метрики"]);

    let mut child = Command::new(bin())
        .current_dir(&dir)
        .args(["--project", "MET", "mcp", "--profile", "read-only"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mcp");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
        )
        .expect("init");
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
                               "params":{"name":"tasq_stats","arguments":{}}})
        )
        .expect("call");
    }
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("initialize");
    line.clear();
    reader.read_line(&mut line).expect("call");
    let response: Value = serde_json::from_str(&line).expect("json");
    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(
        response["result"]["isError"],
        serde_json::json!(false),
        "инструмент отказал: {response}"
    );
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("текст");
    let report: Value = serde_json::from_str(text).expect("json внутри текста");
    assert!(
        report["calls"].as_i64().unwrap_or(0) >= 1,
        "метрики пусты: {report}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Живая аренда видна в отчёте, просроченной нет.
///
/// Раздел аренд отвечает на вопрос, на который `doctor` отвечал снимком базы, а не числом за
/// период: «сколько работы брошено без отчёта». Читается он из основной базы в режиме только
/// чтения — метрика не должна требовать права записи.
#[test]
fn leases_are_reported_without_writing() {
    let dir = temp_dir("leases");
    init(&dir);
    let (code, _, stderr) = run(&dir, &["add", "узел для аренды"]);
    assert_eq!(code, 0, "{stderr}");
    let (code, token, stderr) = run(&dir, &["claim", "MET-1"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(token.contains("MET-1") || !token.is_empty(), "{token}");

    let report = stats(&dir, &[]);
    let leases = &report["leases"];
    assert_eq!(leases["live"], serde_json::json!(1), "{report}");
    assert_eq!(leases["expired_open"], serde_json::json!(0), "{report}");
    assert_eq!(leases["reservations"], serde_json::json!(0), "{report}");
    assert!(
        leases["journal_bytes"].as_u64().is_some(),
        "размер журнала обязан быть числом: {report}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Счётчик один на базу, даже если к ней ходят через символическую ссылку.
///
/// На живой базе это стоило двух файлов: `~/.tasq/tasq.db` — ссылка на
/// `~/.local/share/tasq/hermes.db`, и метрики ложились рядом с каждым путём отдельно. Счётчик,
/// который зависит от того, как его позвали, отвечает не на тот вопрос.
#[test]
fn metrics_sit_beside_the_real_file_not_the_symlink() {
    let dir = temp_dir("symlink");
    init(&dir);
    run(&dir, &["add", "узел"]);

    let link = dir.join("tasq-link.db");
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.join(".tasq/tasq.db"), &link).expect("ссылка");

    // Отчёт по ссылке обязан увидеть тот же счётчик, что и по настоящему файлу.
    let direct = stats(&dir, &[]);
    let via_link = {
        let (code, stdout, stderr) = run(
            &dir,
            &["--db", link.to_str().expect("путь"), "--json", "stat"],
        );
        assert_eq!(code, 0, "{stdout}{stderr}");
        serde_json::from_str::<Value>(&stdout).expect("json")
    };
    // Отчёт по ссылке видит тот же счётчик: разница ровно в единицу — сам этот вызов `stat`,
    // который записал себя до того, как посчитал.
    assert_eq!(
        via_link["calls"].as_i64().unwrap_or(0),
        direct["calls"].as_i64().unwrap_or(0) + 1,
        "{via_link}"
    );
    assert_eq!(
        direct["metrics_path"], via_link["metrics_path"],
        "{via_link}"
    );
    assert!(
        !dir.join("tasq-link-metrics.db").exists(),
        "рядом со ссылкой появился второй счётчик"
    );

    let _ = fs::remove_dir_all(&dir);
}
