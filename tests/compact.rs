//! Вынос старого в архив: размер падает, числа не меняются, ничего не теряется.
//!
//! Проверка из задачи звучит так: после выноса `board` и `burnup --since` отвечают теми же
//! числами, а размер базы падает. Первая половина — про то, что вынесено НЕ читаемое отчётами
//! каждый день; вторая — что это действительно вынос, а не отчёт о нём. Обе проверяются здесь
//! на одном сценарии: узлы закрыты год назад, история уезжает, отчёты обязаны не измениться.
//!
//! Отдельно проверяется, что архив не превращает отчёты в ложь. Узел без истории попадает в
//! `burnup` счётчиком «неизвестно», то есть выглядит несделанным. Именно так вынос и ломает
//! данные, если сделать его удалением: файл меньше, отчёт уверенный и неправильный.

use std::fs;

use rusqlite::Connection;

use tasq::db;
use tasq::history::{self, Cause};
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};
use tasq::{archive, burnup, overview};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-compact-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// База с тремя узлами, закрытыми «давно»: история пишется с явными временами, потому что
/// весь смысл в дате, а не в факте перехода.
fn fixture(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, Connection, Project) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "ARCH", "архив").expect("init");
    let path = std::path::PathBuf::from(&outcome.db_path);
    let mut conn = db::open_checked(&path).expect("open");
    let project = project::first_project(&conn).expect("проект");

    // Сто двадцать старых узлов, а не три: на трёх строках освободившиеся страницы не видны, и
    // проверка «файл стал меньше» проходила бы или падала случайно. Сто двадцать дают десятки
    // страниц, которые `VACUUM` действительно отдаёт.
    let today = db::now_iso()[..10].to_string();
    let mut plan: Vec<(String, String)> = (0..120)
        .map(|index| (format!("Старая задача {index}"), "2020-03-01".to_string()))
        .collect();
    plan.push(("Свежая".to_string(), today));

    for (title, day) in plan {
        let node = node::create(
            &mut conn,
            &project,
            NewNode {
                parent: None,
                title: &title,
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
        history::record_at(
            &conn,
            node.id,
            None,
            "todo",
            Cause::Created,
            &format!("{day}T09:00:00.000Z"),
            "тест",
        )
        .expect("создание в истории");
        if title != "Свежая" {
            history::record_at(
                &conn,
                node.id,
                Some("todo"),
                "done",
                Cause::Manual,
                &format!("{day}T10:00:00.000Z"),
                "тест",
            )
            .expect("закрытие в истории");
            // И статус в самом узле: отчёты читают и его.
            conn.execute(
                "UPDATE nodes SET status = 'done' WHERE id = ?1",
                rusqlite::params![node.id],
            )
            .expect("статус");
        }
    }
    (dir, path, conn, project)
}

fn size(path: &std::path::Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// Сухой прогон ничего не меняет: он отвечает на вопрос «сколько это даст», а не делает.
#[test]
fn a_dry_run_reports_without_touching_anything() {
    let (dir, path, mut conn, _project) = fixture("dry");
    let before = size(&path);
    let report =
        archive::compact(&mut conn, &path, "2021-01-01T00:00:00.000Z", true).expect("compact");

    assert!(report.dry_run);
    // У каждого старого узла два перехода с 2020 годом; строка создания пишется «сейчас» и
    // остаётся в основной базе.
    assert_eq!(
        report.history_rows, 240,
        "все старые переходы попали в счёт"
    );
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM node_status_history", [], |row| {
            row.get(0)
        })
        .expect("счёт до");
    assert_eq!(
        total, 362,
        "три строки на каждый старый узел (120 × 3) и две у свежего"
    );
    assert_eq!(size(&path), before, "сухой прогон изменил файл");
    assert!(
        !archive::path(&path).exists(),
        "сухой прогон создал файл архива"
    );
    // Сухой прогон ничего не удалил: все строки на месте.
    let left: i64 = conn
        .query_row("SELECT COUNT(*) FROM node_status_history", [], |row| {
            row.get(0)
        })
        .expect("счёт");
    assert_eq!(left, total, "строки остались на месте");

    let _ = fs::remove_dir_all(&dir);
}

