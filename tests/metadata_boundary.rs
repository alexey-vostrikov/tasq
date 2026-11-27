//! Граница между метками и свободными полями: заявлена, проверена, не зависит от типа значения.
//!
//! Реестр ключей — `src/meta.rs`. Здесь проверяется то, что реестр обещает, и то, из-за чего он
//! появился: `metadata.saga_id`, пришедший числом, ломал чтение узла целиком, а читался он
//! вообще без аксессора — строковым литералом в одном месте и никак в другом.

use std::fs;

use tasq::db;
use tasq::meta;
use tasq::node;
use tasq::project;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-meta-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn create(conn: &mut rusqlite::Connection, title: &str) -> node::Node {
    let project = project::first_project(conn).expect("проект");
    node::create(
        conn,
        &project,
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
            tags: None,
        },
    )
    .expect("узел")
}

/// Чужое значение свободного поля не должно ломать чтение: число приходит строкой.
///
/// Это ровно тот дефект, который остановил импорт: дамп предшественника писал `saga_id: 21`
/// числом, слой чтения ждал объект строк, и узел не читался вовсе — с ошибкой serde, ничего не
/// говорящей о причине. Проверка идёт через публичную запись: `update` с `meta_set` и чтение
/// аксессором, а не разбором JSON.
#[test]
fn a_reserved_key_comes_back_as_text_whatever_it_was_written_as() {
    let dir = temp_dir("saga-id");
    let outcome = db::init(&dir, "META", "граница").expect("init");
    let path = std::path::PathBuf::from(&outcome.db_path);
    let mut conn = db::open_checked(&path).expect("open");
    let created = create(&mut conn, "узел с происхождением");

    // Число превращается в текст на границе (`normalise_metadata_json`), потому что хранимое
    // поле — объект строк, и тип значения не должен зависеть от того, кто его записал.
    let normalised = node::normalise_metadata_json(&serde_json::json!({
        meta::NODE_SAGA_ID: 21,
        "note": "число и строка в одном объекте",
    }));
    let parsed: std::collections::BTreeMap<String, String> =
        serde_json::from_str(&normalised).expect("объект строк");
    assert_eq!(
        parsed.get(meta::NODE_SAGA_ID).map(String::as_str),
        Some("21")
    );

    let project = project::first_project(&conn).expect("проект");
    node::update(
        &mut conn,
        &project,
        &created.dot_id,
        node::Patch {
            title: None,
            description: None,
            status: None,
            priority: None,
            assigned_to: None,
            due_date: None,
            weight: None,
            estimated_hours: None,
            source_ref: None,
            branch: None,
            tag_add: &[],
            tag_remove: &[],
            tags: None,
            meta_set: &[(meta::NODE_SAGA_ID.to_string(), "21".to_string())],
            meta_remove: &[],
            force: true,
            description_locked: None,
            lease: None,
            actor: None,
        },
    )
    .expect("запись свободного поля");

    let (_, node) = node::resolve_shown(
        &conn,
        &project::first_project(&conn).expect("проект"),
        &created.dot_id,
    )
    .expect("чтение узла");
    assert_eq!(
        node.saga_id(),
        Some("21"),
        "аксессор не нашёл зарезервированный ключ"
    );
    assert_eq!(
        node.metadata.get("note"),
        None,
        "чужое поле не должно появиться само"
    );
}

/// Пустое значение — это «не было», а не «неизвестно».
///
/// Разница видна только в отчётах: `saga_id=""` читался бы как подтверждённое происхождение из
/// системы, где id пустой. Пустая строка в свободных полях — обычный след удаления значения, и
/// толковать её как факт нельзя.
#[test]
fn an_empty_reserved_value_reads_as_absent() {
    let dir = temp_dir("empty");
    let outcome = db::init(&dir, "META", "пусто").expect("init");
    let path = std::path::PathBuf::from(&outcome.db_path);
    let mut conn = db::open_checked(&path).expect("open");
    let created = create(&mut conn, "узел без происхождения");

    let project = project::first_project(&conn).expect("проект");
    node::update(
        &mut conn,
        &project,
        &created.dot_id,
        node::Patch {
            title: None,
            description: None,
            status: None,
            priority: None,
            assigned_to: None,
            due_date: None,
            weight: None,
            estimated_hours: None,
            source_ref: None,
            branch: None,
            tag_add: &[],
            tag_remove: &[],
            tags: None,
            meta_set: &[(meta::NODE_SAGA_ID.to_string(), "   ".to_string())],
            meta_remove: &[],
            force: true,
            description_locked: None,
            lease: None,
            actor: None,
        },
    )
    .expect("запись пустого значения");

    let (_, node) = node::resolve_shown(
        &conn,
        &project::first_project(&conn).expect("проект"),
        &created.dot_id,
    )
    .expect("узел");
    assert_eq!(node.saga_id(), None, "пустое значение выдало себя за факт");
    let _ = fs::remove_dir_all(&dir);
}

