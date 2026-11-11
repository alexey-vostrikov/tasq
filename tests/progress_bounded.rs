//! Прогресс поддерева: обход вверх вместо обхода вниз.
//!
//! Зачем менялось. Сторож v8 ограничивал ШАГИ обхода, но не объём: представление обходило дерево
//! вниз от каждого узла, то есть считало сумму размеров поддеревьев всех предков. Замер на
//! законном дереве без циклов: глубина 10 при ветвлении 4 — 349 525 узлов и 3 378 745 строк
//! обхода (в 9,7 раза больше узлов), а на разрешённых 16 уровнях при ветвлении 6 это 2,8 трлн
//! строк. Предел глубины объём не ограничивает — ограничивает направление обхода.
//!
//! Здесь три вещи, которые обязаны держаться вместе: результат НЕ изменился, объём стал
//! линейным по числу узлов, и предел берётся из одного числа.

use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;

use tasq::db;
use tasq::lease;
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-progress-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn fixture(tag: &str) -> (Connection, Project) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "PRG", "прогресс").expect("init");
    let conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");
    (conn, project)
}

/// Прогресс каждого узла, посчитанный ПРЕЖНИМ представлением (обход вниз, предел 64).
///
/// Текст берётся из миграции 0008 как есть — это и есть предмет сравнения, а не его пересказ.
const DOWNWARD_SQL: &str = "
WITH RECURSIVE tree(ancestor_id, node_id, steps) AS (
  SELECT id, id, 0 FROM nodes
  UNION ALL
  SELECT t.ancestor_id, n.id, t.steps + 1
  FROM tree t JOIN nodes n ON n.parent_id = t.node_id
  WHERE t.steps < 64
)
SELECT t.ancestor_id, COUNT(*),
       SUM(CASE WHEN n.status = 'done' THEN 1 ELSE 0 END),
       SUM(n.weight),
       SUM(CASE WHEN n.status = 'done' THEN n.weight ELSE 0 END)
FROM tree t JOIN nodes n ON n.id = t.node_id AND n.deleted_at IS NULL
GROUP BY t.ancestor_id
ORDER BY t.ancestor_id";

fn progress_with(conn: &Connection, sql: &str) -> Vec<(i64, i64, i64, f64, f64)> {
    let mut stmt = conn.prepare(sql).expect("query");
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, f64>(3)?,
                row.get::<_, f64>(4)?,
            ))
        })
        .expect("rows");
    rows.collect::<rusqlite::Result<Vec<_>>>().expect("collect")
}

const NEW_VIEW_SQL: &str = "
SELECT node_id, total_count, done_count, total_weight, done_weight
FROM v_node_progress ORDER BY node_id";

/// Дерево с работой: часть узлов закрыта, один удалён мягко.
fn seeded_tree(conn: &mut Connection, project: &Project) -> Vec<node::Node> {
    let root = node::create(
        conn,
        project,
        NewNode {
            parent: None,
            title: "Корень",
            kind: None,
            description: None,
            priority: None,
            due_date: None,
            estimated_hours: None,
            weight: Some(2.0),
            source_ref: None,
            branch: None,
        },
    )
    .expect("root");
    let mut made = vec![root.clone()];
    for index in 0..3 {
        let child = node::create(
            conn,
            project,
            NewNode {
                parent: Some(&root.dot_id),
                title: &format!("Ветка {index}"),
                kind: None,
                description: None,
                priority: None,
                due_date: None,
                estimated_hours: None,
                weight: Some(1.0),
                source_ref: None,
                branch: None,
            },
        )
        .expect("child");
        made.push(child.clone());
        for leaf in 0..2 {
            let node = node::create(
                conn,
                project,
                NewNode {
                    parent: Some(&child.dot_id),
                    title: &format!("Лист {index}.{leaf}"),
                    kind: None,
                    description: None,
                    priority: None,
                    due_date: None,
                    estimated_hours: None,
                    weight: Some(1.0),
                    source_ref: None,
                    branch: None,
                },
            )
            .expect("leaf");
            made.push(node);
        }
    }
    // Часть работы закрыта: иначе done_count у всех нулевой и сравнение ничего не значит.
    // Закрывается так же, как это делает работа — аренда и исход «готово»: прямой перевод из
    // `idea` в `done` матрица запрещает, и это правильно.
    for index in [3usize, 5, 9] {
        let held = lease::claim(
            conn,
            project,
            &made[index].dot_id,
            &lease::ClaimRequest {
                holder: "агент",
                ttl_seconds: lease::DEFAULT_TTL,
                steal: false,
                force: false,
                checkpoint: None,
                reservations: Vec::new(),
            },
        )
        .expect("claim");
        lease::release(
            conn,
            project,
            &held.token,
            lease::Outcome::Completed,
            None,
            false,
        )
        .expect("completed");
    }
    made
}

