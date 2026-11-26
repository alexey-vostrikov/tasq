//! Метки и свободные поля узла: колонки, которые были в схеме и не писались ничем.
//!
//! Зачем проверять то, чего раньше не было. `nodes.tags` и `nodes.metadata` существовали с первой
//! версии схемы, ездили в дампе и не заполнялись ни одним путём кода: поле, которое обещает
//! возможность, хуже отсутствующего — на него рассчитывают. Теперь они пишутся, читаются,
//! отбираются и переезжают дампом, и всё это проверяется здесь, а не на слово.
//!
//! Отдельно проверяется, что метка — это МНОЖЕСТВО: порядок и повторы не должны выглядеть
//! изменением. Иначе правка «те же метки в другом порядке» писала бы в журнал пустую запись, а
//! сравнение в тестах и в отчётах ловило бы перестановку как изменение.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use tasq::project::{self, Project};
use tasq::{db, node};

fn fixture(tag: &str) -> (PathBuf, Connection, Project) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-tags-{tag}-{nanos}"));
    let outcome = db::init(&dir, "TAGS", "метки").expect("init");
    let conn = db::open(Path::new(&outcome.db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");
    (dir, conn, project)
}

fn add(conn: &mut Connection, project: &Project, title: &str, tags: &[String]) -> node::Node {
    node::create(
        conn,
        project,
        node::NewNode {
            parent: None,
            title,
            kind: None,
            description: None,
            priority: None,
            due_date: None,
            estimated_hours: None,
            weight: None,
            source_ref: None,
            branch: None,
            tags: (!tags.is_empty()).then_some(tags),
        },
    )
    .expect("узел")
}

fn tags(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

/// Метки пишутся при создании и видны в чтении — без отдельного вызова.
#[test]
fn tags_are_written_on_create_and_read_back() {
    let (dir, mut conn, project) = fixture("create");
    let node = add(&mut conn, &project, "Ручка API", &tags(&["api", "backend"]));
    assert_eq!(node.tags, tags(&["api", "backend"]));

    let loaded = node::resolve(&conn, &project, &node.dot_id).expect("чтение");
    assert_eq!(loaded.tags, tags(&["api", "backend"]));

    // Пустой набор — пустой список, а не NULL и не `[null]`.
    let plain = add(&mut conn, &project, "Без меток", &[]);
    assert!(plain.tags.is_empty());

    let _ = std::fs::remove_dir_all(dir);
}

/// Метка — множество: порядок и повторы не считаются изменением.
#[test]
fn tags_are_a_set_not_a_sequence() {
    let (dir, mut conn, project) = fixture("set");
    let node = add(
        &mut conn,
        &project,
        "Узел",
        &tags(&["dns", "api", "dns", " api "]),
    );
    assert_eq!(
        node.tags,
        tags(&["api", "dns"]),
        "повторы и порядок обязаны нормализоваться при записи"
    );

    // Правка тем же набором в другом порядке — не событие, и в журнале её быть не должно.
    let before: i64 = conn
        .query_row("SELECT COUNT(*) FROM activity_log", [], |row| row.get(0))
        .expect("count");
    node::update(
        &mut conn,
        &project,
        &node.dot_id,
        node::Patch {
            tag_add: &tags(&["api"]),
            ..node::Patch::default()
        },
    )
    .expect("правка");
    let after: i64 = conn
        .query_row("SELECT COUNT(*) FROM activity_log", [], |row| row.get(0))
        .expect("count");
    assert_eq!(
        before, after,
        "добавление уже стоящей метки написало запись в журнал"
    );

    let _ = std::fs::remove_dir_all(dir);
}

/// Три намерения — три разные вещи: добавить, снять, заменить набор.
#[test]
fn adding_removing_and_replacing_are_different_intentions() {
    let (dir, mut conn, project) = fixture("intents");
    let node = add(&mut conn, &project, "Узел", &tags(&["api"]));

    let added = node::update(
        &mut conn,
        &project,
        &node.dot_id,
        node::Patch {
            tag_add: &tags(&["urgent"]),
            ..node::Patch::default()
        },
    )
    .expect("добавление");
    assert_eq!(
        added.tags,
        tags(&["api", "urgent"]),
        "добавление заменило набор"
    );

    let removed = node::update(
        &mut conn,
        &project,
        &node.dot_id,
        node::Patch {
            tag_remove: &tags(&["api"]),
            ..node::Patch::default()
        },
    )
    .expect("снятие");
    assert_eq!(removed.tags, tags(&["urgent"]));

    let replaced = node::update(
        &mut conn,
        &project,
        &node.dot_id,
        node::Patch {
            tags: Some(&tags(&["prod", "api"])),
            ..node::Patch::default()
        },
    )
    .expect("замена");
    assert_eq!(replaced.tags, tags(&["api", "prod"]));

    let _ = std::fs::remove_dir_all(dir);
}

/// Отбор по метке — точный и в SQL: `api` не находит `apiv2`.
#[test]
fn the_tag_filter_matches_exactly() {
    let (dir, mut conn, project) = fixture("filter");
    let api = add(&mut conn, &project, "Ручка", &tags(&["api"]));
    add(&mut conn, &project, "Вторая версия", &tags(&["apiv2"]));
    add(&mut conn, &project, "Без меток", &[]);

    let found = node::list(
        &conn,
        &project,
        &node::Filter {
            tag: Some("api"),
            ..node::Filter::default()
        },
    )
    .expect("список");
    assert_eq!(found.len(), 1, "фильтр по метке вернул не то: {found:?}");
    assert_eq!(found[0].dot_id, api.dot_id);

    let _ = std::fs::remove_dir_all(dir);
}

/// Свободные поля: выставить, прочитать, снять; ключ перезаписывается.
#[test]
fn metadata_fields_can_be_set_read_and_removed() {
    let (dir, mut conn, project) = fixture("meta");
    let node = add(&mut conn, &project, "Узел", &[]);

    let with_meta = node::update(
        &mut conn,
        &project,
        &node.dot_id,
        node::Patch {
            meta_set: &[
                ("port".to_string(), "389".to_string()),
                ("owner".to_string(), "ldap".to_string()),
            ],
            ..node::Patch::default()
        },
    )
    .expect("выставление");
    assert_eq!(
        with_meta.metadata.get("port").map(String::as_str),
        Some("389")
    );

    let overwritten = node::update(
        &mut conn,
        &project,
        &node.dot_id,
        node::Patch {
            meta_set: &[("port".to_string(), "636".to_string())],
            ..node::Patch::default()
        },
    )
    .expect("перезапись");
    assert_eq!(
        overwritten.metadata.get("port").map(String::as_str),
        Some("636"),
        "ключ обязан перезаписываться, а не дублироваться"
    );
    assert_eq!(
        overwritten.metadata.get("owner").map(String::as_str),
        Some("ldap"),
        "чужие ключи не должны теряться"
    );

    let removed = node::update(
        &mut conn,
        &project,
        &node.dot_id,
        node::Patch {
            meta_remove: &["owner".to_string()],
            ..node::Patch::default()
        },
    )
    .expect("снятие");
    assert!(!removed.metadata.contains_key("owner"));

    let _ = std::fs::remove_dir_all(dir);
}

/// Пустой ключ свободного поля — отказ: поле без имени нельзя ни прочитать, ни снять.
#[test]
fn an_empty_metadata_key_is_refused() {
    let (dir, mut conn, project) = fixture("meta-empty");
    let node = add(&mut conn, &project, "Узел", &[]);
    let error = node::update(
        &mut conn,
        &project,
        &node.dot_id,
        node::Patch {
            meta_set: &[("  ".to_string(), "value".to_string())],
            ..node::Patch::default()
        },
    )
    .expect_err("пустой ключ прошёл");
    assert_eq!(error.code(), "INVALID_VALUE");

    let _ = std::fs::remove_dir_all(dir);
}

/// Метки переезжают дампом: экспорт и импорт в чистую базу сохраняют и их, и поля.
#[test]
fn tags_and_metadata_survive_a_dump() {
    let (dir, mut conn, project) = fixture("dump");
    let node = add(&mut conn, &project, "Узел", &tags(&["api", "prod"]));
    let mut metadata = BTreeMap::new();
    metadata.insert("port".to_string(), "389".to_string());
    node::update(
        &mut conn,
        &project,
        &node.dot_id,
        node::Patch {
            meta_set: &[("port".to_string(), "389".to_string())],
            ..node::Patch::default()
        },
    )
    .expect("поле");

    let document = tasq::transfer::export(&conn, &project, false).expect("экспорт");
    let target_dir = std::env::temp_dir().join(format!(
        "{}-restored",
        dir.file_name().expect("имя").to_string_lossy()
    ));
    let outcome = db::init(&target_dir, "TAGS", "метки").expect("init");
    let mut target = db::open(Path::new(&outcome.db_path)).expect("open");
    let restored_project = project::first_project(&target).expect("project");
    tasq::transfer::import(
        &mut target,
        &restored_project,
        &document,
        tasq::transfer::Mode::Merge,
        "тест",
    )
    .expect("импорт");

    let restored = node::resolve(&target, &restored_project, &node.dot_id).expect("узел");
    assert_eq!(
        restored.tags,
        tags(&["api", "prod"]),
        "метки потерялись в дампе"
    );
    assert_eq!(
        restored.metadata.get("port").map(String::as_str),
        Some("389"),
        "свободные поля потерялись в дампе"
    );

    let _ = std::fs::remove_dir_all(&target_dir);
    let _ = std::fs::remove_dir_all(dir);
}

/// Шаблон передаёт метки, и в них работают переменные.
#[test]
fn a_template_carries_tags_with_variables_rendered() {
    let (dir, mut conn, project) = fixture("template");
    let raw = r#"{"nodes": [{"title": "Ручка {name}", "tags": ["api-{name}", "backend"]}]}"#;
    tasq::template::save(&mut conn, Some(&project), "handler", None, raw, "тест")
        .expect("сохранение шаблона");
    let applied = tasq::template::apply(
        &mut conn,
        &project,
        "handler",
        None,
        &[("name".to_string(), "users".to_string())],
    )
    .expect("применение");

    let created = node::resolve(&conn, &project, &applied.created[0]).expect("узел");
    assert_eq!(
        created.tags,
        tags(&["api-users", "backend"]),
        "метки шаблона не дошли до узла или не подставили переменную"
    );

    let _ = std::fs::remove_dir_all(dir);
}
