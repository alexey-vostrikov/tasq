//! Профили инструментов: что видно в `tools/list` (README §13.1).
//!
//! Профиль сужает каталог, а не права базы: скрытый инструмент нельзя вызвать и через
//! `tools/call` — иначе профиль был бы только косметикой.

/// Профиль MCP-сервера.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// Всё, включая миграцию и удаление.
    Full,
    /// Повседневная работа агента: без delete/restore/move и без переноса данных.
    Agent,
    /// Только чтение: ни одна мутирующая команда не видна.
    ReadOnly,
    /// Перенос данных: импорт и экспорт плюс минимум для просмотра.
    Migration,
}

impl Profile {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "full" => Some(Profile::Full),
            "agent" => Some(Profile::Agent),
            "read-only" | "readonly" => Some(Profile::ReadOnly),
            "migration" => Some(Profile::Migration),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Full => "full",
            Profile::Agent => "agent",
            Profile::ReadOnly => "read-only",
            Profile::Migration => "migration",
        }
    }

    /// Целевой бюджет схем в токенах (README §13.2).
    pub fn budget(self) -> usize {
        match self {
            Profile::Full => 6_000,
            Profile::Agent => 4_000,
            Profile::ReadOnly => 2_000,
            Profile::Migration => 800,
        }
    }

    /// Имена инструментов, которые профиль показывает.
    ///
    /// Список задан явно, а не фильтром «по флагу read-only»: так состав профиля виден
    /// в одном месте и не меняется незаметно при добавлении нового инструмента.
    pub fn tools(self) -> &'static [&'static str] {
        match self {
            Profile::Full => ALL_TOOLS,
            Profile::Agent => &[
                "tasq_init",
                "project_list",
                "project_update",
                "node_create",
                "node_get",
                "node_update",
                "node_list",
                "node_archive",
                "tasq_context",
                "tasq_next",
                "tasq_claim",
                "tasq_renew",
                "tasq_release",
                "tasq_reserve",
                "tasq_holdings",
                "tasq_link",
                "tasq_unlink",
                "tasq_graph",
                "comment_add",
                "comment_list",
                "note_save",
                "note_list",
                "note_search",
                "activity_log",
                "session_diff",
                "tasq_search",
                "tasq_dashboard",
                "tasq_burnup",
            ],
            Profile::ReadOnly => &[
                "project_list",
                "node_get",
                "node_list",
                "tasq_context",
                "tasq_next",
                "tasq_holdings",
                "tasq_graph",
                "comment_list",
                "note_list",
                "note_search",
                "activity_log",
                "session_diff",
                "tasq_search",
                "tasq_dashboard",
                "tasq_burnup",
                "tasq_risk",
            ],
            Profile::Migration => &[
                "tasq_init",
                "project_list",
                "project_create",
                "node_get",
                "tasq_import",
                "tasq_export",
            ],
        }
    }

    /// Показывает ли профиль инструмент.
    pub fn allows(self, name: &str) -> bool {
        self.tools().contains(&name)
    }
}

/// Полный каталог v1: 42 инструмента из ТЗ плюс `tasq_risk` из M6 — всего 43 (README §13.2).
pub const ALL_TOOLS: &[&str] = &[
    // Инициализация и проекты (4)
    "tasq_init",
    "project_create",
    "project_list",
    "project_update",
    // Узлы (9)
    "node_create",
    "node_get",
    "node_update",
    "node_list",
    "node_move",
    "node_reorder",
    "node_archive",
    "node_delete",
    "node_restore",
    // Дерево и рекомендации (2)
    "tasq_context",
    "tasq_next",
    // Аренды (3)
    "tasq_claim",
    "tasq_renew",
    "tasq_release",
    // Ресурсы (2)
    "tasq_reserve",
    "tasq_holdings",
    // Зависимости (3)
    "tasq_link",
    "tasq_unlink",
    "tasq_graph",
    // Комментарии (3)
    "comment_add",
    "comment_list",
    "comment_delete",
    // Заметки (4)
    "note_save",
    "note_list",
    "note_search",
    "note_delete",
    // Шаблоны (4)
    "template_save",
    "template_list",
    "template_apply",
    "template_delete",
    // Активность (3)
    "activity_log",
    "session_diff",
    "node_batch_update",
    // Поиск и обзоры (4)
    "tasq_search",
    "tasq_dashboard",
    "tasq_burnup",
    "tasq_risk",
    // Перенос данных (2)
    "tasq_export",
    "tasq_import",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_profile_has_exactly_the_declared_catalogue() {
        // 42 из ТЗ плюс tasq_risk, добавленный в M6: 4+9+2+3+2+3+3+4+4+3+4+2 = 43.
        assert_eq!(ALL_TOOLS.len(), 43, "каталог v1 — 43 инструмента");
        let mut unique: Vec<&str> = ALL_TOOLS.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ALL_TOOLS.len(), "имена не повторяются");
        assert_eq!(Profile::Full.tools().len(), 43);
    }

    #[test]
    fn every_profile_is_a_subset_of_full() {
        for profile in [Profile::Agent, Profile::ReadOnly, Profile::Migration] {
            for name in profile.tools() {
                assert!(
                    ALL_TOOLS.contains(name),
                    "профиль {} ссылается на неизвестный инструмент {name}",
                    profile.as_str()
                );
            }
        }
    }

    #[test]
    fn read_only_has_no_mutating_tools() {
        for forbidden in [
            "node_create",
            "node_update",
            "node_delete",
            "tasq_claim",
            "tasq_release",
            "tasq_import",
        ] {
            assert!(
                !Profile::ReadOnly.allows(forbidden),
                "read-only не должен показывать {forbidden}"
            );
        }
    }

    #[test]
    fn agent_cannot_delete_or_move_data() {
        for forbidden in ["node_delete", "node_restore", "node_move", "tasq_import"] {
            assert!(!Profile::Agent.allows(forbidden), "agent: {forbidden}");
        }
        assert!(Profile::Agent.allows("tasq_claim"));
    }

    #[test]
    fn migration_profile_is_about_data_movement() {
        assert!(Profile::Migration.allows("tasq_import"));
        assert!(Profile::Migration.allows("tasq_export"));
        assert!(!Profile::Migration.allows("tasq_claim"));
        assert_eq!(Profile::Migration.tools().len(), 6);
    }

    #[test]
    fn parse_accepts_both_spellings_of_read_only() {
        assert_eq!(Profile::parse("read-only"), Some(Profile::ReadOnly));
        assert_eq!(Profile::parse("readonly"), Some(Profile::ReadOnly));
        assert_eq!(Profile::parse("nonsense"), None);
    }
}
