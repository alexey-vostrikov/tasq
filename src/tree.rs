//! Поддерево по структуре дерева, а не по строке адреса.
//!
//! Раньше поддерево выбиралось сравнением `path = ? OR path GLOB 'путь.*'`. Это выглядит как
//! работа по индексу, но планом не является: `EXPLAIN QUERY PLAN` показывает
//! `SEARCH nodes USING COVERING INDEX sqlite_autoindex_nodes_1 (project_id=?)`, то есть СКАН всех
//! узлов проекта с построчной проверкой GLOB. Замер на базе 2 015 538 узлов:
//!
//! | поддерево | `path GLOB` | обход по `parent_id` | строк |
//! | --- | --- | --- | --- |
//! | 56 тыс. потомков | 157 мс | 38 мс | 55 987 |
//! | 9.3 тыс. | 127 мс | 6 мс | 9 331 |
//! | 1.5 тыс. | 127 мс | 1 мс | 1 555 |
//! | 43 узла | 130 мс | 0.0 мс | 43 |
//!
//! Ключевое здесь — НЕЗАВИСИМОСТЬ времени GLOB от размера поддерева: 130 мс и на сорока трёх
//! узлах, потому что скан идёт по всей базе. Обход по `idx_nodes_parent` растёт вместе с
//! поддеревом, как и должен: сложность O(поддерево) вместо O(проект).
//!
//! При 96 узлах живой базы разница невидима. Она станет заметной ровно тогда, когда база
//! вырастет, — и тогда её уже нельзя будет починить без смены запросов, поэтому меняем сейчас.
//!
//! Фрагменты ниже подставляются в запрос как подзапрос `IN (…)`. Единственный `?` — это id
//! корня, и он идёт в общем порядке параметров вызывающего запроса: смешивать нумерованные и
//! безымянные placeholder-ы в одном запросе нельзя, поэтому все места использования переведены
//! на безымянные.

/// Предел обхода в шагах — ТО ЖЕ число, что и правило глубины.
///
/// Здесь стояло 64 «потому что столько допускала схема до ужесточения предела». Это давало
/// второе число на одно правило: правило гласит 16 (`node::MAX_DEPTH`), а обход разрешал 64, и
/// расхождение не было видно ни из кода, ни из документации. Базы глубже 16 не создаются с v4,
/// а те, что остались, называет `doctor` — обрезать их молча в отчёте о прогрессе нельзя, но и
/// позволять обходу идти дальше правила незачем: он ограничен тем же пределом.
pub const MAX_STEPS: i64 = crate::node::MAX_DEPTH;

/// Сколько пар (предок, потомок) разрешено перечислить за одно чтение.
///
/// Предел шагов НЕ ограничивает объём: обход порождает по строке на пару «предок — потомок», и
/// на широком дереве этих пар в разы больше, чем узлов (замер: 87 381 узел → 757 305 пар).
/// При миллионах узлов это гигабайты временного файла, и раньше такой обход просто заполнял
/// диск — в 2026-м так ушло 194 ГБ, 20 % диска, за один зависший прогон.
///
/// Поэтому объём считается ДО тяжёлого чтения: верхняя граница «узлы × (предел + 1)» сравнивается
/// с этим числом, и при превышении чтение отказывает `TRAVERSAL_BUDGET` с числами. Отказ виден и
/// объясним; молчаливое заполнение диска — нет.
pub const MAX_SCAN_PAIRS: i64 = 20_000_000;

/// Действующий бюджет обхода: `MAX_SCAN_PAIRS`, если не задан `TASQ_SCAN_BUDGET`.
///
/// Переменная существует для осознанного исключения: на базе, которую владелец считает
/// нормальной, отказ можно поднять, не пересобирая бинарь. Значение читается на каждое чтение,
/// а не один раз при старте: иначе объявленный в окружении предел молча не действовал бы.
pub fn scan_budget() -> i64 {
    parse_budget(std::env::var("TASQ_SCAN_BUDGET").ok().as_deref())
}

/// Разбор значения бюджета. Мусор читается как «не задано», а не как ноль: иначе опечатка в
/// окружении запретила бы любое чтение, и сообщение об этом ничего не объясняло бы.
pub fn parse_budget(value: Option<&str>) -> i64 {
    value
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(MAX_SCAN_PAIRS)
}

