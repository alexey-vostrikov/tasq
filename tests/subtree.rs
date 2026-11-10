//! Поддерево выбирается структурой, а не сравнением строк адреса.
//!
//! Проверяется не «стало быстрее» (время — свойство машины), а два наблюдаемых свойства:
//!
//!   1. Результат обхода совпадает с составом поддерева при любом устройстве дерева — включая
//!      переносы, из-за которых номера идут не по порядку.
//!   2. План запроса идёт по `idx_nodes_parent`, а не сканом всех узлов проекта. Именно это
//!      отделяет O(поддерево) от O(проект): замер на 2 млн узлов показал 130 мс у `path GLOB`
//!      ДАЖЕ на поддереве из 43 узлов, потому что GLOB сканировал всю базу.

use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;

use tasq::db;
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-tree-{tag}-{nanos}"));
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

fn fixture(tag: &str) -> (Connection, Project) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "TREE", "tree").expect("init");
    let conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");
    (conn, project)
}

/// Список поддерева (`ls --parent`) возвращает узел и всех потомков, но не соседей.
#[test]
fn listing_a_subtree_takes_the_node_and_its_descendants_only() {
    let (mut conn, project) = fixture("list");
    let root = add(&mut conn, &project, None, "Корень A");
    let task = add(&mut conn, &project, Some(&root.dot_id), "Задача A1");
    let leaf = add(&mut conn, &project, Some(&task.dot_id), "Подзадача A1.1");
    let sibling = add(&mut conn, &project, None, "Корень B");

    let listed = node::list(
        &conn,
        &project,
        &node::Filter {
            parent: Some(root.dot_id.as_str()),
            include_archived: true,
            ..node::Filter::default()
        },
    )
    .expect("list");
    let ids: Vec<&str> = listed.iter().map(|node| node.dot_id.as_str()).collect();

    assert!(
        ids.contains(&root.dot_id.as_str()),
        "корень не попал: {ids:?}"
    );
    assert!(
        ids.contains(&task.dot_id.as_str()),
        "задача не попала: {ids:?}"
    );
    assert!(
        ids.contains(&leaf.dot_id.as_str()),
        "лист не попал: {ids:?}"
    );
    assert!(
        !ids.contains(&sibling.dot_id.as_str()),
        "в поддерево попал сосед: {ids:?}"
    );
}

/// Перенос внутри поддерева не ломает состав: обход идёт по `parent_id`, а не по префиксу
/// строки, поэтому годятся любые номера, в том числе идущие не по порядку.
#[test]
fn a_moved_branch_still_belongs_to_its_new_parent() {
    let (mut conn, project) = fixture("moved");
    let left = add(&mut conn, &project, None, "Левая");
    let right = add(&mut conn, &project, None, "Правая");
    let branch = add(&mut conn, &project, Some(&left.dot_id), "Ветка");
    let leaf = add(&mut conn, &project, Some(&branch.dot_id), "Лист");

    // После переноса узел получает новый адрес — работаем с ним, а не со старым dot-ID.
    let moved = node::move_node(
        &mut conn,
        &project,
        &branch.dot_id,
        Some(&right.dot_id),
        false,
    )
    .expect("перенос");
    assert_eq!(
        moved.dot_id, "TREE-2.1",
        "ветка должна переехать под правую"
    );

    // Поддерево правой ветки теперь содержит переехавшую ветку и её лист.
    let under_right = node::list(
        &conn,
        &project,
        &node::Filter {
            parent: Some(right.dot_id.as_str()),
            include_archived: true,
            ..node::Filter::default()
        },
    )
    .expect("list");
    let ids: Vec<&str> = under_right
        .iter()
        .map(|node| node.dot_id.as_str())
        .collect();
    assert!(ids.contains(&moved.dot_id.as_str()), "{ids:?}");
    // Лист переехал вместе с веткой: его адрес тоже новый.
    let leaf_now = node::resolve(&conn, &project, &format!("#{}", leaf.id)).expect("leaf");
    assert!(ids.contains(&leaf_now.dot_id.as_str()), "{ids:?}");

    // А под левой веткой её больше нет.
    let under_left = node::list(
        &conn,
        &project,
        &node::Filter {
            parent: Some(left.dot_id.as_str()),
            include_archived: true,
            ..node::Filter::default()
        },
    )
    .expect("list");
    let left_ids: Vec<&str> = under_left.iter().map(|node| node.dot_id.as_str()).collect();
    assert!(!left_ids.contains(&moved.dot_id.as_str()), "{left_ids:?}");
    assert_eq!(
        left_ids,
        vec![left.dot_id.as_str()],
        "под левой остался лишний узел"
    );
}

