//! Миграция v3 → v4: справочник статусов, история переходов, сессии, строгий `entity_type`.
//!
//! Проверяется то, ради чего миграция делается: справочник совпадает с кодом, история
//! восстанавливается из журнала с правильными причинами, пересборка `nodes` и `activity_log` не
//! теряет строк и не рвёт ссылок, а чужие значения отбиваются так, как обещано в документации.

use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;

use tasq::db;
use tasq::status::{Kind, Status};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-v4-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// База «как у пользователя до обновления»: схема v3 и данные во всех таблицах, которые
/// ссылаются на узлы, плюс журнал с переходами статусов — из него и восстанавливается история.
fn seed_v3(db_path: &std::path::Path) {
    fs::create_dir_all(db_path.parent().expect("db dir")).expect("db dir");
    let conn = Connection::open(db_path).expect("create v3 db");
    conn.execute_batch(include_str!("../migrations/0001_init.sql"))
        .expect("v1");
    conn.execute_batch(include_str!("../migrations/0002_search_and_time.sql"))
        .expect("v2");
    conn.execute_batch(include_str!("../migrations/0003_status_funnel.sql"))
        .expect("v3");
    conn.execute_batch(
        r#"
        INSERT INTO meta (key, value) VALUES ('schema_version', '3');
        INSERT INTO meta (key, value) VALUES ('user_version', '3');
        INSERT INTO projects (id, key, name, status, timezone, tags, metadata, created_at, updated_at)
          VALUES (1, 'V3DC', 'до миграции', 'active', 'UTC', '[]', '{}',
                  '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z');
        INSERT INTO nodes (id, project_id, parent_id, depth, seq, path, kind, title, status,
                           weight, created_at, updated_at)
          VALUES (1, 1, NULL, 1, 1, '1', 'epic', 'Старый эпик', 'planned', 2,
                  '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z'),
                 (2, 1, 1, 2, 1, '1.1', 'task', 'Старая задача', 'done', 1,
                  '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z');
        INSERT INTO attempts (id, node_id, attempt_no, holder, token, ttl_seconds,
                              started_at, expires_at, ended_at, end_reason)
          VALUES (1, 2, 1, 'agent', 'tok-old', 900, '2026-01-01T00:00:00.000Z',
                  '2026-01-01T00:15:00.000Z', '2026-01-01T00:10:00.000Z', 'completed');
        INSERT INTO reservations (attempt_id, node_id, kind, value, created_at)
          VALUES (1, 2, 'path', 'src', '2026-01-01T00:00:00.000Z');
        INSERT INTO edges (from_node, to_node, kind, created_at)
          VALUES (2, 1, 'blocks', '2026-01-01T00:00:00.000Z');
        INSERT INTO comments (node_id, author, content, created_at)
          VALUES (2, 'agent', 'живой комментарий', '2026-01-01T00:00:00.000Z');
        INSERT INTO notes (project_id, node_id, title, content, created_at, updated_at)
          VALUES (1, 2, 'Заметка', 'текст', '2026-01-01T00:00:00.000Z',
                  '2026-01-01T00:00:00.000Z');

        -- Журнал: три настоящих перехода статуса разными путями и две строки, которые
        -- историей не являются.
        INSERT INTO activity_log (project_id, node_id, entity_type, entity_id, action,
                                  field_name, old_value, new_value, actor, created_at)
          VALUES (1, 2, 'node', 2, 'transition', 'status', 'todo', 'in_progress', 'человек',
                  '2026-01-01T00:01:00.000Z'),
                 (1, 2, 'node', 2, 'completed', 'status', 'in_progress', 'done', 'agent',
                  '2026-01-01T00:10:00.000Z'),
                 (1, 1, 'node', 1, 'transition', 'status', 'planned', 'todo', 'человек',
                  '2026-01-01T00:02:00.000Z'),
                 -- Статус, которого в справочнике нет: в историю не попадает, иначе ссылка
                 -- на несуществующий статус отбила бы всю миграцию.
                 (1, 1, 'node', 1, 'transition', 'status', 'todo', 'delayed', 'человек',
                  '2026-01-01T00:03:00.000Z'),
                 -- Не статус: поле другое.
                 (1, 2, 'node', 2, 'update', 'title', 'Старая', 'Старая задача', 'человек',
                  '2026-01-01T00:04:00.000Z');
        "#,
    )
    .expect("seed v3 data");
}

