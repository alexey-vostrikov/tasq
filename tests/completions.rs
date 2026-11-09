//! `tasq completions <shell>`: скрипт автодополнения для оболочки.
//!
//! Проверяется поведение, а не текст: команда работает БЕЗ базы (её запускают в новом
//! каталоге, до `tasq init`), отдаёт непустой скрипт для каждой поддерживаемой оболочки,
//! скрипт упоминает подкоманды и глобальные флаги, а неизвестная оболочка отвергается
//! до записи чего-либо.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tasq")
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-completions-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("create temp dir");
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

/// Скрипт существует для каждой оболочки и знает про подкоманды и глобальные флаги.
///
/// Базы в каталоге нет намеренно: дополнение настраивают один раз на машине, а не в
/// проекте, и требование `tasq init` перед ним сделало бы команду бесполезной ровно там,
/// где она нужна.
#[test]
fn every_supported_shell_gets_a_script_without_a_database() {
    let dir = temp_dir("shells");
    assert!(
        !dir.join(".tasq").exists(),
        "предусловие: базы в каталоге нет"
    );

    for shell in ["bash", "zsh", "fish"] {
        let (code, stdout, stderr) = run(&dir, &["completions", shell]);
        assert_eq!(code, 0, "{shell}: {stderr}");
        assert!(
            stdout.len() > 200,
            "{shell}: скрипт подозрительно короткий ({} байт)",
            stdout.len()
        );
        // Регистрация дополнения и хотя бы одна настоящая подкоманда: без них скрипт
        // печатается, но ничего не дополняет.
        let registers = match shell {
            "bash" => stdout.contains("complete"),
            "zsh" => stdout.contains("#compdef") || stdout.contains("compdef"),
            _ => stdout.contains("complete -c"),
        };
        assert!(registers, "{shell}: нет строки регистрации дополнения");
        // Длинные флаги каждая оболочка пишет по-своему (`--project` в bash/zsh,
        // `-l project` в fish), поэтому проверяется имя, а не синтаксис.
        assert!(
            stdout.contains("claim") && stdout.contains("project"),
            "{shell}: скрипт не знает подкоманд или глобальных флагов"
        );
    }
}

/// Алиас в единственном числе: `tasq completion bash` — то же самое.
#[test]
fn the_singular_alias_works() {
    let dir = temp_dir("alias");
    let (plural_code, plural, _) = run(&dir, &["completions", "bash"]);
    let (singular_code, singular, stderr) = run(&dir, &["completion", "bash"]);
    assert_eq!(singular_code, 0, "{stderr}");
    assert_eq!(plural_code, 0);
    assert_eq!(
        singular, plural,
        "алиас обязан отдавать ровно тот же скрипт"
    );
}

/// Неизвестная оболочка — отказ до вывода: пустой stdout и код 2 (неверный аргумент).
#[test]
fn an_unknown_shell_is_refused_before_printing() {
    let dir = temp_dir("unknown");
    let (code, stdout, stderr) = run(&dir, &["completions", "tcsh"]);
    assert_eq!(code, 2, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.is_empty(), "в stdout попал скрипт: {stdout}");
    assert!(
        stderr.contains("bash") || stderr.contains("possible values"),
        "отказ не перечисляет допустимые оболочки: {stderr}"
    );
}

/// `--out` пишет файл и печатает его путь: так дополнение ставится одной командой в
/// каталог, который оболочка читает сама.
#[test]
fn out_writes_the_script_to_a_file() {
    let dir = temp_dir("out");
    let target = dir.join("tasq.bash");
    let (code, stdout, stderr) = run(
        &dir,
        &[
            "completions",
            "bash",
            "--out",
            target.to_str().expect("path"),
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    let written = fs::read_to_string(&target).expect("файл создан");
    assert!(written.contains("claim"), "в файле не тот скрипт");
    assert!(
        stdout.contains("tasq.bash"),
        "путь к файлу не назван в выводе: {stdout}"
    );
}
