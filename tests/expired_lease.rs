//! Истёкшая аренда освобождает узел. Проверяется на всех четырёх сторонах, где это видно:
//! чтение (`lease_holder` и `effective_status`), выбор работы (`next`), хранилище (попытка
//! закрыта, резервы сняты, статус вернулся) и захват после истечения.

use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;

use tasq::db;
use tasq::edge;
use tasq::lease::{self, ClaimRequest};
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};
use tasq::resource::Resource;
use tasq::status::{Kind, Status};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-exp-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
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
    .expect("create node")
}

fn fixture(tag: &str) -> (Connection, Project, node::Node, node::Node) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "EXP", "expired").expect("init");
    let mut conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");
    let epic = add(&mut conn, &project, None, "Эпик");
    let task = add(&mut conn, &project, Some(&epic.dot_id), "Задача");
    (conn, project, epic, task)
}

fn claim(conn: &mut Connection, project: &Project, reference: &str, ttl: i64) -> String {
    lease::claim(
        conn,
        project,
        reference,
        &ClaimRequest {
            holder: "agent",
            ttl_seconds: ttl,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
    )
    .expect("claim")
    .token
}

/// Время в базе двигается руками: ждать TTL в тесте нельзя, а «через час» — единственный способ
/// проверить поведение после истечения.
fn expire_now(conn: &Connection, dot_id: &str) {
    let path = dot_id.split_once('-').expect("dot id").1;
    let changed = conn
        .execute(
            "UPDATE attempts SET expires_at = '2000-01-01T00:00:00.000Z'
             WHERE node_id = (SELECT id FROM nodes WHERE path = ?1) AND ended_at IS NULL",
            rusqlite::params![path],
        )
        .expect("expire");
    assert_eq!(changed, 1, "просрочить нечего: {dot_id}");
}

/// Предрабочий статус: узел должен быть в очереди, иначе `claim` справедливо откажет.
fn set_status(conn: &Connection, node: &node::Node, status: &str) {
    conn.execute(
        "UPDATE nodes SET status = ?1 WHERE id = ?2",
        rusqlite::params![status, node.id],
    )
    .expect("set status");
}

fn node_by_ref(conn: &Connection, project: &Project, reference: &str) -> node::Node {
    node::resolve(conn, project, reference).expect("resolve")
}

/// Главное: истёкшая аренда не видна читателю, а узел возвращается в очередь.
///
/// Раньше `lease_holder` брал любую открытую попытку и узел выглядел занятым, а
/// `effective_status` оставался `in_progress` — то есть узел был недоступен и для `next`,
/// и для человека, хотя аренда кончилась.
#[test]
fn an_expired_lease_stops_holding_the_node() {
    let (mut conn, project, _epic, task) = fixture("read");
    claim(&mut conn, &project, &task.dot_id, 60);
    assert_eq!(
        node_by_ref(&conn, &project, &task.dot_id)
            .lease_holder
            .as_deref(),
        Some("agent"),
        "живая аренда должна быть видна"
    );

    expire_now(&conn, &task.dot_id);

    let after = node_by_ref(&conn, &project, &task.dot_id);
    assert_eq!(
        after.lease_holder, None,
        "истёкшая аренда выдаётся за живую"
    );
    assert_eq!(after.lease_expires_at, None);
    assert_eq!(after.status, "in_progress", "хранимый статус ещё не тронут");
    assert_eq!(
        after.effective_status, "todo",
        "работа не идёт — узел в очереди, а не в работе"
    );
}

/// У эпика нет `todo`: после истечения он возвращается в `planned`. Правило вида узла то же,
/// что в `status::allowed_for`, и тест держит их вместе.
#[test]
fn an_expired_epic_returns_to_planned_not_todo() {
    let (mut conn, project, epic, _task) = fixture("epic");
    claim(&mut conn, &project, &epic.dot_id, 60);
    expire_now(&conn, &epic.dot_id);

    let after = node_by_ref(&conn, &project, &epic.dot_id);
    assert_eq!(after.effective_status, "planned", "эпик вернулся в todo");
    let parsed = Status::parse(&after.effective_status).expect("known status");
    assert!(
        parsed.allowed_for(Kind::Epic),
        "effective_status эпика недопустим для его вида"
    );
}

/// `next` предлагает узел с истёкшей арендой: именно отсюда задача и была замечена — узел
/// исчезал из «что делать дальше» навсегда, пока не запустят `doctor --gc`.
#[test]
fn next_offers_a_node_whose_lease_expired() {
    let (mut conn, project, _epic, task) = fixture("next");
    set_status(&conn, &task, "todo");
    claim(&mut conn, &project, &task.dot_id, 60);
    assert!(
        edge::next_candidates(&conn, &project, 5)
            .expect("next")
            .is_empty(),
        "пока аренда жива, узел предлагать нельзя"
    );

    expire_now(&conn, &task.dot_id);

    let candidates = edge::next_candidates(&conn, &project, 5).expect("next");
    assert!(
        candidates.iter().any(|c| c.dot_id == task.dot_id),
        "узел с истёкшей арендой не предложен: {candidates:?}"
    );
}

/// Освобождение на пути записи: захват любой другой задачи прибирает аренды мёртвых сессий
/// этого проекта — попытка закрыта, резервы сняты, статус возвращён.
#[test]
fn a_write_path_releases_expired_attempts_automatically() {
    let (mut conn, project, epic, task) = fixture("sweep");
    let other = add(&mut conn, &project, Some(&epic.dot_id), "Другая");

    let token = lease::claim(
        &mut conn,
        &project,
        &task.dot_id,
        &ClaimRequest {
            holder: "dead",
            ttl_seconds: 60,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: vec![Resource::path("src/dead.rs")],
        },
    )
    .expect("claim");
    assert!(!token.token.is_empty());
    expire_now(&conn, &task.dot_id);

    // Любая запись в проект: захват второй задачи.
    claim(&mut conn, &project, &other.dot_id, 600);

    let (ended, reason): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT ended_at, end_reason FROM attempts a JOIN nodes n ON n.id = a.node_id
             WHERE n.path = ?1 AND a.end_reason IS NOT NULL",
            rusqlite::params![task.dot_id.split_once('-').expect("dot").1],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("attempt row");
    assert!(ended.is_some(), "попытка мёртвой сессии не закрыта");
    assert_eq!(reason.as_deref(), Some("expired"));

    let open_reservations: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM reservations WHERE released_at IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(
        open_reservations, 0,
        "резервы мёртвой сессии продолжают держать файлы"
    );

    let stored: String = conn
        .query_row(
            "SELECT status FROM nodes WHERE path = ?1",
            rusqlite::params![task.dot_id.split_once('-').expect("dot").1],
            |row| row.get(0),
        )
        .expect("status");
    assert_eq!(stored, "todo", "хранимый статус остался in_progress");
}

/// Сборка возвращает эпик в `planned`, а не в `todo`: у эпика этого статуса не бывает, и
/// хранилище не должно противоречить правилу вида узла.
#[test]
fn the_sweep_returns_an_epic_to_planned() {
    let (mut conn, project, epic, _task) = fixture("epic-sweep");
    claim(&mut conn, &project, &epic.dot_id, 60);
    expire_now(&conn, &epic.dot_id);

    let closed = lease::release_expired(&mut conn, &project, None).expect("sweep");
    assert_eq!(closed, 1, "аренда эпика не собрана");

    let stored: String = conn
        .query_row("SELECT status FROM nodes WHERE id = ?1", [epic.id], |row| {
            row.get(0)
        })
        .expect("status");
    assert_eq!(stored, "planned", "эпик в хранилище остался в {stored}");
}

/// Сборка не трогает то, что закрыли руками: узел, ставший `done` при живой аренде, не
/// возвращается в очередь задним числом.
#[test]
fn a_finished_node_is_not_resurrected() {
    let (mut conn, project, _epic, task) = fixture("done");
    claim(&mut conn, &project, &task.dot_id, 60);
    // Закрыли руками, не освобождая аренду (возможно через force).
    conn.execute(
        "UPDATE nodes SET status = 'done' WHERE path = ?1",
        rusqlite::params![task.dot_id.split_once('-').expect("dot").1],
    )
    .expect("mark done");
    expire_now(&conn, &task.dot_id);

    let reopened = lease::release_expired(&mut conn, &project, None).expect("sweep");

    let stored: String = conn
        .query_row(
            "SELECT status FROM nodes WHERE path = ?1",
            rusqlite::params![task.dot_id.split_once('-').expect("dot").1],
            |row| row.get(0),
        )
        .expect("status");
    assert_eq!(
        stored, "done",
        "закрытый узел вернули в очередь (сборка {reopened})"
    );
}

/// После истечения узел можно взять снова — и это новая попытка с новым номером, а не
/// продолжение мёртвой.
#[test]
fn a_node_can_be_claimed_again_after_expiry() {
    let (mut conn, project, _epic, task) = fixture("reclaim");
    set_status(&conn, &task, "todo");
    claim(&mut conn, &project, &task.dot_id, 60);
    expire_now(&conn, &task.dot_id);

    let again = lease::claim(
        &mut conn,
        &project,
        &task.dot_id,
        &ClaimRequest {
            holder: "next-agent",
            ttl_seconds: 600,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
    )
    .expect("reclaim");
    assert_eq!(again.attempt_no, 2, "вторая попытка, а не та же самая");
    assert_eq!(again.holder, "next-agent");
    assert_eq!(
        node_by_ref(&conn, &project, &task.dot_id).effective_status,
        "in_progress"
    );
}

/// Узел, по которому вызывающий разбирается сам (`claim`), сборкой не трогается: иначе
/// информация «вытеснил истёкшую аренду» пропала бы из ответа.
#[test]
fn the_target_node_is_left_to_the_caller() {
    let (mut conn, project, _epic, task) = fixture("exclude");
    set_status(&conn, &task, "todo");
    claim(&mut conn, &project, &task.dot_id, 60);
    expire_now(&conn, &task.dot_id);

    let target = node_by_ref(&conn, &project, &task.dot_id);
    let closed = lease::release_expired(&mut conn, &project, Some(target.id)).expect("sweep");
    assert_eq!(closed, 0, "сборка тронула целевой узел");

    let still_open: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM attempts WHERE ended_at IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(still_open, 1, "целевая попытка закрыта сборкой");
}