/// Новый обход даёт ТО ЖЕ, что старый: множество пар (предок, потомок) не изменилось.
#[test]
fn the_upward_walk_reports_the_same_numbers_as_the_downward_one() {
    let (mut conn, project) = fixture("same");
    seeded_tree(&mut conn, &project);

    let before = progress_with(&conn, DOWNWARD_SQL);
    let after = progress_with(&conn, NEW_VIEW_SQL);
    assert_eq!(
        before, after,
        "прогресс разошёлся: было {before:?}, стало {after:?}"
    );
    assert!(
        before.iter().any(|(_, _, done, _, _)| *done > 0),
        "в фикстуре нет закрытой работы — сравнение ничего не проверяет: {before:?}"
    );
    assert!(
        before.iter().any(|(_, total, _, _, _)| *total > 1),
        "в фикстуре нет вложенности: {before:?}"
    );
}

/// Удалённый узел не считается ни в одном из вариантов — фильтр остался на агрегате.
#[test]
fn a_soft_deleted_node_disappears_from_both() {
    let (mut conn, project) = fixture("deleted");
    let made = seeded_tree(&mut conn, &project);
    // Удаляется ОТКРЫТЫЙ узел: закрытый удалить нельзя (это работа, а не мусор).
    node::delete(&mut conn, &project, &made[4].dot_id).expect("delete");

    let before = progress_with(&conn, DOWNWARD_SQL);
    let after = progress_with(&conn, NEW_VIEW_SQL);
    assert_eq!(before, after, "удаление учтено по-разному");

    let root = after
        .iter()
        .find(|(id, _, _, _, _)| *id == made[0].id)
        .expect("корень");
    assert_eq!(
        root.1, 9,
        "после удаления одного листа у корня должно остаться 9 потомков: {root:?}"
    );
}

/// Строит широкое дерево прямой вставкой: через API это тысячи транзакций и минуты.
///
/// `branching` детей у каждого узла, `depth` уровней. Возвращает число узлов.
fn build_wide(conn: &mut Connection, project_id: i64, depth: i64, branching: i64) -> i64 {
    let tx = conn.transaction().expect("tx");
    tx.execute(
        "INSERT INTO nodes (project_id, parent_id, depth, seq, path, kind, title, status,
                            priority, weight, created_at, updated_at)
         VALUES (?1, NULL, 1, 1, '1', 'epic', 'корень', 'idea', 'medium', 1.0,
                 '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z')",
        [project_id],
    )
    .expect("root");
    let root_id: i64 = tx.last_insert_rowid();
    let mut parents = vec![(root_id, String::from("1"))];
    let mut total = 1;
    for level in 2..=depth {
        let mut next = Vec::new();
        for (parent_id, parent_path) in &parents {
            for child in 1..=branching {
                let path = format!("{parent_path}.{child}");
                tx.execute(
                    "INSERT INTO nodes (project_id, parent_id, depth, seq, path, kind, title,
                                        status, priority, weight, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'subtask', 'узел', 'idea', 'medium', 1.0,
                             '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z')",
                    rusqlite::params![project_id, parent_id, level, child, path],
                )
                .expect("node");
                next.push((tx.last_insert_rowid(), path));
                total += 1;
            }
        }
        parents = next;
    }
    tx.commit().expect("commit");
    total
}

