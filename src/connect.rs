//! Прописка MCP-сервера в конфиги клиентов (README §14).
//!
//! Правила, которые важнее удобства:
//! 1. Файл, который не разбирается как ожидаемый формат, **не перезаписывается**. Клиентские
//!    конфиги содержат комментарии, якоря и чужие ключи; «перепишем как поняли» — это потеря
//!    пользовательских настроек. В таком случае печатается готовый фрагмент, код выхода 3.
//! 2. Запись атомарна: временный файл в том же каталоге плюс `rename`, перед этим — копия
//!    `<файл>.bak-<время>`.
//! 3. Правится только ключ `tasq` (или добавляется). Остальное содержимое сохраняется,
//!    насколько это позволяет формат: JSON пересобирается с сохранением ключей, TOML — `toml_edit`
//!    без переформатирования.
//! 4. Повторный запуск идемпотентен.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::error::{Error, Result};

/// Где искать конфиг.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Пользовательский файл (`~/.config/…`).
    User,
    /// Файл проекта (`.mcp.json`, `.vscode/mcp.json`).
    Project,
}

impl Scope {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Scope::User),
            "project" => Some(Scope::Project),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
        }
    }
}

/// Формат конфига.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// JSON с указанным ключом верхнего уровня.
    Json,
    /// TOML с указанной таблицей.
    Toml,
    /// YAML; правится только печатью фрагмента — якоря и порядок ключей слишком легко потерять.
    Yaml,
}

/// Описание клиента.
pub struct Client {
    pub id: &'static str,
    /// Путь пользовательского конфига; `None` — клиент живёт только в проекте.
    pub user_path: Option<&'static str>,
    /// Путь проектного конфига.
    pub project_path: Option<&'static str>,
    pub format: Format,
    /// Ключ верхнего уровня, где живут серверы.
    pub key: &'static str,
    pub default_scope: Scope,
    /// Файл, который клиент создаёт сам при первом запуске.
    pub created_by_client: bool,
}

/// Каталог поддерживаемых клиентов (README §14).
pub const CLIENTS: &[Client] = &[
    Client {
        id: "claude-code",
        user_path: Some(".claude.json"),
        project_path: Some(".mcp.json"),
        format: Format::Json,
        key: "mcpServers",
        default_scope: Scope::Project,
        created_by_client: false,
    },
    Client {
        id: "claude-desktop",
        user_path: Some(".config/Claude/claude_desktop_config.json"),
        project_path: None,
        format: Format::Json,
        key: "mcpServers",
        default_scope: Scope::User,
        created_by_client: true,
    },
    Client {
        id: "opencode",
        user_path: Some(".config/opencode/opencode.json"),
        project_path: Some("opencode.json"),
        format: Format::Json,
        key: "mcp",
        default_scope: Scope::User,
        created_by_client: false,
    },
    Client {
        id: "zed",
        user_path: Some(".config/zed/settings.json"),
        project_path: None,
        format: Format::Json,
        key: "context_servers",
        default_scope: Scope::User,
        created_by_client: false,
    },
    Client {
        id: "cursor",
        user_path: Some(".cursor/mcp.json"),
        project_path: Some(".cursor/mcp.json"),
        format: Format::Json,
        key: "mcpServers",
        default_scope: Scope::User,
        created_by_client: false,
    },
    Client {
        id: "vscode",
        user_path: Some(".config/Code/User/mcp.json"),
        project_path: Some(".vscode/mcp.json"),
        format: Format::Json,
        key: "servers",
        default_scope: Scope::Project,
        created_by_client: false,
    },
    Client {
        id: "gemini",
        user_path: Some(".gemini/settings.json"),
        project_path: None,
        format: Format::Json,
        key: "mcpServers",
        default_scope: Scope::User,
        created_by_client: false,
    },
    Client {
        id: "codex",
        user_path: Some(".codex/config.toml"),
        project_path: None,
        format: Format::Toml,
        key: "mcp_servers",
        default_scope: Scope::User,
        created_by_client: false,
    },
    Client {
        id: "goose",
        user_path: Some(".config/goose/config.yaml"),
        project_path: None,
        format: Format::Yaml,
        key: "extensions",
        default_scope: Scope::User,
        created_by_client: false,
    },
    Client {
        id: "crush",
        user_path: Some(".config/crush/crush.json"),
        project_path: None,
        format: Format::Json,
        key: "mcp",
        default_scope: Scope::User,
        created_by_client: false,
    },
    Client {
        id: "windsurf",
        user_path: Some(".codeium/windsurf/mcp_config.json"),
        project_path: None,
        format: Format::Json,
        key: "mcpServers",
        default_scope: Scope::User,
        created_by_client: false,
    },
];

