//! Тема TUI выбирается флагом и переменной окружения, а неизвестное имя — отказ.
//!
//! Интеграционный тест, а не модульный: проверяется ПУТЬ ЧЕРЕЗ CLI (`tasq tui --once --theme`),
//! и переменная `CARGO_BIN_EXE_tasq` существует только здесь. Модульный тест на `Theme::by_name`
//! проверяет разбор имени, но не то, что флаг доехал до экрана.

use std::process::Command;

use tasq::db;

fn init(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-theme-{tag}-{nanos}"));
    let outcome = db::init(&dir, "THM", "theme").expect("init");
    std::path::PathBuf::from(outcome.db_path)
}

/// Каждая встроенная тема принимается флагом.
#[test]
fn every_builtin_theme_is_accepted_by_the_flag() {
    let path = init("accept");
    let bin = env!("CARGO_BIN_EXE_tasq");
    for name in [
        "default",
        "default-light",
        "gruvbox",
        "gruvbox-light",
        "nord",
        "nord-light",
    ] {
        let run = Command::new(bin)
            .args([
                "--db",
                path.to_str().expect("path"),
                "tui",
                "--once",
                "--theme",
                name,
            ])
            .output()
            .expect("run tasq");
        assert!(
            run.status.success(),
            "тема {name} отвергнута: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        let frame = String::from_utf8_lossy(&run.stdout);
        assert!(frame.contains("проекты"), "тема {name}: кадр не нарисован");
    }
}

/// Неизвестное имя — отказ с названным полем и перечнем допустимых значений.
#[test]
fn an_unknown_theme_is_refused_with_the_list() {
    let path = init("refuse");
    let bin = env!("CARGO_BIN_EXE_tasq");
    let run = Command::new(bin)
        .args([
            "--db",
            path.to_str().expect("path"),
            "tui",
            "--once",
            "--theme",
            "bogus",
        ])
        .output()
        .expect("run tasq");
    assert!(!run.status.success(), "мусор в --theme принят");
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr
            .lines()
            .next()
            .unwrap_or_default()
            .contains("INVALID_VALUE"),
        "отказ не назван кодом: {stderr}"
    );
    assert!(stderr.contains("theme"), "отказ не называет поле: {stderr}");
    assert!(
        stderr.contains("nord") && stderr.contains("gruvbox"),
        "отказ не перечисляет допустимые темы: {stderr}"
    );
}

/// Переменная окружения `TASQ_THEME` работает так же, как флаг, и флаг её перекрывает.
#[test]
fn the_env_variable_selects_the_theme_and_the_flag_wins() {
    let path = init("env");
    let bin = env!("CARGO_BIN_EXE_tasq");

    let from_env = Command::new(bin)
        .args(["--db", path.to_str().expect("path"), "tui", "--once"])
        .env("TASQ_THEME", "nord")
        .output()
        .expect("run tasq");
    assert!(
        from_env.status.success(),
        "тема из TASQ_THEME отвергнута: {}",
        String::from_utf8_lossy(&from_env.stderr)
    );

    // Мусор в переменной тоже отказ: молча подставить умолчание значило бы соврать о том,
    // какая тема на экране.
    let broken = Command::new(bin)
        .args(["--db", path.to_str().expect("path"), "tui", "--once"])
        .env("TASQ_THEME", "nonsense")
        .output()
        .expect("run tasq");
    assert!(!broken.status.success(), "мусор в TASQ_THEME принят");

    // Флаг сильнее переменной.
    let flag_wins = Command::new(bin)
        .args([
            "--db",
            path.to_str().expect("path"),
            "tui",
            "--once",
            "--theme",
            "gruvbox",
        ])
        .env("TASQ_THEME", "nonsense")
        .output()
        .expect("run tasq");
    assert!(
        flag_wins.status.success(),
        "флаг не перекрыл переменную: {}",
        String::from_utf8_lossy(&flag_wins.stderr)
    );
}