fn migrated(tag: &str) -> (Connection, PathBuf, PathBuf) {
    let dir = temp_dir(tag);
    let db_path = db::db_path_in(&dir);
    seed_v3(&db_path);
    let outcome = db::init(&dir, "V3DC", "до миграции").expect("init");
    assert_eq!(outcome.schema_version, db::SUPPORTED_SCHEMA_VERSION);
    let conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    (conn, dir, db_path)
}

/// Справочник статусов совпадает с кодом: те же имена, тот же порядок, те же закрытые статусы.
///
/// Это и есть ответ на вопрос «где живёт матрица»: переходы остались в коде, но порядок воронки
/// и признак закрытости теперь данные — и обязаны совпадать с `Status::all()`, иначе доска и
/// справочник разъедутся молча.
#[test]
fn the_status_registry_matches_the_code() {
    let (conn, _dir, _path) = migrated("registry");

    let mut stmt = conn
        .prepare("SELECT name, rank, terminal FROM statuses ORDER BY rank")
        .expect("prepare");
    let rows: Vec<(String, i64, i64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows");

    let code: Vec<&str> = Status::all().iter().map(|status| status.as_str()).collect();
    let registry: Vec<&str> = rows.iter().map(|(name, _, _)| name.as_str()).collect();
    assert_eq!(
        registry, code,
        "справочник и Status::all() разошлись по составу и порядку"
    );

    for (index, (name, rank, terminal)) in rows.iter().enumerate() {
        assert_eq!(
            *rank,
            index as i64 + 1,
            "{name}: rank должен идти подряд с единицы"
        );
        let closed = matches!(name.as_str(), "done" | "cancelled");
        assert_eq!(
            *terminal,
            i64::from(closed),
            "{name}: terminal означает «работа по узлу закончена»"
        );
    }

    // Статус, которого нет в справочнике, отбивается схемой, а не только кодом: это backstop
    // для чужих дампов и ручных правок базы.
    let unknown = conn.execute(
        "INSERT INTO nodes (project_id, parent_id, depth, seq, path, kind, title, status,
                            created_at, updated_at)
         VALUES (1, NULL, 1, 9, '9', 'epic', 'чужой статус', 'delayed',
                 '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z')",
        [],
    );
    assert!(unknown.is_err(), "схема приняла статус вне справочника");
}

/// Запись истории в том виде, в каком её удобно сравнивать: кортеж из пяти полей в `assert_eq!`
/// читается как пять чисел без смысла, отдельный тип называет их.
type HistoryRow = (i64, Option<String>, String, String, Option<String>);

/// История восстанавливается из журнала: все переходы на месте, причины выведены из действия,
/// а строка со статусом вне справочника и не-статусное поле в историю не попадают.
#[test]
fn history_is_rebuilt_from_the_log_with_causes() {
    let (conn, _dir, _path) = migrated("history");

    let mut stmt = conn
        .prepare(
            "SELECT h.node_id, h.from_status, h.to_status, h.cause, h.actor
             FROM node_status_history h ORDER BY h.at",
        )
        .expect("prepare");
    let rows: Vec<HistoryRow> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows");

    assert_eq!(
        rows.len(),
        3,
        "история должна содержать три перехода: {rows:?}"
    );
    assert_eq!(
        rows[0],
        (
            2,
            Some("todo".to_string()),
            "in_progress".to_string(),
            "manual".to_string(),
            Some("человек".to_string())
        ),
        "ручной переход"
    );
    assert_eq!(
        rows[1],
        (
            1,
            Some("planned".to_string()),
            "todo".to_string(),
            "manual".to_string(),
            Some("человек".to_string())
        ),
        "переход эпика"
    );
    assert_eq!(
        rows[2],
        (
            2,
            Some("in_progress".to_string()),
            "done".to_string(),
            "lease_completed".to_string(),
            Some("agent".to_string())
        ),
        "закрытие через аренду отличается от ручного перехода"
    );

    // Никаких следов строк, которые историей не являются.
    let delayed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM node_status_history WHERE to_status = 'delayed'",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(delayed, 0, "статус вне справочника попал в историю");
}

