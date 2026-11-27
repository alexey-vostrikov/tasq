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

/// Проверка чисел в README против бинаря.
///
/// README — не справочник, его не сгенерировать целиком, но числа в нём стареют молча: на момент
/// этой проверки он обещал 43 инструмента (в каталоге 45) и старые токены профилей. Правило
/// простое: число либо берётся из бинаря, либо в тексте его нет. Эта функция держит ту половину
/// правила, которую бинарь знает сам — количество инструментов и стоимость профилей.
///
/// Форма строк зафиксирована шаблоном, а не угадыванием: если строку перепишут так, что шаблон
/// перестанет её находить, проверка скажет об этом прямо. Молча пропущенный шаблон — та же ложь,
/// Числа в документации против бинаря.
///
/// Проверяются **все** файлы разом, а не только `README.md`: нормативная часть живёт в
/// `docs/spec.md` (§5–19), вход — в README, и числа встречаются то там, то здесь. Требовать
/// «строка обязана быть в README» значило бы после разделения документов либо продублировать
/// числа (две копии — две правды), либо сломать гейт на ровном месте.
///
/// Поэтому: каждая нужная строка обязана найтись хотя бы в одном файле, а расхождение
/// называется вместе с файлом и номером строки — иначе в каком из двух документов искать, не
/// понять.
pub fn check_facts(
    files: &[std::path::PathBuf],
    tool_count: usize,
    profiles: &[(String, i64)],
    profile_tools: &[(String, usize)],
) -> Vec<String> {
    let texts: Vec<(std::path::PathBuf, String)> = files
        .iter()
        .filter_map(|path| {
            std::fs::read_to_string(path)
                .ok()
                .map(|text| (path.clone(), text))
        })
        .collect();
    if texts.is_empty() {
        let names: Vec<String> = files
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        return vec![format!("документация не найдена: {}", names.join(", "))];
    }

    let mut drift = Vec::new();

    // «| `docs/mcp-tools.md` | каталог `tools/list` | 47 инструментов, ...»
    let mut tools_seen = false;
    for (path, text) in &texts {
        for (index, line) in text.lines().enumerate() {
            let Some(rest) = line.split("каталог `tools/list` | ").nth(1) else {
                continue;
            };
            tools_seen = true;
            let documented: usize = rest
                .split_whitespace()
                .next()
                .and_then(|word| word.parse().ok())
                .unwrap_or_else(|| {
                    drift.push(format!(
                        "{}:{}: числа инструментов не разобрать",
                        path.display(),
                        index + 1
                    ));
                    0
                });
            if documented != tool_count {
                drift.push(format!(
                    "{}:{}: в тексте {documented} инструментов, в каталоге {tool_count}",
                    path.display(),
                    index + 1
                ));
            }
        }
    }
    if !tools_seen {
        drift.push("строка «каталог `tools/list` | N инструментов» не найдена".to_string());
    }

    // «профили: full 5785 / agent 3884 / read-only 1926 / migration 603 токенов»
    let mut tokens_seen = false;
    for (path, text) in &texts {
        for (index, line) in text.lines().enumerate() {
            let Some(rest) = line.split("профили: ").nth(1) else {
                continue;
            };
            tokens_seen = true;
            for (name, actual) in profiles {
                // Профиль ищется по имени и числу после него: «read-only 1926» — имя с дефисом,
                // поэтому поиск по границе, а не по началу слова.
                let Some(position) = rest.find(&format!("{name} ")) else {
                    drift.push(format!(
                        "{}:{}: в строке профилей нет «{name}»",
                        path.display(),
                        index + 1
                    ));
                    continue;
                };
                let documented: i64 = rest[position + name.len() + 1..]
                    .split_whitespace()
                    .next()
                    .and_then(|word| word.parse().ok())
                    .unwrap_or_else(|| {
                        drift.push(format!(
                            "{}:{}: стоимость «{name}» не разобрать",
                            path.display(),
                            index + 1
                        ));
                        0
                    });
                if documented != *actual {
                    drift.push(format!(
                        "{}:{}: в тексте {name} {documented} токенов, в каталоге {actual}",
                        path.display(),
                        index + 1
                    ));
                }
            }
        }
    }
    if !tokens_seen {
        drift.push("строка «профили: …» не найдена".to_string());
    }

    // Таблица профилей живёт в одном файле из набора (сейчас — в `docs/spec.md`), и требовать её
    // от каждого значило бы запретить разделение документов. Ищем, где она есть, и проверяем там;
    // отсутствие во всех файлах — отдельная жалоба, чтобы таблица не исчезла молча.
    let mut table_seen = false;
    for (path, text) in &texts {
        if !text.contains("| профиль | инструментов |") {
            continue;
        }
        table_seen = true;
        drift.extend(check_profile_table(path, text, profiles, profile_tools));
    }
    if !table_seen {
        drift.push("таблица профилей «| профиль | инструментов |» не найдена".to_string());
    }
    drift
}

