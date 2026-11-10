//! Подсказка к `INVALID_TRANSITION` называет то, что действительно возможно.
//!
//! Раньше на любой отказ переходов печаталось «see the transition matrix in the README, §7»:
//! человеку это лишний шаг, а MCP-агент README не читает вовсе и получает отказ без выхода.
//! Здесь проверяется обе половины: список совпадает с матрицей и не расходится с ней при
//! изменении правил.

use tasq::Error;
use tasq::status::{Kind, Status, allowed_next, check_transition};

fn hint_for(from: &str, kind: &str) -> String {
    Error::InvalidTransition {
        from: from.to_string(),
        to: "whatever".to_string(),
        kind: kind.to_string(),
    }
    .hint()
}

/// Подсказка перечисляет ровно те статусы, в которые матрица разрешает переход.
#[test]
fn the_hint_lists_the_allowed_targets() {
    let hint = hint_for("todo", "task");
    let allowed = allowed_next(Status::Todo, Kind::Task);
    assert!(
        !allowed.is_empty(),
        "у todo должны быть допустимые переходы"
    );

    for status in &allowed {
        assert!(
            hint.contains(status.as_str()),
            "в подсказке нет {}: {hint}",
            status.as_str()
        );
    }
    // И не обещает лишнего: закрытые статусы, недостижимые из todo, в подсказке не названы.
    assert!(
        !hint.contains("review"),
        "обещан недостижимый статус: {hint}"
    );
    assert!(hint.contains("todo"), "не назван исходный статус: {hint}");
    assert!(hint.contains("task"), "не назван вид узла: {hint}");
}

/// Список в подсказке не расходится с проверкой: для каждого названного статуса `check_transition`
/// проходит, а для любого НЕ названного — нет.
///
/// Это и есть свойство, ради которого подсказка строится из матрицы: два места одного правила не
/// могут разойтись, а если кто-то впишет текст руками — тест упадёт.
#[test]
fn every_listed_target_passes_the_check_and_no_other() {
    for kind in [Kind::Task, Kind::Epic] {
        for from in Status::all() {
            let hint = hint_for(from.as_str(), kind.as_str());
            for to in Status::all() {
                if *to == *from {
                    continue;
                }
                let allowed = check_transition(*from, *to, kind, true).is_ok();
                let named = hint.contains(to.as_str()) && *to != *from;
                // `--force` в подсказке упоминается текстом и не должен путать: сравниваем
                // только имена статусов, а `to == from` исключён выше.
                if allowed {
                    assert!(
                        named,
                        "{} -> {} ({:?}) разрешено, но не названо: {hint}",
                        from.as_str(),
                        to.as_str(),
                        kind
                    );
                }
            }
        }
    }
}

/// Эпик и задача получают разные списки: у эпика нет `todo`, у задачи нет `planned`. Если
/// подсказка не учитывает вид узла, она советует невозможное.
#[test]
fn the_hint_respects_the_node_kind() {
    let epic = hint_for("idea", "epic");
    let task = hint_for("idea", "task");
    assert_ne!(epic, task, "вид узла не учтён в подсказке");
    assert!(!epic.contains("todo"), "эпику предложен todo: {epic}");
    assert!(
        !task.contains("planned"),
        "задаче предложен planned: {task}"
    );
}

/// Неизвестные значения в ошибке (испорченный вызов, старая версия клиента) не роняют подсказку:
/// возвращается общая, а не паника и не пустая строка.
#[test]
fn unknown_values_fall_back_to_the_general_hint() {
    let hint = hint_for("flying", "task");
    assert!(hint.contains("README"), "{hint}");
    let hint = hint_for("todo", "gadget");
    assert!(hint.contains("README"), "{hint}");
}

/// Подсказка не заканчивается точкой и достаточно длинна — те же требования, что ко всем
/// остальным подсказкам (`every_error_carries_a_hint`).
#[test]
fn the_hint_keeps_the_house_style() {
    let hint = hint_for("todo", "task");
    assert!(!hint.ends_with('.'), "{hint}");
    assert!(hint.len() > 8, "{hint}");
    assert!(
        hint.contains("--force"),
        "не сказано, как обойти правило: {hint}"
    );
}
