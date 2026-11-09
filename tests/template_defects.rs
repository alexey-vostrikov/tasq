//! Шаблоны: разбор входа, проверка дерева и подстановка переменных.
//!
//! Шаблон приходит от человека и из файла, поэтому каждая ошибка в нём должна быть
//! названа до записи: битый JSON, пустой список, недопустимый вид или приоритет,
//! отсутствующая переменная. Иначе половина дерева создастся, а половина — нет.

use std::path::PathBuf;

use rusqlite::Connection;

use tasq::db;
use tasq::node;
use tasq::project::{self, Project};
use tasq::template::{self, TemplateTree};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-template-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn fixture(tag: &str) -> (Connection, Project) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "TASQ", "demo").expect("init");
    let conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");
    (conn, project)
}

fn save(conn: &mut Connection, project: &Project, name: &str, raw: &str) -> tasq::Result<()> {
    template::save(conn, project, name, None, raw, "-").map(|_| ())
}

/// Вход шаблона: битый JSON, скаляр, объект без `nodes`, пустой список.
#[test]
fn template_input_is_parsed_strictly() {
    for (raw, needle) in [
        ("{ не json", "not valid JSON"),
        ("42", "expected an array or an object"),
        // Объект без `nodes` разбирается в пустое дерево и отбивается тем же «no nodes».
        (r#"{"foo": 1}"#, "no nodes"),
        ("[]", "no nodes"),
        (r#"{"nodes": []}"#, "no nodes"),
    ] {
        let error = TemplateTree::parse(raw).expect_err(raw);
        assert_eq!(error.code(), "INVALID_VALUE", "вход {raw}");
        assert!(
            error.to_string().contains(needle),
            "вход {raw}: {error} — ожидалось «{needle}»"
        );
    }

    // Обе допустимые формы дают одно дерево.
    let from_array = TemplateTree::parse(r#"[{"title": "Задача"}]"#).expect("array");
    let from_object = TemplateTree::parse(r#"{"nodes": [{"title": "Задача"}]}"#).expect("object");
    assert_eq!(from_array.nodes.len(), from_object.nodes.len());
}

/// Подстановка: незакрытая скобка остаётся текстом, отсутствующая переменная — отказ.
#[test]
fn rendering_handles_unfinished_braces() {
    let values = vec![("resource".to_string(), "orders".to_string())];

    assert_eq!(
        template::render("{resource}: контроллер", &values).expect("render"),
        "orders: контроллер"
    );
    // Хвост без закрывающей скобки — не переменная, а обычный текст.
    assert_eq!(
        template::render("фигурная { скобка", &values).expect("render"),
        "фигурная { скобка"
    );
    // Неизвестное имя — отказ с именем переменной в сообщении.
    let error = template::render("{missing}", &values).expect_err("нет значения");
    assert_eq!(error.code(), "INVALID_VALUE");
    assert!(error.to_string().contains("{missing}"), "{error}");
}

/// Проверка дерева до записи: пустое имя узла, недопустимый вид или приоритет.
#[test]
fn invalid_trees_are_refused_before_writing() {
    let (mut conn, project) = fixture("validate");

    let error =
        save(&mut conn, &project, "empty-title", r#"[{"title": "   "}]"#).expect_err("пустое имя");
    assert_eq!(error.code(), "INVALID_VALUE");
    assert!(error.to_string().contains("empty title"), "{error}");

    let error = save(
        &mut conn,
        &project,
        "bad-kind",
        r#"[{"title": "Узел", "kind": "эпичок"}]"#,
    )
    .expect_err("вид не из схемы");
    assert_eq!(error.code(), "INVALID_VALUE");
    assert!(error.to_string().contains("kind"), "{error}");

    let error = save(
        &mut conn,
        &project,
        "bad-priority",
        r#"[{"title": "Узел", "priority": "срочно"}]"#,
    )
    .expect_err("приоритет не из схемы");
    assert_eq!(error.code(), "INVALID_VALUE");
    assert!(error.to_string().contains("priority"), "{error}");

    // В базе после отказов пусто: шаблон не сохранился.
    let templates = template::list(&conn, &project).expect("list");
    assert!(templates.is_empty(), "{templates:?}");
}

/// Слишком глубокое дерево отбивается пределом, а не переполнением стека.
#[test]
fn deep_template_is_bounded() {
    let (mut conn, project) = fixture("deep");

    // Собираем вложенность 34 уровня: предел — 32.
    let mut node = String::from(r#"{"title": "лист"}"#);
    for _ in 0..34 {
        node = format!(r#"{{"title": "уровень", "children": [{node}]}}"#);
    }
    let raw = format!("[{node}]");

    let error = save(&mut conn, &project, "deep", &raw).expect_err("глубокий шаблон");
    assert_eq!(error.code(), "DEPTH_LIMIT");
}

/// Имя шаблона обязательно, а поиск по id для отсутствующего шаблона — отказ.
#[test]
fn names_and_ids_are_checked() {
    let (mut conn, project) = fixture("names");

    let error = save(&mut conn, &project, "   ", r#"[{"title": "Узел"}]"#).expect_err("имя");
    assert_eq!(error.code(), "INVALID_VALUE");
    assert!(error.to_string().contains("name"), "{error}");

    save(&mut conn, &project, "api", r#"[{"title": "Узел"}]"#).expect("save");

    let error = template::by_id(&conn, &project, 999).expect_err("нет id");
    assert_eq!(error.code(), "INVALID_VALUE");
    assert!(error.to_string().contains("999"), "{error}");

    // Повторное сохранение под тем же именем обновляет, а не дублирует.
    save(
        &mut conn,
        &project,
        "api",
        r#"[{"title": "Узел"}, {"title": "Второй"}]"#,
    )
    .expect("update");
    let stored = template::by_name(&conn, &project, "api").expect("by name");
    assert_eq!(stored.tree.nodes.len(), 2);
    assert_eq!(template::list(&conn, &project).expect("list").len(), 1);
}

/// Применение без родителя сажает дерево в корень проекта, а не создаёт «TASQ--».
#[test]
fn applying_at_root_names_the_project() {
    let (mut conn, project) = fixture("root");
    save(
        &mut conn,
        &project,
        "pair",
        r#"[{"title": "{resource}"}, {"title": "{resource}: тесты"}]"#,
    )
    .expect("save");

    let applied = template::apply(
        &mut conn,
        &project,
        "pair",
        None,
        &[("resource".to_string(), "orders".to_string())],
    )
    .expect("apply");

    assert_eq!(applied.parent, "TASQ", "родитель — ключ проекта");
    assert_eq!(applied.created.len(), 2);
    let nodes = node::list(&conn, &project, &node::Filter::default()).expect("list");
    let titles: Vec<&str> = nodes.iter().map(|node| node.title.as_str()).collect();
    assert!(titles.contains(&"orders"), "{titles:?}");
    assert!(titles.contains(&"orders: тесты"), "{titles:?}");

    // Отсутствующая переменная отбивается до создания узлов.
    let before = nodes.len();
    let error = template::apply(&mut conn, &project, "pair", None, &[]).expect_err("нет значения");
    assert_eq!(error.code(), "INVALID_VALUE");
    assert!(error.to_string().contains("missing values"), "{error}");
    let after = node::list(&conn, &project, &node::Filter::default()).expect("list");
    assert_eq!(after.len(), before, "ничего не создано при отказе");
}
