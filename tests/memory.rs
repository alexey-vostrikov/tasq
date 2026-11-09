//! Заметки, комментарии, шаблоны, поиск, цепочка контекста, доска, время, пакетные правки.

use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;

use tasq::db;
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};
use tasq::status::Status;
use tasq::{comment, lease, note, overview, template};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-mem-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn fixture(tag: &str) -> (Connection, Project) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "TASQ", "demo").expect("init");
    let conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = project::default_project(&conn).expect("project");
    (conn, project)
}

fn add(conn: &mut Connection, project: &Project, parent: Option<&str>, title: &str) -> node::Node {
    node::create(
        conn,
        project,
        NewNode {
            parent,
            title,
            kind: None,
            description: None,
            priority: None,
            due_date: None,
            estimated_hours: None,
            weight: None,
            source_ref: None,
            branch: None,
        },
    )
    .expect("node")
}

#[test]
fn notes_cover_all_eight_types() {
    let (mut conn, project) = fixture("notes");
    for kind in [
        "general",
        "decision",
        "context",
        "meeting",
        "technical",
        "blocker",
        "progress",
        "release",
    ] {
        let saved = note::save(
            &mut conn,
            &project,
            note::SaveNote {
                id: None,
                title: Some(&format!("{kind} note")),
                content: Some("body"),
                note_type: Some(kind),
                node: None,
                actor: Some("agent"),
            },
        )
        .expect("save note");
        assert_eq!(saved.note_type, kind);
    }
    let all = note::list(&conn, &project, &note::Filter::default()).expect("list");
    assert_eq!(all.len(), 8);

    let decisions = note::list(
        &conn,
        &project,
        &note::Filter {
            note_type: Some("decision"),
            ..note::Filter::default()
        },
    )
    .expect("list decisions");
    assert_eq!(decisions.len(), 1);

    let err = note::list(
        &conn,
        &project,
        &note::Filter {
            note_type: Some("nonsense"),
            ..note::Filter::default()
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), "INVALID_VALUE");
}

#[test]
fn note_update_and_delete() {
    let (mut conn, project) = fixture("noteupd");
    let epic = add(&mut conn, &project, None, "Epic");
    let saved = note::save(
        &mut conn,
        &project,
        note::SaveNote {
            id: None,
            title: Some("Схема"),
            content: Some("v1"),
            note_type: Some("decision"),
            node: Some(&epic.dot_id),
            actor: None,
        },
    )
    .expect("save");
    assert_eq!(saved.dot_id.as_deref(), Some(epic.dot_id.as_str()));

    let updated = note::save(
        &mut conn,
        &project,
        note::SaveNote {
            id: Some(saved.id),
            title: None,
            content: Some("v2: выбрали WAL"),
            note_type: None,
            node: None,
            actor: None,
        },
    )
    .expect("update");
    assert_eq!(updated.content, "v2: выбрали WAL");
    assert_eq!(updated.title, "Схема", "не переданное поле не затирается");

    note::delete(&mut conn, &project, saved.id, "-").expect("delete");
    assert!(
        note::list(&conn, &project, &note::Filter::default())
            .expect("list")
            .is_empty()
    );
}

#[test]
fn comments_are_soft_deleted_and_restorable() {
    let (mut conn, project) = fixture("comments");
    let epic = add(&mut conn, &project, None, "Epic");

    let first = comment::add(&mut conn, &project, &epic.dot_id, "начал", "agent-a").expect("add");
    let second =
        comment::add(&mut conn, &project, &epic.dot_id, "продолжил", "agent-b").expect("add");
    assert_eq!(
        comment::list(&conn, &project, &epic.dot_id, false)
            .expect("list")
            .len(),
        2
    );

    let removed = comment::delete(&mut conn, &project, first.id, "agent-a", Some("устарело"))
        .expect("soft delete");
    assert!(removed.is_deleted);
    assert_eq!(removed.delete_reason.as_deref(), Some("устарело"));

    let visible = comment::list(&conn, &project, &epic.dot_id, false).expect("list");
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].id, second.id);

    let everything = comment::list(&conn, &project, &epic.dot_id, true).expect("list all");
    assert_eq!(everything.len(), 2, "удалённый виден по запросу");

    let restored = comment::restore(&mut conn, &project, first.id, "agent-a").expect("restore");
    assert!(!restored.is_deleted);
    assert_eq!(
        comment::list(&conn, &project, &epic.dot_id, false)
            .expect("list")
            .len(),
        2
    );

    // повторное удаление того же комментария — ошибка, а не тихий no-op
    comment::delete(&mut conn, &project, first.id, "agent-a", None).expect("delete");
    let err = comment::delete(&mut conn, &project, first.id, "agent-a", None).unwrap_err();
    assert_eq!(err.code(), "ALREADY_IN_STATE");
}

