//! Проекты: создание, перечисление и выбор проекта для работы.

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
    let dir = std::env::temp_dir().join(format!("tasq-projects-{tag}-{nanos}"));
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

fn run_env(dir: &Path, env: &[(&str, &str)], args: &[&str]) -> (i32, String, String) {
    let mut command = Command::new(bin());
    command.current_dir(dir).args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    let out = command.output().expect("run tasq");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn a_new_database_lists_exactly_one_project() {
    let dir = temp_dir("one");
    assert_eq!(
        run(&dir, &["init", "--key", "ALPHA", "--name", "Первый"]).0,
        0
    );

    let (code, stdout, stderr) = run(&dir, &["list", "projects"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("ALPHA"), "{stdout}");
    assert!(stdout.contains("Первый"), "{stdout}");
    assert!(stdout.contains('*'), "активный проект помечен:\n{stdout}");
}

#[test]
fn create_project_registers_it_and_points_at_the_next_step() {
    let dir = temp_dir("create");
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);

    let (code, stdout, stderr) = run(&dir, &["create", "project", "PRJ", "--name", "Новый"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("created project PRJ"), "{stdout}");
    // Без селектора созданный проект остаётся недостижимым, поэтому подсказка — часть ответа.
    assert!(stdout.contains("--project PRJ"), "{stdout}");
    assert!(stdout.contains("TASQ_PROJECT=PRJ"), "{stdout}");

    let (code, stdout, _) = run(&dir, &["list", "projects"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("PRJ"), "{stdout}");
    assert_eq!(
        stdout.matches("active").count(),
        2,
        "два проекта:\n{stdout}"
    );
}

#[test]
fn a_key_is_normalised_to_uppercase() {
    let dir = temp_dir("case");
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);

    let (code, stdout, stderr) = run(&dir, &["--json", "create", "project", "prj"]);
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(value["key"], serde_json::json!("PRJ"));
    assert_eq!(
        value["name"],
        serde_json::json!("PRJ"),
        "имя по умолчанию = ключ"
    );
}

#[test]
fn duplicate_and_invalid_keys_are_refused() {
    let dir = temp_dir("dupe");
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);

    let (code, _, stderr) = run(&dir, &["create", "project", "ALPHA"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("DUPLICATE_KEY"), "{stderr}");

    for bad in ["A", "TOOLONGKEY", "PR-J", "ПРЖ"] {
        let (code, _, stderr) = run(&dir, &["create", "project", bad]);
        assert_eq!(code, 1, "ключ {bad:?} должен быть отвергнут");
        assert!(stderr.contains("INVALID_KEY"), "ключ {bad:?}: {stderr}");
    }
}

#[test]
fn work_goes_to_the_project_named_by_the_selector() {
    let dir = temp_dir("select");
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);
    assert_eq!(run(&dir, &["create", "project", "PRJ"]).0, 0);

    // Проектов два и селектора нет — это не «первый по порядку», а отказ со списком:
    // куда писать, знает только тот, кто ставит задачу.
    let (code, stdout, stderr) = run(&dir, &["add", "Узел без проекта"]);
    assert_eq!(code, 1, "stdout: {stdout}");
    assert!(stderr.contains("PROJECT_REQUIRED"), "{stderr}");
    assert!(
        stderr.contains("ALPHA") && stderr.contains("PRJ"),
        "отказ перечисляет доступные ключи: {stderr}"
    );
    // И ничего не создалось ни в одном из проектов.
    for key in ["ALPHA", "PRJ"] {
        let (_, listed, _) = run(&dir, &["--project", key, "ls"]);
        assert!(
            !listed.contains("-1"),
            "в {key} ничего не появилось: {listed}"
        );
    }

    // С выбором — второй проект, и dot-ID отличается.
    assert_eq!(run(&dir, &["--project", "PRJ", "add", "Узел в PRJ"]).0, 0);
    let (_, stdout, _) = run(&dir, &["--project", "PRJ", "ls"]);
    assert!(stdout.contains("PRJ-1"), "{stdout}");
    assert!(
        !stdout.contains("ALPHA-1"),
        "чужие узлы не видны:\n{stdout}"
    );
    // Селектор переменной окружения работает так же, как флаг.
    assert_eq!(
        run_env(&dir, &[("TASQ_PROJECT", "ALPHA")], &["add", "Узел в ALPHA"]).0,
        0
    );
    let (_, stdout, _) = run(&dir, &["--project", "ALPHA", "ls"]);
    assert!(stdout.contains("ALPHA-1"), "{stdout}");

    // `ls` — обзорная команда: без селектора она показывает ВСЕ проекты (адрес узла
    // начинается с ключа своего проекта), а не отказывает и не выбирает первый.
    let (code, stdout, stderr) = run(&dir, &["ls"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("ALPHA-1"), "обзор без селектора: {stdout}");
    assert!(stdout.contains("PRJ-1"), "обзор без селектора: {stdout}");

    // Запись без селектора по-прежнему требует явного выбора: там угадывать нечего.
    let (code, _, stderr) = run(&dir, &["add", "Узел без проекта"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("PROJECT_REQUIRED"), "{stderr}");

    let (_, stdout, _) = run(&dir, &["--project", "ALPHA", "ls"]);
    assert!(stdout.contains("ALPHA-1"), "{stdout}");
    assert!(!stdout.contains("PRJ-1"), "{stdout}");
}

#[test]
fn the_selector_also_comes_from_the_environment() {
    let dir = temp_dir("env");
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);
    assert_eq!(run(&dir, &["create", "project", "PRJ"]).0, 0);

    let (code, _, stderr) = run_env(&dir, &[("TASQ_PROJECT", "PRJ")], &["add", "Узел через env"]);
    assert_eq!(code, 0, "{stderr}");

    let (_, stdout, _) = run_env(&dir, &[("TASQ_PROJECT", "PRJ")], &["ls"]);
    assert!(stdout.contains("PRJ-1"), "{stdout}");

    // Флаг важнее переменной окружения.
    let (code, stdout, _) = run_env(
        &dir,
        &[("TASQ_PROJECT", "ALPHA")],
        &["--project", "PRJ", "ls"],
    );
    assert_eq!(code, 0);
    assert!(stdout.contains("PRJ-1"), "флаг перекрывает env:\n{stdout}");
}

#[test]
fn an_unknown_project_key_names_the_available_ones() {
    let dir = temp_dir("unknown");
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);
    assert_eq!(run(&dir, &["create", "project", "PRJ"]).0, 0);

    let (code, _, stderr) = run(&dir, &["--project", "NOPE", "ls"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("PROJECT_NOT_FOUND"), "{stderr}");
    assert!(
        stderr.contains("ALPHA") && stderr.contains("PRJ"),
        "известные ключи названы:\n{stderr}"
    );
    assert!(
        stderr.contains("tasq list projects"),
        "подсказка есть:\n{stderr}"
    );
}

#[test]
fn node_counts_in_the_list_follow_the_work() {
    let dir = temp_dir("counts");
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);
    assert_eq!(run(&dir, &["create", "project", "PRJ"]).0, 0);
    assert_eq!(run(&dir, &["--project", "PRJ", "add", "Один"]).0, 0);
    assert_eq!(run(&dir, &["--project", "PRJ", "add", "Два"]).0, 0);

    let (code, stdout, _) = run(&dir, &["--json", "list", "projects"]);
    assert_eq!(code, 0);
    let projects: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let alpha = projects
        .as_array()
        .expect("array")
        .iter()
        .find(|project| project["key"] == serde_json::json!("ALPHA"))
        .expect("ALPHA");
    let prj = projects
        .as_array()
        .expect("array")
        .iter()
        .find(|project| project["key"] == serde_json::json!("PRJ"))
        .expect("PRJ");
    assert_eq!(alpha["nodes"], serde_json::json!(0));
    assert_eq!(prj["nodes"], serde_json::json!(2));
}

#[test]
fn count_ignores_soft_deleted_nodes() {
    let dir = temp_dir("deleted");
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);
    // Удалять можно только `todo`-узел: корневой эпик не удаляется по матрице переходов,
    // поэтому берём задачу внутри эпика.
    assert_eq!(run(&dir, &["add", "Эпик"]).0, 0);
    assert_eq!(run(&dir, &["add", "Удаляемый", "--parent", "ALPHA-1"]).0, 0);
    assert_eq!(run(&dir, &["rm", "ALPHA-1.1"]).0, 0);

    let (_, stdout, _) = run(&dir, &["--json", "list", "projects"]);
    let projects: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(
        projects[0]["nodes"],
        serde_json::json!(1),
        "удалённый узел не считается: {projects}"
    );
}

