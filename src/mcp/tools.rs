//! Каталог инструментов MCP: схемы и обработчики (README §13.2).
//!
//! Каждое описание держится коротким осознанно: `tools/list` целиком уезжает в контекст модели
//! при каждом подключении, и бюджет в токенах — гейт, а не пожелание (см. `tasq tokens`).

use std::path::PathBuf;

use rusqlite::Connection;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::project::Project;
use crate::status::{Kind, Status};
use crate::{comment, edge, lease, node, note, overview, template, transfer};

/// Контекст вызова: открытая база, проект и каталог для файловых операций.
pub struct Context {
    pub conn: Connection,
    pub project: Project,
    pub db_path: PathBuf,
    pub cwd: PathBuf,
}

fn text(result: &impl serde::Serialize) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(result).unwrap_or_else(|_| "{}".to_string()),
        }],
        "isError": false,
    })
}

fn error_content(err: &Error) -> Value {
    let payload = json!({
        "code": err.code(),
        "message": err.to_string(),
        "hint": err.hint(),
    });
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&payload).unwrap_or_default(),
        }],
        "isError": true,
    })
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    arg_str(args, key).ok_or_else(|| Error::InvalidValue {
        field: key.to_string(),
        value: "required".to_string(),
    })
}

fn arg_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

fn arg_f64(args: &Value, key: &str) -> Option<f64> {
    args.get(key).and_then(Value::as_f64)
}

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn arg_status(args: &Value, key: &str) -> Result<Option<Status>> {
    match arg_str(args, key) {
        Some(value) => Status::parse(value)
            .map(Some)
            .ok_or_else(|| Error::InvalidValue {
                field: key.to_string(),
                value: value.to_string(),
            }),
        None => Ok(None),
    }
}

fn arg_kind(args: &Value, key: &str) -> Result<Option<Kind>> {
    match arg_str(args, key) {
        Some(value) => Kind::parse(value)
            .map(Some)
            .ok_or_else(|| Error::InvalidValue {
                field: key.to_string(),
                value: value.to_string(),
            }),
        None => Ok(None),
    }
}

fn arg_holder(args: &Value) -> String {
    if let Some(holder) = arg_str(args, "holder") {
        return holder.to_string();
    }
    std::env::var("TASQ_HOLDER").unwrap_or_else(|_| "mcp".to_string())
}

/// Ресурс из строки `path:src/api`, `glob:src/**/*.rs`, `port:8080`.
fn parse_resource(raw: &str) -> crate::resource::Resource {
    match raw.split_once(':') {
        Some((prefix, rest)) if crate::resource::Kind::parse(prefix).is_some() => {
            let kind = crate::resource::Kind::parse(prefix).unwrap_or(crate::resource::Kind::Path);
            crate::resource::Resource {
                kind,
                value: rest.trim().to_string(),
                mode: crate::resource::Mode::Exclusive,
            }
        }
        _ => crate::resource::Resource::logical(raw.trim()),
    }
}

fn arg_resources(args: &Value) -> Vec<crate::resource::Resource> {
    args.get("reserve")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(parse_resource)
                .collect()
        })
        .unwrap_or_default()
}

/// Обработчик одного инструмента.
pub fn call(ctx: &mut Context, name: &str, args: &Value) -> Value {
    let result = dispatch(ctx, name, args);
    match result {
        Ok(value) => value,
        Err(err) => error_content(&err),
    }
}