#[test]
fn template_variables_are_collected_and_rendered() {
    let (mut conn, project) = fixture("tpl");
    let epic = add(&mut conn, &project, None, "Эпик");
    let saved = template::save(
        &mut conn,
        &project,
        "api-handler",
        Some("Новая ручка API"),
        r#"[
            {"title": "Ручка {resource}", "kind": "task", "description": "GET /{resource}",
             "children": [
               {"title": "Тест {resource}", "kind": "subtask"},
               {"title": "Документация {resource}", "kind": "subtask"}
             ]}
        ]"#,
        "-",
    )
    .expect("save template");

    assert_eq!(saved.variables, vec!["resource".to_string()]);
    assert_eq!(saved.tree.nodes.len(), 1);
    assert_eq!(saved.tree.nodes[0].children.len(), 2);

    let applied = template::apply(
        &mut conn,
        &project,
        "api-handler",
        Some(&epic.dot_id),
        &[("resource".to_string(), "orders".to_string())],
    )
    .expect("apply");
    assert_eq!(applied.created.len(), 3);

    let nodes = node::list(&conn, &project, &node::Filter::default()).expect("list");
    let titles: Vec<&str> = nodes.iter().map(|n| n.title.as_str()).collect();
    assert!(titles.contains(&"Ручка orders"), "{titles:?}");
    assert!(titles.contains(&"Тест orders"), "{titles:?}");
    let task = nodes
        .iter()
        .find(|n| n.title == "Ручка orders")
        .expect("task");
    assert_eq!(task.description.as_deref(), Some("GET /orders"));
    assert_eq!(task.kind, "task");
    assert_eq!(
        nodes.iter().filter(|n| n.kind == "subtask").count(),
        2,
        "дети созданы как подзадачи"
    );
}

#[test]
fn template_apply_fails_before_writing_when_a_variable_is_missing() {
    let (mut conn, project) = fixture("tplmiss");
    template::save(
        &mut conn,
        &project,
        "release",
        None,
        r#"{"nodes": [{"title": "Выпустить {version}", "description": "тег {version}"}]}"#,
        "-",
    )
    .expect("save");

    let err = template::apply(&mut conn, &project, "release", None, &[]).unwrap_err();
    assert_eq!(err.code(), "INVALID_VALUE");
    assert!(err.to_string().contains("version"), "{err}");

    let nodes = node::list(&conn, &project, &node::Filter::default()).expect("list");
    assert!(nodes.is_empty(), "при ошибке не создаётся ничего");
}

#[test]
fn template_rejects_bad_kind_and_duplicate_name_updates_in_place() {
    let (mut conn, project) = fixture("tplkind");
    let err = template::save(
        &mut conn,
        &project,
        "bad",
        None,
        r#"[{"title": "X", "kind": "epiccc"}]"#,
        "-",
    )
    .unwrap_err();
    assert_eq!(err.code(), "INVALID_VALUE");

    template::save(
        &mut conn,
        &project,
        "same",
        None,
        r#"[{"title": "A"}]"#,
        "-",
    )
    .expect("first");
    template::save(
        &mut conn,
        &project,
        "same",
        None,
        r#"[{"title": "B"}]"#,
        "-",
    )
    .expect("second");
    let templates = template::list(&conn, &project).expect("list");
    assert_eq!(templates.len(), 1, "имя уникально в проекте");
    assert_eq!(templates[0].tree.nodes[0].title, "B");
}