#[test]
fn creating_a_project_needs_a_database() {
    let dir = temp_dir("nodb");
    let (code, _, stderr) = run(&dir, &["create", "project", "PRJ"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("DB_NOT_FOUND"), "{stderr}");
    assert!(
        !dir.join(".tasq").exists(),
        "база не создаётся побочным эффектом"
    );
}

#[test]
fn a_project_is_renamed_by_name() {
    let dir = temp_dir("rename-name");
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);
    assert_eq!(run(&dir, &["create", "project", "PRJ"]).0, 0);

    let (code, stdout, stderr) = run(&dir, &["update", "project", "PRJ", "--name", "Новое имя"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("updated project PRJ"), "{stdout}");

    let (_, stdout, _) = run(&dir, &["--json", "list", "projects"]);
    let projects: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let prj = projects
        .as_array()
        .expect("array")
        .iter()
        .find(|project| project["key"] == serde_json::json!("PRJ"))
        .expect("PRJ");
    assert_eq!(prj["name"], serde_json::json!("Новое имя"));
}

/// Ключ входит в dot-ID, поэтому его смена — операция над всем проектом: адреса узлов
/// меняются разом, и это должно быть видно в выводе, а не выясняться потом.
#[test]
fn renaming_the_key_moves_every_dot_id() {
    let dir = temp_dir("rename-key");
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);
    assert_eq!(run(&dir, &["create", "project", "PRJ"]).0, 0);
    assert_eq!(run(&dir, &["--project", "PRJ", "add", "Задача"]).0, 0);
    assert_eq!(run(&dir, &["--project", "PRJ", "add", "Вторая"]).0, 0);

    let (code, stdout, stderr) = run(&dir, &["update", "project", "PRJ", "--new-key", "BETA"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("renamed to BETA"), "{stdout}");
    assert!(
        stdout.contains("dot-IDs of this project now start with BETA-"),
        "предупреждение о смене адресов есть:
{stdout}"
    );

    // Новые адреса работают, старых больше нет.
    let (_, stdout, _) = run(&dir, &["--project", "BETA", "ls"]);
    assert!(stdout.contains("BETA-1"), "{stdout}");
    assert!(stdout.contains("BETA-2"), "{stdout}");
    assert!(
        !stdout.contains("PRJ-"),
        "старый префикс исчез:
{stdout}"
    );

    let (code, _, stderr) = run(&dir, &["--project", "PRJ", "ls"]);
    assert_eq!(code, 1, "старый ключ больше не проект: {stderr}");
    assert!(stderr.contains("PROJECT_NOT_FOUND"), "{stderr}");
}

#[test]
fn a_key_collision_on_rename_is_refused() {
    let dir = temp_dir("rename-collision");
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);
    assert_eq!(run(&dir, &["create", "project", "PRJ"]).0, 0);

    let (code, _, stderr) = run(&dir, &["update", "project", "PRJ", "--new-key", "ALPHA"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("DUPLICATE_KEY"), "{stderr}");

    // Ключ проекта остался прежним.
    let (_, stdout, _) = run(&dir, &["--json", "list", "projects"]);
    let projects: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert!(
        projects
            .as_array()
            .expect("array")
            .iter()
            .any(|project| project["key"] == serde_json::json!("PRJ"))
    );
}

#[test]
fn status_is_validated_against_the_schema() {
    let dir = temp_dir("status");
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);

    let (code, _, stderr) = run(
        &dir,
        &["update", "project", "ALPHA", "--status", "nonsense"],
    );
    assert_eq!(code, 1);
    assert!(stderr.contains("INVALID_VALUE"), "{stderr}");
    assert!(
        stderr.contains("on_hold"),
        "перечислены допустимые:
{stderr}"
    );

    let (code, _, stderr) = run(
        &dir,
        &["update", "project", "ALPHA", "--status", "archived"],
    );
    assert_eq!(code, 0, "{stderr}");
    let (_, stdout, _) = run(&dir, &["--json", "list", "projects"]);
    let projects: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(projects[0]["status"], serde_json::json!("archived"));
}