fn dispatch(ctx: &mut Context, name: &str, args: &Value) -> Result<Value> {
    match name {
        // ------------------------------------------------ инициализация и проекты
        "tasq_init" => {
            let key = arg_str(args, "key").unwrap_or("TASQ");
            let project_name = arg_str(args, "name").unwrap_or(key);
            let outcome = crate::db::init(&ctx.cwd, key, project_name)?;
            Ok(text(&outcome))
        }
        "project_create" => {
            // Создание идёт через ядро: там проверка ключа, отказ на дубликат и нормализация
            // регистра. Раньше вставка шла напрямую, и `{"key":"prj"}` создавал проект
            // с ключом в нижнем регистре, несовместимым с dot-ID.
            let key = req_str(args, "key")?;
            let name = arg_str(args, "name");
            let project = crate::project::create(&mut ctx.conn, key, name)?;
            Ok(text(&json!({
                "project": project,
                "active_project": ctx.project.key,
                "switched": false,
                // Молчаливая смена контекста опаснее лишнего вызова: агент, создавший
                // проект «на будущее», получил бы узлы в нём.
                "hint": format!(
                    "this session still works in {}; call project_select with key {} to switch",
                    ctx.project.key, project.key
                ),
            })))
        }
        "project_select" => {
            let key = req_str(args, "key")?;
            let project = crate::project::select(&ctx.conn, Some(key))?;
            let previous = ctx.project.key.clone();
            ctx.project = project.clone();
            Ok(text(&json!({
                "active_project": project.key,
                "name": project.name,
                "previous": previous,
                "switched": previous != project.key,
            })))
        }
        "project_list" => {
            let mut stmt = ctx.conn.prepare(
                "SELECT p.id, p.key, p.name, p.status,
                        (SELECT COUNT(*) FROM nodes n WHERE n.project_id = p.id AND n.deleted_at IS NULL)
                 FROM projects p ORDER BY p.id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "key": row.get::<_, String>(1)?,
                    "name": row.get::<_, String>(2)?,
                    "status": row.get::<_, String>(3)?,
                    "nodes": row.get::<_, i64>(4)?,
                }))
            })?;
            let projects: Vec<Value> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(text(&projects))
        }
        "project_update" => {
            // Правка идёт через ядро: там проверка статуса по схеме, нормализация ключа
            // и отказ на занятый ключ. Раньше SQL собирался на месте, и `--status nonsense`
            // отбивался `CHECK` базы как STORAGE_ERROR, то есть «внутренняя ошибка».
            let target = arg_str(args, "key").unwrap_or(&ctx.project.key);
            let patch = crate::project::Patch {
                name: arg_str(args, "name"),
                description: arg_str(args, "description"),
                status: arg_str(args, "status"),
                key: arg_str(args, "new_key"),
            };
            let (updated, previous_key) =
                crate::project::update(&mut ctx.conn, Some(target), &patch)?;
            // Если правили проект сессии (в том числе переименовали ключ) — контекст
            // должен остаться на нём, иначе следующий вызов уйдёт в первый по порядку.
            if ctx.project.id == updated.id {
                ctx.project = updated.clone();
            }
            Ok(text(&json!({
                "project": updated,
                "previous_key": previous_key,
                "active_project": ctx.project.key,
                "nodes": crate::project::live_nodes(&ctx.conn, &updated)?,
            })))
        }
        "project_delete" => {
            let target = arg_str(args, "key").unwrap_or(&ctx.project.key).to_string();
            let deleting_active = {
                let project = crate::project::select(&ctx.conn, Some(&target))?;
                project.id == ctx.project.id
            };
            let nodes = {
                let project = crate::project::select(&ctx.conn, Some(&target))?;
                crate::project::live_nodes(&ctx.conn, &project)?
            };
            let removed =
                crate::project::delete(&mut ctx.conn, Some(&target), arg_bool(args, "force"))?;
            // Удалили проект сессии — переезжаем в первый оставшийся, иначе контекст
            // указывает на несуществующую строку и все вызовы падают.
            let mut switched_to = None;
            if deleting_active {
                match crate::project::default_project(&ctx.conn) {
                    Ok(next) => {
                        switched_to = Some(next.key.clone());
                        ctx.project = next;
                    }
                    Err(_) => {
                        return Ok(text(&json!({
                            "deleted": removed.key,
                            "nodes": nodes,
                            "active_project": Value::Null,
                            "hint": "no projects left; call tasq_init or project_create",
                        })));
                    }
                }
            }
            Ok(text(&json!({
                "deleted": removed.key,
                "name": removed.name,
                "nodes": nodes,
                "active_project": ctx.project.key,
                "switched_to": switched_to,
                "remaining": crate::project::known_keys(&ctx.conn)?,
            })))
        }

        // ------------------------------------------------ узлы
        "node_create" => {
            let kind = arg_kind(args, "kind")?;
            let created = node::create(
                &mut ctx.conn,
                &ctx.project,
                node::NewNode {
                    parent: arg_str(args, "parent"),
                    title: req_str(args, "title")?,
                    kind,
                    description: arg_str(args, "description"),
                    priority: arg_str(args, "priority"),
                    due_date: arg_str(args, "due_date"),
                    estimated_hours: arg_f64(args, "estimated_hours"),
                    weight: arg_f64(args, "weight"),
                    source_ref: arg_str(args, "source_ref"),
                    branch: arg_str(args, "branch"),
                },
            )?;
            Ok(text(&created))
        }
        "node_get" => {
            let node = node::resolve(&ctx.conn, &ctx.project, req_str(args, "ref")?)?;
            let parent = if node.depth > 1 {
                node.path
                    .rsplit_once('.')
                    .and_then(|(head, _)| node::resolve(&ctx.conn, &ctx.project, head).ok())
                    .map(|parent| parent.dot_id)
            } else {
                None
            };
            let children = node::list(
                &ctx.conn,
                &ctx.project,
                &node::Filter {
                    parent: Some(&node.dot_id),
                    ..node::Filter::default()
                },
            )?
            .into_iter()
            .filter(|child| child.id != node.id)
            .map(|child| {
                json!({"dot_id": child.dot_id, "title": child.title, "status": child.effective_status})
            })
            .collect::<Vec<_>>();
            let comments = comment::list(&ctx.conn, &ctx.project, &node.dot_id, false)?;
            Ok(text(&json!({
                "node": node,
                "parent": parent,
                "children": children,
                "comments": comments,
            })))
        }
        "node_update" => {
            let status = arg_status(args, "status")?;
            let description_locked = match arg_str(args, "description_locked") {
                Some("true") => Some(true),
                Some("false") => Some(false),
                Some(other) => {
                    return Err(Error::InvalidValue {
                        field: "description_locked".to_string(),
                        value: other.to_string(),
                    });
                }
                None => None,
            };
            let updated = node::update(
                &mut ctx.conn,
                &ctx.project,
                req_str(args, "ref")?,
                node::Patch {
                    title: arg_str(args, "title"),
                    description: arg_str(args, "description"),
                    status,
                    priority: arg_str(args, "priority"),
                    assigned_to: arg_str(args, "assigned_to"),
                    due_date: arg_str(args, "due_date"),
                    weight: arg_f64(args, "weight"),
                    estimated_hours: arg_f64(args, "estimated_hours"),
                    source_ref: arg_str(args, "source_ref"),
                    branch: arg_str(args, "branch"),
                    force: arg_bool(args, "force"),
                    description_locked,
                    lease: arg_str(args, "token").is_some(),
                    actor: Some(&arg_holder(args)),
                },
            )?;
            Ok(text(&updated))
        }
        "node_list" => {
            let nodes = node::list(
                &ctx.conn,
                &ctx.project,
                &node::Filter {
                    parent: arg_str(args, "parent"),
                    status: arg_str(args, "status"),
                    assignee: arg_str(args, "assignee"),
                    overdue: arg_bool(args, "overdue"),
                    include_archived: arg_bool(args, "include_archived"),
                    include_deleted: false,
                    limit: arg_i64(args, "limit"),
                },
            )?;
            Ok(text(&nodes))
        }
        "node_move" => {
            let moved = node::move_node(
                &mut ctx.conn,
                &ctx.project,
                req_str(args, "ref")?,
                arg_str(args, "parent"),
                arg_bool(args, "renumber"),
            )?;
            Ok(text(&moved))
        }
        "node_reorder" => {
            let refs: Vec<String> = args
                .get("refs")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if refs.is_empty() {
                return Err(Error::InvalidValue {
                    field: "refs".to_string(),
                    value: "required".to_string(),
                });
            }
            node::reorder(&mut ctx.conn, &ctx.project, &refs)?;
            Ok(text(&json!({"reordered": refs.len()})))
        }
        "node_archive" => {
            let affected = node::archive(
                &mut ctx.conn,
                &ctx.project,
                req_str(args, "ref")?,
                arg_bool(args, "undo"),
            )?;
            Ok(text(&json!({"affected": affected})))
        }
        "node_delete" => {
            node::delete(&mut ctx.conn, &ctx.project, req_str(args, "ref")?)?;
            Ok(text(&json!({"deleted": true})))
        }
        "node_restore" => {
            let restored = node::restore(&mut ctx.conn, &ctx.project, req_str(args, "ref")?)?;
            Ok(text(&restored))
        }

        // ------------------------------------------------ дерево и рекомендации
        "tasq_context" => {
            let chain = overview::context(&ctx.conn, &ctx.project, req_str(args, "ref")?)?;
            Ok(text(&chain))
        }
        "tasq_next" => {
            let limit = arg_i64(args, "limit").unwrap_or(3).clamp(1, 20) as usize;
            let candidates = edge::next_candidates(&ctx.conn, &ctx.project, limit)?;
            Ok(text(&candidates))
        }

        // ------------------------------------------------ аренды
        "tasq_claim" => {
            let holder = arg_holder(args);
            let held = lease::claim(
                &mut ctx.conn,
                &ctx.project,
                req_str(args, "ref")?,
                &lease::ClaimRequest {
                    holder: &holder,
                    ttl_seconds: arg_i64(args, "ttl").unwrap_or(lease::DEFAULT_TTL),
                    steal: arg_bool(args, "steal"),
                    force: arg_bool(args, "force"),
                    checkpoint: arg_str(args, "checkpoint"),
                    reservations: arg_resources(args),
                },
            )?;
            Ok(text(&held))
        }
        "tasq_renew" => {
            let renewed = lease::renew(
                &mut ctx.conn,
                &ctx.project,
                req_str(args, "token")?,
                arg_i64(args, "ttl").unwrap_or(lease::DEFAULT_TTL),
            )?;
            Ok(text(&renewed))
        }
        "tasq_release" => {
            let outcome = match arg_str(args, "outcome").unwrap_or("released") {
                "released" => lease::Outcome::Released,
                "failed" => lease::Outcome::Failed,
                "completed" => lease::Outcome::Completed,
                other => {
                    return Err(Error::InvalidValue {
                        field: "outcome".to_string(),
                        value: other.to_string(),
                    });
                }
            };
            let released = lease::release(
                &mut ctx.conn,
                &ctx.project,
                req_str(args, "token")?,
                outcome,
                arg_str(args, "checkpoint"),
                arg_bool(args, "force"),
            )?;
            Ok(text(&released))
        }

        // ------------------------------------------------ ресурсы
        "tasq_reserve" => {
            let token = req_str(args, "token")?;
            // `remove` — список ресурсов. Присланное `true` раньше молча игнорировалось,
            // и вызов «снять резерв» на деле добавлял его: ошибка в типе не должна
            // превращаться в противоположное действие.
            if let Some(raw) = args.get("remove")
                && !raw.is_array()
            {
                return Err(Error::InvalidValue {
                    field: "remove".to_string(),
                    value: format!("{raw} (expected an array of resources)"),
                });
            }
            if let Some(remove) = args.get("remove").and_then(Value::as_array) {
                let values: Vec<crate::resource::Resource> = remove
                    .iter()
                    .filter_map(Value::as_str)
                    .map(parse_resource)
                    .collect();
                let removed = lease::unreserve(&mut ctx.conn, &ctx.project, token, Some(&values))?;
                let add = arg_resources(args);
                if add.is_empty() {
                    return Ok(text(&json!({"released": removed})));
                }
                let held = lease::reserve(&mut ctx.conn, &ctx.project, token, &add)?;
                return Ok(text(&json!({"released": removed, "held": held})));
            }
            let add = arg_resources(args);
            if add.is_empty() {
                return Ok(text(
                    &json!({"released": lease::unreserve(&mut ctx.conn, &ctx.project, token, None)?}),
                ));
            }
            let held = lease::reserve(&mut ctx.conn, &ctx.project, token, &add)?;
            Ok(text(&held))
        }
        "tasq_holdings" => {
            let live = lease::holdings(&ctx.conn, &ctx.project)?;
            Ok(text(&live))
        }

        // ------------------------------------------------ зависимости
        "tasq_link" => {
            let kind =
                edge::Kind::parse(arg_str(args, "kind").unwrap_or("blocks")).ok_or_else(|| {
                    Error::InvalidValue {
                        field: "kind".to_string(),
                        value: "blocks|related|duplicates|discovered_from|split_from".to_string(),
                    }
                })?;
            let linked = edge::link(
                &mut ctx.conn,
                &ctx.project,
                req_str(args, "from")?,
                req_str(args, "to")?,
                kind,
                &arg_holder(args),
            )?;
            Ok(text(&linked))
        }
        "tasq_unlink" => {
            let kind =
                edge::Kind::parse(arg_str(args, "kind").unwrap_or("blocks")).ok_or_else(|| {
                    Error::InvalidValue {
                        field: "kind".to_string(),
                        value: "blocks|related|duplicates|discovered_from|split_from".to_string(),
                    }
                })?;
            let removed = edge::unlink(
                &mut ctx.conn,
                &ctx.project,
                req_str(args, "from")?,
                req_str(args, "to")?,
                kind,
                &arg_holder(args),
            )?;
            Ok(text(&json!({"removed": removed})))
        }
        "tasq_graph" => {
            let graph = edge::graph(
                &ctx.conn,
                &ctx.project,
                req_str(args, "ref")?,
                arg_i64(args, "depth").unwrap_or(3),
            )?;
            if arg_bool(args, "mermaid") {
                return Ok(text(
                    &json!({"mermaid": graph.to_mermaid(), "graph": graph}),
                ));
            }
            Ok(text(&graph))
        }

        // ------------------------------------------------ комментарии
        "comment_add" => {
            let holder = arg_holder(args);
            let added = comment::add(
                &mut ctx.conn,
                &ctx.project,
                req_str(args, "ref")?,
                req_str(args, "content")?,
                &holder,
            )?;
            Ok(text(&added))
        }
        "comment_list" => {
            let comments = comment::list(
                &ctx.conn,
                &ctx.project,
                req_str(args, "ref")?,
                arg_bool(args, "include_deleted"),
            )?;
            Ok(text(&comments))
        }
        "comment_delete" => {
            let id = arg_i64(args, "id").ok_or_else(|| Error::InvalidValue {
                field: "id".to_string(),
                value: "required".to_string(),
            })?;
            let holder = arg_holder(args);
            let result = if arg_bool(args, "restore") {
                comment::restore(&mut ctx.conn, &ctx.project, id, &holder)?
            } else {
                comment::delete(
                    &mut ctx.conn,
                    &ctx.project,
                    id,
                    &holder,
                    arg_str(args, "reason"),
                )?
            };
            Ok(text(&result))
        }

        // ------------------------------------------------ заметки
        "note_save" => {
            let saved = note::save(
                &mut ctx.conn,
                &ctx.project,
                note::SaveNote {
                    id: arg_i64(args, "id"),
                    title: arg_str(args, "title"),
                    content: arg_str(args, "content"),
                    note_type: arg_str(args, "note_type"),
                    node: arg_str(args, "ref"),
                    actor: Some(&arg_holder(args)),
                },
            )?;
            Ok(text(&saved))
        }
        "note_list" => {
            let notes = note::list(
                &ctx.conn,
                &ctx.project,
                &note::Filter {
                    note_type: arg_str(args, "note_type"),
                    node: arg_str(args, "ref"),
                    limit: arg_i64(args, "limit"),
                },
            )?;
            Ok(text(&notes))
        }
        "note_search" => {
            let notes = note::search(&ctx.conn, &ctx.project, req_str(args, "query")?)?;
            Ok(text(&notes))
        }
        "note_delete" => {
            let id = arg_i64(args, "id").ok_or_else(|| Error::InvalidValue {
                field: "id".to_string(),
                value: "required".to_string(),
            })?;
            let removed = note::delete(&mut ctx.conn, &ctx.project, id, &arg_holder(args))?;
            Ok(text(&json!({"removed": removed})))
        }

        // ------------------------------------------------ шаблоны
        "template_save" => {
            let tree = args.get("tree").ok_or_else(|| Error::InvalidValue {
                field: "tree".to_string(),
                value: "required".to_string(),
            })?;
            let saved = template::save(
                &mut ctx.conn,
                &ctx.project,
                req_str(args, "name")?,
                arg_str(args, "description"),
                &serde_json::to_string(tree).unwrap_or_else(|_| "[]".to_string()),
                &arg_holder(args),
            )?;
            Ok(text(&saved))
        }
        "template_list" => {
            let templates = template::list(&ctx.conn, &ctx.project)?;
            Ok(text(&templates))
        }
        "template_apply" => {
            let values: Vec<(String, String)> = args
                .get("values")
                .and_then(Value::as_object)
                .map(|object| {
                    object
                        .iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|text| (key.clone(), text.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let applied = template::apply(
                &mut ctx.conn,
                &ctx.project,
                req_str(args, "name")?,
                arg_str(args, "parent"),
                &values,
            )?;
            Ok(text(&applied))
        }
        "template_delete" => {
            let removed = template::delete(
                &mut ctx.conn,
                &ctx.project,
                req_str(args, "name")?,
                &arg_holder(args),
            )?;
            Ok(text(&json!({"removed": removed})))
        }

        // ------------------------------------------------ активность
        "activity_log" => {
            let limit = arg_i64(args, "limit").unwrap_or(20).clamp(1, 200);
            let records = crate::activity::recent(&ctx.conn, ctx.project.id, limit)?;
            Ok(text(&records))
        }
        "session_diff" => {
            let since = req_str(args, "since")?;
            let mut stmt = ctx.conn.prepare(
                "SELECT action, entity_type, COUNT(*)
                 FROM activity_log
                 WHERE project_id = ?1 AND created_at >= ?2
                 GROUP BY action, entity_type ORDER BY COUNT(*) DESC",
            )?;
            let rows = stmt.query_map(rusqlite::params![ctx.project.id, since], |row| {
                Ok(json!({
                    "action": row.get::<_, String>(0)?,
                    "entity": row.get::<_, String>(1)?,
                    "count": row.get::<_, i64>(2)?,
                }))
            })?;
            let grouped: Vec<Value> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            let records = crate::activity::recent(&ctx.conn, ctx.project.id, 50)?
                .into_iter()
                .filter(|record| record.created_at.as_str() >= since)
                .collect::<Vec<_>>();
            Ok(text(
                &json!({"since": since, "summary": grouped, "recent": records}),
            ))
        }
        "node_batch_update" => {
            let refs: Vec<String> = args
                .get("refs")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let status = arg_status(args, "status")?.ok_or_else(|| Error::InvalidValue {
                field: "status".to_string(),
                value: "required".to_string(),
            })?;
            let holder = arg_holder(args);
            let result = overview::bulk_status(
                &mut ctx.conn,
                &ctx.project,
                &refs,
                status,
                arg_bool(args, "force"),
                &holder,
            )?;
            Ok(text(&result))
        }

        // ------------------------------------------------ поиск и обзоры
        "tasq_search" => {
            let hits = overview::search(&ctx.conn, &ctx.project, req_str(args, "query")?)?;
            Ok(text(&hits))
        }
        "tasq_dashboard" => {
            let board = overview::board(&ctx.conn, &ctx.project)?;
            let live = lease::holdings(&ctx.conn, &ctx.project)?;
            let next = edge::next_candidates(&ctx.conn, &ctx.project, 3)?;
            let risky = crate::burnup::at_risk(&ctx.conn, &ctx.project, 0.2)?;
            Ok(text(&json!({
                "project": ctx.project.key,
                "board": board,
                "leases": live,
                "next": next,
                "at_risk": risky,
            })))
        }
        "tasq_burnup" => {
            let metric = crate::burnup::Metric::parse(arg_str(args, "metric").unwrap_or("count"))
                .ok_or_else(|| Error::InvalidValue {
                field: "metric".to_string(),
                value: "count|points|hours".to_string(),
            })?;
            let series = crate::burnup::series(
                &ctx.conn,
                &ctx.project,
                metric,
                arg_str(args, "since"),
                arg_str(args, "until"),
            )?;
            if arg_bool(args, "ascii") {
                return Ok(text(&json!({
                    "series": series,
                    "chart": crate::burnup::ascii(&series, 70),
                })));
            }
            Ok(text(&series))
        }
        "tasq_risk" => {
            let risky = crate::burnup::at_risk(
                &ctx.conn,
                &ctx.project,
                arg_f64(args, "threshold").unwrap_or(0.2),
            )?;
            Ok(text(&risky))
        }

        // ------------------------------------------------ перенос данных
        "tasq_export" => {
            let document =
                transfer::export(&ctx.conn, &ctx.project, arg_bool(args, "include_deleted"))?;
            Ok(text(&document))
        }
        "tasq_import" => {
            let document = args.get("document").ok_or_else(|| Error::InvalidValue {
                field: "document".to_string(),
                value: "required".to_string(),
            })?;
            let mode = transfer::Mode::parse(arg_str(args, "mode").unwrap_or("merge")).ok_or_else(
                || Error::InvalidValue {
                    field: "mode".to_string(),
                    value: "merge|replace".to_string(),
                },
            )?;
            let imported = if arg_bool(args, "from_saga") {
                transfer::import_saga(&mut ctx.conn, &ctx.project, document, &arg_holder(args))?
            } else {
                transfer::import(
                    &mut ctx.conn,
                    &ctx.project,
                    document,
                    mode,
                    &arg_holder(args),
                )?
            };
            Ok(text(&imported))
        }

        other => Err(Error::InvalidValue {
            field: "tool".to_string(),
            value: format!("unknown tool {other}"),
        }),
    }
}

/// Описание инструмента для `tools/list`.
///
/// Правила бюджета (README §13.2): описание ≤ 200 символов, описание параметра ≤ 80,
/// параметров ≤ 8, аннотации на каждом инструменте.
pub fn definition(name: &str) -> Option<Value> {
    let def = match name {
        // ------------------------------------------------ инициализация и проекты
        "tasq_init" => tool(
            name,
            "Initialise the tracker in the current directory and register a project",
            json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Project key for dot-IDs, A-Z and 0-9"},
                    "name": {"type": "string", "description": "Project name"}
                }
            }),
            (false, false, false),
        ),
        "project_create" => tool(
            name,
            "Create another project in the same database",
            json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Unique key, A-Z and 0-9"},
                    "name": {"type": "string", "description": "Project name"},
                    "description": {"type": "string", "description": "What the project is about"}
                },
                "required": ["key", "name"]
            }),
            (false, false, false),
        ),
        "project_list" => tool(
            name,
            "List projects with their node counts",
            json!({"type": "object", "properties": {}}),
            (true, false, true),
        ),
        "project_select" => tool(
            name,
            "Switch this session to another project; every later call uses it",
            json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Project key to work in"}
                },
                "required": ["key"]
            }),
            (false, false, false),
        ),
        "project_update" => tool(
            name,
            "Rename a project or change its description and status",
            json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Project to change (default: this session's)"},
                    "name": {"type": "string", "description": "New name"},
                    "description": {"type": "string", "description": "New description"},
                    "status": {"type": "string", "enum": ["active", "on_hold", "completed", "archived"]},
                    "new_key": {"type": "string", "description": "New key; every dot-ID of the project changes"}
                }
            }),
            (false, false, true),
        ),
        "project_delete" => tool(
            name,
            "Delete a project with all of its nodes, notes and history",
            json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Project to delete (default: this session's)"},
                    "force": {"type": "boolean", "description": "Delete even if it still has live nodes or is the last one"}
                }
            }),
            (true, false, true),
        ),
        // ------------------------------------------------ узлы
        "node_create" => tool(
            name,
            "Create a node; without parent it becomes a root epic",
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "Node title"},
                    "parent": {"type": "string", "description": "Parent: dot-ID, path or #id"},
                    "kind": {"type": "string", "enum": ["epic", "task", "subtask", "micro"]},
                    "description": {"type": "string", "description": "The specification"},
                    "priority": {"type": "string", "enum": ["low", "medium", "high", "critical"]},
                    "due_date": {"type": "string", "description": "YYYY-MM-DD"},
                    "estimated_hours": {"type": "number", "description": "Estimate in hours"},
                    "weight": {"type": "number", "description": "Points for burn-up, default 1"}
                },
                "required": ["title"]
            }),
            (false, false, false),
        ),
        "node_get" => tool(
            name,
            "One node with children, comments and the parent chain",
            json!({
                "type": "object",
                "properties": {"ref": {"type": "string", "description": "dot-ID, path or #id"}},
                "required": ["ref"]
            }),
            (true, false, true),
        ),
        "node_update" => tool(
            name,
            "Change node fields; only the given fields are touched",
            json!({
                "type": "object",
                "properties": {
                    "ref": {"type": "string", "description": "dot-ID, path or #id"},
                    "title": {"type": "string", "description": "New title"},
                    "description": {"type": "string", "description": "New specification"},
                    "status": {"type": "string", "description": "Target status"},
                    "priority": {"type": "string", "enum": ["low", "medium", "high", "critical"]},
                    "assigned_to": {"type": "string", "description": "Who works on it"},
                    "due_date": {"type": "string", "description": "YYYY-MM-DD"},
                    "force": {"type": "boolean", "description": "Allow guarded transitions"}
                },
                "required": ["ref"]
            }),
            (false, false, true),
        ),
        "node_list" => tool(
            name,
            "List nodes in tree order with light fields",
            json!({
                "type": "object",
                "properties": {
                    "parent": {"type": "string", "description": "Only this subtree"},
                    "status": {"type": "string", "description": "Filter by effective status"},
                    "assignee": {"type": "string", "description": "Filter by assignee"},
                    "overdue": {"type": "boolean", "description": "Only overdue nodes"},
                    "include_archived": {"type": "boolean", "description": "Show archived too"},
                    "limit": {"type": "integer", "description": "Max rows"}
                }
            }),
            (true, false, true),
        ),
        "node_move" => tool(
            name,
            "Move a node under another parent; descendant dot-IDs change",
            json!({
                "type": "object",
                "properties": {
                    "ref": {"type": "string", "description": "Node to move"},
                    "parent": {"type": "string", "description": "New parent; omit for root"},
                    "renumber": {"type": "boolean", "description": "Take the next free number"}
                },
                "required": ["ref"]
            }),
            (false, false, true),
        ),
        "node_reorder" => tool(
            name,
            "Set the order of siblings",
            json!({
                "type": "object",
                "properties": {"refs": {"type": "array", "items": {"type": "string"},
                                        "description": "Siblings in the wanted order"}},
                "required": ["refs"]
            }),
            (false, false, true),
        ),
        "node_archive" => tool(
            name,
            "Archive or unarchive a subtree; nothing is deleted",
            json!({
                "type": "object",
                "properties": {
                    "ref": {"type": "string", "description": "Subtree root"},
                    "undo": {"type": "boolean", "description": "Unarchive instead"}
                },
                "required": ["ref"]
            }),
            (false, false, true),
        ),
        "node_delete" => tool(
            name,
            "Soft-delete a todo node; history stays in the database",
            json!({
                "type": "object",
                "properties": {"ref": {"type": "string", "description": "Node to remove"}},
                "required": ["ref"]
            }),
            (false, true, true),
        ),
        "node_restore" => tool(
            name,
            "Bring back a soft-deleted node",
            json!({
                "type": "object",
                "properties": {"ref": {"type": "string", "description": "Removed node"}},
                "required": ["ref"]
            }),
            (false, false, true),
        ),
        // ------------------------------------------------ дерево и рекомендации
        "tasq_context" => tool(
            name,
            "Context chain from the root epic down to the node: the why and the what",
            json!({
                "type": "object",
                "properties": {"ref": {"type": "string", "description": "Target node"}},
                "required": ["ref"]
            }),
            (true, false, true),
        ),
        "tasq_next" => tool(
            name,
            "What to work on next, with the reason for the choice",
            json!({
                "type": "object",
                "properties": {"limit": {"type": "integer", "description": "How many candidates"}}
            }),
            (true, false, true),
        ),
        // ------------------------------------------------ аренды
        "tasq_claim" => tool(
            name,
            "Take an expiring lease on a node; returns the token for renew and release",
            json!({
                "type": "object",
                "properties": {
                    "ref": {"type": "string", "description": "Node to claim"},
                    "holder": {"type": "string", "description": "Agent name"},
                    "ttl": {"type": "integer", "description": "Seconds, 60..28800"},
                    "steal": {"type": "boolean", "description": "Take over a live lease"},
                    "checkpoint": {"type": "string", "description": "Note for the next holder"},
                    "reserve": {"type": "array", "items": {"type": "string"},
                                "description": "Resources to reserve with the lease"},
                    "force": {"type": "boolean", "description": "Claim despite blockers"}
                },
                "required": ["ref"]
            }),
            (false, false, false),
        ),
        "tasq_renew" => tool(
            name,
            "Extend a lease before it expires",
            json!({
                "type": "object",
                "properties": {
                    "token": {"type": "string", "description": "Lease token"},
                    "ttl": {"type": "integer", "description": "New TTL in seconds"}
                },
                "required": ["token"]
            }),
            (false, false, true),
        ),
        "tasq_release" => tool(
            name,
            "Close a lease: released, failed or completed",
            json!({
                "type": "object",
                "properties": {
                    "token": {"type": "string", "description": "Lease token"},
                    "outcome": {"type": "string", "enum": ["released", "failed", "completed"]},
                    "checkpoint": {"type": "string", "description": "Note for the next holder"},
                    "force": {"type": "boolean", "description": "Finish despite open children"}
                },
                "required": ["token"]
            }),
            (false, false, true),
        ),
        // ------------------------------------------------ ресурсы
        "tasq_reserve" => tool(
            name,
            "Add or drop resources on a live lease: paths, globs and logical keys",
            json!({
                "type": "object",
                "properties": {
                    "token": {"type": "string", "description": "Lease token"},
                    "reserve": {"type": "array", "items": {"type": "string"},
                                "description": "path:src/api, glob:src/**/*.rs, port:8080"},
                    "remove": {"type": "array", "items": {"type": "string"},
                               "description": "Resource values to release"}
                },
                "required": ["token"]
            }),
            (false, false, true),
        ),
        "tasq_holdings" => tool(
            name,
            "Live leases and the resources they hold",
            json!({"type": "object", "properties": {}}),
            (true, false, true),
        ),
        // ------------------------------------------------ зависимости
        "tasq_link" => tool(
            name,
            "Link two nodes; blocks means from waits for to",
            json!({
                "type": "object",
                "properties": {
                    "from": {"type": "string", "description": "The node that waits"},
                    "to": {"type": "string", "description": "The node it waits for"},
                    "kind": {"type": "string",
                             "enum": ["blocks", "related", "duplicates", "discovered_from", "split_from"]}
                },
                "required": ["from", "to"]
            }),
            (false, false, false),
        ),
        "tasq_unlink" => tool(
            name,
            "Remove a link between two nodes",
            json!({
                "type": "object",
                "properties": {
                    "from": {"type": "string", "description": "Source node"},
                    "to": {"type": "string", "description": "Target node"},
                    "kind": {"type": "string", "description": "Relation kind, default blocks"}
                },
                "required": ["from", "to"]
            }),
            (false, false, true),
        ),
        "tasq_graph" => tool(
            name,
            "Neighbourhood of a node: waits for, unblocks, transitive blockers, Mermaid",
            json!({
                "type": "object",
                "properties": {
                    "ref": {"type": "string", "description": "Node to inspect"},
                    "depth": {"type": "integer", "description": "How deep to walk blockers"},
                    "mermaid": {"type": "boolean", "description": "Include a Mermaid diagram"}
                },
                "required": ["ref"]
            }),
            (true, false, true),
        ),
        // ------------------------------------------------ комментарии
        "comment_add" => tool(
            name,
            "Add a comment to a node: a thread that survives sessions",
            json!({
                "type": "object",
                "properties": {
                    "ref": {"type": "string", "description": "Target node"},
                    "content": {"type": "string", "description": "Comment text"},
                    "holder": {"type": "string", "description": "Author name"}
                },
                "required": ["ref", "content"]
            }),
            (false, false, false),
        ),
        "comment_list" => tool(
            name,
            "Comments of a node in chronological order",
            json!({
                "type": "object",
                "properties": {
                    "ref": {"type": "string", "description": "Target node"},
                    "include_deleted": {"type": "boolean", "description": "Show removed ones"}
                },
                "required": ["ref"]
            }),
            (true, false, true),
        ),
        "comment_delete" => tool(
            name,
            "Soft-delete a comment, or restore it with restore=true",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "Comment id"},
                    "reason": {"type": "string", "description": "Why it is removed"},
                    "restore": {"type": "boolean", "description": "Bring it back instead"}
                },
                "required": ["id"]
            }),
            (false, true, true),
        ),
        // ------------------------------------------------ заметки
        "note_save" => tool(
            name,
            "Create or update a note: decision, context, meeting, blocker, release",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "Update this note"},
                    "title": {"type": "string", "description": "Note title"},
                    "content": {"type": "string", "description": "Note body"},
                    "note_type": {"type": "string",
                                  "enum": ["general", "decision", "context", "meeting",
                                           "technical", "blocker", "progress", "release"]},
                    "ref": {"type": "string", "description": "Attach to a node"}
                }
            }),
            (false, false, false),
        ),
        "note_list" => tool(
            name,
            "Notes of the project, newest first",
            json!({
                "type": "object",
                "properties": {
                    "note_type": {"type": "string", "description": "Filter by type"},
                    "ref": {"type": "string", "description": "Filter by node"},
                    "limit": {"type": "integer", "description": "Max rows"}
                }
            }),
            (true, false, true),
        ),
        "note_search" => tool(
            name,
            "Search notes by keyword",
            json!({
                "type": "object",
                "properties": {"query": {"type": "string", "description": "Keyword"}},
                "required": ["query"]
            }),
            (true, false, true),
        ),
        "note_delete" => tool(
            name,
            "Delete a note",
            json!({
                "type": "object",
                "properties": {"id": {"type": "integer", "description": "Note id"}},
                "required": ["id"]
            }),
            (false, true, true),
        ),
        // ------------------------------------------------ шаблоны
        "template_save" => tool(
            name,
            "Create or replace a template: a node tree with {variable} placeholders",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Template name"},
                    "description": {"type": "string", "description": "What it is for"},
                    "tree": {"type": "object", "description": "Array of nodes or {nodes: [...]}"}
                },
                "required": ["name", "tree"]
            }),
            (false, false, false),
        ),
        "template_list" => tool(
            name,
            "List templates with their variables",
            json!({"type": "object", "properties": {}}),
            (true, false, true),
        ),
        "template_apply" => tool(
            name,
            "Create nodes from a template under a parent",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Template name"},
                    "parent": {"type": "string", "description": "Parent node for the roots"},
                    "values": {"type": "object", "description": "Variable values by name"}
                },
                "required": ["name"]
            }),
            (false, false, false),
        ),
        "template_delete" => tool(
            name,
            "Delete a template",
            json!({
                "type": "object",
                "properties": {"name": {"type": "string", "description": "Template name"}},
                "required": ["name"]
            }),
            (false, true, true),
        ),
        // ------------------------------------------------ активность
        "activity_log" => tool(
            name,
            "Recent changes: what happened and when",
            json!({
                "type": "object",
                "properties": {"limit": {"type": "integer", "description": "How many records"}}
            }),
            (true, false, true),
        ),
        "session_diff" => tool(
            name,
            "What changed since a timestamp: counts by action plus recent records",
            json!({
                "type": "object",
                "properties": {"since": {"type": "string", "description": "ISO-8601 timestamp"}},
                "required": ["since"]
            }),
            (true, false, true),
        ),
        "node_batch_update" => tool(
            name,
            "Move several nodes to one status; failures are reported per node",
            json!({
                "type": "object",
                "properties": {
                    "refs": {"type": "array", "items": {"type": "string"},
                             "description": "Nodes to update"},
                    "status": {"type": "string", "description": "Target status"},
                    "force": {"type": "boolean", "description": "Allow guarded transitions"}
                },
                "required": ["refs", "status"]
            }),
            (false, false, false),
        ),
        // ------------------------------------------------ поиск и обзоры
        "tasq_search" => tool(
            name,
            "Full-text search across nodes and notes",
            json!({
                "type": "object",
                "properties": {"query": {"type": "string", "description": "Words or an FTS query"}},
                "required": ["query"]
            }),
            (true, false, true),
        ),
        "tasq_dashboard" => tool(
            name,
            "One-call overview: board, live leases, next candidates, at-risk nodes",
            json!({"type": "object", "properties": {}}),
            (true, false, true),
        ),
        "tasq_burnup" => tool(
            name,
            "Burn-up series and forecast by count, points or hours",
            json!({
                "type": "object",
                "properties": {
                    "metric": {"type": "string", "enum": ["count", "points", "hours"]},
                    "since": {"type": "string", "description": "YYYY-MM-DD"},
                    "until": {"type": "string", "description": "YYYY-MM-DD"},
                    "ascii": {"type": "boolean", "description": "Include an ASCII chart"}
                }
            }),
            (true, false, true),
        ),
        "tasq_risk" => tool(
            name,
            "Nodes that fall behind their own due date",
            json!({
                "type": "object",
                "properties": {"threshold": {"type": "number", "description": "Allowed lag, 0..1"}}
            }),
            (true, false, true),
        ),
        // ------------------------------------------------ перенос данных
        "tasq_export" => tool(
            name,
            "Export the project as JSON with a checksum",
            json!({
                "type": "object",
                "properties": {"include_deleted": {"type": "boolean", "description": "Keep removed nodes"}}
            }),
            (true, false, true),
        ),
        "tasq_import" => tool(
            name,
            "Import an export document, from tasq or from saga-mcp",
            json!({
                "type": "object",
                "properties": {
                    "document": {"type": "object", "description": "Export payload"},
                    "mode": {"type": "string", "enum": ["merge", "replace"]},
                    "from_saga": {"type": "boolean", "description": "Read a saga tracker_export"}
                },
                "required": ["document"]
            }),
            (false, false, false),
        ),
        _ => return None,
    };
    Some(def)
}

