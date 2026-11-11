//! `tasq tui --once`: ширина берётся у терминала, а `--color` печатает кадр в цвете.
//!
//! Проверяется через CLI, а не через функцию отрисовки: важно, что флаг доехал до экрана и что
//! ширина действительно уважается в том, что человек видит, а не только в промежуточной строке.

use std::process::{Command, Output};

use tasq::db;

fn init(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-once-{tag}-{nanos}"));
    let outcome = db::init(&dir, "ONE", "once").expect("init");
    // Узел, чтобы кадр был непустым: на пустой доске проверка ширины прошла бы и на сломанной
    // вёрстке.
    let mut conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = tasq::project::first_project(&conn).expect("project");
    tasq::node::create(
        &mut conn,
        &project,
        tasq::node::NewNode {
            parent: None,
            title: "узел для кадра --once",
            kind: None,
            description: None,
            priority: Some("high"),
            due_date: None,
            estimated_hours: None,
            weight: None,
            source_ref: None,
            branch: None,
        },
    )
    .expect("node");
    std::path::PathBuf::from(outcome.db_path)
}

fn run(path: &std::path::Path, args: &[&str], columns: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tasq"))
        .args(["--db", path.to_str().expect("path"), "tui", "--once"])
        .args(args)
        .env("COLUMNS", columns)
        .output()
        .expect("run tasq")
}

/// Ширина из `COLUMNS` уважается: ни одна строка кадра не шире заданной.
#[test]
fn the_frame_follows_the_requested_width() {
    let path = init("width");
    for columns in ["80", "100", "160"] {
        let out = run(&path, &[], columns);
        assert!(
            out.status.success(),
            "COLUMNS={columns}: команда упала: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let frame = String::from_utf8_lossy(&out.stdout);
        let limit = columns.parse::<usize>().expect("число");
        for line in frame.lines() {
            let length = line.chars().count();
            assert!(
                length <= limit,
                "COLUMNS={columns}: строка из {length} знаков шире кадра: {line:?}"
            );
        }
        assert!(
            frame.contains("ONE-1"),
            "COLUMNS={columns}: узла нет в кадре"
        );
    }
}

/// Без `--color` кадр печатается без escape-последовательностей: его сравнивают в гейте.
#[test]
fn the_plain_frame_has_no_escapes() {
    let path = init("plain");
    let frame = String::from_utf8_lossy(&run(&path, &[], "120").stdout).to_string();
    assert!(
        !frame.contains('\u{1b}'),
        "в обычном кадре появились escape-последовательности"
    );
}

/// `--color` печатает тот же текст в цвете: снимаем escape и сверяем с обычным кадром.
#[test]
fn the_coloured_frame_is_the_same_text() {
    let path = init("color");
    let plain = String::from_utf8_lossy(&run(&path, &[], "120").stdout).to_string();
    let coloured = String::from_utf8_lossy(&run(&path, &["--color"], "120").stdout).to_string();

    assert!(
        coloured.contains('\u{1b}'),
        "с --color кадр нарисован без цвета"
    );
    let stripped = tasq::tui::strip_ansi(&coloured);
    assert_eq!(
        stripped, plain,
        "цветной кадр отличается текстом от обычного"
    );
}

/// `--color` без `--once` — отказ: интерактивный экран и так цветной, и флаг там означает
/// недоразумение, а не настройку.
#[test]
fn color_without_once_is_refused() {
    let path = init("refuse");
    let out = Command::new(env!("CARGO_BIN_EXE_tasq"))
        .args(["--db", path.to_str().expect("path"), "tui", "--color"])
        .output()
        .expect("run tasq");
    assert!(!out.status.success(), "--color без --once принят");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--color") && stderr.contains("--once"),
        "отказ не объясняет, что флаг работает только с --once: {stderr}"
    );
}

/// Мусор в `COLUMNS` не ломает кадр: ширина откатывается к значению по умолчанию.
#[test]
fn a_broken_columns_value_falls_back() {
    let path = init("garbage");
    let out = run(&path, &[], "широко");
    assert!(
        out.status.success(),
        "мусор в COLUMNS уронил кадр: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let frame = String::from_utf8_lossy(&out.stdout);
    assert!(
        frame.lines().any(|line| line.chars().count() > 100),
        "кадр сверстался уже, чем запасная ширина: fallback не сработал"
    );
}