#[test]
fn updating_nothing_is_an_error() {
    let dir = temp_dir("nochange");
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);
    let (code, _, stderr) = run(&dir, &["update", "project", "ALPHA"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("nothing to change"), "{stderr}");
}

#[test]
fn a_non_empty_project_needs_force_to_be_deleted() {
    let dir = temp_dir("delete-nonempty");
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);
    assert_eq!(run(&dir, &["create", "project", "PRJ"]).0, 0);
    assert_eq!(run(&dir, &["--project", "PRJ", "add", "Задача"]).0, 0);

    let (code, _, stderr) = run(&dir, &["delete", "project", "PRJ"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("PROJECT_NOT_EMPTY"), "{stderr}");
    assert!(
        stderr.contains("--force"),
        "подсказка называет флаг:
{stderr}"
    );

    // Отказ ничего не удалил.
    let (_, stdout, _) = run(&dir, &["list", "projects"]);
    assert!(stdout.contains("PRJ"), "{stdout}");

    let (code, stdout, stderr) = run(&dir, &["delete", "project", "PRJ", "--force"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("deleted project PRJ"), "{stdout}");
    assert!(stdout.contains("remaining: ALPHA"), "{stdout}");

    let (_, stdout, _) = run(&dir, &["list", "projects"]);
    assert!(
        !stdout.contains("PRJ"),
        "проект удалён:
{stdout}"
    );
}

#[test]
fn deletion_takes_the_content_with_it() {
    let dir = temp_dir("delete-cascade");
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);
    assert_eq!(run(&dir, &["create", "project", "PRJ"]).0, 0);
    assert_eq!(run(&dir, &["--project", "PRJ", "add", "Эпик"]).0, 0);
    assert_eq!(
        run(
            &dir,
            &["--project", "PRJ", "add", "Задача", "--parent", "PRJ-1"]
        )
        .0,
        0
    );
    assert_eq!(
        run(
            &dir,
            &[
                "--project",
                "PRJ",
                "note",
                "save",
                "--title",
                "N",
                "--content",
                "c"
            ]
        )
        .0,
        0
    );
    assert_eq!(run(&dir, &["delete", "project", "PRJ", "--force"]).0, 0);

    // Каскад сработал: в базе не осталось ни узлов, ни заметок чужого проекта.
    let (_, stdout, _) = run(&dir, &["--json", "list", "projects"]);
    let projects: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(projects.as_array().map(Vec::len), Some(1));
    assert_eq!(projects[0]["nodes"], serde_json::json!(0));

    let (code, _, stderr) = run(&dir, &["doctor", "--check"]);
    assert_eq!(code, 0, "целостность после каскада: {stderr}");
}

