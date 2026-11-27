//! Сторож между описанием CLI и объявлением операций (TASQDEV-20.1).
//!
//! Схема MCP и `docs/mcp-tools.md` теперь строятся из `src/surface.rs`, а команды CLI живут в
//! clap. Две поверхности остаются двумя — но разойтись молча они больше не могут: здесь
//! сравниваются ФАКТИЧЕСКОЕ дерево clap и таблица.
//!
//! Что именно ловится. `tags` и `metadata` появились сначала в CLI, потом в MCP, и сутки две
//! поверхности обещали разное. Поэтому проверяются обе стороны:
//!
//! - каждая команда CLI обязана быть названа операцией или попасть в `CLI_ONLY` с причиной —
//!   иначе новая команда существует, а агент о ней не знает;
//! - каждый флаг команды обязан быть аргументом операции, кроме глобальных и перечисленных в
//!   `CLI_ONLY_ARGUMENT` с причиной — иначе новая возможность есть только у человека;
//! - каждый аргумент операции обязан существовать в CLI (или быть названным как MCP-only) —
//!   иначе схема обещает агенту то, чего команда не принимает.
//!
//! Проверка идёт по дереву clap, а не по тексту `--help`: имена аргументов там те же, что
//! разбирает код, поэтому подменить одно другим нельзя.
//!
//! Этот модуль — часть `main.rs`, а не `lib.rs`, потому что `Cli` объявлен здесь: `lib.rs` о
//! командах не знает, и вынести проверку в него значило бы завести в библиотеке второе описание
//! тех же команд.

use std::collections::{BTreeMap, BTreeSet};

use clap::CommandFactory;

use super::Cli;

use tasq::surface::{CLI_ONLY, NOT_AN_ARGUMENT, OPERATIONS};

/// Одна листовая команда CLI: путь, флаги и позиционные аргументы.
struct Leaf {
    path: String,
    args: BTreeSet<String>,
}

/// Обходит дерево clap и собирает листья.
fn leaves(cmd: &clap::Command, prefix: &str, out: &mut Vec<Leaf>) {
    let subs: Vec<&clap::Command> = cmd.get_subcommands().collect();
    if subs.is_empty() {
        let mut args = BTreeSet::new();
        for arg in cmd.get_arguments() {
            // Глобальные флаги (`--json`, `--db`, `--project`, `--flush`) унаследованы всеми
            // командами и аргументами операции не являются.
            if arg.is_global_set() || NOT_AN_ARGUMENT.contains(&arg.get_id().as_str()) {
                continue;
            }
            // Имя берётся с провода — то, что человек пишет после `--`, а не имя поля в коде:
            // `#[arg(long = "include-archived")] archived: bool` для пользователя и для MCP —
            // это `include_archived`, и сравнивать надо именно его. Позиционные аргументы
            // длинного имени не имеют, у них остаётся id.
            match arg.get_long() {
                Some(long) => args.insert(long.replace('-', "_")),
                None => args.insert(arg.get_id().to_string()),
            };
        }
        out.push(Leaf {
            path: prefix.trim().to_string(),
            args,
        });
        return;
    }
    for sub in subs {
        let path = format!("{prefix} {}", sub.get_name());
        leaves(sub, &path, out);
    }
}

/// Дерево CLI как есть.
fn cli_leaves() -> Vec<Leaf> {
    let command = Cli::command();
    let mut out = Vec::new();
    leaves(&command, "", &mut out);
    out
}

/// Аргументы операции, переведённые в имена CLI.
///
/// Имя в MCP и имя флага совпадают далеко не всегда (`description` против `--desc`), и это
/// нормально: у агента аргумент называет смысл, у человека флаг — действие. Расхождение
/// разрешается таблицей ниже, а не догадкой, поэтому новое расхождение не проедет тихо.
fn cli_names(op: &tasq::surface::Operation) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for arg in op.args {
        if arg.name == "project" {
            continue;
        }
        match arg.cli {
            tasq::surface::CliName::Same => {
                out.insert(arg.name.to_string());
            }
            tasq::surface::CliName::Renamed(name) => {
                out.insert(name.to_string());
            }
            tasq::surface::CliName::None => {}
        }
    }
    for (flag, _) in op.cli_only {
        out.insert((*flag).to_string());
    }
    out
}

/// Каждая команда CLI названа операцией или в списке «только CLI» с причиной.
#[test]
fn every_cli_command_is_declared() {
    let mut undeclared = Vec::new();
    for leaf in cli_leaves() {
        let covered = tasq::surface::by_cli(&leaf.path).is_some()
            || CLI_ONLY.iter().any(|(path, _)| *path == leaf.path);
        if !covered {
            undeclared.push(leaf.path);
        }
    }
    assert!(
        undeclared.is_empty(),
        "команды CLI без объявления: {undeclared:?}\n\
         либо опишите операцию в src/surface.rs (поле cli), либо добавьте путь в CLI_ONLY с причиной"
    );
}

/// Каждая операция с командой CLI указывает на существующую команду.
#[test]
fn every_declared_cli_path_exists() {
    let paths: BTreeSet<String> = cli_leaves().into_iter().map(|leaf| leaf.path).collect();
    let mut broken = Vec::new();
    for op in OPERATIONS {
        if let Some(path) = op.cli
            && !paths.contains(path)
        {
            broken.push(format!("{} -> {path}", op.tool));
        }
    }
    assert!(
        broken.is_empty(),
        "объявление ссылается на несуществующие команды CLI: {broken:?}"
    );
}

/// Флаги CLI и аргументы операции описывают одно и то же.
#[test]
fn cli_flags_and_operation_arguments_agree() {
    let mut problems = Vec::new();
    for leaf in cli_leaves() {
        let Some(op) = tasq::surface::by_cli(&leaf.path) else {
            continue;
        };
        let declared = cli_names(op);
        // Флаг есть у человека, но не объявлен агенту.
        for flag in leaf.args.difference(&declared) {
            problems.push(format!(
                "{}: флаг --{} есть в CLI, но не объявлен у {}",
                leaf.path, flag, op.tool
            ));
        }
        // Аргумент обещан агенту, но команда его не принимает.
        for arg in declared.difference(&leaf.args) {
            problems.push(format!(
                "{}: аргумент {} объявлен у {}, но CLI его не принимает",
                leaf.path, arg, op.tool
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// У каждой операции уникальное имя инструмента и непустое описание.
#[test]
fn the_table_is_well_formed() {
    let mut seen = BTreeMap::new();
    for op in OPERATIONS {
        if let Some(previous) = seen.insert(op.tool, op.cli) {
            panic!(
                "инструмент {} объявлен дважды (cli: {:?} и {:?})",
                op.tool, previous, op.cli
            );
        }
        assert!(!op.summary.is_empty(), "{}: пустое описание", op.tool);
        let mut names = BTreeSet::new();
        for arg in op.args {
            assert!(
                names.insert(arg.name),
                "{}: аргумент {} объявлен дважды",
                op.tool,
                arg.name
            );
        }
    }
    // Каждая команда «только CLI» названа один раз и с причиной.
    let mut paths = BTreeSet::new();
    for (path, why) in CLI_ONLY {
        assert!(paths.insert(*path), "путь {path} повторён в CLI_ONLY");
        assert!(!why.is_empty(), "{path}: причина не названа");
    }
}
