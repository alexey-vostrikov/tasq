//! Пересборка снимка схемы MCP: `cargo run --example dump_schema`.
//!
//! Снимок `tests/fixtures/mcp-schema.json` — контракт, который видят модели, и правится он не
//! рукой: ручная правка полусотни инструментов означает, что однажды кто-то поправит `required` в
//! одном месте и забудет в другом, а тест этого не заметит — он сравнивает файл с тем, что вышло
//! из генератора. Здесь тот же код, что у сервера, и порядок ключей стабильный, чтобы дифф показывал
//! изменение, а не перестановку.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use tasq::mcp::profile::ALL_TOOLS;
use tasq::mcp::tools;

fn main() {
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    for name in ALL_TOOLS {
        let mut definition = tools::definition(name)
            .unwrap_or_else(|| panic!("инструмент {name} объявлен в каталоге, но схемы нет"));
        // `required` — множество: сортируем, иначе порядок из объявления попадал бы в дифф.
        if let Some(required) = definition
            .get_mut("inputSchema")
            .and_then(Value::as_object_mut)
            .and_then(|schema| schema.get_mut("required"))
            .and_then(Value::as_array_mut)
        {
            required.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
        }
        out.insert((*name).to_string(), definition);
    }
    let value = Value::Object(Map::from_iter(out));
    let text = serde_json::to_string_pretty(&value).expect("сериализация");
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp-schema.json");
    std::fs::write(&path, text + "\n").expect("запись снимка");
    println!("снимок схемы обновлён: {} инструмент(ов)", ALL_TOOLS.len());
}