/// Клиент по идентификатору.
pub fn client(id: &str) -> Option<&'static Client> {
    CLIENTS.iter().find(|client| client.id == id)
}

/// Как подключаться.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transport {
    /// `command` + `args`: сервер сам поднимается по stdio.
    Stdio { command: String, args: Vec<String> },
    /// Готовый HTTP-адрес (loopback).
    Http { url: String },
}

/// Фрагмент для YAML-конфига goose. Рендерится руками: ради шести полей тянуть
/// YAML-сериализатор в бинарь незачем, а формат здесь фиксированный.
pub fn yaml_snippet(transport: &Transport) -> String {
    match transport {
        Transport::Stdio { command, args } => {
            let mut out = String::from(
                "extensions:\n  tasq:\n    type: stdio\n    name: tasq\n    enabled: true\n",
            );
            out.push_str(&format!("    cmd: {command}\n    args:\n"));
            for arg in args {
                out.push_str(&format!("      - {arg}\n"));
            }
            out
        }
        Transport::Http { url } => format!(
            "extensions:\n  tasq:\n    type: streamable_http\n    name: tasq\n    enabled: true\n    uri: {url}\n"
        ),
    }
}

/// Состояние одного клиента для `--list`.
#[derive(Debug, Clone, Serialize)]
pub struct State {
    pub client: String,
    pub scope: String,
    pub path: String,
    pub file_exists: bool,
    pub configured: bool,
    pub format: String,
}

fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Путь конфига клиента для выбранного охвата.
pub fn path_for(client: &Client, scope: Scope, cwd: &Path) -> Result<PathBuf> {
    let relative = match scope {
        Scope::User => client.user_path,
        Scope::Project => client.project_path,
    };
    let relative = relative.ok_or_else(|| Error::InvalidValue {
        field: "scope".to_string(),
        value: format!("{} has no {} config", client.id, scope.as_str()),
    })?;
    Ok(match scope {
        Scope::User => home().join(relative),
        Scope::Project => cwd.join(relative),
    })
}

/// Фрагмент, который нужно вставить.
pub fn snippet(client: &Client, transport: &Transport) -> Value {
    match transport {
        Transport::Stdio { command, args } => match client.format {
            Format::Toml => serde_json::json!({
                "command": command,
                "args": args,
            }),
            Format::Yaml => serde_json::json!({
                "type": "stdio",
                "name": "tasq",
                "enabled": true,
                "cmd": command,
                "args": args,
            }),
            Format::Json => {
                // Zed и opencode ждут другой набор полей, чем claude/vscode: у них сервер
                // описывается типом команды, а не просто command+args.
                match client.id {
                    "zed" => serde_json::json!({
                        "command": {"path": command, "args": args, "env": {}},
                    }),
                    "opencode" | "crush" => serde_json::json!({
                        "type": "local",
                        "command": std::iter::once(command.clone())
                            .chain(args.iter().cloned())
                            .collect::<Vec<_>>(),
                        "enabled": true,
                    }),
                    _ => serde_json::json!({
                        "command": command,
                        "args": args,
                    }),
                }
            }
        },
        Transport::Http { url } => match client.format {
            Format::Toml => serde_json::json!({ "url": url }),
            Format::Yaml => serde_json::json!({ "type": "streamable_http", "uri": url }),
            Format::Json => match client.id {
                "zed" => serde_json::json!({ "command": Value::Null, "url": url }),
                "opencode" | "crush" => serde_json::json!({ "type": "remote", "url": url }),
                _ => serde_json::json!({ "url": url }),
            },
        },
    }
}

/// Ключ, в который писать. У Zed он менялся (`context_servers` → `agent_servers`),
/// поэтому выбираем по тому, что уже есть в файле, а не по константе: иначе запись
/// уедет в ключ, который эта версия редактора не читает.
pub fn resolve_key(client: &Client, document: &Value) -> String {
    if client.id == "zed" {
        for candidate in ["agent_servers", "context_servers"] {
            if document.get(candidate).is_some() {
                return candidate.to_string();
            }
        }
        return "agent_servers".to_string();
    }
    client.key.to_string()
}

