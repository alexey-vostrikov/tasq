//! Инкрементальное обновление доски обязано совпадать с полным чтением.
//!
//! Смысл проверки. Экран после изменения читает не доску целиком, а только затронутые узлы и их
//! предков. Такой ответ может разойтись с полным чтением тремя способами, и все три молчаливы:
//! забытый предок (у него остаётся старый прогресс), забытый потомок переехавшего узла (строка с
//! прежним адресом), удалённый узел, оставшийся на доске. Поэтому каждое действие проверяется
//! сравнением двух ответов целиком — `Board` умеет сравниваться, и это сравнение и есть приёмка.
//!
//! Действия перечислены по одному, а не «сделаем побольше и сравним»: разошедшийся случай
//! обязан называться своим именем, иначе следующий разбор начинается с поиска, какое из десяти
//! действий сломало доску.

use rusqlite::Connection;

use tasq::overview::{self, BoardFilter, Refresh};
use tasq::project::{self, Project};
use tasq::status::Status;
use tasq::{db, lease, node};

struct Fixture {
    dir: std::path::PathBuf,
    conn: Connection,
    project: Project,
}

fn fixture(tag: &str) -> Fixture {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-delta-{tag}-{nanos}"));
    let outcome = db::init(&dir, "DELT", "инкремент").expect("init");
    let conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");
    Fixture { dir, conn, project }
}

impl Fixture {
    fn add(&mut self, parent: Option<&str>, title: &str) -> node::Node {
        node::create(
            &mut self.conn,
            &self.project,
            node::NewNode {
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
                tags: None,
            },
        )
        .expect("create")
    }

    /// Полное чтение доски — эталон, с которым сверяется инкремент.
    fn full(&self, filter: BoardFilter) -> overview::Board {
        overview::board_with(&self.conn, &self.project, filter).expect("board")
    }
}

/// Каждый переключатель доски проверяется отдельно: фильтр применяется и в SQL, и при сборке из
/// состояния, и расхождение между этими двумя отборами иначе осталось бы незамеченным.
fn filters() -> Vec<(&'static str, BoardFilter)> {
    vec![
        ("по умолчанию", BoardFilter::default()),
        (
            "скрыть выполненное",
            BoardFilter {
                hide_done: true,
                ..BoardFilter::default()
            },
        ),
        (
            "показать архив",
            BoardFilter {
                show_archived: true,
                ..BoardFilter::default()
            },
        ),
        (
            "только ветка",
            BoardFilter {
                branch: Some("feat/x".to_string()),
                ..BoardFilter::default()
            },
        ),
    ]
}

/// Готовит базу с деревом и двумя ветками, чтобы фильтрам было что отбирать.
fn seeded(tag: &str) -> (Fixture, String, String) {
    let mut fixture = fixture(tag);
    let epic = fixture.add(None, "Эпик");
    let task = fixture.add(Some(&epic.dot_id), "Задача");
    let sub = fixture.add(Some(&task.dot_id), "Подзадача");
    let other = fixture.add(None, "Второй корень");
    for (reference, branch) in [(&task.dot_id, "feat/x"), (&other.dot_id, "feat/y")] {
        node::update(
            &mut fixture.conn,
            &fixture.project,
            reference,
            node::Patch {
                branch: Some(branch),
                actor: Some("тест"),
                ..node::Patch::default()
            },
        )
        .expect("branch");
    }
    node::update(
        &mut fixture.conn,
        &fixture.project,
        &other.dot_id,
        node::Patch {
            due_date: Some("2020-01-01"),
            actor: Some("тест"),
            ..node::Patch::default()
        },
    )
    .expect("due");
    (fixture, sub.dot_id, epic.dot_id)
}