fn tool(name: &str, description: &str, schema: Value, hints: (bool, bool, bool)) -> Value {
    let (read_only, destructive, idempotent) = hints;
    json!({
        "name": name,
        "description": description,
        "annotations": {
            "title": name,
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": idempotent,
            "openWorldHint": false,
        },
        "inputSchema": schema,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::profile::ALL_TOOLS;

    #[test]
    fn every_tool_in_the_catalogue_has_a_definition() {
        for name in ALL_TOOLS {
            assert!(
                definition(name).is_some(),
                "инструмент {name} объявлен в каталоге, но схемы нет"
            );
        }
    }

    #[test]
    fn descriptions_stay_within_the_budget() {
        for name in ALL_TOOLS {
            let def = definition(name).expect("definition");
            let description = def["description"].as_str().expect("description");
            assert!(
                description.chars().count() <= 200,
                "{name}: описание {} символов",
                description.chars().count()
            );
            let properties = def["inputSchema"]["properties"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            assert!(
                properties.len() <= 8,
                "{name}: {} параметров",
                properties.len()
            );
            for (key, value) in properties {
                if let Some(text) = value.get("description").and_then(Value::as_str) {
                    assert!(
                        text.chars().count() <= 80,
                        "{name}.{key}: описание параметра {} символов",
                        text.chars().count()
                    );
                }
            }
        }
    }

    #[test]
    fn every_tool_carries_annotations() {
        for name in ALL_TOOLS {
            let def = definition(name).expect("definition");
            let annotations = &def["annotations"];
            assert_eq!(annotations["openWorldHint"], json!(false), "{name}");
            assert!(annotations["readOnlyHint"].is_boolean(), "{name}");
            assert!(annotations["destructiveHint"].is_boolean(), "{name}");
            assert!(annotations["idempotentHint"].is_boolean(), "{name}");
        }
    }

    #[test]
    fn mutating_tools_are_not_marked_read_only() {
        for name in ["node_create", "node_update", "tasq_claim", "tasq_import"] {
            let def = definition(name).expect("definition");
            assert_eq!(def["annotations"]["readOnlyHint"], json!(false), "{name}");
        }
        for name in ["node_list", "tasq_dashboard", "tasq_search"] {
            let def = definition(name).expect("definition");
            assert_eq!(def["annotations"]["readOnlyHint"], json!(true), "{name}");
        }
    }
}
