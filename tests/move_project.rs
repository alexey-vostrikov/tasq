//! Перенос узла в другой проект: адрес перестраивается, ссылки обновляются, прежнее место
//! остаётся в комментарии.
//!
//! Решение владельца (TASQDEV-16.1): dot-ID перестраивается целиком, ссылки обновляются во всех
//! неархивных задачах, в перенесённый узел добавляется комментарий о прежней точке, архивные
//! записи сохраняют старый адрес намеренно.

use std::fs;
use std::path::PathBuf;
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
    let dir = std::env::temp_dir().join(format!("tasq-mv-project-{tag}-{nanos}"));
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

fn json(dir: &std::path::Path, args: &[&str]) -> Value {
    let (code, stdout, stderr) = run(dir, args);
    assert_eq!(code, 0, "{args:?}: {stderr}");
    serde_json::from_str(&stdout).unwrap_or_else(|err| panic!("{args:?}: {err}: {stdout}"))
}

/// Два проекта в одной базе: `SRC` с узлом и потомками, `DST` пустой.
fn seeded(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    assert_eq!(run(&dir, &["init", "--key", "SRC"]).0, 0);
    assert_eq!(run(&dir, &["add", "Эпик", "--json"]).0, 0);
    assert_eq!(run(&dir, &["add", "Задача", "--parent", "SRC-1"]).0, 0);
    assert_eq!(run(&dir, &["add", "Подзадача", "--parent", "SRC-1.1"]).0, 0);
    // `create project` берёт ключ позиционным аргументом, а не флагом.
    assert_eq!(
        run(&dir, &["create", "project", "DST", "--name", "Целевой"]).0,
        0
    );
    dir
}

/// Ссылка на узел в тексте соседа: описание, комментарий, заметка и `source_ref`.
fn seed_references(dir: &std::path::Path) {
    assert_eq!(
        run(
            dir,
            &[
                "add",
                "Сосед",
                "--project",
                "SRC",
                "--desc",
                "блокирует SRC-1.1 и весь SRC-1 целиком, но не SRC-2",
                "--source",
                "см. SRC-1.1",
            ],
        )
        .0,
        0
    );
    assert_eq!(
        run(
            dir,
            &[
                "comment",
                "add",
                "SRC-2",
                "ждём SRC-1.1, потом SRC-2",
                "--project",
                "SRC"
            ]
        )
        .0,
        0
    );
    assert_eq!(
        run(
            dir,
            &[
                "note",
                "save",
                "--title",
                "Решение",
                "--content",
                "разобрать SRC-1.1",
                "--project",
                "SRC",
            ]
        )
        .0,
        0
    );
}

#[test]
fn the_subtree_moves_and_the_address_is_rebuilt() {
    let dir = seeded("move");
    let report = json(&dir, &["mv", "SRC-1", "--to-project", "DST", "--json"]);

    assert_eq!(report["previous_dot_id"], "SRC-1");
    assert_eq!(report["dot_id"], "DST-1");
    assert_eq!(report["previous_project"], "SRC");
    assert_eq!(report["project"], "DST");
    assert_eq!(report["nodes"], 3, "сам узел и двое потомков");

    // Поддерево переехало целиком, адреса пересчитаны.
    let rows = json(
        &dir,
        &[
            "ls",
            "--json",
            "--fields",
            "dot_id,path",
            "--project",
            "DST",
        ],
    );
    let dots: Vec<&str> = rows
        .as_array()
        .expect("array")
        .iter()
        .map(|row| row["dot_id"].as_str().expect("dot_id"))
        .collect();
    assert_eq!(dots, vec!["DST-1", "DST-1.1", "DST-1.1.1"], "{rows}");

    // В исходном проекте пусто.
    let source = json(
        &dir,
        &["ls", "--json", "--fields", "dot_id", "--project", "SRC"],
    );
    assert_eq!(source.as_array().expect("array").len(), 0, "{source}");
}

