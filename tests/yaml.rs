//! YAML как равноправный формат дампа: тот же документ, читаемый глазами и правимый руками.
//!
//! Проверяется не «файл похож на YAML», а свойства: результат разбирается обратно в тот же
//! документ (круговой путь), контрольная сумма JSON и YAML одного состояния совпадает, а
//! комментарии и якоря в прочитанном YAML не ломают разбор.

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
    let dir = std::env::temp_dir().join(format!("tasq-yaml-{tag}-{nanos}"));
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

fn seeded(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);
    assert_eq!(run(&dir, &["add", "Эпик"]).0, 0);
    assert_eq!(
        run(
            &dir,
            &["add", "Задача: с двоеточием", "--parent", "ALPHA-1"]
        )
        .0,
        0
    );
    assert_eq!(
        run(
            &dir,
            &["comment", "add", "ALPHA-1.1", "текст с #решёткой и — тире"]
        )
        .0,
        0
    );
    dir
}

/// YAML-дамп разбирается обратно в тот же документ: `tables` и `counts` совпадают, а
/// контрольная сумма — та же, что у JSON. Иначе формат был бы «для глаз», а не для обмена.
#[test]
fn yaml_round_trips_to_the_same_document() {
    let dir = seeded("roundtrip");
    let (code, json_text, stderr) = run(&dir, &["export", "--all"]);
    assert_eq!(code, 0, "{stderr}");
    let (code, yaml_text, stderr) = run(&dir, &["export", "--all", "--format", "yaml"]);
    assert_eq!(code, 0, "{stderr}");

    let from_json: serde_json::Value = serde_json::from_str(&json_text).expect("json");
    let from_yaml: serde_json::Value = tasq::transfer::read_yaml(&yaml_text).expect("yaml");

    assert_eq!(
        from_json["sha256"], from_yaml["sha256"],
        "сумма YAML-документа отличается от JSON"
    );
    assert_eq!(
        from_json["counts"], from_yaml["counts"],
        "счётчики разошлись"
    );
    assert_eq!(
        from_json["tables"]["nodes"].as_array().map(Vec::len),
        from_yaml["tables"]["nodes"].as_array().map(Vec::len),
        "число узлов в YAML другое"
    );

    // Заголовок и текст с пунктуацией, ломающей наивный YAML, доехали без искажений.
    let titles: Vec<&str> = from_yaml["tables"]["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .filter_map(|node| node["title"].as_str())
        .collect();
    assert!(
        titles.contains(&"Задача: с двоеточием"),
        "заголовок с двоеточием искажён: {titles:?}"
    );
}

/// Расширение `--out` выбирает формат: `.yaml` без флага даёт YAML, `.json` — JSON.
/// Молчаливого «всегда JSON» быть не должно, иначе расширение врёт.
#[test]
fn the_extension_of_out_selects_the_format() {
    let dir = seeded("byext");
    let yaml_path = dir.join("dump.yaml");
    let json_path = dir.join("dump.json");

    assert_eq!(
        run(
            &dir,
            &["export", "--all", "--out", yaml_path.to_str().expect("p")]
        )
        .0,
        0
    );
    assert_eq!(
        run(
            &dir,
            &["export", "--all", "--out", json_path.to_str().expect("p")]
        )
        .0,
        0
    );

    let yaml_text = fs::read_to_string(&yaml_path).expect("yaml");
    let json_text = fs::read_to_string(&json_path).expect("json");

    assert!(
        !yaml_text.trim_start().starts_with('{'),
        "в .yaml записан JSON: {}",
        &yaml_text[..60.min(yaml_text.len())]
    );
    assert!(
        yaml_text.contains("kind: tasq-dump"),
        "нет ключа kind: {yaml_text}"
    );
    assert!(json_text.trim_start().starts_with('{'), "в .json не JSON");
    // Оба файла описывают одно состояние.
    let a: serde_json::Value = tasq::transfer::read_yaml(&yaml_text).expect("yaml parse");
    let b: serde_json::Value = serde_json::from_str(&json_text).expect("json parse");
    assert_eq!(a["sha256"], b["sha256"]);
}

/// Явный флаг важнее расширения, а неизвестный формат — отказ до записи файла.
#[test]
fn an_unknown_format_is_refused_before_writing() {
    let dir = seeded("bad");
    let target = dir.join("dump.yaml");

    let (code, stdout, stderr) = run(
        &dir,
        &[
            "export",
            "--all",
            "--format",
            "toml",
            "--out",
            target.to_str().expect("p"),
        ],
    );
    assert_eq!(code, 1, "stdout: {stdout}");
    assert!(stderr.contains("INVALID_VALUE"), "{stderr}");
    assert!(!target.exists(), "файл создан при отказе");

    // Явный json при расширении .yaml — это осознанный выбор пользователя.
    let (code, _, stderr) = run(
        &dir,
        &[
            "export",
            "--all",
            "--format",
            "json",
            "--out",
            target.to_str().expect("p"),
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    let text = fs::read_to_string(&target).expect("file");
    assert!(text.trim_start().starts_with('{'), "флаг json не применён");
}

/// YAML с комментариями и якорем читается: дамп — текстовый файл, который правят руками,
/// и комментарий в нём не должен ломать разбор.
#[test]
fn comments_and_anchors_are_readable() {
    let yaml = "\
# комментарий в начале файла
kind: tasq-dump
format: 1
schema_version: 3
shared: &row
  title: общий
tables:
  nodes: []
  notes: []
counts: *row
";
    let value = tasq::transfer::read_yaml(yaml).expect("комментарии и якоря не мешают");
    assert_eq!(value["kind"], "tasq-dump");
    assert_eq!(
        value["counts"]["title"], "общий",
        "якорь не развернулся: {value}"
    );
}

/// Экспорт одного проекта тоже умеет YAML — формат относится к документу, а не к режиму.
#[test]
fn the_single_project_export_is_available_as_yaml() {
    let dir = seeded("project");
    let (code, text, stderr) = run(&dir, &["--project", "ALPHA", "export", "--format", "yaml"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(text.contains("nodes:"), "{text}");
    let value = tasq::transfer::read_yaml(&text).expect("yaml");
    assert_eq!(value["data"]["project"]["key"], "ALPHA");
}
