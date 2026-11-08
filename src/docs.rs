//! `tasq docs` — документация, снятая с самого бинаря.
//!
//! Руками написанный справочник команд расходится с кодом на первой же правке флага.
//! Здесь `docs/cli.md` собирается из описания CLI (clap), `docs/mcp-tools.md` — из каталога
//! инструментов с их стоимостью в токенах, `docs/schema.md` — из свежей базы, то есть из
//! фактических `CREATE TABLE`/`CREATE VIEW`. Гейт `--check` сравнивает файлы на диске
//! с тем, что получилось бы сейчас, и падает при расхождении.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::budget;
use crate::error::{Error, Result};
use crate::mcp::profile::{ALL_TOOLS, Profile};
use crate::mcp::tools;

/// Один сгенерированный файл.
pub struct Generated {
    pub name: String,
    pub body: String,
}

/// Собирает все файлы документации.
pub fn generate(cli: &clap::Command) -> Result<Vec<Generated>> {
    Ok(vec![
        Generated {
            name: "cli.md".to_string(),
            body: render_cli(cli),
        },
        Generated {
            name: "mcp-tools.md".to_string(),
            body: render_tools(),
        },
        Generated {
            name: "schema.md".to_string(),
            body: render_schema()?,
        },
    ])
}

fn escape(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

/// Справочник CLI: каждая подкоманда, её аргументы и флаги.
fn render_cli(cli: &clap::Command) -> String {
    let mut out = String::new();
    out.push_str("# Справочник CLI\n\n");
    out.push_str("Файл сгенерирован командой `tasq docs`; править его руками бессмысленно — ");
    out.push_str("гейт `tasq docs --check` в CI падает, если содержимое разошлось с бинарём.\n\n");
    out.push_str(&format!(
        "```\n{} {}\n{}\n```\n\n",
        cli.get_name(),
        cli.get_version().unwrap_or(""),
        cli.get_about()
            .map(|about| about.to_string())
            .unwrap_or_default()
    ));

    out.push_str("## Глобальные флаги\n\n");
    out.push_str("| флаг | значение | по умолчанию | описание |\n");
    out.push_str("| --- | --- | --- | --- |\n");
    for argument in cli
        .get_arguments()
        .filter(|argument| argument.is_global_set())
    {
        out.push_str(&format_argument_row(argument));
    }
    out.push('\n');

    out.push_str("\n## Команды\n\n");
    render_commands(cli, &mut out, 2);
    out
}

fn render_commands(parent: &clap::Command, out: &mut String, level: usize) {
    let hashes = "#".repeat(level);
    for subcommand in parent.get_subcommands() {
        let name = subcommand.get_name();
        // Служебная команда автодополнения не документируется: её нет в интерфейсе.
        if name == "help" {
            continue;
        }
        out.push_str(&format!("{hashes} `{name}`\n\n"));
        if let Some(about) = subcommand.get_about() {
            out.push_str(&format!("{about}\n\n"));
        }
        if let Some(long) = subcommand.get_long_about() {
            out.push_str(&format!("{long}\n\n"));
        }
        out.push_str(&format!("```\ntasq {name} [ОПЦИИ]\n```\n\n"));

        let arguments: Vec<&clap::Arg> = subcommand
            .get_arguments()
            .filter(|argument| !argument.is_global_set())
            .collect();
        let options: Vec<&&clap::Arg> = arguments
            .iter()
            .filter(|argument| argument.get_long().is_some() || argument.get_short().is_some())
            .collect();
        if !options.is_empty() {
            out.push_str("| флаг | значение | по умолчанию | описание |\n");
            out.push_str("| --- | --- | --- | --- |\n");
            for argument in options {
                out.push_str(&format_argument_row(argument));
            }
            out.push('\n');
        }

        if subcommand.has_subcommands() {
            render_commands(subcommand, out, level + 1);
        }
    }
}

fn format_argument_row(argument: &clap::Arg) -> String {
    let flag = match (argument.get_short(), argument.get_long()) {
        (Some(short), Some(long)) => format!("`-{short}`, `--{long}`"),
        (None, Some(long)) => format!("`--{long}`"),
        (Some(short), None) => format!("`-{short}`"),
        (None, None) => format!("`{}`", argument.get_id()),
    };
    let value = if argument.get_action().takes_values() {
        format!(
            "`{}`",
            argument
                .get_value_names()
                .and_then(|names| names.first())
                .map(|name| name.to_string())
                .unwrap_or_else(|| "VALUE".to_string())
        )
    } else {
        "—".to_string()
    };
    let default = argument
        .get_default_values()
        .first()
        .map(|value| format!("`{}`", value.to_string_lossy()))
        .unwrap_or_else(|| "—".to_string());
    let help = argument
        .get_help()
        .map(|help| escape(&help.to_string()))
        .unwrap_or_default();
    format!("| {flag} | {value} | {default} | {help} |\n")
}

/// Каталог инструментов MCP: имя, профили, стоимость в токенах, обязательные аргументы.
fn render_tools() -> String {
    let mut out = String::new();
    out.push_str("# Инструменты MCP\n\n");
    out.push_str("Файл сгенерирован командой `tasq docs` из каталога самого сервера: ");
    out.push_str(
        "имя, описание и схема берутся из того же кода, что отвечает на `tools/list`.\n\n",
    );
    out.push_str(&format!("Всего инструментов: {}.\n\n", ALL_TOOLS.len()));

    out.push_str("## Профили\n\n");
    out.push_str("| профиль | инструментов | бюджет, токенов | факт |\n");
    out.push_str("| --- | --- | --- | --- |\n");
    for profile in [
        Profile::Full,
        Profile::Agent,
        Profile::ReadOnly,
        Profile::Migration,
    ] {
        let report = budget::measure(profile);
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            profile.as_str(),
            report.tools,
            report.budget,
            report.tokens
        ));
    }
    out.push('\n');

    out.push_str("## Каталог\n\n");
    out.push_str("| инструмент | профили | токенов | обязательные аргументы | описание |\n");
    out.push_str("| --- | --- | --- | --- | --- |\n");
    for name in ALL_TOOLS {
        let Some(definition) = tools::definition(name) else {
            continue;
        };
        let profiles: Vec<&str> = [
            Profile::Full,
            Profile::Agent,
            Profile::ReadOnly,
            Profile::Migration,
        ]
        .into_iter()
        .filter(|profile| profile.allows(name))
        .map(Profile::as_str)
        .collect();
        let required: Vec<String> = definition["inputSchema"]["required"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|item| item.as_str().map(|value| format!("`{value}`")))
                    .collect()
            })
            .unwrap_or_default();
        let tokens = budget::count_tokens(
            &serde_json::to_string(&definition).unwrap_or_else(|_| String::new()),
        );
        let description = definition["description"]
            .as_str()
            .map(escape)
            .unwrap_or_default();
        out.push_str(&format!(
            "| `{name}` | {} | {tokens} | {} | {description} |\n",
            profiles.join(", "),
            if required.is_empty() {
                "—".to_string()
            } else {
                required.join(", ")
            }
        ));
    }
    out
}