/// Ссылки обновляются во всех неархивных записях: описание, `source_ref`, комментарий, заметка.
#[test]
fn references_are_rewritten_across_the_base() {
    let dir = seeded("refs");
    seed_references(&dir);

    let report = json(&dir, &["mv", "SRC-1", "--to-project", "DST", "--json"]);
    assert!(
        report["references"]["nodes"].as_u64().unwrap_or(0) >= 1,
        "описание соседа обязано обновиться: {report}"
    );
    assert!(
        report["references"]["comments"].as_u64().unwrap_or(0) >= 1,
        "комментарий обязан обновиться: {report}"
    );
    assert!(
        report["references"]["notes"].as_u64().unwrap_or(0) >= 1,
        "заметка обязана обновиться: {report}"
    );

    // Сосед остался в SRC и его текст ссылается на новый адрес.
    let neighbour = json(&dir, &["show", "SRC-2", "--json", "--project", "SRC"]);
    let description = neighbour["description"].as_str().expect("description");
    assert!(description.contains("DST-1.1"), "{description}");
    assert!(
        description.contains("SRC-2"),
        "посторонний адрес переписан быть не должен: {description}"
    );
    assert_eq!(
        neighbour["source_ref"].as_str(),
        Some("см. DST-1.1"),
        "{neighbour}"
    );

    // Комментарий к соседу — тоже.
    let comments = json(
        &dir,
        &["comment", "ls", "SRC-2", "--json", "--project", "SRC"],
    );
    let texts: Vec<&str> = comments
        .as_array()
        .expect("array")
        .iter()
        .map(|row| row["content"].as_str().expect("content"))
        .collect();
    assert!(
        texts.iter().any(|text| text.contains("DST-1.1")),
        "{texts:?}"
    );

    // Заметка — тоже.
    let notes = json(&dir, &["note", "ls", "--json", "--project", "SRC"]);
    let contents: Vec<&str> = notes
        .as_array()
        .expect("array")
        .iter()
        .map(|row| row["content"].as_str().expect("content"))
        .collect();
    assert!(
        contents.iter().any(|text| text.contains("DST-1.1")),
        "{contents:?}"
    );
}

/// Архив — застывшая история: его текст не переписывается, старый адрес остаётся.
#[test]
fn archived_records_keep_the_old_address() {
    let dir = seeded("archive");
    // Адрес берётся из ответа, а не угадывается: корневой номер в проекте зависит от того,
    // сколько эпиков там уже есть, и `SRC-3` в этой фикстуре не существует.
    let archived_node = json(
        &dir,
        &[
            "add",
            "Архивная",
            "--project",
            "SRC",
            "--desc",
            "старая ссылка SRC-1.1",
            "--json",
        ],
    );
    let archived_dot = archived_node["dot_id"]
        .as_str()
        .expect("dot_id")
        .to_string();
    assert_eq!(run(&dir, &["archive", &archived_dot]).0, 0);

    json(&dir, &["mv", "SRC-1", "--to-project", "DST", "--json"]);

    // Архивный узел читается обычным `show`: архив скрывает его из списков, но не из базы.
    let archived = json(&dir, &["show", &archived_dot, "--json", "--project", "SRC"]);
    assert_eq!(
        archived["description"].as_str(),
        Some("старая ссылка SRC-1.1"),
        "архивная запись обязана хранить прежний адрес: {archived}"
    );
}

/// В перенесённый узел добавляется комментарий о прежнем месте.
#[test]
fn the_moved_node_carries_a_comment_about_its_previous_home() {
    let dir = seeded("comment");
    json(&dir, &["mv", "SRC-1", "--to-project", "DST", "--json"]);

    let comments = json(
        &dir,
        &["comment", "ls", "DST-1", "--json", "--project", "DST"],
    );
    let rows = comments.as_array().expect("array");
    assert_eq!(rows.len(), 1, "{comments}");
    let text = rows[0]["content"].as_str().expect("content");
    assert!(text.contains("SRC-1"), "{text}");
    assert!(text.contains("DST-1"), "{text}");
    assert!(text.contains("SRC"), "проект-источник назван: {text}");
    assert_eq!(rows[0]["author"], "tasq");
}