/// Одна строка таблицы профилей из README: `| `full` | 45 | ≤ 6 000 | 5 428 |`.
///
/// Нужна, чтобы таблица §13.2 не старела молча: числа инструментов и токенов там такие же
/// факты, как и в строке «каталог `tools/list`», и проверяются тем же прогоном.
fn check_profile_table(
    readme: &Path,
    text: &str,
    profiles: &[(String, i64)],
    profile_tools: &[(String, usize)],
) -> Vec<String> {
    let mut drift = Vec::new();
    for (name, tokens) in profiles {
        let Some((index, line)) = text
            .lines()
            .enumerate()
            .find(|(_, line)| line.starts_with(&format!("| `{name}` |")))
        else {
            drift.push(format!(
                "{}: нет строки таблицы профилей для «{name}»",
                readme.display()
            ));
            continue;
        };
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cells.len() < 4 {
            drift.push(format!(
                "{}:{}: строка профиля «{name}» короче таблицы",
                readme.display(),
                index + 1
            ));
            continue;
        }
        // Ячейки: имя, инструментов, бюджет-цель, факт токенов. Пробелы-разделители тысяч
        // пишутся для человека, поэтому числа разбираются без них.
        let documented_tools: usize = cells[1].parse().unwrap_or_else(|_| {
            drift.push(format!(
                "{}:{}: число инструментов у «{name}» не разобрать",
                readme.display(),
                index + 1
            ));
            0
        });
        let documented_tokens: i64 = cells[cells.len() - 1]
            .replace([' ', '\u{202f}'], "")
            .parse()
            .unwrap_or_else(|_| {
                drift.push(format!(
                    "{}:{}: стоимость «{name}» не разобрать",
                    readme.display(),
                    index + 1
                ));
                0
            });
        if let Some((_, actual)) = profile_tools.iter().find(|(key, _)| key == name)
            && documented_tools != *actual
        {
            drift.push(format!(
                "{}:{}: README обещает {name} {documented_tools} инструментов, в профиле {actual}",
                readme.display(),
                index + 1
            ));
        }
        if documented_tokens != *tokens {
            drift.push(format!(
                "{}:{}: README обещает {name} {documented_tokens} токенов, в каталоге {tokens}",
                readme.display(),
                index + 1
            ));
        }
    }
    drift
}

/// Факты, которые README обязан повторять без ошибок: количество инструментов и стоимость
/// профилей — те же числа, что печатает `tasq tokens`, поэтому берутся они из одного места.
pub fn readme_facts() -> (usize, Vec<(String, i64)>) {
    let profiles = budget::measure_all()
        .into_iter()
        .map(|report| (report.profile, report.tokens as i64))
        .collect();
    (ALL_TOOLS.len(), profiles)
}

/// Сколько инструментов в каждом профиле — для проверки таблицы §13.2.
pub fn profile_sizes() -> Vec<(String, usize)> {
    budget::measure_all()
        .into_iter()
        .map(|report| (report.profile, report.tools))
        .collect()
}
