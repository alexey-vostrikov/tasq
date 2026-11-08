//! Резервы: таблица конфликтов, атомарность с арендой, освобождение при истечении.

use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;

use tasq::db;
use tasq::lease::{self, ClaimRequest, Outcome};
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};
use tasq::resource::{self, Resource};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-res-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn fixture(tag: &str) -> (Connection, Project, node::Node) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "TASQ", "demo").expect("init");
    let mut conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = project::default_project(&conn).expect("project");
    let mut make = |title: &str, parent: Option<&str>| {
        node::create(
            &mut conn,
            &project,
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
    };
    let epic = make("Epic", None);
    let task = make("Task", Some(&epic.dot_id));
    (conn, project, task)
}

fn claim_with(
    conn: &mut Connection,
    project: &Project,
    reference: &str,
    holder: &str,
    reservations: Vec<Resource>,
) -> tasq::Result<lease::Lease> {
    lease::claim(
        conn,
        project,
        reference,
        &ClaimRequest {
            holder,
            ttl_seconds: 900,
            steal: false,
            force: false,
            checkpoint: None,
            reservations,
        },
    )
}

#[test]
fn claim_takes_the_files_atomically() {
    let (mut conn, project, task) = fixture("claim");
    let lease = claim_with(
        &mut conn,
        &project,
        &task.dot_id,
        "agent-a",
        vec![Resource::path("src/api"), Resource::logical("port:8080")],
    )
    .expect("claim with reservations");

    assert_eq!(
        lease.reservations,
        vec!["path:src/api".to_string(), "logical:port:8080".to_string()]
    );
    let node = node::resolve(&conn, &project, &task.dot_id).expect("node");
    assert_eq!(node.effective_status, "in_progress");
}

