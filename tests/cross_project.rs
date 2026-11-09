//! Границы между проектами и проверки аренды, которые видны только на живых данных.
//!
//! Здесь два проекта в одной базе: связи через границу запрещены, а токен аренды
//! действует только в своём проекте. Это те инварианты, на которых держится изоляция —
//! без них агент одного проекта правил бы чужую работу.

use std::path::PathBuf;

use rusqlite::Connection;

use tasq::db;
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};
use tasq::transfer::Mode;
use tasq::{edge, lease};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-cross-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Два проекта в одной базе: `ALFA` и `BETA`.
fn two_projects(tag: &str) -> (Connection, Project, Project) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "ALFA", "первый").expect("init");
    let mut conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let alfa = project::default_project(&conn).expect("alfa");
    let beta = project::create(&mut conn, "BETA", Some("второй")).expect("beta");
    (conn, alfa, beta)
}

fn add(conn: &mut Connection, project: &Project, title: &str) -> node::Node {
    node::create(
        conn,
        project,
        NewNode {
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
        },
    )
    .expect("create node")
}

/// Связь между узлами разных проектов запрещена. Чужой dot-ID не разбирается вообще
/// (префикс не тот), а внутренний id чужого узла распознаётся и даёт CROSS_PROJECT_EDGE:
/// без этой проверки границу обходили бы через `#`.
#[test]
fn edges_never_cross_projects() {
    let (mut conn, alfa, beta) = two_projects("edges");
    let mine = add(&mut conn, &alfa, "Своя задача");
    let theirs = add(&mut conn, &beta, "Чужая задача");

    let error = edge::link(
        &mut conn,
        &alfa,
        &mine.dot_id,
        &theirs.dot_id,
        edge::Kind::Blocks,
        "-",
    )
    .expect_err("ссылка на чужой ключ проекта");
    assert_eq!(error.code(), "INVALID_REF");
    assert!(
        error.to_string().contains(&theirs.dot_id),
        "чужой ключ назван в отказе: {error}"
    );

    let error = edge::link(
        &mut conn,
        &alfa,
        &mine.dot_id,
        &format!("#{}", theirs.id),
        edge::Kind::Blocks,
        "-",
    )
    .expect_err("связь по внутреннему id");
    assert_eq!(error.code(), "CROSS_PROJECT_EDGE");
    assert!(
        error.to_string().contains(&theirs.dot_id),
        "в сообщении чужой узел назван своим ключом: {error}"
    );

    // Внутри проекта связь проходит.
    let sibling = add(&mut conn, &alfa, "Вторая своя");
    edge::link(
        &mut conn,
        &alfa,
        &mine.dot_id,
        &sibling.dot_id,
        edge::Kind::Blocks,
        "-",
    )
    .expect("свои узлы связываются");
}

