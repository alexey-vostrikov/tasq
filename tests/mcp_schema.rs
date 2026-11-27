//! Схема MCP на проводе — против золотого снимка.
//!
//! Зачем снимок, если схема теперь генерируется из одного объявления. Затем, что «одна копия»
//! защищает от расхождения между поверхностями, но не от случайного изменения самой поверхности:
//! переименованный аргумент или суженное перечисление ломает клиента молча, а таблица в этом
//! случае выглядит совершенно нормально. Снимок — это контракт, который видят модели, и его
//! изменение обязано быть замечено в диффе, а не выясниться у потребителя.
//!
//! Порядок такой же, как у любого снимка: изменил схему намеренно — обнови файл и объясни в
//! коммите, что именно поменялось и почему.

use std::collections::BTreeSet;

use serde_json::Value;
use tasq::mcp::profile::{ALL_TOOLS, Profile};
use tasq::mcp::tools;
use tasq::surface;

fn golden() -> Value {
    let text = include_str!("fixtures/mcp-schema.json");
    serde_json::from_str(text).expect("снимок схемы читается как JSON")
}

/// Нормализация: `required` — множество, порядок в нём не значит ничего.
fn normalise(mut value: Value) -> Value {
    for tool in value
        .as_object_mut()
        .into_iter()
        .flat_map(|map| map.values_mut())
    {
        // `get_mut`, а не индексирование: индексация отсутствующего ключа вставляет `null`, и
        // нормализация сама стала бы источником расхождения.
        if let Some(required) = tool
            .get_mut("inputSchema")
            .and_then(Value::as_object_mut)
            .and_then(|schema| schema.get_mut("required"))
            .and_then(Value::as_array_mut)
        {
            required.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
        }
    }
    value
}

/// Сегодняшняя схема — та же, что отдаёт `tools/list`.
fn actual() -> Value {
    let mut out = serde_json::Map::new();
    for name in ALL_TOOLS {
        let definition = tools::definition(name)
            .unwrap_or_else(|| panic!("инструмент {name} объявлен в каталоге, но схемы нет"));
        out.insert((*name).to_string(), definition);
    }
    normalise(Value::Object(out))
}

/// Схема на проводе совпадает со снимком, инструмент за инструментом.
///
/// Сравнение идёт по каждому инструменту отдельно, а не «равно/не равно»: расхождение должно
/// называть имя, иначе в диффе из полусотни инструментов виновник не виден.
#[test]
fn the_wire_schema_matches_the_snapshot() {
    let golden = golden();
    let actual = actual();
    let golden_tools: BTreeSet<&String> = golden
        .as_object()
        .expect("снимок — объект")
        .keys()
        .collect();
    let actual_tools: BTreeSet<&String> =
        actual.as_object().expect("схема — объект").keys().collect();
    assert_eq!(
        golden_tools, actual_tools,
        "набор инструментов разошёлся со снимком: обновите tests/fixtures/mcp-schema.json \
         и объясните изменение"
    );
    for name in golden_tools {
        assert_eq!(
            golden[name], actual[name],
            "{name}: схема на проводе разошлась со снимком"
        );
    }
}

/// У каждого объявленного инструмента есть описание, аргументы которого понимает разбор.
///
/// Проверка ловит то, что уже случалось: инструмент объявлен в каталоге, а схемы у него нет.
#[test]
fn every_tool_has_a_definition_and_a_matching_operation() {
    for name in ALL_TOOLS {
        assert!(
            tools::definition(name).is_some(),
            "{name}: каталог обещает инструмент, а схемы нет"
        );
        assert!(
            surface::operation(name).is_some(),
            "{name}: схемы нет в объявлении"
        );
    }
    // И наоборот: объявленное обязано быть в каталоге, иначе объявление врёт про себя.
    for op in surface::OPERATIONS {
        assert!(
            ALL_TOOLS.contains(&op.tool),
            "{}: объявлен, но в каталог не попал",
            op.tool
        );
    }
}

/// Профили — подмножества `full`, и объявленных инструментов хватает каждому.
///
/// Отдельная проверка на объявление, а не только на каталог: профиль называет имена руками, и
/// переименование в таблице иначе отвалилось бы только у клиента.
#[test]
fn every_profile_tool_is_declared() {
    let declared: BTreeSet<&str> = surface::OPERATIONS.iter().map(|op| op.tool).collect();
    for profile in [
        Profile::Full,
        Profile::Agent,
        Profile::ReadOnly,
        Profile::Migration,
    ] {
        for name in profile.tools() {
            assert!(
                declared.contains(name),
                "{}: профиль {} обещает инструмент, которого нет в объявлении",
                name,
                profile.as_str()
            );
        }
    }
}