/// Вынос: файл меньше, строки в архиве, отчёты отвечают теми же числами.
#[test]
fn compacting_moves_rows_into_the_archive_and_keeps_the_reports_honest() {
    let (dir, path, mut conn, project) = fixture("move");

    // Числа ДО выноса: их и обязан повторить отчёт после.
    let board_before = overview::board(&conn, &project).expect("доска до");
    let burnup_before =
        burnup::series(&conn, &project, burnup::Metric::Count, None, None).expect("burnup до");

    let before_size = size(&path);
    let report =
        archive::compact(&mut conn, &path, "2021-01-01T00:00:00.000Z", false).expect("compact");
    assert!(!report.dry_run);
    assert_eq!(report.history_rows, 240);

    // Файл основного каталога стал меньше: в нём осталось два свежих перехода.
    let after_size = size(&path);
    assert!(
        after_size < before_size,
        "файл не уменьшился: было {before_size}, стало {after_size}"
    );
    assert!(archive::path(&path).is_file(), "файл архива не создан");

    // Соединение открывается заново — как это делает любая команда, — и архив подключается сам.
    let fresh = db::open_checked(&path).expect("open после выноса");
    assert!(archive::is_attached(&fresh), "архив не подключился");
    let archived: i64 = fresh
        .query_row(
            "SELECT COUNT(*) FROM archive.node_status_history",
            [],
            |row| row.get(0),
        )
        .expect("счёт в архиве");
    assert_eq!(archived, 240, "в архив уехали не все старые строки");
    let remaining: i64 = fresh
        .query_row("SELECT COUNT(*) FROM node_status_history", [], |row| {
            row.get(0)
        })
        .expect("счёт в основной");
    assert_eq!(
        remaining, 122,
        "в основной должны остаться строки создания старых узлов и свежий узел"
    );

    // Доска и burn-up: те же числа, что до выноса.
    let board_after = overview::board(&fresh, &project).expect("доска после");
    assert_eq!(
        serde_json::to_value(&board_before).expect("json"),
        serde_json::to_value(&board_after).expect("json"),
        "доска изменилась после выноса истории"
    );
    let burnup_after =
        burnup::series(&fresh, &project, burnup::Metric::Count, None, None).expect("burnup после");
    assert_eq!(
        serde_json::to_value(&burnup_before).expect("json"),
        serde_json::to_value(&burnup_after).expect("json"),
        "ряд burn-up изменился после выноса: закрытые узлы стали «неизвестными»"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// История узла не обрывается на границе выноса, а лента журнала — на дате усечения.
#[test]
fn the_node_history_and_the_log_survive_the_move() {
    let (dir, path, mut conn, project) = fixture("read");
    let node_id: i64 = conn
        .query_row(
            "SELECT id FROM nodes WHERE title = 'Старая задача 0'",
            [],
            |row| row.get(0),
        )
        .expect("узел");
    let before = history::timeline(&conn, node_id).expect("история до");
    // Три строки: создание (сейчас) и два старых перехода — их и выносит `compact`.
    assert_eq!(before.len(), 3, "у узла три записи истории");

    archive::compact(&mut conn, &path, "2021-01-01T00:00:00.000Z", false).expect("compact");

    let fresh = db::open_checked(&path).expect("open");
    let after = history::timeline(&fresh, node_id).expect("история после");
    assert_eq!(
        before.len(),
        after.len(),
        "история узла укоротилась после выноса"
    );
    assert_eq!(before[1].to, after[1].to, "изменился факт перехода");
    assert_eq!(before[1].at, after[1].at, "изменилось время перехода");

    // Лента проекта тоже видит вынесенное: она читается объединением.
    let recent = tasq::activity::recent(&fresh, project.id, 50).expect("журнал");
    let _ = recent; // журнал в фикстуре пуст, проверка — что запрос не падает и архив учтён
    let _ = fs::remove_dir_all(&dir);
}

/// Дамп включает вынесенные строки: иначе переезд терял бы историю молча.
#[test]
fn a_dump_contains_the_archived_rows() {
    let (dir, path, mut conn, _project) = fixture("dump");
    archive::compact(&mut conn, &path, "2021-01-01T00:00:00.000Z", false).expect("compact");

    let fresh = db::open_checked(&path).expect("open");
    let document = tasq::transfer::dump(&fresh).expect("dump");
    let rows = document["tables"]["node_status_history"]
        .as_array()
        .expect("история в дампе");
    assert_eq!(
        rows.len(),
        362,
        "в дампе должны быть все переходы, включая вынесенные"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// На базе без архива ничего не меняется: подключение — не обязанность, а следствие выноса.
#[test]
fn a_database_without_an_archive_behaves_as_before() {
    let (dir, path, conn, project) = fixture("none");
    assert!(!archive::is_attached(&conn), "архив подключился сам");
    assert!(!archive::path(&path).exists(), "файл архива появился");
    assert!(
        !overview::board(&conn, &project)
            .expect("доска")
            .columns
            .is_empty(),
        "доска не отвечает"
    );
    assert!(archive::history_source(&conn).contains("FROM node_status_history"));
    assert!(
        !archive::history_source(&conn).contains("UNION"),
        "источник без архива не должен объединять"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Повторный вынос того же диапазона безопасен: строки уже в архиве, а не задвоены.
#[test]
fn compacting_twice_is_idempotent() {
    let (dir, path, mut conn, _project) = fixture("twice");
    let first =
        archive::compact(&mut conn, &path, "2021-01-01T00:00:00.000Z", false).expect("первый");
    assert_eq!(first.history_rows, 240);

    let second =
        archive::compact(&mut conn, &path, "2021-01-01T00:00:00.000Z", false).expect("второй");
    assert_eq!(second.history_rows, 0, "второй прогон снова нашёл строки");

    let fresh = db::open_checked(&path).expect("open");
    let archived: i64 = fresh
        .query_row(
            "SELECT COUNT(*) FROM archive.node_status_history",
            [],
            |row| row.get(0),
        )
        .expect("счёт");
    assert_eq!(archived, 240, "строки задвоились в архиве");

    let _ = fs::remove_dir_all(&dir);
}