/// Архив и распаковка действуют на поддерево целиком и не задевают соседей.
#[test]
fn archiving_covers_the_subtree_and_spares_siblings() {
    let (mut conn, project) = fixture("archive");
    let root = add(&mut conn, &project, None, "Корень");
    let task = add(&mut conn, &project, Some(&root.dot_id), "Задача");
    let leaf = add(&mut conn, &project, Some(&task.dot_id), "Лист");
    let other = add(&mut conn, &project, None, "Другой корень");

    let affected = node::archive(&mut conn, &project, &root.dot_id, false).expect("archive");
    assert_eq!(affected, 3, "архив должен покрыть корень, задачу и лист");

    let archived = |conn: &Connection, node: &node::Node| -> bool {
        conn.query_row(
            "SELECT archived_at FROM nodes WHERE id = ?1",
            [node.id],
            |row| row.get::<_, Option<String>>(0),
        )
        .expect("archived_at")
        .is_some()
    };
    assert!(archived(&conn, &root));
    assert!(archived(&conn, &task));
    assert!(archived(&conn, &leaf));
    assert!(!archived(&conn, &other), "сосед попал под архив");

    let restored = node::archive(&mut conn, &project, &root.dot_id, true).expect("undo");
    assert_eq!(restored, 3);
    assert!(!archived(&conn, &root));
}

/// План запроса поддерева идёт по индексу `parent_id`, а не сканом узлов проекта.
///
/// Это и есть свойство, ради которого `path GLOB` убран: сравнение строк вырождалось в скан,
/// и его стоимость росла с размером БАЗЫ, а не поддерева.
#[test]
fn the_subtree_query_uses_the_parent_index() {
    let (conn, _project) = fixture("plan");
    let sql = format!(
        "SELECT id FROM nodes WHERE id IN ({})",
        tasq::tree::subtree_sql()
    );
    let mut stmt = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("plan");
    let plan: Vec<String> = stmt
        .query_map([1i64], |row| row.get::<_, String>(3))
        .expect("rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect");

    let text = plan.join("\n");
    assert!(
        text.contains("idx_nodes_parent"),
        "обход не использует индекс по parent_id:\n{text}"
    );
    assert!(
        !text.contains("SCAN nodes"),
        "обход сканирует таблицу целиком:\n{text}"
    );
}

/// Обход поддерева ограничен по шагам, поэтому цикл в испорченной базе не превращается в
/// бесконечность.
///
/// Без предела рекурсивный обход на цикле не кончается, и `ls --parent` на такой базе повесил бы
/// всё, что вызывает список. Предел не «правило» (его роль играет `MAX_DEPTH` при записи), а
/// сторож: представление и запросы обязаны завершаться на любых данных.
#[test]
fn a_cyclic_base_does_not_make_the_subtree_endless() {
    let (mut conn, project) = fixture("cyclic");
    let root = add(&mut conn, &project, None, "Корень");
    let child = add(&mut conn, &project, Some(&root.dot_id), "Ребёнок");

    // Замыкаем дерево: корень становится потомком своего ребёнка.
    conn.execute(
        "UPDATE nodes SET parent_id = ?1 WHERE id = ?2",
        rusqlite::params![child.id, root.id],
    )
    .expect("break the tree");

    // Считаем строки в САМОМ обходе, а не в списке: `id IN (подзапрос)` дедуплицирует id, и на
    // цикле из двух узлов список останется коротким при любом пределе — по нему предел не
    // проверить. Обход же выдаёт по строке на шаг.
    let steps: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM ({})", tasq::tree::descendants_sql()),
            [child.id],
            |row| row.get(0),
        )
        .expect("обход на циклической базе");
    assert!(
        steps <= 65,
        "обход не ограничен: {steps} шагов вместо не более 65"
    );
    assert!(steps >= 2, "обход не дошёл даже до ребёнка: {steps}");

    // И сам список завершается, а не висит.
    let listed = node::list(
        &conn,
        &project,
        &node::Filter {
            parent: Some(child.dot_id.as_str()),
            include_archived: true,
            ..node::Filter::default()
        },
    )
    .expect("список на циклической базе");
    assert!(!listed.is_empty(), "список пуст, хотя поддерево не пустое");
}