/// Проверяет, что чтение прогресса уложится в бюджет; иначе отказ до начала обхода.
///
/// `project` — `None` значит «вся база»: обзор по всем проектам считает узлы всех.
pub fn check_scan_budget(
    conn: &rusqlite::Connection,
    project: Option<&crate::project::Project>,
) -> crate::error::Result<()> {
    check_scan_budget_with(conn, project, scan_budget())
}

/// То же с явным пределом: бюджет нужен и тестам, а переменная окружения в процессе — общая и
/// в Rust 2024 меняется только `unsafe`-вызовом. Явный параметр честнее подмены окружения.
pub fn check_scan_budget_with(
    conn: &rusqlite::Connection,
    project: Option<&crate::project::Project>,
    limit: i64,
) -> crate::error::Result<()> {
    let nodes: i64 = match project {
        Some(project) => conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE project_id = ?1 AND deleted_at IS NULL",
            [project.id],
            |row| row.get(0),
        )?,
        None => conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE deleted_at IS NULL",
            [],
            |row| row.get(0),
        )?,
    };
    let pairs = nodes.saturating_mul(MAX_STEPS + 1);
    if pairs > limit {
        return Err(crate::error::Error::TraversalBudget {
            nodes,
            pairs,
            limit,
        });
    }
    Ok(())
}

/// `id` поддерева ВКЛЮЧАЯ корень. Один параметр: id корня.
pub const SUBTREE_SQL: &str = "WITH RECURSIVE subtree(id, steps) AS (
       SELECT ?, 0
       UNION ALL
       SELECT child.id, subtree.steps + 1
         FROM nodes child JOIN subtree ON child.parent_id = subtree.id
        WHERE subtree.steps < MAX_STEPS
     )
     SELECT id FROM subtree";

/// `id` поддерева БЕЗ корня (потомки). Один параметр: id корня.
pub const DESCENDANTS_SQL: &str = "WITH RECURSIVE subtree(id, steps) AS (
       SELECT ?, 0
       UNION ALL
       SELECT child.id, subtree.steps + 1
         FROM nodes child JOIN subtree ON child.parent_id = subtree.id
        WHERE subtree.steps < MAX_STEPS
     )
     SELECT id FROM subtree WHERE steps > 0";

/// Максимальная глубина в поддереве включая корень. Один параметр: id корня.
pub const SUBTREE_MAX_DEPTH_SQL: &str = "WITH RECURSIVE subtree(id, steps) AS (
       SELECT ?, 0
       UNION ALL
       SELECT child.id, subtree.steps + 1
         FROM nodes child JOIN subtree ON child.parent_id = subtree.id
        WHERE subtree.steps < MAX_STEPS
     )
     SELECT MAX(steps) FROM subtree";

/// Фрагмент с подставленным пределом шагов: константа в SQL-тексте не подставляется, а держать
/// число в двух местах (здесь и в тексте) — способ разойтись молча.
fn with_limit(sql: &str) -> String {
    sql.replace("MAX_STEPS", &MAX_STEPS.to_string())
}

/// Запрос поддерева с подставленным пределом. Возвращает текст, готовый к вклейке в `IN (…)`.
pub fn subtree_sql() -> String {
    with_limit(SUBTREE_SQL)
}

pub fn descendants_sql() -> String {
    with_limit(DESCENDANTS_SQL)
}

pub fn subtree_max_depth_sql() -> String {
    with_limit(SUBTREE_MAX_DEPTH_SQL)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Предел шагов подставляется в текст: в самом фрагменте остаётся только слово-заглушка,
    /// иначе число пришлось бы держать в двух местах.
    #[test]
    fn the_limit_is_inlined() {
        for sql in [subtree_sql(), descendants_sql(), subtree_max_depth_sql()] {
            assert!(
                !sql.contains("MAX_STEPS"),
                "заглушка осталась в тексте: {sql}"
            );
            assert!(
                sql.contains(&MAX_STEPS.to_string()),
                "предел не подставлен: {sql}"
            );
        }
    }

    /// У каждого фрагмента ровно один параметр — id корня. Больше быть не может: второй
    /// placeholder сдвинул бы параметры вызывающего запроса.
    #[test]
    fn each_fragment_takes_exactly_one_parameter() {
        for sql in [subtree_sql(), descendants_sql(), subtree_max_depth_sql()] {
            assert_eq!(sql.matches('?').count(), 1, "не один параметр: {sql}");
        }
    }
}