#[test]
fn a_closed_child_updates_its_parents_progress() {
    let (mut fixture, sub, _epic) = seeded("close");
    // Стартовое состояние читается ДО действия: иначе сравнивать было бы нечего.
    for (what, filter) in filters() {
        let mut state = overview::BoardState::full(&fixture.conn, &fixture.project, filter.clone())
            .expect("state");
        node::update(
            &mut fixture.conn,
            &fixture.project,
            &sub,
            node::Patch {
                status: Some(Status::Todo),
                actor: Some("тест"),
                ..node::Patch::default()
            },
        )
        .expect("todo");
        let refresh = state
            .refresh(&fixture.conn, &fixture.project, filter.clone())
            .expect("refresh");
        assert_eq!(
            refresh,
            Refresh::Delta { nodes: 3 },
            "должны перечитаться узел и два его предка ({what})"
        );
        assert_eq!(
            state.board(),
            &fixture.full(filter),
            "доска разошлась с полной после смены статуса ({what})"
        );
        // Возврат в исходное — чтобы следующий фильтр проверялся на том же состоянии.
        node::update(
            &mut fixture.conn,
            &fixture.project,
            &sub,
            node::Patch {
                status: Some(Status::Idea),
                actor: Some("тест"),
                ..node::Patch::default()
            },
        )
        .expect("idea");
    }
    let _ = std::fs::remove_dir_all(&fixture.dir);
}

#[test]
fn a_new_node_and_a_deleted_node_are_both_picked_up() {
    let mut fixture = fixture("add-del");
    let epic = fixture.add(None, "Эпик");
    let gone = fixture.add(Some(&epic.dot_id), "Будет удалён");
    let kept = fixture.add(Some(&epic.dot_id), "Останется");

    let filter = BoardFilter::default();
    let mut state =
        overview::BoardState::full(&fixture.conn, &fixture.project, filter.clone()).expect("state");

    let fresh = fixture.add(Some(&epic.dot_id), "Новый");
    let refresh = state
        .refresh(&fixture.conn, &fixture.project, filter.clone())
        .expect("refresh");
    assert!(matches!(refresh, Refresh::Delta { .. }), "{refresh:?}");
    assert_eq!(
        state.board(),
        &fixture.full(filter.clone()),
        "новый узел не появился на доске"
    );
    assert!(
        serde_json::to_string(state.board())
            .expect("json")
            .contains(&fresh.dot_id),
        "нового узла нет среди строк"
    );

    node::delete(&mut fixture.conn, &fixture.project, &gone.dot_id).expect("delete");
    state
        .refresh(&fixture.conn, &fixture.project, filter.clone())
        .expect("refresh");
    assert_eq!(
        state.board(),
        &fixture.full(filter.clone()),
        "удалённый узел остался на доске"
    );
    let text = serde_json::to_string(state.board()).expect("json");
    assert!(
        !text.contains(&gone.dot_id),
        "строка удалённого узла осталась"
    );
    assert!(
        text.contains(&kept.dot_id),
        "живой узел пропал вместе с удалённым"
    );
    let _ = std::fs::remove_dir_all(&fixture.dir);
}

#[test]
fn archiving_hides_the_node_only_when_the_filter_says_so() {
    let (mut fixture, sub, _epic) = seeded("archive");
    for (what, filter) in filters() {
        let mut state = overview::BoardState::full(&fixture.conn, &fixture.project, filter.clone())
            .expect("state");
        node::archive(&mut fixture.conn, &fixture.project, &sub, false).expect("archive");
        state
            .refresh(&fixture.conn, &fixture.project, filter.clone())
            .expect("refresh");
        assert_eq!(
            state.board(),
            &fixture.full(filter),
            "доска разошлась после архивации ({what})"
        );
        node::archive(&mut fixture.conn, &fixture.project, &sub, true).expect("unarchive");
    }
    let _ = std::fs::remove_dir_all(&fixture.dir);
}

