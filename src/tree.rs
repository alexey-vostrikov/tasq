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

/// Предел обхода в шагах.
///
/// Это сторож завершаемости, а не правило: при записи глубина ограничена `node::MAX_DEPTH` (16),
/// но база, записанная другой версией бинаря или руками, может содержать цикл — и тогда обход
/// без предела не кончится. 64 — столько, сколько допускала схема до ужесточения предела.
const MAX_STEPS: i64 = 64;

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