/// Схема базы: таблицы и представления из свежей миграции, а не пересказ вслепую.
fn render_schema() -> Result<String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("tasq-docs-{nanos}"));
    std::fs::create_dir_all(&dir)?;
    let outcome = crate::db::init(&dir, "DOCS", "docs")?;
    let conn = crate::db::open(Path::new(&outcome.db_path))?;
    let body = render_schema_from(&conn)?;
    // Временный каталог убирается целиком: документация не должна оставлять мусор.
    let _ = std::fs::remove_dir_all(&dir);
    Ok(body)
}

fn render_schema_from(conn: &Connection) -> Result<String> {
    let mut out = String::new();
    out.push_str("# Схема базы\n\n");
    out.push_str("Файл сгенерирован командой `tasq docs` из пустой базы, созданной текущими ");
    out.push_str(
        "миграциями: это фактические `CREATE TABLE` и `CREATE VIEW`, а не их пересказ.\n\n",
    );

    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    out.push_str(&format!("Версия схемы: **v{version}**.\n\n"));

    for (kind, title) in [
        ("table", "Таблицы"),
        ("view", "Представления"),
        ("index", "Индексы"),
    ] {
        out.push_str(&format!("## {title}\n\n"));
        let mut stmt = conn.prepare(
            "SELECT name, COALESCE(sql, '') FROM sqlite_master
             WHERE type = ?1 AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '%_fts_%'
             ORDER BY name",
        )?;
        let rows = stmt.query_map([kind], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut any = false;
        for row in rows {
            let (name, sql) = row?;
            any = true;
            out.push_str(&format!("### `{name}`\n\n"));
            if sql.trim().is_empty() {
                out.push_str("(создан без SQL-текста: автоматический индекс)\n\n");
            } else {
                out.push_str(&format!("```sql\n{}\n```\n\n", sql.trim()));
            }
        }
        if !any {
            out.push_str("(пусто)\n\n");
        }
    }
    Ok(out)
}

/// Записывает файлы в каталог.
pub fn write(dir: &Path, files: &[Generated]) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let mut written = Vec::new();
    for file in files {
        let path = dir.join(&file.name);
        std::fs::write(&path, &file.body)?;
        written.push(path);
    }
    Ok(written)
}

/// Сравнивает сгенерированное с файлами на диске; возвращает расхождения.
pub fn check(dir: &Path, files: &[Generated]) -> Result<Vec<String>> {
    let mut drift = Vec::new();
    for file in files {
        let path = dir.join(&file.name);
        match std::fs::read_to_string(&path) {
            Ok(existing) if existing == file.body => {}
            Ok(_) => drift.push(format!("{} устарел", path.display())),
            Err(_) => drift.push(format!("{} отсутствует", path.display())),
        }
    }
    Ok(drift)
}

/// Ошибка гейта: документация разошлась с бинарём.
pub fn drift_error(drift: &[String]) -> Error {
    Error::InvalidValue {
        field: "docs".to_string(),
        value: format!("{}; run `tasq docs`", drift.join(", ")),
    }
}