/// Пересборка `nodes` и `activity_log` не теряет строк и не рвёт ссылок: каскадный `DROP TABLE`
/// с включёнными ключами — главный подводный камень этой миграции.
#[test]
fn the_rebuild_keeps_every_row_and_every_link() {
    let (conn, _dir, _path) = migrated("counts");
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |row| row.get(0)).expect("count") };

    assert_eq!(count("SELECT COUNT(*) FROM nodes"), 2);
    assert_eq!(count("SELECT COUNT(*) FROM attempts"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM reservations"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM edges"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM comments"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM notes"), 1);
    assert_eq!(
        count("SELECT COUNT(*) FROM activity_log"),
        5,
        "журнал целиком"
    );

    let violations = {
        let mut stmt = conn.prepare("PRAGMA foreign_key_check").expect("pragma");
        stmt.query_map([], |_| Ok(())).expect("rows").count()
    };
    assert_eq!(violations, 0, "внешние ключи целы после пересборки");

    // FTS перестроен из новой таблицы: оба узла находятся по своим словам (стемминга в FTS
    // нет, поэтому «Старая» и «Старый» — разные запросы).
    for (query, expected) in [("Старая", 1), ("Старый", 1)] {
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes_fts WHERE nodes_fts MATCH ?1",
                [query],
                |row| row.get(0),
            )
            .unwrap_or_else(|err| panic!("{query}: {err}"));
        assert_eq!(hits, expected, "{query}: внешний контент FTS не перечитан");
    }
}

/// `entity_type` теперь ограничен списком: опечатка в коде отбивается базой, а не живёт годами.
#[test]
fn entity_type_must_be_one_of_the_known_values() {
    let (conn, _dir, _path) = migrated("entity");

    let typo = conn.execute(
        "INSERT INTO activity_log (project_id, node_id, entity_type, entity_id, action, created_at)
         VALUES (1, 2, 'coment', 2, 'create', '2026-01-01T00:00:00.000Z')",
        [],
    );
    assert!(typo.is_err(), "опечатка в entity_type прошла");

    // Все значения, которые пишет код, проходят.
    for value in [
        "node", "comment", "note", "edge", "project", "template", "batch", "import",
    ] {
        conn.execute(
            "INSERT INTO activity_log (project_id, node_id, entity_type, entity_id, action, created_at)
             VALUES (1, NULL, ?1, 0, 'create', '2026-01-01T00:00:00.000Z')",
            rusqlite::params![value],
        )
        .unwrap_or_else(|err| panic!("{value}: {err}"));
    }
}

/// Сессии: старая аренда остаётся без сессии (догадку за факт не выдаём), а новая её получает.
#[test]
fn sessions_exist_and_old_attempts_have_no_session() {
    let (conn, _dir, _path) = migrated("sessions");

    let missing: Option<String> = conn
        .query_row("SELECT session_id FROM attempts WHERE id = 1", [], |row| {
            row.get(0)
        })
        .expect("attempt");
    assert_eq!(missing, None, "старой аренде приписали сессию");

    conn.execute(
        "INSERT INTO sessions (id, holder, project_key, created_at, last_seen_at)
         VALUES ('s-1', 'agent', 'V3DC', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z')",
        [],
    )
    .expect("session insert");

    let wrong = conn.execute(
        "UPDATE attempts SET session_id = 's-unknown' WHERE id = 1",
        [],
    );
    assert!(wrong.is_err(), "ссылка на несуществующую сессию прошла");

    conn.execute("UPDATE attempts SET session_id = 's-1' WHERE id = 1", [])
        .expect("ссылка на существующую сессию");
}

/// Повторный `init` на мигрированной базе ничего не делает: миграция не должна применяться
/// дважды (второй прогон `ALTER TABLE ADD COLUMN` упал бы на существующей колонке).
#[test]
fn a_second_init_is_a_no_op() {
    let (_conn, dir, _path) = migrated("again");
    let second = db::init(&dir, "V3DC", "до миграции").expect("re-init");
    assert_eq!(second.schema_version, db::SUPPORTED_SCHEMA_VERSION);
    assert!(!second.migrated, "миграции уже применены");
    assert!(!second.project_created);
}

/// `epic` не бывает в `todo`, `task` — в `planned`: ограничение по виду живёт в коде, а
/// справочник обязан пропускать оба вида. Тест держит связь между ними, а не дублирует матрицу.
#[test]
fn the_registry_holds_statuses_for_both_kinds() {
    let (conn, _dir, _path) = migrated("kinds");
    for kind in [Kind::Epic, Kind::Task] {
        for status in Status::all() {
            if !status.allowed_for(kind) {
                continue;
            }
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM statuses WHERE name = ?1",
                    [status.as_str()],
                    |row| row.get(0),
                )
                .expect("count");
            assert_eq!(
                found,
                1,
                "{}: статуса {kind:?} нет в справочнике",
                status.as_str()
            );
        }
    }
}