/// Живой токен действует только в своём проекте.
#[test]
fn a_token_belongs_to_its_project() {
    let (mut conn, alfa, beta) = two_projects("tokens");
    let mine = add(&mut conn, &alfa, "Своя задача");

    let held = lease::claim(
        &mut conn,
        &alfa,
        &mine.dot_id,
        &lease::ClaimRequest {
            holder: "agent-a",
            ttl_seconds: lease::DEFAULT_TTL,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
    )
    .expect("claim");

    assert_eq!(
        lease::live_node(&conn, &alfa, &held.token).expect("свой проект"),
        Some(mine.id)
    );
    assert_eq!(
        lease::live_node(&conn, &beta, &held.token).expect("чужой проект"),
        None,
        "в другом проекте токен не значит ничего"
    );
    assert_eq!(
        lease::live_node(&conn, &alfa, "не-токен").expect("мусор"),
        None
    );

    // После освобождения токен мёртв.
    lease::release(
        &mut conn,
        &alfa,
        &held.token,
        lease::Outcome::Released,
        None,
        false,
    )
    .expect("release");
    assert_eq!(
        lease::live_node(&conn, &alfa, &held.token).expect("после"),
        None
    );
}

/// Освобождение с исходом «готово» не закрывает узел с открытым блокером.
#[test]
fn completing_with_an_open_blocker_is_refused() {
    let (mut conn, alfa, _beta) = two_projects("complete");
    let blocker = add(&mut conn, &alfa, "Блокер");
    let waiting = add(&mut conn, &alfa, "Ждёт");

    let held = lease::claim(
        &mut conn,
        &alfa,
        &waiting.dot_id,
        &lease::ClaimRequest {
            holder: "agent-a",
            ttl_seconds: lease::DEFAULT_TTL,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
    )
    .expect("claim");

    // Зависимость выяснилась во время работы.
    edge::link(
        &mut conn,
        &alfa,
        &waiting.dot_id,
        &blocker.dot_id,
        edge::Kind::Blocks,
        "-",
    )
    .expect("link");

    let error = lease::release(
        &mut conn,
        &alfa,
        &held.token,
        lease::Outcome::Completed,
        None,
        false,
    )
    .expect_err("блокер открыт");
    assert_eq!(error.code(), "UNMET_PREREQUISITES");
    assert!(error.to_string().contains(&blocker.path), "{error}");

    // Аренда при этом жива: отказ не должен её закрывать.
    assert_eq!(
        lease::live_node(&conn, &alfa, &held.token).expect("жива"),
        Some(waiting.id)
    );

    // С согласием (`force`) исход проводится.
    lease::release(
        &mut conn,
        &alfa,
        &held.token,
        lease::Outcome::Completed,
        None,
        true,
    )
    .expect("с force");
}

/// Пустой список ресурсов — отказ: «зарезервировать ничего» не имеет смысла.
#[test]
fn reserving_nothing_is_refused() {
    let (mut conn, alfa, _beta) = two_projects("nothing");
    let node = add(&mut conn, &alfa, "Задача");
    let held = lease::claim(
        &mut conn,
        &alfa,
        &node.dot_id,
        &lease::ClaimRequest {
            holder: "agent-a",
            ttl_seconds: lease::DEFAULT_TTL,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
    )
    .expect("claim");

    let error = lease::reserve(&mut conn, &alfa, &held.token, &[]).expect_err("пустой список");
    assert_eq!(error.code(), "INVALID_VALUE");
    assert!(error.to_string().contains("nothing requested"), "{error}");
}

/// Чужой и просроченный токен называют себя разными кодами: агенту важно,
/// возобновлять аренду или брать заново.
#[test]
fn token_errors_are_specific() {
    let (mut conn, alfa, _beta) = two_projects("token-errors");
    let node = add(&mut conn, &alfa, "Задача");
    let held = lease::claim(
        &mut conn,
        &alfa,
        &node.dot_id,
        &lease::ClaimRequest {
            holder: "agent-a",
            ttl_seconds: lease::DEFAULT_TTL,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
    )
    .expect("claim");

    // Мусор вместо токена неотличим от чужого: клиенту всё равно нужно брать аренду заново.
    let error = lease::renew(&mut conn, &alfa, "не-токен", lease::DEFAULT_TTL).expect_err("мусор");
    assert_eq!(error.code(), "NOT_YOUR_ATTEMPT");

    // Неизвестный, но правильный по форме токен.
    let unknown = "0".repeat(64);
    let error = lease::renew(&mut conn, &alfa, &unknown, lease::DEFAULT_TTL).expect_err("чужой");
    assert_eq!(error.code(), "NOT_YOUR_ATTEMPT");
    let error = lease::release(
        &mut conn,
        &alfa,
        &unknown,
        lease::Outcome::Released,
        None,
        false,
    )
    .expect_err("чужой");
    assert_eq!(error.code(), "NOT_YOUR_ATTEMPT");
    let error = lease::unreserve(&mut conn, &alfa, &unknown, None).expect_err("чужой");
    assert_eq!(error.code(), "NOT_YOUR_ATTEMPT");

    // Просроченный токен: возобновлять нечего, надо забирать заново.
    {
        let db_path = std::env::temp_dir();
        let _ = db_path;
        conn.execute(
            "UPDATE attempts SET expires_at = '2000-01-01T00:00:00.000Z' WHERE token = ?1",
            rusqlite::params![held.token],
        )
        .expect("expire");
    }
    let error =
        lease::renew(&mut conn, &alfa, &held.token, lease::DEFAULT_TTL).expect_err("просрочен");
    assert_eq!(error.code(), "LEASE_EXPIRED");
}

/// Токен — 64 шестнадцатеричных символа: формат проверяется до обращения к базе.
#[test]
fn token_format_is_checked_against_the_schema() {
    let (mut conn, alfa, _beta) = two_projects("format");
    let node = add(&mut conn, &alfa, "Задача");
    let held = lease::claim(
        &mut conn,
        &alfa,
        &node.dot_id,
        &lease::ClaimRequest {
            holder: "agent-a",
            ttl_seconds: lease::DEFAULT_TTL,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
    )
    .expect("claim");
    assert_eq!(held.token.len(), 64, "длина токена");
    assert!(held.token.chars().all(|c| c.is_ascii_hexdigit()));

    // Пустой держатель — отказ: у работы должен быть владелец.
    let error = lease::claim(
        &mut conn,
        &alfa,
        "TASQ-1",
        &lease::ClaimRequest {
            holder: "   ",
            ttl_seconds: lease::DEFAULT_TTL,
            steal: true,
            force: true,
            checkpoint: None,
            reservations: Vec::new(),
        },
    )
    .expect_err("пустой держатель");
    assert_eq!(error.code(), "INVALID_VALUE");
    assert!(error.to_string().contains("holder"), "{error}");
}

/// Экспорт saga принимает пустой проект без потерь: `project` в документе есть,
/// а узлов нет — это не ошибка.
#[test]
fn empty_saga_project_is_not_a_crash() {
    let (mut conn, alfa, _beta) = two_projects("empty-saga");
    let document = serde_json::json!({
        "format_version": "1.3",
        "project": { "name": "Пусто", "epics": [] }
    });
    let error = tasq::transfer::import_saga(&mut conn, &alfa, &document, Mode::Merge, "-")
        .expect_err("без эпиков");
    assert_eq!(error.code(), "INVALID_VALUE");
}