/// История, комментарии и аренды переезжают вместе с узлом: они держатся за `node_id`.
#[test]
fn history_and_comments_travel_with_the_node() {
    let dir = seeded("history");
    assert_eq!(
        run(
            &dir,
            &[
                "comment",
                "add",
                "SRC-1",
                "работа начата",
                "--project",
                "SRC"
            ]
        )
        .0,
        0
    );
    assert_eq!(run(&dir, &["set", "SRC-1", "--status", "backlog"]).0, 0);

    json(&dir, &["mv", "SRC-1", "--to-project", "DST", "--json"]);

    let history = json(&dir, &["history", "DST-1", "--json", "--project", "DST"]);
    let causes: Vec<&str> = history["transitions"]
        .as_array()
        .expect("transitions")
        .iter()
        .map(|row| row["cause"].as_str().expect("cause"))
        .collect();
    assert!(causes.contains(&"created"), "{causes:?}");
    assert!(causes.contains(&"manual"), "{causes:?}");

    let comments = json(
        &dir,
        &["comment", "ls", "DST-1", "--json", "--project", "DST"],
    );
    let texts: Vec<&str> = comments
        .as_array()
        .expect("array")
        .iter()
        .map(|row| row["content"].as_str().expect("content"))
        .collect();
    assert!(texts.contains(&"работа начата"), "{texts:?}");
}

/// Перенос под родителя целевого проекта: номер берётся свободный.
#[test]
fn a_parent_in_the_target_project_is_honoured() {
    let dir = seeded("parent");
    assert_eq!(run(&dir, &["add", "Хозяин", "--project", "DST"]).0, 0);

    let report = json(
        &dir,
        &[
            "mv",
            "SRC-1.1",
            "--to-project",
            "DST",
            "--parent",
            "DST-1",
            "--json",
        ],
    );
    assert_eq!(report["dot_id"], "DST-1.1", "{report}");
    assert_eq!(report["nodes"], 2, "задача и её подзадача");
}

/// Занятый адрес в целевом проекте не роняет перенос: берётся следующий свободный номер.
#[test]
fn a_taken_number_takes_the_next_free_one() {
    let dir = seeded("taken");
    assert_eq!(run(&dir, &["add", "Уже есть", "--project", "DST"]).0, 0);

    let report = json(&dir, &["mv", "SRC-1", "--to-project", "DST", "--json"]);
    assert_eq!(
        report["dot_id"], "DST-2",
        "номер 1 в целевом проекте занят: {report}"
    );
}