#[test]
fn the_last_project_cannot_be_deleted_without_force() {
    let dir = temp_dir("delete-last");
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);

    let (code, _, stderr) = run(&dir, &["delete", "project", "ALPHA"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("LAST_PROJECT"), "{stderr}");

    let (code, _, stderr) = run(&dir, &["delete", "project", "ALPHA", "--force"]);
    assert_eq!(code, 0, "{stderr}");

    // База без проектов — состояние, о котором команды говорят прямо.
    let (code, _, stderr) = run(&dir, &["ls"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("NO_PROJECT"), "{stderr}");
}

#[test]
fn update_and_delete_default_to_the_selected_project() {
    let dir = temp_dir("defaults");
    assert_eq!(run(&dir, &["init", "--key", "ALPHA"]).0, 0);
    assert_eq!(run(&dir, &["create", "project", "PRJ"]).0, 0);

    // Без аргумента берётся проект из --project, а не первый по порядку.
    let (code, stdout, stderr) = run(
        &dir,
        &[
            "--project",
            "PRJ",
            "update",
            "project",
            "--name",
            "Из селектора",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("updated project PRJ"), "{stdout}");

    let (code, _, stderr) = run(&dir, &["--project", "PRJ", "delete", "project"]);
    assert_eq!(code, 0, "{stderr}");

    let (_, stdout, _) = run(&dir, &["list", "projects"]);
    assert!(stdout.contains("ALPHA"), "{stdout}");
    assert!(!stdout.contains("PRJ"), "{stdout}");
}