/// Состояние клиентов: есть ли файл и прописан ли уже `tasq`.
pub fn states(cwd: &Path) -> Vec<State> {
    let mut out = Vec::new();
    for client in CLIENTS {
        let scope = client.default_scope;
        let Ok(path) = path_for(client, scope, cwd) else {
            continue;
        };
        let exists = path.is_file();
        let configured = exists && is_configured(client, &path);
        out.push(State {
            client: client.id.to_string(),
            scope: scope.as_str().to_string(),
            path: path.display().to_string(),
            file_exists: exists,
            configured,
            format: match client.format {
                Format::Json => "json".to_string(),
                Format::Toml => "toml".to_string(),
                Format::Yaml => "yaml".to_string(),
            },
        });
    }
    out
}

/// Прописан ли `tasq` в файле. Ошибки разбора читаются как «не прописан»: это безопаснее,
/// чем считать конфиг настроенным и ничего не делать.
fn is_configured(client: &Client, path: &Path) -> bool {
    match client.format {
        Format::Json => {
            let Ok(text) = std::fs::read_to_string(path) else {
                return false;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                return false;
            };
            let key = resolve_key(client, &value);
            value
                .get(&key)
                .and_then(|servers| servers.get("tasq"))
                .is_some()
        }
        Format::Toml => {
            let Ok(text) = std::fs::read_to_string(path) else {
                return false;
            };
            let Ok(document) = text.parse::<toml_edit::DocumentMut>() else {
                return false;
            };
            document
                .get(client.key)
                .and_then(|item| item.get("tasq"))
                .is_some()
        }
        Format::Yaml => std::fs::read_to_string(path)
            .map(|text| text.contains("tasq:"))
            .unwrap_or(false),
    }
}

/// Итог правки одного файла.
#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub client: String,
    pub path: String,
    pub action: String,
    pub backup: Option<String>,
    /// Готовый фрагмент печатается, когда файл править нельзя или нечем.
    pub snippet: Option<String>,
}

/// Копия файла рядом: `<файл>.bak-<YYYYmmdd-HHMMSS>`.
fn backup(path: &Path) -> Result<PathBuf> {
    let stamp = crate::db::now_iso()
        .replace([':', '-'], "")
        .replace('T', "-")
        .chars()
        .take(15)
        .collect::<String>();
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".bak-{stamp}"));
    let mut target = path.with_file_name(name);
    // Две правки в одну секунду не должны затирать копию друг друга.
    let mut suffix = 1;
    while target.exists() {
        let mut name = target.file_name().unwrap_or_default().to_os_string();
        name.push(format!("-{suffix}"));
        target = target.with_file_name(name);
        suffix += 1;
    }
    std::fs::copy(path, &target)?;
    Ok(target)
}

/// Атомарная запись: временный файл в том же каталоге плюс `rename`.
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temp_name = path.file_name().unwrap_or_default().to_os_string();
    temp_name.push(".tmp-tasq");
    let temp = path.with_file_name(temp_name);
    std::fs::write(&temp, content)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

/// Прописывает сервер `tasq`.
pub fn apply(
    client: &Client,
    scope: Scope,
    cwd: &Path,
    transport: &Transport,
    dry_run: bool,
    remove: bool,
) -> Result<Outcome> {
    let path = path_for(client, scope, cwd)?;
    let snippet = snippet(client, transport);
    let pretty = serde_json::to_string_pretty(&snippet).expect("serialisable");

    if dry_run {
        return Ok(Outcome {
            client: client.id.to_string(),
            path: path.display().to_string(),
            action: if remove {
                "would-remove"
            } else {
                "would-write"
            }
            .to_string(),
            backup: None,
            snippet: Some(pretty),
        });
    }

    // YAML правится только руками: комментарии, якоря и порядок ключей важнее автоматики.
    if client.format == Format::Yaml {
        let snippet = if remove {
            format!("удалите из {} блок `tasq` (расширение goose)", client.key)
        } else {
            yaml_snippet(transport)
        };
        return Ok(Outcome {
            client: client.id.to_string(),
            path: path.display().to_string(),
            action: "manual".to_string(),
            backup: None,
            snippet: Some(snippet),
        });
    }

    let existed = path.is_file();
    let text = if existed {
        std::fs::read_to_string(&path)?
    } else {
        String::new()
    };

    match client.format {
        Format::Json => apply_json(client, &path, &text, existed, &snippet, remove),
        Format::Toml => apply_toml(client, &path, &text, existed, &snippet, remove),
        Format::Yaml => unreachable!("yaml обработан выше"),
    }
}