#[test]
fn search_finds_nodes_and_notes_by_prefix() {
    let (mut conn, project) = fixture("search");
    let epic = add(&mut conn, &project, None, "Миграции схемы");
    node::update(
        &mut conn,
        &project,
        &epic.dot_id,
        node::Patch {
            description: Some("описать таблицы и индексы"),
            ..node::Patch::default()
        },
    )
    .expect("description");
    note::save(
        &mut conn,
        &project,
        note::SaveNote {
            id: None,
            title: Some("Решение по WAL"),
            content: Some("включаем WAL для параллельных читателей"),
            note_type: Some("decision"),
            node: Some(&epic.dot_id),
            actor: None,
        },
    )
    .expect("note");

    let hits = overview::search(&conn, &project, "миграц").expect("search");
    assert!(
        hits.iter()
            .any(|hit| hit.entity == "node" && hit.title.contains("Миграции")),
        "{hits:?}"
    );

    let by_body = overview::search(&conn, &project, "параллельных").expect("search body");
    assert!(
        by_body.iter().any(|hit| hit.entity == "note"),
        "поиск идёт и по тексту заметки: {by_body:?}"
    );

    // пунктуация не должна ломать поиск
    let odd = overview::search(&conn, &project, "WAL: параллельных").expect("odd query");
    assert!(!odd.is_empty(), "запрос со спецсимволом не падает");

    let err = overview::search(&conn, &project, "   ").unwrap_err();
    assert_eq!(err.code(), "INVALID_VALUE");
}

#[test]
fn search_index_follows_edits() {
    let (mut conn, project) = fixture("searchupd");
    let epic = add(&mut conn, &project, None, "Первая версия");
    assert!(
        overview::search(&conn, &project, "Вторая")
            .expect("search")
            .is_empty()
    );

    node::update(
        &mut conn,
        &project,
        &epic.dot_id,
        node::Patch {
            title: Some("Вторая версия"),
            ..node::Patch::default()
        },
    )
    .expect("rename");

    let hits = overview::search(&conn, &project, "Вторая").expect("search");
    assert_eq!(hits.len(), 1, "индекс обновился триггером: {hits:?}");
    assert!(
        overview::search(&conn, &project, "Первая")
            .expect("search")
            .is_empty(),
        "старый заголовок выпал из индекса"
    );
}

#[test]
fn context_chain_walks_from_the_root() {
    let (mut conn, project) = fixture("ctx");
    let epic = add(&mut conn, &project, None, "Эпик");
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Задача");
    let sub = add(&mut conn, &project, Some(&task.dot_id), "Подзадача");
    node::update(
        &mut conn,
        &project,
        &epic.dot_id,
        node::Patch {
            description: Some("зачем это всё"),
            ..node::Patch::default()
        },
    )
    .expect("description");

    let chain = overview::context(&conn, &project, &sub.dot_id).expect("context");
    let dots: Vec<&str> = chain.iter().map(|entry| entry.dot_id.as_str()).collect();
    assert_eq!(
        dots,
        vec![
            epic.dot_id.as_str(),
            task.dot_id.as_str(),
            sub.dot_id.as_str()
        ]
    );
    assert_eq!(chain[0].description.as_deref(), Some("зачем это всё"));
    assert_eq!(chain[2].kind, "subtask");
}