#[test]
fn a_lease_shows_up_immediately() {
    let (mut fixture, sub, _epic) = seeded("lease");
    let filter = BoardFilter::default();
    let mut state =
        overview::BoardState::full(&fixture.conn, &fixture.project, filter.clone()).expect("state");
    assert_eq!(state.board().leased, 0, "аренда взялась до взятия");

    let held = lease::claim(
        &mut fixture.conn,
        &fixture.project,
        &sub,
        &lease::ClaimRequest {
            holder: "агент",
            ttl_seconds: 600,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
    )
    .expect("claim");
    state
        .refresh(&fixture.conn, &fixture.project, filter.clone())
        .expect("refresh");
    assert_eq!(state.board().leased, 1, "аренда не появилась на доске");
    assert_eq!(
        state.board(),
        &fixture.full(filter.clone()),
        "доска разошлась после взятия в работу"
    );

    lease::release(
        &mut fixture.conn,
        &fixture.project,
        &held.token,
        lease::Outcome::Released,
        None,
        false,
    )
    .expect("release");
    state
        .refresh(&fixture.conn, &fixture.project, filter.clone())
        .expect("refresh");
    assert_eq!(state.board().leased, 0, "аренда осталась после сдачи");
    assert_eq!(
        state.board(),
        &fixture.full(filter),
        "доска разошлась после сдачи"
    );
    let _ = std::fs::remove_dir_all(&fixture.dir);
}

#[test]
fn a_move_updates_the_addresses_under_the_node() {
    let (mut fixture, sub, epic) = seeded("move");
    let filter = BoardFilter::default();
    let mut state =
        overview::BoardState::full(&fixture.conn, &fixture.project, filter.clone()).expect("state");

    // Эпик встаёт под второй корень: адрес меняется у эпика, задачи и подзадачи сразу.
    let other = fixture
        .conn
        .query_row(
            "SELECT path FROM nodes WHERE project_id = ?1 AND depth = 1 AND path <> ?2
             ORDER BY path LIMIT 1",
            rusqlite::params![
                fixture.project.id,
                epic_path(&fixture.conn, &fixture.project, &epic)
            ],
            |row| row.get::<_, String>(0),
        )
        .expect("другой корень");
    let other_dot = fixture.project.dot_id(&other);
    node::move_node(
        &mut fixture.conn,
        &fixture.project,
        &epic,
        Some(&other_dot),
        false,
    )
    .expect("move");

    state
        .refresh(&fixture.conn, &fixture.project, filter.clone())
        .expect("refresh");
    assert_eq!(
        state.board(),
        &fixture.full(filter.clone()),
        "доска разошлась после переезда поддерева"
    );
    let text = serde_json::to_string(state.board()).expect("json");
    assert!(
        !text.contains(&format!("\"{sub}\"")),
        "на доске остался прежний адрес подзадачи: {sub}"
    );
    let _ = std::fs::remove_dir_all(&fixture.dir);
}

fn epic_path(conn: &Connection, project: &Project, dot_id: &str) -> String {
    conn.query_row(
        "SELECT path FROM nodes WHERE project_id = ?1 AND ?2 = ?3 || '-' || path",
        rusqlite::params![project.id, dot_id, project.key],
        |row| row.get(0),
    )
    .expect("path")
}

#[test]
fn a_foreign_project_change_does_not_touch_the_board() {
    let (mut fixture, _sub, _epic) = seeded("foreign");
    let filter = BoardFilter::default();
    let mut state =
        overview::BoardState::full(&fixture.conn, &fixture.project, filter.clone()).expect("state");
    // Второй проект в той же базе: правка в нём не должна ни менять доску, ни гонять чтение.
    let other = project::create(&mut fixture.conn, "OTHER", Some("соседний")).expect("project");
    node::create(
        &mut fixture.conn,
        &other,
        node::NewNode {
            parent: None,
            title: "Чужой узел",
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
    .expect("create");
    let refresh = state
        .refresh(&fixture.conn, &fixture.project, filter.clone())
        .expect("refresh");
    assert_eq!(
        refresh,
        Refresh::Nothing,
        "правка соседа попала в свой проект"
    );
    assert_eq!(
        state.board(),
        &fixture.full(filter),
        "доска изменилась от чужой правки"
    );
    // Точка отсчёта сдвинулась: повторное обновление тоже ничего не находит.
    let again = state
        .refresh(&fixture.conn, &fixture.project, BoardFilter::default())
        .expect("refresh");
    assert_eq!(again, Refresh::Nothing, "точка отсчёта не сдвинулась");
    let _ = std::fs::remove_dir_all(&fixture.dir);
}

#[test]
fn a_changed_filter_rereads_everything() {
    let (fixture, _sub, _epic) = seeded("filter");
    let mut state =
        overview::BoardState::full(&fixture.conn, &fixture.project, BoardFilter::default())
            .expect("state");
    let refresh = state
        .refresh(
            &fixture.conn,
            &fixture.project,
            BoardFilter {
                hide_done: true,
                ..BoardFilter::default()
            },
        )
        .expect("refresh");
    assert_eq!(
        refresh,
        Refresh::Full,
        "смена фильтра обязана перечитать доску"
    );
    assert_eq!(
        state.board(),
        &fixture.full(BoardFilter {
            hide_done: true,
            ..BoardFilter::default()
        }),
        "доска после смены фильтра не та"
    );
    let _ = std::fs::remove_dir_all(&fixture.dir);
}
