//! Ошибка взятия работы должна называть настоящую причину.
//!
//! Дефект, который здесь закреплён: вставка попытки переводила ЛЮБУЮ ошибку SQLite в
//! `ACTIVE_ATTEMPT_EXISTS`. Из-за этого нарушение внешнего ключа (`attempts.session_id` ссылается
//! на `sessions`) выглядело как «узел занят другим процессом», причём с пустым сроком («until -»),
//! и настоящая причина — не заведённая сессия — терялась. Такой отчёт хуже молчания: он
//! отправляет искать аренду, которой нет.

use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;

use tasq::db;
use tasq::lease::{self, ClaimRequest};
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};
use tasq::session;

fn fixture(tag: &str) -> (Connection, Project, String) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir: PathBuf = std::env::temp_dir().join(format!("tasq-claim-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    let outcome = db::init(&dir, "AAA", "проект").expect("init");
    let mut conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");
    let epic = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: None,
            title: "Эпик",
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
    let dot_id = epic.dot_id;
    (conn, project, dot_id)
}

fn claim_as(
    conn: &mut Connection,
    project: &Project,
    dot_id: &str,
    session: Option<&str>,
) -> Result<(), tasq::error::Error> {
    lease::claim_as(
        conn,
        project,
        dot_id,
        &ClaimRequest {
            holder: "agent",
            ttl_seconds: 600,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
        session,
    )
    .map(|_| ())
}

/// Сессия, названная при взятии работы, не мешает взять её: id заводится в транзакции до
/// вставки аренды. До исправления здесь был отказ `ACTIVE_ATTEMPT_EXISTS` на свободном узле.
#[test]
fn a_named_session_does_not_block_the_claim_it_makes() {
    let (mut conn, project, dot_id) = fixture("fresh-session");
    let id = session::new_id().expect("id");
    claim_as(&mut conn, &project, &dot_id, Some(&id)).expect("взятие работы с сессией");
    assert_eq!(
        session::affinity(&conn, &id).expect("affinity").as_deref(),
        Some("AAA")
    );
}

/// Нарушение внешнего ключа не выдаётся за занятость: подсовываем аренду с несуществующей
/// сессией напрямую и проверяем, что вставка падает как нарушение FK, а не как «занято».
#[test]
fn a_foreign_key_violation_is_not_reported_as_a_busy_node() {
    let (conn, project, dot_id) = fixture("fk");
    let node_id: i64 = conn
        .query_row(
            "SELECT id FROM nodes WHERE title = 'Эпик' AND project_id = ?1",
            [project.id],
            |row| row.get(0),
        )
        .expect("node");
    let err = conn
        .execute(
            "INSERT INTO attempts (node_id, attempt_no, holder, token, ttl_seconds,
                                   started_at, expires_at, session_id)
             VALUES (?1, 1, 'ghost', 'tok', 60, '2026-01-01T00:00:00.000Z',
                     '2026-01-01T00:01:00.000Z', 'no-such-session')",
            [node_id],
        )
        .expect_err("внешний ключ обязан отказать");
    let text = err.to_string();
    assert!(
        text.to_lowercase().contains("foreign key"),
        "ожидалось нарушение внешнего ключа, получено: {text}"
    );
    let _ = dot_id;
}

/// Любая другая ошибка вставки тоже не выдаётся за занятость.
///
/// Триггер поднимает `RAISE(ABORT)` — это тот же код `SQLITE_CONSTRAINT`, что и у конфликта
/// живых попыток, но причина другая. Различать их обязательно: иначе «узел занят» становится
/// ответом на любую проблему записи.
#[test]
fn an_unrelated_insert_failure_is_not_reported_as_a_busy_node() {
    let (mut conn, project, dot_id) = fixture("trigger");
    conn.execute_batch(
        "CREATE TRIGGER deny_claim BEFORE INSERT ON attempts
         BEGIN SELECT RAISE(ABORT, 'склад закрыт'); END;",
    )
    .expect("триггер");

    let err = claim_as(&mut conn, &project, &dot_id, None).expect_err("вставка обязана отказать");
    let text = format!("{err:?}");
    assert!(
        !text.contains("ActiveAttemptExists"),
        "чужая ошибка выдана за занятость: {text}"
    );
    assert!(
        text.contains("склад закрыт"),
        "настоящая причина потерялась: {text}"
    );
}

/// Занятый узел действительно называется занятым, и срок в отчёте — настоящий, а не «-».
#[test]
fn a_live_lease_reports_the_real_expiry() {
    let (mut conn, project, dot_id) = fixture("live");
    claim_as(&mut conn, &project, &dot_id, None).expect("первое взятие");
    let err = claim_as(&mut conn, &project, &dot_id, None).expect_err("второе взятие");
    match err {
        tasq::error::Error::ActiveAttemptExists {
            holder, expires_at, ..
        } => {
            assert_eq!(holder, "agent", "должен быть назван настоящий держатель");
            assert_ne!(expires_at, "-", "срок аренды известен и не должен теряться");
        }
        other => panic!("ожидалось «занято», получено {other:?}"),
    }
}