/// Ключ `projects.metadata.reopen` читается по имени из реестра и меняет строгость перехода.
///
/// Проверка не «поле существует», а «настройка действует»: значение `allow` обязано снимать
/// требование `--force` при реоткрытии. Иначе реестр описывал бы ключ, который ни на что не
/// влияет, — а такой ключ хуже отсутствующего.
#[test]
fn the_project_reopen_policy_is_read_from_its_reserved_key() {
    let dir = temp_dir("reopen");
    let outcome = db::init(&dir, "META", "политика").expect("init");
    let path = std::path::PathBuf::from(&outcome.db_path);
    let conn = db::open_checked(&path).expect("open");

    let strict = project::first_project(&conn).expect("проект");
    assert_eq!(
        strict.reopen,
        tasq::status::Reopen::ForceRequired,
        "по умолчанию реоткрытие требует --force"
    );

    let metadata = serde_json::json!({ meta::PROJECT_REOPEN: "allow" }).to_string();
    conn.execute(
        "UPDATE projects SET metadata = ?1 WHERE id = ?2",
        rusqlite::params![metadata, strict.id],
    )
    .expect("настройка проекта");

    let relaxed = project::first_project(&conn).expect("проект после настройки");
    assert_eq!(
        relaxed.reopen,
        tasq::status::Reopen::Allowed,
        "политика не прочиталась из зарезервированного ключа"
    );

    // Неизвестное значение — строгий режим, а не разрешение: опечатка не ослабляет правило.
    conn.execute(
        "UPDATE projects SET metadata = ?1 WHERE id = ?2",
        rusqlite::params![
            serde_json::json!({ meta::PROJECT_REOPEN: "разрешить" }).to_string(),
            strict.id
        ],
    )
    .expect("опечатка в настройке");
    let typo = project::first_project(&conn).expect("проект с опечаткой");
    assert_eq!(
        typo.reopen,
        tasq::status::Reopen::ForceRequired,
        "опечатка ослабила правило"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Метки и свободные поля переживают дамп и заливку, каждый в своём виде.
///
/// Метка остаётся множеством (порядок нормализован), поле — объектом строк. Если бы они хранились
/// одним полем, эта проверка была бы одна на два разных обещания; сейчас их два, и оба обязаны
/// выполняться после переезда базы, а не только при прямой записи.
#[test]
fn tags_and_metadata_survive_a_dump_and_restore() {
    let source = temp_dir("dump-src");
    let outcome = db::init(&source, "META", "дамп").expect("init");
    let path = std::path::PathBuf::from(&outcome.db_path);
    let mut conn = db::open_checked(&path).expect("open");
    let created = create(&mut conn, "узел с метками");

    let project = project::first_project(&conn).expect("проект");
    node::update(
        &mut conn,
        &project,
        &created.dot_id,
        node::Patch {
            title: None,
            description: None,
            status: None,
            priority: None,
            assigned_to: None,
            due_date: None,
            weight: None,
            estimated_hours: None,
            source_ref: None,
            branch: None,
            tag_add: &["api".to_string(), "urgent".to_string()],
            tag_remove: &[],
            tags: None,
            meta_set: &[
                (meta::NODE_SAGA_ID.to_string(), "77".to_string()),
                ("source".to_string(), "legacy".to_string()),
            ],
            meta_remove: &[],
            force: true,
            description_locked: None,
            lease: None,
            actor: None,
        },
    )
    .expect("метки и поля");

    // Экспорт проекта, а не `dump` всей базы: приёмник — одна новая база с одним проектом,
    // и заливать в неё документ со всеми таблицами значило бы проверять не то, что нужно.
    let export = tasq::transfer::export(
        &conn,
        &project::first_project(&conn).expect("проект"),
        false,
    )
    .expect("export");
    let text = serde_json::to_string(&export).expect("json");

    let target = temp_dir("dump-dst");
    let loaded = db::init(&target, "META", "дамп").expect("init");
    let target_path = std::path::PathBuf::from(&loaded.db_path);
    let mut fresh = db::open_checked(&target_path).expect("open");
    let document: serde_json::Value = serde_json::from_str(&text).expect("разбор дампа");
    let target_project = project::first_project(&fresh).expect("проект заливки");
    tasq::transfer::import(
        &mut fresh,
        &target_project,
        &document,
        tasq::transfer::Mode::Replace,
        "тест",
    )
    .expect("import");

    let nodes = node::list(
        &fresh,
        &project::first_project(&fresh).expect("проект"),
        &node::Filter::default(),
    )
    .expect("список");
    let restored = nodes
        .iter()
        .find(|candidate| candidate.title == "узел с метками")
        .expect("узел после заливки");
    assert_eq!(
        restored.tags,
        vec!["api".to_string(), "urgent".to_string()],
        "метки потерялись или переупорядочились"
    );
    assert_eq!(restored.saga_id(), Some("77"), "происхождение потерялось");
    assert_eq!(
        restored.metadata.get("source").map(String::as_str),
        Some("legacy"),
        "чужое поле потерялось"
    );

    let _ = fs::remove_dir_all(&source);
    let _ = fs::remove_dir_all(&target);
}
