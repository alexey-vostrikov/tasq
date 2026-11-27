//! Снимок каталога MCP в JSON: имя, описание, схема аргументов, аннотации.
//!
//! Нужен один раз — чтобы перенести рукописные схемы в объявление (`src/surface.rs`) без
//! опечаток: таблица генерируется из этого снимка, а тест сверяет сгенерированное с ним же.
//! Дальше снимок остаётся сторожем формы: любое изменение схемы на проводе обязано быть
//! замечено, а не проехать тихо.
use tasq::mcp::profile::Profile;
use tasq::mcp::tools;

fn main() {
    let mut out = serde_json::Map::new();
    for profile in [Profile::Full] {
        for name in profile.tools() {
            let Some(definition) = tools::definition(name) else {
                eprintln!("нет описания для {name}");
                std::process::exit(1);
            };
            out.insert((*name).to_string(), definition);
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::Value::Object(out)).expect("json")
    );
}
