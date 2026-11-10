//! `next` с привязкой к сессии: сначала свой проект, чужие — после исчерпания.
//!
//! Зачем. Три параллельные сессии, каждая в своём проекте, — рабочий сценарий. Без привязки
//! `next` либо требовал выбрать проект, либо предлагал задачи из всех сразу, и сессия прыгала
//! между проектами на каждом вызове. Правило, которое здесь закреплено: пока в своём проекте
//! есть что брать — берётся своё; когда там пусто — предлагаются остальные, и об этом сказано.
//!
//! Что считается «пусто»: `next` возвращает пустой список для проекта (все узлы закрыты,
//! заблокированы или под живой арендой). Проект при этом не «закончен» — в нём могут остаться
//! идеи и отложенное, которые `next` не предлагает по своему правилу. Это осознанно: ждать,
//! пока проект закроется целиком, значит не предлагать ничего неделями.

use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;

use tasq::db;
use tasq::edge::{self, Origin};
use tasq::lease::{self, ClaimRequest};
use tasq::node::{self, NewNode, Patch};
use tasq::project::{self, Project};
use tasq::session;
use tasq::status::Status;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-aff-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn fixture(tag: &str) -> (Connection, Project, Project) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "AAA", "первый").expect("init");
    let mut conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let first = project::first_project(&conn).expect("project");
    let second = project::create(&mut conn, "BBB", Some("второй")).expect("second");
    (conn, first, second)
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

/// Готовая к работе задача в проекте.
///
/// Корень проекта — эпик, а у эпика нет `todo` (правило вида узла), поэтому задача создаётся
/// под ним. `next` предлагает только готовое, так что без этого шага кандидатов нет вовсе.
fn add_ready(conn: &mut Connection, project: &Project, title: &str) -> node::Node {
    let epic = add(conn, project, None, &format!("Эпик {title}"));
    let task = add(conn, project, Some(&epic.dot_id), title);
    make_ready(conn, project, &task);
    task
}

/// Переводит узел в `todo`, чтобы он стал кандидатом: `next` предлагает только готовое к работе.
fn make_ready(conn: &mut Connection, project: &Project, node: &node::Node) {
    node::update(
        conn,
        project,
        &node.dot_id,
        Patch {
            status: Some(Status::Todo),
            actor: Some("человек"),
            ..Patch::default()
        },
    )
    .expect("todo");
}

fn keys(candidates: &[edge::Candidate]) -> Vec<String> {
    candidates
        .iter()
        .map(|candidate| candidate.dot_id.clone())
        .collect()
}

/// Привязка держит сессию в своём проекте, даже когда в другом есть кандидаты с большим score.
#[test]
fn the_bound_project_wins_even_if_another_scores_higher() {
    let (mut conn, first, second) = fixture("bound");
    let own = add_ready(&mut conn, &first, "Своя");
    // В чужом проекте задача с критическим приоритетом — она бы выиграла без привязки.
    // В чужом проекте задача критического приоритета — без привязки она бы выиграла.
    let foreign_epic = add(&mut conn, &second, None, "Эпик чужой");
    let foreign = add(&mut conn, &second, Some(&foreign_epic.dot_id), "Чужая");
    node::update(
        &mut conn,
        &second,
        &foreign.dot_id,
        Patch {
            status: Some(Status::Todo),
            priority: Some("critical"),
            actor: Some("человек"),
            ..Patch::default()
        },
    )
    .expect("critical todo");

    let next = edge::next_across(&conn, Some("AAA"), 5).expect("next");
    assert_eq!(
        keys(&next.candidates),
        vec![own.dot_id.clone()],
        "{:?}",
        next.candidates
    );
    assert_eq!(next.origin, vec![("AAA".to_string(), Origin::Affinity)]);
}

/// Когда в своём проекте брать нечего — предлагаются чужие, с пометкой `Fallback`.
#[test]
fn an_exhausted_project_falls_back_to_others() {
    let (mut conn, first, second) = fixture("fallback");
    // В своём проекте есть узел, но он не кандидат: остался в `idea`.
    let _idle = add(&mut conn, &first, None, "Идея");
    let ready = add_ready(&mut conn, &second, "Готовая");

    let next = edge::next_across(&conn, Some("AAA"), 5).expect("next");
    assert_eq!(
        keys(&next.candidates),
        vec![ready.dot_id.clone()],
        "{:?}",
        next.candidates
    );
    assert_eq!(next.origin, vec![("BBB".to_string(), Origin::Fallback)]);
}

/// Без привязки проекты перечисляются как есть, без пометок: «своего» не существует.
#[test]
fn without_affinity_every_project_is_listed() {
    let (mut conn, first, second) = fixture("unbound");
    let a = add_ready(&mut conn, &first, "Из AAA");
    let b = add_ready(&mut conn, &second, "Из BBB");

    let next = edge::next_across(&conn, None, 5).expect("next");
    let found = keys(&next.candidates);
    assert!(found.contains(&a.dot_id), "{found:?}");
    assert!(found.contains(&b.dot_id), "{found:?}");
    assert!(
        next.origin
            .iter()
            .all(|(_, place)| *place == Origin::Unbound),
        "без привязки не должно быть пометок о «своём»: {:?}",
        next.origin
    );
}