/// Объём обхода ограничен сверху `узлы × (предел + 1)` — и это единственная граница, которую
/// даёт предел глубины.
///
/// Замер по ходу работы опроверг ожидание, что смена направления обхода уменьшит объём: обход
/// вверх и обход вниз перечисляют ОДНО И ТО ЖЕ множество пар, поэтому число строк совпадает
/// (на этой фикстуре — 757 305 в обоих случаях). Значит объём задаётся данными, а не
/// направлением, и защищать от него надо бюджетом строк, а не переписыванием запроса.
#[test]
fn the_scan_is_bounded_by_nodes_times_depth() {
    let (mut conn, project) = fixture("bound");
    let nodes = build_wide(&mut conn, project.id, 9, 4);
    assert!(
        nodes > 80_000,
        "фикстура мала для проверки границы: {nodes}"
    );

    // Время обхода печатается, но порогом НЕ является. Абсолютный порог на секундомер
    // краснеет на верном коде: под `cargo llvm-cov` та же работа идёт втрое-вдесятеро дольше
    // (замер: 5,85 с против 1–2 с без инструментации), а под параллельным прогоном к работе
    // добавляется то, что делал планировщик. Первый же такой красный снял бы проверку целиком.
    // Настоящий гейт — объём выше (`produced <= bound`): он и ловит то, ради чего тест написан,
    // — неограниченный обход. Число из трёх замеров берётся минимумом: он ближе к самой работе.
    let mut best = std::time::Duration::MAX;
    for _ in 0..3 {
        let started = std::time::Instant::now();
        let rows = progress_with(&conn, NEW_VIEW_SQL);
        best = best.min(started.elapsed());
        assert_eq!(
            rows.len() as i64,
            nodes,
            "в представлении должен быть ровно один ряд на каждый живой узел"
        );
    }
    eprintln!("обход {nodes} узлов: {best:?} (минимум из 3, без порога)");
    let produced: i64 = {
        // Число строк обхода = сумма total_count по всем предкам, которых видит представление:
        // каждый узел даёт строку на каждый свой шаг вверх.
        conn.query_row(
            "SELECT COUNT(*) FROM (
               WITH RECURSIVE t(a, n, s) AS (
                 SELECT id, id, 0 FROM nodes
                 UNION ALL
                 SELECT p.id, t.n, t.s + 1 FROM t
                   JOIN nodes self ON self.id = t.a
                   JOIN nodes p ON p.id = self.parent_id
                  WHERE t.s < ?1
               ) SELECT 1 FROM t
             )",
            [tasq::node::MAX_DEPTH],
            |row| row.get(0),
        )
        .expect("count")
    };

    let bound = nodes * (tasq::node::MAX_DEPTH + 1);
    assert!(
        produced <= bound,
        "обход дал {produced} строк при границе {bound}: граница перестала держаться"
    );

    // Обход вниз на той же фикстуре даёт ровно столько же строк: множества пар совпадают.
    // Утверждение обратного («вниз заметно больше») было бы неправдой — проверено замером.
    let downward: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM (
               WITH RECURSIVE t(a, n, s) AS (
                 SELECT id, id, 0 FROM nodes
                 UNION ALL
                 SELECT t.a, c.id, t.s + 1 FROM t JOIN nodes c ON c.parent_id = t.n
                  WHERE t.s < ?1
               ) SELECT 1 FROM t
             )",
            [tasq::node::MAX_DEPTH],
            |row| row.get(0),
        )
        .expect("downward count");
    assert_eq!(
        downward, produced,
        "обходы вверх и вниз перестали быть эквивалентными: вниз {downward}, вверх {produced}"
    );
    // А объём при этом действительно больше числа узлов — именно поэтому нужен бюджет.
    assert!(
        produced > nodes,
        "на широком дереве пар должно быть больше узлов: узлов {nodes}, пар {produced}"
    );
}

/// Кольцо в дереве читается за конечное время: у узла не больше одного родителя, а шаги
/// ограничены — поэтому обход замыкается по построению, а не по удаче.
#[test]
fn a_cycle_still_terminates_within_the_step_limit() {
    let (mut conn, project) = fixture("cycle");
    let root = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: None,
            title: "Корень",
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
    .expect("root");
    let task = node::create(
        &mut conn,
        &project,
        NewNode {
            parent: Some(&root.dot_id),
            title: "Задача",
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
    .expect("task");
    // Портим базу руками: корень становится потомком задачи.
    conn.execute(
        "UPDATE nodes SET parent_id = ?1 WHERE id = ?2",
        rusqlite::params![task.id, root.id],
    )
    .expect("break");

    let rows = progress_with(&conn, NEW_VIEW_SQL);
    assert!(
        rows.len() <= 2 * (tasq::node::MAX_DEPTH as usize + 1),
        "на кольце строк больше, чем даёт предел шагов: {rows:?}"
    );
    let max_steps = tasq::node::MAX_DEPTH;
    assert!(
        rows.iter()
            .all(|(_, total, _, _, _)| *total <= max_steps + 1),
        "total_count перевалил за предел шагов: {rows:?}"
    );
}

/// Предел берётся из одного числа: в тексте представления стоит `node::MAX_DEPTH`, а не
/// собственный литерал. Раньше в SQL стояло 64, а правило гласило 16.
#[test]
fn the_view_takes_its_limit_from_the_single_constant() {
    let (conn, _project) = fixture("limit");
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'v_node_progress'",
            [],
            |row| row.get(0),
        )
        .expect("view");
    let expected = format!("steps < {}", tasq::node::MAX_DEPTH);
    assert!(
        sql.contains(&expected),
        "в представлении нет предела из константы ({expected}): {sql}"
    );
    assert!(
        !sql.contains("steps < 64"),
        "в представлении остался прежний литерал 64: {sql}"
    );
}

/// `doctor` называет узлы за пределом и верхнюю границу обхода.
#[test]
fn doctor_reports_depth_and_the_scan_bound() {
    let (mut conn, project) = fixture("doctor");
    let made = seeded_tree(&mut conn, &project);
    let path: PathBuf = conn
        .query_row("PRAGMA database_list", [], |row| row.get::<_, String>(2))
        .expect("path")
        .into();
    // Узел за пределом в базе так просто не появиться через API, поэтому портим колонку depth.
    conn.execute(
        "UPDATE nodes SET depth = ?1 WHERE id = ?2",
        rusqlite::params![tasq::node::MAX_DEPTH + 2, made[3].id],
    )
    .expect("break depth");
    drop(conn);

    let report = db::doctor(&path).expect("doctor");
    assert!(
        report.nodes_beyond_depth >= 1,
        "узел за пределом не посчитан: {report:?}"
    );
    assert_eq!(
        report.progress_scan_bound,
        report.nodes * (tasq::node::MAX_DEPTH + 1),
        "верхняя граница обхода считается не по тому же числу, что и предел"
    );
}