/// Уникальность адреса гарантирована структурой: у одного родителя номера детей не повторяются.
///
/// Это страховка от порчи строки `path`: даже если её испортить руками, два разных узла не
/// смогут получить один адрес, потому что адрес — цепочка `seq` от корня.
#[test]
fn siblings_cannot_share_a_number() {
    let (mut conn, project) = fixture("unique");
    let root = add(&mut conn, &project, None, "Корень");
    let child = add(&mut conn, &project, Some(&root.dot_id), "Ребёнок");

    // Пробуем записать тот же номер рядом с существующим ребёнком.
    let clash = conn.execute(
        "INSERT INTO nodes (project_id, parent_id, depth, seq, path, kind, title, status,
                            created_at, updated_at)
         VALUES (?1, ?2, 2, ?3, '1.99', 'task', 'двойник', 'idea',
                 '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z')",
        rusqlite::params![project.id, root.id, child.seq],
    );
    assert!(clash.is_err(), "два ребёнка получили один номер");

    // Два КОРНЯ с одинаковым номером тоже недопустимы: NULL в уникальном индексе SQLite считает
    // разными значениями, поэтому индекс построен по COALESCE(parent_id, 0).
    let other_root = conn.execute(
        "INSERT INTO nodes (project_id, parent_id, depth, seq, path, kind, title, status,
                            created_at, updated_at)
         VALUES (?1, NULL, 1, ?2, '99', 'epic', 'второй корень', 'idea',
                 '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z')",
        rusqlite::params![project.id, root.seq],
    );
    assert!(other_root.is_err(), "два корня получили один номер");
}

/// Глубина поддерева считается по структуре: предел срабатывает на самом длинном пути вниз,
/// а не на числе сегментов у одного узла.
#[test]
fn the_depth_limit_walks_the_longest_branch() {
    let (mut conn, project) = fixture("depth");
    let root = add(&mut conn, &project, None, "Корень");
    let mut deepest = root.dot_id.clone();
    // Ветка ровно до предела: перенос её целиком под корень должен упереться в потолок.
    for level in 2..=node::MAX_DEPTH {
        let node = add(
            &mut conn,
            &project,
            Some(&deepest),
            &format!("уровень {level}"),
        );
        deepest = node.dot_id;
    }
    assert_eq!(
        node::resolve(&conn, &project, &deepest)
            .expect("deepest")
            .depth,
        node::MAX_DEPTH
    );

    // Второй корень с одним ребёнком: перенос первого под него должен быть отвергнут,
    // потому что самый глубокий потомок ушёл бы на 17-й уровень.
    let other = add(&mut conn, &project, None, "Другой корень");
    add(&mut conn, &project, Some(&other.dot_id), "ребёнок");

    let error = node::move_node(&mut conn, &project, &root.dot_id, Some(&deepest), false)
        .expect_err("перенос в собственного потомка");
    assert_eq!(error.code(), "MOVE_INTO_SELF");

    let under_other = node::move_node(
        &mut conn,
        &project,
        &root.dot_id,
        Some(&other.dot_id),
        false,
    )
    .expect_err("перенос, выводящий потомка за предел глубины");
    assert_eq!(under_other.code(), "DEPTH_LIMIT", "{under_other}");
}