/// Ошибки: неизвестный проект, перенос в свой же проект.
#[test]
fn refusals_name_the_reason() {
    let dir = seeded("refuse");

    let (code, _out, stderr) = run(&dir, &["mv", "SRC-1", "--to-project", "NOPE"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("PROJECT_NOT_FOUND"), "{stderr}");

    let (code, _out, stderr) = run(&dir, &["mv", "SRC-1", "--to-project", "SRC"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("INVALID_VALUE"), "{stderr}");
    assert!(stderr.contains("mv"), "подсказка называет замену: {stderr}");
}

/// Задача, поставленная в корень, становится эпиком — вид подчиняется глубине.
///
/// До этой правки перенос менял `depth` и `path`, но не `kind`: в базе оказывался «эпик»
/// глубины 3 (или «задача» глубины 1), и набор допустимых статусов начинал зависеть от того,
/// каким путём узел сюда попал.
#[test]
fn a_task_moved_to_the_root_becomes_an_epic() {
    let dir = seeded("kind-root");
    // SRC-1.1 — задача с подзадачей SRC-1.1.1.
    let report = json(&dir, &["mv", "SRC-1.1", "--to-project", "DST", "--json"]);
    assert_eq!(report["dot_id"], "DST-1", "{report}");
    assert_eq!(
        report["rekinded"], 2,
        "вид сменился у задачи и у её подзадачи: {report}"
    );

    let moved = json(&dir, &["show", "DST-1", "--json", "--project", "DST"]);
    assert_eq!(moved["kind"], "epic", "глубина 1 — это эпик: {moved}");
    assert_eq!(moved["depth"], 1);

    let child = json(&dir, &["show", "DST-1.1", "--json", "--project", "DST"]);
    assert_eq!(
        child["kind"], "task",
        "бывшая подзадача стала задачей: {child}"
    );
    assert_eq!(child["depth"], 2);
}

/// Эпик со всем поддеревом переезжает, и виды всех уровней остаются прежними.
#[test]
fn an_epic_travels_with_its_whole_subtree() {
    let dir = seeded("kind-epic");
    let report = json(&dir, &["mv", "SRC-1", "--to-project", "DST", "--json"]);
    assert_eq!(report["nodes"], 3);
    assert_eq!(
        report["rekinded"], 0,
        "глубины те же — вид меняться не должен: {report}"
    );

    for (dot, kind, depth) in [
        ("DST-1", "epic", 1),
        ("DST-1.1", "task", 2),
        ("DST-1.1.1", "subtask", 3),
    ] {
        let node = json(&dir, &["show", dot, "--json", "--project", "DST"]);
        assert_eq!(node["kind"], kind, "{dot}: {node}");
        assert_eq!(node["depth"], depth, "{dot}: {node}");
    }
}

/// Эпик, опущенный под задачу, становится подзадачей — и это работает в своём проекте тоже.
#[test]
fn an_epic_moved_under_a_task_becomes_a_subtask() {
    let dir = seeded("kind-down");
    // Второй эпик с задачей — под неё опускаем первый эпик. Адреса берутся из ответа, а не
    // угадываются: номер нового корня зависит от того, сколько их уже есть в проекте.
    let host = json(&dir, &["add", "Второй эпик", "--project", "SRC", "--json"]);
    let host_dot = host["dot_id"].as_str().expect("dot_id").to_string();
    let child_of_host = json(
        &dir,
        &[
            "add",
            "Его задача",
            "--parent",
            &host_dot,
            "--project",
            "SRC",
            "--json",
        ],
    );
    let child_dot = child_of_host["dot_id"]
        .as_str()
        .expect("dot_id")
        .to_string();

    let report = json(
        &dir,
        &[
            "mv",
            "SRC-1",
            "--parent",
            &child_dot,
            "--json",
            "--project",
            "SRC",
        ],
    );
    assert_eq!(report["depth"], 3, "{report}");
    assert_eq!(
        report["kind"], "subtask",
        "эпик глубины 3 — подзадача: {report}"
    );

    // Дети сдвинулись вместе с ним на две ступени: задача была глубины 2 и стала 4, подзадача —
    // 3 и 5. Всё, что глубже третьего уровня, — микро-задача, поэтому обе теперь `micro`.
    let moved_dot = report["dot_id"].as_str().expect("dot_id").to_string();
    let child = json(
        &dir,
        &[
            "show",
            &format!("{moved_dot}.1"),
            "--json",
            "--project",
            "SRC",
        ],
    );
    assert_eq!(child["depth"], 4, "{child}");
    assert_eq!(child["kind"], "micro", "{child}");
    let grandchild = json(
        &dir,
        &[
            "show",
            &format!("{moved_dot}.1.1"),
            "--json",
            "--project",
            "SRC",
        ],
    );
    assert_eq!(grandchild["depth"], 5, "{grandchild}");
    assert_eq!(grandchild["kind"], "micro", "{grandchild}");
}

/// Статус приводится к набору нового вида: эпику не бывает `todo`, остальным — `planned`.
#[test]
fn the_status_follows_the_new_kind() {
    let dir = seeded("kind-status");
    // Эпик SRC-1 в planned (стартовый для эпика после backlog), задача SRC-1.1 в todo.
    assert_eq!(
        run(
            &dir,
            &["set", "SRC-1", "--status", "backlog", "--project", "SRC"]
        )
        .0,
        0
    );
    assert_eq!(
        run(
            &dir,
            &["set", "SRC-1", "--status", "planned", "--project", "SRC"]
        )
        .0,
        0
    );
    assert_eq!(run(&dir, &["set", "SRC-1.1", "--status", "backlog"]).0, 0);
    assert_eq!(run(&dir, &["set", "SRC-1.1", "--status", "todo"]).0, 0);

    // Задача в корень: todo запрещён эпику, статус становится planned.
    json(&dir, &["mv", "SRC-1.1", "--to-project", "DST", "--json"]);
    let moved = json(&dir, &["show", "DST-1", "--json", "--project", "DST"]);
    assert_eq!(moved["kind"], "epic");
    assert_eq!(
        moved["status"], "planned",
        "эпику запрещён todo — статус приведён к planned: {moved}"
    );

    // Эпик под задачу: planned запрещён остальным, статус становится todo.
    let host = json(&dir, &["add", "Хозяин", "--project", "SRC", "--json"]);
    let host_dot = host["dot_id"].as_str().expect("dot_id").to_string();
    let child = json(
        &dir,
        &[
            "add",
            "Его задача",
            "--parent",
            &host_dot,
            "--project",
            "SRC",
            "--json",
        ],
    );
    let child_dot = child["dot_id"].as_str().expect("dot_id").to_string();
    let lowered_report = json(
        &dir,
        &[
            "mv",
            "SRC-1",
            "--parent",
            &child_dot,
            "--json",
            "--project",
            "SRC",
        ],
    );
    let lowered_dot = lowered_report["dot_id"]
        .as_str()
        .expect("dot_id")
        .to_string();
    let lowered = json(&dir, &["show", &lowered_dot, "--json", "--project", "SRC"]);
    assert_eq!(lowered["kind"], "subtask", "{lowered}");
    assert_eq!(
        lowered["status"], "todo",
        "подзадаче запрещён planned — статус приведён к todo: {lowered}"
    );
}

/// Приёмка 16.3: после переноса цепочка контекста и журнал читаются на новом месте.
///
/// `ctx` идёт от корня к узлу по `parent_id`, а `log` — по журналу проекта: если бы перенос
/// оставлял потомков в старом проекте или терял записи, оба ответа были бы неполными, и заметить
/// это можно было бы только глазами.
#[test]
fn context_and_log_are_complete_after_the_move() {
    let dir = seeded("ctx");
    assert_eq!(
        run(
            &dir,
            &[
                "comment",
                "add",
                "SRC-1.1",
                "решение по задаче",
                "--project",
                "SRC"
            ]
        )
        .0,
        0
    );
    json(&dir, &["mv", "SRC-1", "--to-project", "DST", "--json"]);

    // `ctx` на самом глубоком узле: цепочка от нового эпика до него.
    let chain = json(&dir, &["ctx", "DST-1.1.1", "--json", "--project", "DST"]);
    let chain_text = serde_json::to_string(&chain).expect("json");
    assert!(chain_text.contains("DST-1"), "{chain_text}");
    assert!(
        !chain_text.contains("\"SRC-"),
        "в цепочке остался старый адрес: {chain_text}"
    );

    // Журнал целевого проекта знает о переезде и о правках, сделанных до него.
    let log = json(
        &dir,
        &["log", "--json", "--limit", "50", "--project", "DST"],
    );
    let actions: Vec<&str> = log
        .as_array()
        .expect("array")
        .iter()
        .map(|row| row["action"].as_str().expect("action"))
        .collect();
    assert!(
        actions.contains(&"move_project"),
        "в журнале нет переезда: {actions:?}"
    );
    // Журнал — история ПРОЕКТА, и создание узла произошло в исходном: в журнале целевого его
    // нет и быть не должно. Полнота истории узла живёт в другой таблице — `node_status_history`,
    // которая держится за `node_id` и переезжает вместе с ним. Проверяется именно она.
    assert!(
        !actions.contains(&"create"),
        "записи исходного проекта переехали в журнал целевого: {actions:?}"
    );
    let history = json(&dir, &["history", "DST-1.1", "--json", "--project", "DST"]);
    let causes: Vec<&str> = history["transitions"]
        .as_array()
        .expect("transitions")
        .iter()
        .map(|row| row["cause"].as_str().expect("cause"))
        .collect();
    assert!(
        causes.contains(&"created"),
        "история узла обязана помнить создание: {causes:?}"
    );

    // Комментарий, написанный до переезда, читается на новом месте.
    let comments = json(
        &dir,
        &["comment", "ls", "DST-1.1", "--json", "--project", "DST"],
    );
    let texts: Vec<&str> = comments
        .as_array()
        .expect("array")
        .iter()
        .map(|row| row["content"].as_str().expect("content"))
        .collect();
    assert!(texts.contains(&"решение по задаче"), "{texts:?}");
}
