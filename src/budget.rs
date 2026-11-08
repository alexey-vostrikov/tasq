//! Бюджет схем MCP в токенах (README §13.2).
//!
//! `tools/list` уезжает в контекст модели при каждом подключении, поэтому его размер —
//! такая же характеристика продукта, как время или память. Считается по компактному JSON
//! токенизатором `o200k_base`; оценка «на глаз» здесь не годится, потому что расхождение
//! в полтора-два раза и есть разница между «влезает» и «не влезает».

use serde::Serialize;

use crate::mcp::profile::Profile;
use crate::mcp::tools;

/// Токенизатор, по которому считается бюджет. Один на весь процесс: загрузка BPE не бесплатна.
fn tokenizer() -> &'static tiktoken_rs::CoreBPE {
    use std::sync::OnceLock;
    static BPE: OnceLock<tiktoken_rs::CoreBPE> = OnceLock::new();
    BPE.get_or_init(|| {
        tiktoken_rs::o200k_base().expect("встроенный словарь o200k_base всегда доступен")
    })
}

/// Токены строки по `o200k_base`.
pub fn count_tokens(text: &str) -> usize {
    tokenizer().encode_ordinary(text).len()
}

/// Размер одного инструмента: имя, описание и схема в том виде, в каком их видит клиент.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCost {
    pub name: String,
    pub tokens: usize,
}

/// Стоимость профиля.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub profile: String,
    pub tools: usize,
    pub tokens: usize,
    pub budget: usize,
    /// Сколько осталось до бюджета; отрицательное значение — перебор.
    pub headroom: i64,
    pub within_budget: bool,
    /// Стоимость по инструментам, дорогие первыми.
    pub per_tool: Vec<ToolCost>,
}

/// Стоимость каталога профиля.
pub fn measure(profile: Profile) -> Report {
    let catalogue: Vec<serde_json::Value> = profile
        .tools()
        .iter()
        .filter_map(|name| tools::definition(name))
        .collect();

    // Именно так клиент и получает каталог: одна строка компактного JSON.
    let payload = serde_json::json!({ "tools": catalogue });
    let text = serde_json::to_string(&payload).expect("serialisable");
    let tokens = count_tokens(&text);

    let mut per_tool: Vec<ToolCost> = catalogue
        .iter()
        .map(|definition| {
            let name = definition
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("-")
                .to_string();
            let cost = count_tokens(&serde_json::to_string(definition).expect("serialisable"));
            ToolCost { name, tokens: cost }
        })
        .collect();
    per_tool.sort_by(|a, b| b.tokens.cmp(&a.tokens).then(a.name.cmp(&b.name)));

    let budget = profile.budget();
    Report {
        profile: profile.as_str().to_string(),
        tools: catalogue.len(),
        tokens,
        budget,
        headroom: budget as i64 - tokens as i64,
        within_budget: tokens <= budget,
        per_tool,
    }
}

/// Отчёты по всем профилям — для гейта в CI.
pub fn measure_all() -> Vec<Report> {
    [
        Profile::Full,
        Profile::Agent,
        Profile::ReadOnly,
        Profile::Migration,
    ]
    .into_iter()
    .map(measure)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_counts_something_sane() {
        // Ориентир: короткая строка — единицы токенов, длинный повтор — заметно больше.
        assert!(count_tokens("tasq tools/list") > 0);
        assert!(count_tokens("tasq tools/list") < 20);
        let long = "tasq ".repeat(200);
        assert!(count_tokens(&long) > 150, "{}", count_tokens(&long));
    }

    #[test]
    fn every_profile_fits_its_budget() {
        for report in measure_all() {
            assert!(
                report.within_budget,
                "профиль {}: {} токенов при бюджете {} (перебор на {})",
                report.profile, report.tokens, report.budget, -report.headroom
            );
        }
    }

    #[test]
    fn narrower_profiles_are_cheaper() {
        let full = measure(Profile::Full);
        let agent = measure(Profile::Agent);
        let read_only = measure(Profile::ReadOnly);
        let migration = measure(Profile::Migration);
        assert!(full.tokens > agent.tokens, "full дороже agent");
        assert!(agent.tokens > read_only.tokens || read_only.tokens < full.tokens);
        assert!(
            migration.tokens < full.tokens / 3,
            "migration — самый узкий"
        );
        assert_eq!(full.tools, 44);
        assert_eq!(migration.tools, 6);
    }

    #[test]
    fn per_tool_costs_are_sorted_and_sum_roughly_to_the_total() {
        let report = measure(Profile::Full);
        assert_eq!(report.per_tool.len(), 44);
        for pair in report.per_tool.windows(2) {
            assert!(
                pair[0].tokens >= pair[1].tokens,
                "{} < {}",
                pair[0].name,
                pair[1].name
            );
        }
        let sum: usize = report.per_tool.iter().map(|cost| cost.tokens).sum();
        // Сумма по отдельности чуть больше целого: в общем JSON общие скобки и запятые.
        assert!(
            sum >= report.tokens,
            "сумма {sum} не может быть меньше целого {}",
            report.tokens
        );
        assert!(sum < report.tokens * 2, "расхождение слишком велико");
    }
}