/// Привязка берётся из сессии, а привязывается она взятием работы, а не просмотром.
#[test]
fn claiming_binds_the_session_and_that_drives_next() {
    let (mut conn, first, second) = fixture("claim");
    let own = add_ready(&mut conn, &first, "Своя");
    let foreign = add_ready(&mut conn, &second, "Чужая");

    let id = session::new_id().expect("id");
    session::ensure(&conn, &id, "agent").expect("ensure");
    assert_eq!(
        session::affinity(&conn, &id).expect("affinity"),
        None,
        "новая сессия не привязана, пока ничего не взяла"
    );

    let held = lease::claim_as(
        &mut conn,
        &first,
        &own.dot_id,
        &ClaimRequest {
            holder: "agent",
            ttl_seconds: 600,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
        Some(&id),
    )
    .expect("claim");

    assert_eq!(
        session::affinity(&conn, &id).expect("affinity").as_deref(),
        Some("AAA"),
        "взятие работы должно привязать сессию к проекту"
    );

    // Аренда помнит сессию: иначе нельзя ответить «что взяла эта сессия».
    let stored: Option<String> = conn
        .query_row(
            "SELECT session_id FROM attempts WHERE id = ?1",
            [held.attempt_id],
            |row| row.get(0),
        )
        .expect("attempt");
    assert_eq!(stored.as_deref(), Some(id.as_str()));

    // Своя задача занята этой же сессией, значит в своём проекте брать нечего — предлагается чужая.
    let next = edge::next_across(&conn, Some("AAA"), 5).expect("next");
    assert_eq!(
        keys(&next.candidates),
        vec![foreign.dot_id.clone()],
        "занятая своей сессией задача не должна предлагаться снова: {:?}",
        next.candidates
    );
    assert_eq!(next.origin, vec![("BBB".to_string(), Origin::Fallback)]);
}

/// Две параллельные сессии не тянут друг друга: у каждой своя привязка.
#[test]
fn two_sessions_keep_their_own_projects() {
    let (mut conn, first, second) = fixture("parallel");
    let a = add_ready(&mut conn, &first, "Задача AAA");
    let b = add_ready(&mut conn, &second, "Задача BBB");

    let s1 = session::new_id().expect("id1");
    let s2 = session::new_id().expect("id2");
    assert_ne!(s1, s2, "идентификаторы сессий должны различаться");

    session::ensure(&conn, &s1, "hermes").expect("ensure1");
    session::ensure(&conn, &s2, "hermes").expect("ensure2");

    for (id, project, node_ref) in [(&s1, &first, &a.dot_id), (&s2, &second, &b.dot_id)] {
        lease::claim_as(
            &mut conn,
            project,
            node_ref,
            &ClaimRequest {
                holder: "hermes",
                ttl_seconds: 600,
                steal: false,
                force: false,
                checkpoint: None,
                reservations: Vec::new(),
            },
            Some(id),
        )
        .expect("claim");
    }

    assert_eq!(
        session::affinity(&conn, &s1).expect("a1").as_deref(),
        Some("AAA")
    );
    assert_eq!(
        session::affinity(&conn, &s2).expect("a2").as_deref(),
        Some("BBB")
    );

    // Обе задачи заняты — по одной каждой сессией, поэтому предлагать нечего ни той, ни другой.
    // Это и есть проверка, что чужая занятая задача не считается свободной: `next` отбирает по
    // отсутствию живой аренды, а не по «моё/не моё».
    let c1 = edge::next_across(&conn, Some("AAA"), 5).expect("next1");
    assert!(
        c1.candidates.is_empty(),
        "занятые задачи предложены снова: {c1:?}"
    );
    let c2 = edge::next_across(&conn, Some("BBB"), 5).expect("next2");
    assert!(
        c2.candidates.is_empty(),
        "занятые задачи предложены снова: {c2:?}"
    );

    // Привязки при этом остаются разными и не перетекают друг в друга.
    assert_eq!(
        session::affinity(&conn, &s1).expect("a1").as_deref(),
        Some("AAA")
    );
    assert_eq!(
        session::affinity(&conn, &s2).expect("a2").as_deref(),
        Some("BBB")
    );
}

/// При работе по нескольким проектам кандидат несёт свой проект и причину выбора, а при явном
/// проекте полей нет: там проект известен из запроса.
#[test]
fn candidates_carry_their_project_only_when_several_are_scanned() {
    let (mut conn, first, second) = fixture("marks");
    let a = add_ready(&mut conn, &first, "Из AAA");
    let b = add_ready(&mut conn, &second, "Из BBB");

    let next = edge::next_across(&conn, None, 5).expect("next");
    for candidate in &next.candidates {
        assert!(
            candidate.project.is_some(),
            "проект не проставлен: {candidate:?}"
        );
        assert_eq!(candidate.origin, Some(Origin::Unbound));
    }

    let single = edge::next_candidates(&conn, &first, 5).expect("one project");
    assert_eq!(
        keys(&single),
        vec![a.dot_id.clone()],
        "явный проект — только его задачи"
    );
    assert!(
        single.iter().all(|candidate| candidate.project.is_none()),
        "при явном проекте поле проекта лишнее: {single:?}"
    );
    // Второй проект существует и в общем перечислении виден.
    let all = keys(&next.candidates);
    assert!(
        all.contains(&a.dot_id) && all.contains(&b.dot_id),
        "{all:?}"
    );
}

/// `next` без сессии не падает и не требует проекта: привязки нет, проекты перечислены.
#[test]
fn next_works_without_a_session_at_all() {
    let (mut conn, first, _second) = fixture("nosession");
    let a = add_ready(&mut conn, &first, "Задача");

    let next = edge::next_across(&conn, None, 5).expect("next");
    assert_eq!(keys(&next.candidates), vec![a.dot_id.clone()]);
    assert!(
        next.origin
            .iter()
            .all(|(_, place)| *place == Origin::Unbound)
    );
}
