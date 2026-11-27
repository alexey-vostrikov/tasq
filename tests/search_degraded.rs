//! Поиск честно говорит, когда ответил не индексом.
//!
//! Запасной путь существовал всегда, и всегда молча: FTS5 не разбирает `файл:строка`, а без
//! индекса запрос к `comments_fts` вообще падает. Тогда поиск отступал к сравнению по словам —
//! без ранжирования и без словоформ — и человек видел другой ответ, не зная этого. На живой базе
//! так отвечал даже обычный `find "миграция"`, потому что схема оставалась старой и индекса не
//! было вовсе (TASQDEV-19).
//!
//! Теперь отступление видно в ответе: `degraded` называет поверхность и причину, `complete`
//! отвечает на вопрос одной булевой величиной. Проверяются обе причины по отдельности — они
//! означают разное и чинятся по-разному.

use std::fs;

use rusqlite::Connection;

use tasq::db;
use tasq::node::{self, NewNode};
use tasq::overview::{self, Fallback};
use tasq::project::{self, Project};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-degraded-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn fixture(tag: &str) -> (std::path::PathBuf, Connection, Project) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "TASQ", "поиск").expect("init");
    let path = std::path::PathBuf::from(&outcome.db_path);
    let mut conn = db::open_checked(&path).expect("open");
    let project = project::first_project(&conn).expect("проект");
    for title in ["Миграция схемы", "Хранилище на WAL"] {
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
                tags: None,
            },
        )
        .expect("узел");
    }
    (dir, conn, project)
}

/// Обычный запрос отвечает индексом и не жалуется.
#[test]
fn an_indexed_answer_is_complete() {
    let (dir, conn, project) = fixture("complete");
    let found = overview::search(&conn, &project, "миграция").expect("поиск");
    assert!(!found.hits.is_empty(), "находки есть");
    assert!(
        found.is_complete(),
        "чистый запрос не должен считаться неполным: {:?}",
        found.degraded
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Запрос, который FTS5 не разобрал, ищется по словам — и это названо, но не как поломка.
///
/// Причина `QueryNotUnderstood`, а не `IndexMissing`: индекс на месте, чинить нечего. Разница
/// видна по коду в ответе, и путать их нельзя — иначе «запусти migrate» советовали бы в ответ на
/// незакрытую кавычку.
///
/// Проверяется именно незакрытая кавычка: двоеточие и дефис до FTS не доходят — `fts_expression`
/// собирает выражение из слов и отбрасывает пунктуацию, поэтому обычный текст разбирается всегда.
/// Сырым запрос уходит только когда в нём есть `"`, `OR` или `AND`, и вот там синтаксис может
/// не сойтись.
#[test]
fn a_query_the_index_cannot_parse_reports_itself() {
    let (dir, conn, project) = fixture("syntax");
    let found = overview::search(&conn, &project, "\"миграция").expect("поиск");
    assert!(!found.is_complete(), "ответ по словам не бывает полным");
    assert!(
        found
            .degraded
            .iter()
            .any(|item| item.reason == Fallback::QueryNotUnderstood),
        "ожидалась причина QueryNotUnderstood: {:?}",
        found.degraded
    );
    assert!(
        found
            .degraded
            .iter()
            .all(|item| item.reason == Fallback::QueryNotUnderstood),
        "индекс на месте, значит IndexMissing быть не должно: {:?}",
        found.degraded
    );
    // И находки при этом есть: отступление — это не отказ искать.
    assert!(
        found.hits.iter().any(|hit| hit.title.contains("Миграция")),
        "запасной путь обязан находить: {:?}",
        found.hits
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Индекса нет — причина `IndexMissing`, и она называет команду, которая это чинит.
///
/// Сценарий воспроизводит живой дефект: база на схеме v10, `comments_fts` отсутствует. Поиск
/// обязан и найти (по словам), и сказать, что индекса не было.
#[test]
fn a_missing_index_is_named_as_a_defect_with_the_command_that_fixes_it() {
    let (dir, conn, project) = fixture("missing");
    conn.execute_batch(
        "DROP TRIGGER comments_fts_ai;
         DROP TRIGGER comments_fts_ad;
         DROP TRIGGER comments_fts_au;
         DROP TABLE comments_fts;",
    )
    .expect("удаление индекса комментариев");

    let found = overview::search(&conn, &project, "миграция").expect("поиск");
    let comments = found
        .degraded
        .iter()
        .find(|item| item.surface == "comments")
        .expect("поверхность комментариев обязана пожаловаться");
    assert_eq!(comments.reason, Fallback::IndexMissing);
    assert_eq!(comments.reason.code(), "index_missing");
    assert!(
        comments.note.contains("tasq migrate"),
        "пометка обязана называть команду: {}",
        comments.note
    );
    // Узлы и заметки отвечают индексом — жалоба не должна быть общей.
    assert!(
        !found.degraded.iter().any(|item| item.surface == "nodes"),
        "поверхность узлов не должна жаловаться: {:?}",
        found.degraded
    );
    let _ = fs::remove_dir_all(&dir);
}

/// В JSON рядом с находками едут `complete` и `degraded` — потребителю не надо ничего выводить.
#[test]
fn the_json_envelope_carries_the_quality_of_the_answer() {
    let (dir, conn, project) = fixture("json");
    let clean = serde_json::to_value(overview::search(&conn, &project, "миграция").expect("поиск"))
        .expect("json");
    assert_eq!(clean["complete"], serde_json::json!(true));
    assert_eq!(clean["degraded"], serde_json::json!([]));
    assert!(
        clean["hits"]
            .as_array()
            .is_some_and(|hits| !hits.is_empty()),
        "{clean}"
    );

    conn.execute_batch("DROP TABLE comments_fts")
        .expect("индекс");
    let degraded =
        serde_json::to_value(overview::search(&conn, &project, "миграция").expect("поиск"))
            .expect("json");
    assert_eq!(degraded["complete"], serde_json::json!(false));
    let reasons: Vec<&str> = degraded["degraded"]
        .as_array()
        .expect("список")
        .iter()
        .map(|item| item["reason"].as_str().expect("причина"))
        .collect();
    assert_eq!(reasons, vec!["index_missing"], "{degraded}");

    let _ = fs::remove_dir_all(&dir);
}

/// Командная строка называет отступление в stderr, а находки остаются в stdout.
///
/// Разделение важно для скриптов: `find | …` получает только находки, а человек видит
/// предупреждение. Если бы предупреждение шло в stdout, любой разбор вывода сломался бы.
#[test]
fn the_command_line_puts_the_note_on_stderr_and_the_hits_on_stdout() {
    let (dir, _, _) = fixture("cli");
    let path = dir.join(".tasq/tasq.db");
    {
        let conn = db::open(&path).expect("open");
        conn.execute_batch("DROP TABLE comments_fts")
            .expect("индекс");
    }

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tasq"))
        .current_dir(&dir)
        .args(["--project", "TASQ", "find", "миграция"])
        .output()
        .expect("запуск tasq find");
    assert!(out.status.success(), "поиск не должен падать");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("Миграция"),
        "находки обязаны быть в stdout: {stdout}"
    );
    assert!(
        !stdout.contains("index"),
        "предупреждение не должно попадать в stdout: {stdout}"
    );
    assert!(
        stderr.contains("index_missing") || stderr.contains("without an index"),
        "предупреждение обязано быть в stderr: {stderr}"
    );

    let _ = fs::remove_dir_all(&dir);
}