#[test]
fn overlapping_path_blocks_the_claim_and_names_the_holder() {
    let (mut conn, project, task) = fixture("overlap");
    node::create(
        &mut conn,
        &project,
        NewNode {
            parent: None,
            title: "Second epic",
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
    .expect("second epic");

    let first = claim_with(
        &mut conn,
        &project,
        &task.dot_id,
        "agent-a",
        vec![Resource::path("src/api")],
    )
    .expect("first claim");

    let err = claim_with(
        &mut conn,
        &project,
        "TASQ-2",
        "agent-b",
        vec![Resource::path("src/api/handlers.rs")],
    )
    .unwrap_err();
    assert_eq!(err.code(), "RESOURCE_RESERVATION_CONFLICT");
    let message = err.to_string();
    assert!(message.contains("agent-a"), "назван держатель: {message}");
    assert!(message.contains("src/api"), "назван ресурс: {message}");
    assert!(
        message.contains(&first.expires_at),
        "назван срок: {message}"
    );

    // узел не остался занятым: захват откатился целиком
    let epic = node::resolve(&conn, &project, "TASQ-2").expect("epic");
    assert_eq!(epic.status, "planned");
    assert_eq!(epic.lease_holder, None);
}

#[test]
fn segment_boundary_is_respected() {
    let (mut conn, project, task) = fixture("boundary");
    node::create(
        &mut conn,
        &project,
        NewNode {
            parent: None,
            title: "Other epic",
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
    .expect("epic");

    claim_with(
        &mut conn,
        &project,
        &task.dot_id,
        "agent-a",
        vec![Resource::path("src/api")],
    )
    .expect("first");

    // src/apix — другой каталог, не пересекается с src/api
    let lease = claim_with(
        &mut conn,
        &project,
        "TASQ-2",
        "agent-b",
        vec![Resource::path("src/apix/other.rs")],
    )
    .expect("не должен конфликтовать");
    assert_eq!(
        lease.reservations,
        vec!["path:src/apix/other.rs".to_string()]
    );
}

#[test]
fn glob_patterns_conflict_conservatively() {
    let (mut conn, project, task) = fixture("glob");
    node::create(
        &mut conn,
        &project,
        NewNode {
            parent: None,
            title: "Other epic",
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
    .expect("epic");

    claim_with(
        &mut conn,
        &project,
        &task.dot_id,
        "agent-a",
        vec![Resource::glob("src/**/*.rs")],
    )
    .expect("first");

    let err = claim_with(
        &mut conn,
        &project,
        "TASQ-2",
        "agent-b",
        vec![Resource::path("src/main.rs")],
    )
    .unwrap_err();
    assert_eq!(err.code(), "RESOURCE_RESERVATION_CONFLICT");

    // а другой каталог верхнего уровня — свободен
    let lease = claim_with(
        &mut conn,
        &project,
        "TASQ-2",
        "agent-b",
        vec![Resource::path("tests/main.rs")],
    )
    .expect("не пересекается");
    assert_eq!(lease.reservations.len(), 1);
}

#[test]
fn shared_reservations_coexist_with_shared_only() {
    let (mut conn, project, task) = fixture("shared");
    for title in ["Second", "Third"] {
        node::create(
            &mut conn,
            &project,
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
        .expect("epic");
    }

    claim_with(
        &mut conn,
        &project,
        &task.dot_id,
        "reader-a",
        vec![Resource::path("docs/spec.md").shared()],
    )
    .expect("shared claim");

    claim_with(
        &mut conn,
        &project,
        "TASQ-2",
        "reader-b",
        vec![Resource::path("docs/spec.md").shared()],
    )
    .expect("второй shared допустим");

    let err = claim_with(
        &mut conn,
        &project,
        "TASQ-3",
        "writer",
        vec![Resource::path("docs/spec.md")],
    )
    .unwrap_err();
    assert_eq!(err.code(), "RESOURCE_RESERVATION_CONFLICT");
}

#[test]
fn overlapping_resources_inside_one_request_are_allowed() {
    let (mut conn, project, task) = fixture("selfoverlap");

    // Один исполнитель вправе держать и каталог, и шаблон, который в него попадает:
    // собственные резервы не конфликтуют сами с собой.
    let lease = claim_with(
        &mut conn,
        &project,
        &task.dot_id,
        "agent-a",
        vec![Resource::path("src/web"), Resource::glob("src/*/types.rs")],
    )
    .expect("свои резервы не мешают друг другу");
    assert_eq!(lease.dot_id, task.dot_id);

    let held = resource::attempt_reservations(&conn, lease.attempt_id).expect("reservations");
    assert_eq!(held.len(), 2);
}

#[test]
fn release_frees_the_resources() {
    let (mut conn, project, task) = fixture("release");
    node::create(
        &mut conn,
        &project,
        NewNode {
            parent: None,
            title: "Other epic",
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
    .expect("epic");

    let lease = claim_with(
        &mut conn,
        &project,
        &task.dot_id,
        "agent-a",
        vec![Resource::path("src/api")],
    )
    .expect("claim");
    lease::release(
        &mut conn,
        &project,
        &lease.token,
        Outcome::Released,
        None,
        false,
    )
    .expect("release");

    claim_with(
        &mut conn,
        &project,
        "TASQ-2",
        "agent-b",
        vec![Resource::path("src/api")],
    )
    .expect("после освобождения ресурс свободен");
}

#[test]
fn expiry_frees_the_resources_without_gc() {
    let (mut conn, project, task) = fixture("expiry");
    node::create(
        &mut conn,
        &project,
        NewNode {
            parent: None,
            title: "Other epic",
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
    .expect("epic");

    let lease = claim_with(
        &mut conn,
        &project,
        &task.dot_id,
        "dead-agent",
        vec![Resource::path("src/api")],
    )
    .expect("claim");
    conn.execute(
        "UPDATE attempts SET expires_at = '2000-01-01T00:00:00.000Z' WHERE id = ?1",
        rusqlite::params![lease.attempt_id],
    )
    .expect("expire");

    // резерв истёкшей аренды не блокирует: аренда живёт по времени
    claim_with(
        &mut conn,
        &project,
        "TASQ-2",
        "agent-b",
        vec![Resource::path("src/api")],
    )
    .expect("истёкший резерв не держит ресурс");
}

#[test]
fn reserve_adds_to_a_live_lease_and_unreserve_removes() {
    let (mut conn, project, task) = fixture("add");
    let lease = claim_with(&mut conn, &project, &task.dot_id, "agent", Vec::new()).expect("claim");

    let held = lease::reserve(
        &mut conn,
        &project,
        &lease.token,
        &[
            Resource::path("src/api"),
            Resource::glob("migrations/*.sql"),
        ],
    )
    .expect("reserve");
    assert_eq!(held.len(), 2);

    // повторная заявка того же ресурса не дублируется
    let again = lease::reserve(
        &mut conn,
        &project,
        &lease.token,
        &[Resource::path("src/api")],
    )
    .expect("idempotent reserve");
    assert_eq!(again.len(), 2);

    let removed = lease::unreserve(
        &mut conn,
        &project,
        &lease.token,
        Some(&[Resource::path("src/api")]),
    )
    .expect("unreserve one");
    assert_eq!(removed, 1);
    let left = lease::unreserve(&mut conn, &project, &lease.token, None).expect("unreserve all");
    assert_eq!(left, 1);
}

#[test]
fn reserve_refuses_someone_elses_resources() {
    let (mut conn, project, task) = fixture("foreign");
    node::create(
        &mut conn,
        &project,
        NewNode {
            parent: None,
            title: "Other epic",
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
    .expect("epic");

    claim_with(
        &mut conn,
        &project,
        &task.dot_id,
        "agent-a",
        vec![Resource::path("src/api")],
    )
    .expect("first");
    let second =
        claim_with(&mut conn, &project, "TASQ-2", "agent-b", Vec::new()).expect("second claim");

    let err = lease::reserve(
        &mut conn,
        &project,
        &second.token,
        &[Resource::path("src/api")],
    )
    .unwrap_err();
    assert_eq!(err.code(), "RESOURCE_RESERVATION_CONFLICT");
    assert!(err.to_string().contains("agent-a"), "{err}");
}

#[test]
fn paths_outside_the_project_are_rejected() {
    let (mut conn, project, task) = fixture("outside");
    for bad in ["../outside", "/etc/passwd", "."] {
        let err = claim_with(
            &mut conn,
            &project,
            &task.dot_id,
            "agent",
            vec![Resource::path(bad)],
        )
        .unwrap_err();
        assert_eq!(err.code(), "PATH_OUTSIDE_PROJECT", "путь {bad}: {err}");
    }
    // узел не тронут отказами
    let node = node::resolve(&conn, &project, &task.dot_id).expect("node");
    assert_eq!(node.status, "todo");
}

#[test]
fn limits_are_enforced() {
    let (mut conn, project, task) = fixture("limit");
    let too_many: Vec<Resource> = (0..resource::MAX_PER_ATTEMPT + 1)
        .map(|index| {
            let key = format!("port:{index}");
            Resource::logical(&key)
        })
        .collect();
    let err = claim_with(&mut conn, &project, &task.dot_id, "agent", too_many).unwrap_err();
    assert_eq!(err.code(), "TOO_MANY_RESERVATIONS");

    let lease = claim_with(&mut conn, &project, &task.dot_id, "agent", Vec::new()).expect("claim");
    let long = format!("port:{}", "x".repeat(resource::MAX_VALUE_BYTES));
    let err = lease::reserve(
        &mut conn,
        &project,
        &lease.token,
        &[Resource::logical(&long)],
    )
    .unwrap_err();
    assert_eq!(err.code(), "INVALID_VALUE");
}

#[test]
fn holdings_list_the_reservations() {
    let (mut conn, project, task) = fixture("holdings");
    claim_with(
        &mut conn,
        &project,
        &task.dot_id,
        "agent",
        vec![Resource::path("src/api")],
    )
    .expect("claim");

    let live = lease::holdings(&conn, &project).expect("holdings");
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].reservations, vec!["path:src/api".to_string()]);
}

#[test]
fn gc_releases_reservations_of_expired_attempts() {
    let (mut conn, project, task) = fixture("gc");
    let lease = claim_with(
        &mut conn,
        &project,
        &task.dot_id,
        "dead",
        vec![Resource::path("src/api")],
    )
    .expect("claim");
    conn.execute(
        "UPDATE attempts SET expires_at = '2000-01-01T00:00:00.000Z' WHERE id = ?1",
        rusqlite::params![lease.attempt_id],
    )
    .expect("expire");

    let closed = lease::gc(&mut conn, &project).expect("gc");
    assert_eq!(closed, 1);

    let live: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM reservations WHERE released_at IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(live, 0, "после уборки живых резервов нет");
}

/// Префикс `./` снимается до проверки на пустоту: иначе шаблон `./` сохранялся бы пустым
/// и не проходил повторную нормализацию (найдено property-тестом идемпотентности).
#[test]
fn glob_prefix_is_stripped_before_the_emptiness_check() {
    let raw = Resource {
        kind: resource::Kind::Glob,
        value: "./".to_string(),
        mode: resource::Mode::Exclusive,
    };
    let error = resource::normalize(&raw).expect_err("./ не шаблон");
    assert_eq!(error.code(), "INVALID_VALUE");

    let ok = Resource {
        kind: resource::Kind::Glob,
        value: "./src/*.rs".to_string(),
        mode: resource::Mode::Exclusive,
    };
    let normalized = resource::normalize(&ok).expect("normalize");
    assert_eq!(normalized.value, "src/*.rs");
    let twice = resource::normalize(&normalized).expect("повторная нормализация");
    assert_eq!(twice, normalized);
}

/// Пробел или табуляция на краю сегмента пути отвергается: иначе нормализация
/// неидемпотентна (`a\t/` → `a\t`, повторный проход → `a`). Найдено property-тестом в CI.
#[test]
fn whitespace_at_segment_edges_is_rejected() {
    // Пробелы на краю всего значения снимаются `trim` и потому идемпотентны — это нормально.
    // Неидемпотентны только пробелы внутри пути, на краю сегмента.
    for value in ["a\t/", "a /", "src/ a/x", "src/a \t/b"] {
        match resource::normalize(&Resource::path(value)) {
            Ok(normalized) => panic!("{value:?} принят как {:?}", normalized.value),
            Err(error) => assert_eq!(error.code(), "INVALID_VALUE", "{value:?}"),
        }
    }

    // Обычные пути не задеты.
    for value in ["src/api", "src/api/", "./src/api", "src//api"] {
        let normalized = resource::normalize(&Resource::path(value)).expect("normalize");
        assert_eq!(normalized.value, "src/api", "{value:?}");
        let twice = resource::normalize(&normalized).expect("twice");
        assert_eq!(twice, normalized, "{value:?}");
    }
}