#[test]
fn board_groups_by_status_and_flags_overdue_and_leases() {
    let (mut conn, project) = fixture("board");
    let epic = add(&mut conn, &project, None, "Эпик");
    let overdue = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: Some(&epic.dot_id),
            title: "Просроченная",
            kind: None,
            description: None,
            priority: None,
            due_date: Some("2000-01-01"),
            estimated_hours: None,
            weight: None,
            source_ref: None,
            branch: None,
        },
    )
    .expect("overdue");
    let leased = add(&mut conn, &project, Some(&epic.dot_id), "В работе");
    let _lease = lease::claim(
        &mut conn,
        &project,
        &leased.dot_id,
        &lease::ClaimRequest::new("agent-a"),
    )
    .expect("claim");

    let board = overview::board(&conn, &project).expect("board");
    assert_eq!(board.overdue, 1, "просрочка посчитана");
    assert_eq!(board.leased, 1, "живая аренда видна на доске");
    let in_progress = board
        .columns
        .iter()
        .find(|column| column.status == "in_progress")
        .expect("in_progress column");
    assert_eq!(in_progress.rows[0].lease_holder.as_deref(), Some("agent-a"));
    let todo_column = board
        .columns
        .iter()
        .find(|column| column.status == "todo")
        .expect("todo column");
    let overdue_row = todo_column
        .rows
        .iter()
        .find(|row| row.dot_id == overdue.dot_id)
        .expect("overdue row in the todo column");
    assert!(overdue_row.days_overdue > 0);
}

#[test]
fn time_report_counts_attempt_durations() {
    let (mut conn, project) = fixture("time");
    let epic = add(&mut conn, &project, None, "Эпик");
    let task = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: Some(&epic.dot_id),
            title: "Задача",
            kind: None,
            description: None,
            priority: None,
            due_date: None,
            estimated_hours: Some(4.0),
            weight: None,
            source_ref: None,
            branch: None,
        },
    )
    .expect("task");

    // попытка длиной два часа: started_at в прошлом, ended_at — сейчас
    let token = lease::claim(
        &mut conn,
        &project,
        &task.dot_id,
        &lease::ClaimRequest::new("agent-a"),
    )
    .expect("claim")
    .token;
    conn.execute(
        "UPDATE attempts SET started_at = strftime('%Y-%m-%dT%H:%M:%fZ','now','-2 hours')
         WHERE token = ?1",
        rusqlite::params![token],
    )
    .expect("backdate");
    lease::release(
        &mut conn,
        &project,
        &token,
        lease::Outcome::Released,
        None,
        false,
    )
    .expect("release");

    let rows = overview::time_report(&conn, &project, None).expect("time");
    let row = rows
        .iter()
        .find(|row| row.dot_id == task.dot_id)
        .expect("task row");
    assert_eq!(row.estimated_hours, Some(4.0));
    assert!(
        (row.actual_hours - 2.0).abs() < 0.2,
        "факт ≈ 2 ч, получено {}",
        row.actual_hours
    );
    assert_eq!(row.attempts, 1);
}

#[test]
fn bulk_status_reports_each_failure() {
    let (mut conn, project) = fixture("bulk");
    let epic = add(&mut conn, &project, None, "Эпик");
    let first = add(&mut conn, &project, Some(&epic.dot_id), "Первая");
    let second = add(&mut conn, &project, Some(&epic.dot_id), "Вторая");

    let result = overview::bulk_status(
        &mut conn,
        &project,
        &[
            first.dot_id.clone(),
            second.dot_id.clone(),
            "TASQ-99".to_string(),
        ],
        Status::Blocked,
        false,
        "agent",
    )
    .expect("bulk");

    assert_eq!(result.updated.len(), 2);
    assert_eq!(result.failed.len(), 1);
    assert_eq!(result.failed[0].code, "NODE_NOT_FOUND");

    for node in [&first, &second] {
        let reread = node::resolve(&conn, &project, &node.dot_id).expect("node");
        assert_eq!(reread.status, "blocked");
    }
}

#[test]
fn schema_v2_is_applied_on_existing_database() {
    let dir = temp_dir("migrate");
    // init доводит базу до текущей версии
    let outcome = db::init(&dir, "TASQ", "demo").expect("init");
    assert_eq!(outcome.schema_version, db::SUPPORTED_SCHEMA_VERSION);
    let conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");

    let version: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .expect("version");
    assert_eq!(version, "2");

    // повторный init ничего не ломает
    let second = db::init(&dir, "TASQ", "demo").expect("re-init");
    assert_eq!(second.schema_version, 2);
    assert!(!second.project_created);
}