fn apply_json(
    client: &Client,
    path: &Path,
    text: &str,
    existed: bool,
    snippet: &Value,
    remove: bool,
) -> Result<Outcome> {
    // Пустой или отсутствующий файл — это `{}`. Всё остальное должно разбираться строго:
    // конфиги с комментариями (так устроен, например, settings.json у Zed) не переписываем.
    let mut document: Value = if text.trim().is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_str::<Value>(text) {
            Ok(value) => value,
            Err(err) => {
                // Разобрать файл нельзя, но ключ угадать можно по тексту: у Zed он
                // зависит от версии, и подсказка не должна отправлять в неверное место.
                let key = if client.id == "zed" && !text.contains("context_servers") {
                    "agent_servers".to_string()
                } else {
                    client.key.to_string()
                };
                return Ok(Outcome {
                    client: client.id.to_string(),
                    path: path.display().to_string(),
                    action: "unparsable".to_string(),
                    backup: None,
                    snippet: Some(format!(
                        "файл не разобран как JSON ({err}); вставьте вручную в ключ {key}:\n{}",
                        serde_json::to_string_pretty(&serde_json::json!({"tasq": snippet}))
                            .unwrap_or_default()
                    )),
                });
            }
        }
    };

    if !document.is_object() {
        return Ok(Outcome {
            client: client.id.to_string(),
            path: path.display().to_string(),
            action: "unparsable".to_string(),
            backup: None,
            snippet: Some("верхний уровень не объект; вставьте вручную".to_string()),
        });
    }

    let key = resolve_key(client, &document);
    let entry = document
        .as_object_mut()
        .expect("object")
        .entry(key.clone())
        .or_insert_with(|| serde_json::json!({}));
    if !entry.is_object() {
        *entry = serde_json::json!({});
    }
    let servers = entry.as_object_mut().expect("object");
    let changed = if remove {
        servers.remove("tasq").is_some()
    } else {
        // Идемпотентность: тот же фрагмент второй раз — не повод переписывать файл.
        servers.get("tasq") != Some(snippet)
    };
    if !changed && !remove {
        return Ok(Outcome {
            client: client.id.to_string(),
            path: path.display().to_string(),
            action: "unchanged".to_string(),
            backup: None,
            snippet: None,
        });
    }
    if !remove {
        servers.insert("tasq".to_string(), snippet.clone());
    } else if servers.is_empty() {
        // Ключ остался бы пустым объектом — убираем контейнер, который сами и создали.
        document.as_object_mut().expect("object").remove(&key);
    }

    if !changed {
        return Ok(Outcome {
            client: client.id.to_string(),
            path: path.display().to_string(),
            action: "unchanged".to_string(),
            backup: None,
            snippet: None,
        });
    }

    let backup_path = if existed { Some(backup(path)?) } else { None };
    let mut text = serde_json::to_string_pretty(&document).expect("serialisable");
    text.push('\n');
    write_atomic(path, &text)?;
    Ok(Outcome {
        client: client.id.to_string(),
        path: path.display().to_string(),
        action: if remove { "removed" } else { "written" }.to_string(),
        backup: backup_path.map(|p| p.display().to_string()),
        snippet: None,
    })
}

fn apply_toml(
    client: &Client,
    path: &Path,
    text: &str,
    existed: bool,
    snippet: &Value,
    remove: bool,
) -> Result<Outcome> {
    let mut document = if text.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        match text.parse::<toml_edit::DocumentMut>() {
            Ok(document) => document,
            Err(err) => {
                return Ok(Outcome {
                    client: client.id.to_string(),
                    path: path.display().to_string(),
                    action: "unparsable".to_string(),
                    backup: None,
                    snippet: Some(format!(
                        "файл не разобран как TOML ({err}); добавьте вручную:\n[{} . tasq]\ncommand = {:?}\nargs = […]",
                        client.key,
                        snippet
                            .get("command")
                            .and_then(Value::as_str)
                            .unwrap_or("tasq")
                    )),
                });
            }
        }
    };

    let table = document.entry(client.key).or_insert_with(|| {
        // Неявная (`implicit`) таблица не печатается заголовком: в файле остаётся
        // только `[mcp_servers.tasq]`, без пустой `[mcp_servers]`.
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        toml_edit::Item::Table(table)
    });
    if !table.is_table() {
        *table = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let Some(table) = table.as_table_mut() else {
        return Ok(Outcome {
            client: client.id.to_string(),
            path: path.display().to_string(),
            action: "unparsable".to_string(),
            backup: None,
            snippet: Some("ключ занят не таблицей".to_string()),
        });
    };

    let changed = if remove {
        table.remove("tasq").is_some()
    } else {
        let command = snippet.get("command").and_then(Value::as_str);
        let args: Vec<&str> = snippet
            .get("args")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        // Тот же фрагмент второй раз — не повод переписывать файл.
        let already = table.get("tasq").map(|existing| {
            let existing_command = existing.get("command").and_then(|item| item.as_str());
            let existing_args: Vec<&str> = existing
                .get("args")
                .and_then(|item| item.as_array())
                .map(|array| array.iter().filter_map(|value| value.as_str()).collect())
                .unwrap_or_default();
            existing_command == command && existing_args == args
        });

        if already == Some(true) {
            return Ok(Outcome {
                client: client.id.to_string(),
                path: path.display().to_string(),
                action: "unchanged".to_string(),
                backup: None,
                snippet: None,
            });
        }

        let mut server = toml_edit::Table::new();
        if let Some(command) = command {
            server.insert("command", toml_edit::value(command));
        }
        if !args.is_empty() {
            let mut array = toml_edit::Array::new();
            for arg in &args {
                array.push(*arg);
            }
            server.insert("args", toml_edit::value(array));
        }
        table.insert("tasq", toml_edit::Item::Table(server));
        true
    };

    if !changed {
        return Ok(Outcome {
            client: client.id.to_string(),
            path: path.display().to_string(),
            action: "unchanged".to_string(),
            backup: None,
            snippet: None,
        });
    }

    let backup_path = if existed { Some(backup(path)?) } else { None };
    write_atomic(path, &document.to_string())?;
    Ok(Outcome {
        client: client.id.to_string(),
        path: path.display().to_string(),
        action: if remove { "removed" } else { "written" }.to_string(),
        backup: backup_path.map(|p| p.display().to_string()),
        snippet: None,
    })
}

/// Проверка записанного: файл читается заново, ключ на месте.
pub fn verify(client: &Client, scope: Scope, cwd: &Path) -> Result<bool> {
    let path = path_for(client, scope, cwd)?;
    if !path.is_file() {
        return Ok(false);
    }
    Ok(is_configured(client, &path))
}

/// Сводка по клиентам для сообщений: сколько файлов есть и где уже прописано.
pub fn summary(cwd: &Path) -> BTreeMap<String, usize> {
    let states = states(cwd);
    let mut out = BTreeMap::new();
    out.insert("clients".to_string(), states.len());
    out.insert(
        "files_present".to_string(),
        states.iter().filter(|state| state.file_exists).count(),
    );
    out.insert(
        "configured".to_string(),
        states.iter().filter(|state| state.configured).count(),
    );
    out
}

/// Ошибка «нечего править»: файла нет и клиент создаёт его сам.
pub fn missing_file(client: &Client, path: &Path) -> Error {
    Error::InvalidValue {
        field: "path".to_string(),
        value: format!(
            "{} for {} does not exist{}",
            path.display(),
            client.id,
            if client.created_by_client {
                "; start the client once so it creates the file"
            } else {
                ""
            }
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_client_has_a_path_for_its_default_scope() {
        for client in CLIENTS {
            let cwd = Path::new("/tmp");
            let path = path_for(client, client.default_scope, cwd);
            assert!(
                path.is_ok(),
                "{}: нет пути для охвата {}",
                client.id,
                client.default_scope.as_str()
            );
        }
    }

    #[test]
    fn client_ids_are_unique_and_cover_the_expected_set() {
        let mut ids: Vec<&str> = CLIENTS.iter().map(|client| client.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), CLIENTS.len(), "идентификаторы не повторяются");
        assert_eq!(CLIENTS.len(), 11, "одиннадцать клиентов из ТЗ");
        for expected in [
            "claude-code",
            "claude-desktop",
            "opencode",
            "zed",
            "cursor",
            "vscode",
            "gemini",
            "codex",
            "goose",
            "crush",
            "windsurf",
        ] {
            assert!(client(expected).is_some(), "нет клиента {expected}");
        }
    }

    #[test]
    fn snippet_shape_depends_on_the_client() {
        let transport = Transport::Stdio {
            command: "tasq".to_string(),
            args: vec!["mcp".to_string()],
        };
        let claude = snippet(client("claude-code").expect("client"), &transport);
        assert_eq!(claude["command"], serde_json::json!("tasq"));
        assert_eq!(claude["args"], serde_json::json!(["mcp"]));

        // opencode и crush ждут одну команду со всеми аргументами
        let opencode = snippet(client("opencode").expect("client"), &transport);
        assert_eq!(opencode["type"], serde_json::json!("local"));
        assert_eq!(opencode["command"], serde_json::json!(["tasq", "mcp"]));

        // zed — вложенный command
        let zed = snippet(client("zed").expect("client"), &transport);
        assert_eq!(zed["command"]["path"], serde_json::json!("tasq"));
    }
}
