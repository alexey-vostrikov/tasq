//! Каталог инструментов MCP: схемы и обработчики (spec §13.2).
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

/// Контекст вызова: открытая база, выбранный проект и каталог для файловых операций.
///
/// Проект **необязателен**: он выбирается явно (`project_select` или аргумент `project` у
/// вызова), а «первого по порядку создания» не бывает. Пока проект не выбран, инструменты,
/// работающие внутри одного проекта, отказывают `PROJECT_REQUIRED` со списком ключей —
/// инструменты уровня базы (`project_list`, `project_create`, `tasq_init`) работают всегда.
pub struct Context {
    pub conn: Connection,
    pub active: Option<Project>,
    pub db_path: PathBuf,
    pub cwd: PathBuf,
    /// Идентификатор этой сессии: свой у каждого MCP-процесса.
    ///
    /// По нему `next` знает, к какому проекту сессия привязана, а аренда — кто именно её взял.
    /// Три параллельных сессии «hermes» различаются только этим полем: `holder` у них общий.
    pub session: String,
}

impl Context {
    /// Проект вызова: аргумент `project` важнее выбранного в сессии.
    ///
    /// Возвращается владеемый `Project`, а не ссылка: иначе вызов вида
    /// `node::delete(&mut ctx.conn, &project, …)` не собрался бы — изменяемое заимствование
    /// поля и неизменяемое всего контекста в одном вызове.
    pub fn project(&self, args: &Value) -> Result<Project> {
        if let Some(key) = arg_str(args, "project") {
            return crate::project::select(&self.conn, Some(key));
        }
        if let Some(active) = &self.active {
            return Ok(active.clone());
        }
        // Проект не выбран и не задан — но ссылка может нести ключ (`DEV-1.1`): адрес узла
        // самодостаточен, и сессия без проекта обязана уметь открыть то, что ей назвали.
        if let Some(reference) = referenced_argument(args)
            && let Some(key) = crate::node::ref_key(reference)
        {
            return crate::project::select(&self.conn, Some(&key));
        }
        // В базе с единственным проектом выбора нет — это не дефолт среди вариантов.
        crate::project::select(&self.conn, None)
    }

    /// Ключ активного проекта для ответов (`null`, если проект не выбран).
    fn active_key(&self) -> Value {
        match &self.active {
            Some(project) => Value::from(project.key.clone()),
            None => Value::Null,
        }
    }
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

/// Первый аргумент-ссылка вызова: по ним проект определяется, когда он не задан явно.
const REFERENCE_ARGUMENTS: &[&str] = &["ref", "reference", "parent", "from", "to", "node"];

fn referenced_argument(args: &Value) -> Option<&str> {
    REFERENCE_ARGUMENTS
        .iter()
        .filter_map(|key| arg_str(args, key))
        .find(|value| crate::node::ref_key(value).is_some())
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

/// Список строк из массива аргумента: `["dot_id","title"]`.
///
/// Строка вместо массива — отказ, а не молчаливое «одно поле из запятой»: клиенты MCP передают
/// массивы массивами, и догадка о чужой форме ввода скрыла бы ошибку вызова.
fn arg_strings(args: &Value, key: &str) -> Result<Vec<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()),
        Some(other) => Err(Error::InvalidValue {
            field: key.to_string(),
            value: format!("{other} (expected an array of strings)"),
        }),
    }
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
    // Замер вокруг вызова: имя инструмента, длительность и код ошибки. Пишется после ответа и в
    // отдельный файл (`crate::metrics`), поэтому чтение через MCP по-прежнему не трогает базу.
    let started = std::time::Instant::now();
    // Типы проверяются по опубликованной схеме до обработчика: иначе `weight: "много"`
    // молча превращался в значение по умолчанию, и агент не узнавал, что ошибся.
    let result = check_arguments(name, args).and_then(|()| dispatch(ctx, name, args));
    crate::metrics::record(
        &ctx.db_path,
        name,
        started.elapsed(),
        result.as_ref().err().map(|err| err.code()).unwrap_or("ok"),
        crate::metrics::take_retries(),
    );
    match result {
        Ok(value) => value,
        Err(err) => error_content(&err),
    }
}

/// Сверяет аргументы с объявленными типами схемы инструмента.
///
/// Проверяется только верхний уровень и элементы массивов строк: глубже начинается
/// доменный разбор документа, у которого свои коды. `null` означает «не передано».
fn check_arguments(name: &str, args: &Value) -> Result<()> {
    let definition = definition(name);
    let Some(schema) = definition.as_ref().and_then(|def| def.get("inputSchema")) else {
        return Ok(());
    };
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Ok(());
    };
    let Some(arguments) = args.as_object() else {
        return Err(Error::InvalidValue {
            field: "arguments".to_string(),
            value: format!("{args} (expected an object)"),
        });
    };

    for (key, value) in arguments {
        if value.is_null() {
            continue;
        }
        let Some(expected) = properties.get(key).and_then(|prop| prop.get("type")) else {
            continue; // Лишние поля игнорируются: клиенты добавляют свои.
        };
        let expected = expected.as_str().unwrap_or_default();
        let ok = match expected {
            "string" => value.is_string(),
            "number" => value.is_number(),
            "integer" => value.as_i64().is_some(),
            "boolean" => value.is_boolean(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            _ => true,
        };
        if !ok {
            return Err(Error::InvalidValue {
                field: key.clone(),
                value: format!("{value} (expected {expected})"),
            });
        }
    }
    Ok(())
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
            // Проект создан, но контекст не переключён: `switched` и подсказка объясняют,
            // каким вызовом это сделать. Молчаливая смена опаснее лишнего шага — агент,
            // создавший проект «на будущее», иначе писал бы узлы в него не заметив.
            let active = ctx.active_key();
            let hint = match &ctx.active {
                Some(current) => format!(
                    "this session still works in {}; call project_select with key {} to switch",
                    current.key, project.key
                ),
                None => format!(
                    "no project is selected in this session; call project_select with key {}",
                    project.key
                ),
            };
            Ok(text(&json!({
                "project": project,
                "active_project": active,
                "switched": false,
                "hint": hint,
            })))
        }
        "project_select" => {
            let key = req_str(args, "key")?;
            let project = crate::project::select(&ctx.conn, Some(key))?;
            let previous = ctx.active.as_ref().map(|active| active.key.clone());
            ctx.active = Some(project.clone());
            Ok(text(&json!({
                "active_project": project.key,
                "name": project.name,
                "previous": previous,
                // Ничего не было выбрано — это не «переключение», и ответ это различает.
                "switched": previous.as_deref() != Some(project.key.as_str()),
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
            // Без `key` правим проект сессии; если его нет — отказ, а не «первый по порядку».
            let target = match arg_str(args, "key") {
                Some(key) => key.to_string(),
                None => ctx.project(args)?.key,
            };
            let patch = crate::project::Patch {
                name: arg_str(args, "name"),
                description: arg_str(args, "description"),
                status: arg_str(args, "status"),
                key: arg_str(args, "new_key"),
            };
            let (updated, previous_key) =
                crate::project::update(&mut ctx.conn, Some(&target), &patch)?;
            // Если правили проект сессии (в том числе переименовали ключ) — контекст
            // должен остаться на нём, а не съехать на другой проект.
            if let Some(active) = &ctx.active
                && active.id == updated.id
            {
                ctx.active = Some(updated.clone());
            }
            Ok(text(&json!({
                "project": updated,
                "previous_key": previous_key,
                "active_project": ctx.active_key(),
                "nodes": crate::project::live_nodes(&ctx.conn, &updated)?,
            })))
        }
        "project_delete" => {
            let target = match arg_str(args, "key") {
                Some(key) => key.to_string(),
                None => ctx.project(args)?.key,
            };
            let deleting_active = {
                let project = crate::project::select(&ctx.conn, Some(&target))?;
                ctx.active.as_ref().map(|active| active.id) == Some(project.id)
            };
            let nodes = {
                let project = crate::project::select(&ctx.conn, Some(&target))?;
                crate::project::live_nodes(&ctx.conn, &project)?
            };
            let removed =
                crate::project::delete(&mut ctx.conn, Some(&target), arg_bool(args, "force"))?;
            // Удалили проект сессии — контекст больше ни на что не указывает. Если в базе
            // остался ровно один проект, сессия переезжает в него (выбора нет — есть одна
            // возможность); если их несколько, активного снова нет и нужен project_select.
            let mut switched_to = None;
            if deleting_active {
                ctx.active = None;
                // Единственный оставшийся проект — не «дефолт», а единственная
                // возможность: сессия переезжает туда и говорит об этом (`switched_to`).
                match crate::project::first_project_if_single(&ctx.conn) {
                    Ok(Some(next)) => {
                        switched_to = Some(next.key.clone());
                        ctx.active = Some(next);
                    }
                    Ok(None) => {
                        // Пустая база и несколько проектов — разные подсказки: в первом
                        // случае выбирать не из чего, во втором нужен явный выбор.
                        let hint = if crate::project::count(&ctx.conn)? == 0 {
                            "no projects left; call tasq_init or project_create"
                        } else {
                            "several projects remain; call project_list and then project_select"
                        };
                        return Ok(text(&json!({
                            "deleted": removed.key,
                            "nodes": nodes,
                            "active_project": Value::Null,
                            "hint": hint,
                        })));
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
                "active_project": ctx.active_key(),
                "switched_to": switched_to,
                "remaining": crate::project::known_keys(&ctx.conn)?,
            })))
        }

        // ------------------------------------------------ узлы
        "node_create" => {
            let project = ctx.project(args)?;
            let kind = arg_kind(args, "kind")?;
            // Метки при создании, а не отдельным вызовом: иначе первая запись журнала про узел
            // была бы неполной, а вторая — про метки, которых у него «не было».
            let tags = arg_strings(args, "tags")?;
            let created = node::create(
                &mut ctx.conn,
                &project,
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
                    tags: (!tags.is_empty()).then_some(tags.as_slice()),
                },
            )?;
            Ok(text(&created))
        }
        "node_get" => {
            let project = ctx.project(args)?;
            let (_, node) = node::resolve_shown(&ctx.conn, &project, req_str(args, "ref")?)?;
            let parent = if node.depth > 1 {
                node.path
                    .rsplit_once('.')
                    .and_then(|(head, _)| node::resolve(&ctx.conn, &project, head).ok())
                    .map(|parent| parent.dot_id)
            } else {
                None
            };
            let children = node::list(
                &ctx.conn,
                &project,
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
            let comments = comment::list(&ctx.conn, &project, &node.dot_id, false)?;
            Ok(text(&json!({
                "node": node,
                "parent": parent,
                "children": children,
                "comments": comments,
            })))
        }
        "node_update" => {
            let project = ctx.project(args)?;
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
            // Право аренды считается ДО захвата соединения: иначе заимствование
            // пересекается с `&mut ctx.conn`.
            let lease_node = match arg_str(args, "token") {
                Some(token) => crate::lease::live_node(&ctx.conn, &project, token)?,
                None => None,
            };
            // Метки — замена набора, и это единственный параметр меток в MCP. Добавление и
            // снятие поштучно остались в CLI: агенту проще прочитать узел и записать набор, чем
            // держать третий-четвёртый параметр в схеме, которая стоит токенов в каждом сеансе.
            // Свободные поля (`metadata`) в MCP не выведены вовсе: это свойство для инструментов и
            // человека, а не то, что агент правит в работе.
            let set_tags = arg_strings(args, "tags")?;
            let updated = node::update(
                &mut ctx.conn,
                &project,
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
                    tag_add: &[],
                    tag_remove: &[],
                    tags: (!set_tags.is_empty()).then_some(set_tags.as_slice()),
                    meta_set: &[],
                    meta_remove: &[],
                    force: arg_bool(args, "force"),
                    description_locked,
                    // Токен должен быть живым и указывать на этот же узел: иначе
                    // любой запрос с полем `token` ставил бы в работу чужую задачу.
                    lease: lease_node,
                    actor: Some(&arg_holder(args)),
                },
            )?;
            Ok(text(&updated))
        }
        "node_list" => {
            let project = ctx.project(args)?;
            let excluded_projects: Vec<String> = args
                .get("exclude_projects")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let nodes = node::list(
                &ctx.conn,
                &project,
                &node::Filter {
                    parent: arg_str(args, "parent"),
                    status: arg_str(args, "status"),
                    assignee: arg_str(args, "assignee"),
                    tag: arg_str(args, "tag"),
                    branch: arg_str(args, "branch"),
                    exclude_projects: &excluded_projects,
                    exclude_done: arg_bool(args, "exclude_done"),
                    overdue: arg_bool(args, "overdue"),
                    include_archived: arg_bool(args, "include_archived"),
                    include_deleted: false,
                    limit: arg_i64(args, "limit"),
                    offset: arg_i64(args, "offset"),
                },
            )?;
            // Выбор полей: без `fields` ответ прежний, с ним — только запрошенное. Это главный
            // способ не отдавать агенту двести килобайт описаний ради адреса и статуса.
            let fields = arg_strings(args, "fields")?;
            if fields.is_empty() {
                return Ok(text(&nodes));
            }
            let joined = fields.join(",");
            let selected = node::parse_fields(&joined)?;
            let rows: Vec<Value> = nodes
                .iter()
                .map(|node| node::project_json(node, &selected))
                .collect();
            Ok(text(&rows))
        }
        "node_move" => {
            let project = ctx.project(args)?;
            // Перенос в другой проект — отдельная ветка: адрес перестраивается, и ответ обязан
            // назвать прежний, новый и число переписанных ссылок.
            if let Some(key) = arg_str(args, "to_project") {
                let report = node::move_to_project(
                    &mut ctx.conn,
                    &project,
                    req_str(args, "ref")?,
                    key,
                    arg_str(args, "parent"),
                    arg_bool(args, "renumber"),
                )?;
                return Ok(text(&report));
            }
            let moved = node::move_node(
                &mut ctx.conn,
                &project,
                req_str(args, "ref")?,
                arg_str(args, "parent"),
                arg_bool(args, "renumber"),
            )?;
            Ok(text(&moved))
        }
        "node_reorder" => {
            let project = ctx.project(args)?;
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
            node::reorder(&mut ctx.conn, &project, &refs)?;
            Ok(text(&json!({"reordered": refs.len()})))
        }
        "node_archive" => {
            let project = ctx.project(args)?;
            let affected = node::archive(
                &mut ctx.conn,
                &project,
                req_str(args, "ref")?,
                arg_bool(args, "undo"),
            )?;
            Ok(text(&json!({"affected": affected})))
        }
        "node_delete" => {
            let project = ctx.project(args)?;
            node::delete(&mut ctx.conn, &project, req_str(args, "ref")?)?;
            Ok(text(&json!({"deleted": true})))
        }
        "node_restore" => {
            let project = ctx.project(args)?;
            let restored = node::restore(&mut ctx.conn, &project, req_str(args, "ref")?)?;
            Ok(text(&restored))
        }

        // ------------------------------------------------ дерево и рекомендации
        "tasq_context" => {
            let project = ctx.project(args)?;
            let chain = overview::context(&ctx.conn, &project, req_str(args, "ref")?)?;
            Ok(text(&chain))
        }
        "tasq_history" => {
            let project = ctx.project(args)?;
            let (_, node) = node::resolve_shown(&ctx.conn, &project, req_str(args, "ref")?)?;
            let transitions = crate::history::timeline(&ctx.conn, node.id)?;
            Ok(text(&serde_json::json!({
                "dot_id": node.dot_id,
                "status": node.status,
                "transitions": transitions,
            })))
        }
        "tasq_next" => {
            let limit = arg_i64(args, "limit").unwrap_or(3).clamp(1, 20) as usize;
            // Явный `project` в аргументах сильнее привязки — как и в CLI.
            if arg_str(args, "project").is_some() {
                let project = ctx.project(args)?;
                let candidates =
                    edge::next_candidates_on(&ctx.conn, &project, limit, arg_str(args, "branch"))?;
                return Ok(text(&candidates));
            }
            let affinity = crate::session::affinity(&ctx.conn, &ctx.session)?;
            let next = edge::next_across_on(
                &ctx.conn,
                affinity.as_deref(),
                limit,
                arg_str(args, "branch"),
            )?;
            // Ответ остаётся массивом кандидатов: клиенты разбирают его как список, и смена
            // формы сломала бы их ради сведений, которые умещаются в поле строки. Откуда взят
            // кандидат, видно в `origin` каждого элемента (`affinity`/`fallback`/`unbound`).
            Ok(text(&next.candidates))
        }

        // ------------------------------------------------ аренды
        "tasq_claim" => {
            let project = ctx.project(args)?;
            let holder = arg_holder(args);
            let held = lease::claim_as(
                &mut ctx.conn,
                &project,
                req_str(args, "ref")?,
                &lease::ClaimRequest {
                    holder: &holder,
                    ttl_seconds: arg_i64(args, "ttl").unwrap_or(lease::DEFAULT_TTL),
                    steal: arg_bool(args, "steal"),
                    force: arg_bool(args, "force"),
                    checkpoint: arg_str(args, "checkpoint"),
                    reservations: arg_resources(args),
                },
                // Взятие работы привязывает сессию к проекту: «работала здесь» — не то же
                // самое, что «смотрела сюда».
                Some(ctx.session.as_str()),
            )?;
            Ok(text(&held))
        }
        "tasq_renew" => {
            let project = ctx.project(args)?;
            let renewed = lease::renew(
                &mut ctx.conn,
                &project,
                req_str(args, "token")?,
                arg_i64(args, "ttl").unwrap_or(lease::DEFAULT_TTL),
            )?;
            Ok(text(&renewed))
        }
        "tasq_release" => {
            let project = ctx.project(args)?;
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
                &project,
                req_str(args, "token")?,
                outcome,
                arg_str(args, "checkpoint"),
                arg_bool(args, "force"),
            )?;
            Ok(text(&released))
        }

        // ------------------------------------------------ ресурсы
        "tasq_reserve" => {
            let project = ctx.project(args)?;
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
                let removed = lease::unreserve(&mut ctx.conn, &project, token, Some(&values))?;
                let add = arg_resources(args);
                if add.is_empty() {
                    return Ok(text(&json!({"released": removed})));
                }
                let held = lease::reserve(&mut ctx.conn, &project, token, &add)?;
                return Ok(text(&json!({"released": removed, "held": held})));
            }
            let add = arg_resources(args);
            if add.is_empty() {
                return Ok(text(
                    &json!({"released": lease::unreserve(&mut ctx.conn, &project, token, None)?}),
                ));
            }
            let held = lease::reserve(&mut ctx.conn, &project, token, &add)?;
            Ok(text(&held))
        }
        "tasq_holdings" => {
            let project = ctx.project(args)?;
            let live = lease::holdings(&ctx.conn, &project)?;
            Ok(text(&live))
        }

        // ------------------------------------------------ зависимости
        "tasq_link" => {
            let project = ctx.project(args)?;
            let kind =
                edge::Kind::parse(arg_str(args, "kind").unwrap_or("blocks")).ok_or_else(|| {
                    Error::InvalidValue {
                        field: "kind".to_string(),
                        value: "blocks|related|duplicates|discovered_from|split_from".to_string(),
                    }
                })?;
            let linked = edge::link(
                &mut ctx.conn,
                &project,
                req_str(args, "from")?,
                req_str(args, "to")?,
                kind,
                &arg_holder(args),
            )?;
            Ok(text(&linked))
        }
        "tasq_unlink" => {
            let project = ctx.project(args)?;
            let kind =
                edge::Kind::parse(arg_str(args, "kind").unwrap_or("blocks")).ok_or_else(|| {
                    Error::InvalidValue {
                        field: "kind".to_string(),
                        value: "blocks|related|duplicates|discovered_from|split_from".to_string(),
                    }
                })?;
            let removed = edge::unlink(
                &mut ctx.conn,
                &project,
                req_str(args, "from")?,
                req_str(args, "to")?,
                kind,
                &arg_holder(args),
            )?;
            Ok(text(&json!({"removed": removed})))
        }
        "tasq_graph" => {
            let project = ctx.project(args)?;
            let graph = edge::graph(
                &ctx.conn,
                &project,
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
            let project = ctx.project(args)?;
            let holder = arg_holder(args);
            let added = comment::add(
                &mut ctx.conn,
                &project,
                req_str(args, "ref")?,
                req_str(args, "content")?,
                &holder,
            )?;
            Ok(text(&added))
        }
        "comment_list" => {
            let project = ctx.project(args)?;
            let comments = comment::list(
                &ctx.conn,
                &project,
                req_str(args, "ref")?,
                arg_bool(args, "include_deleted"),
            )?;
            Ok(text(&comments))
        }
        "comment_update" => {
            // Правка комментария, а не удаление и запись заново: дата «когда это решили»
            // принадлежит истории, и подменять её датой переписывания нельзя (см. `comment::edit`).
            let id = arg_i64(args, "id").ok_or_else(|| Error::InvalidValue {
                field: "id".to_string(),
                value: "required".to_string(),
            })?;
            let content = req_str(args, "content")?;
            let project = ctx.project(args)?;
            let author = arg_holder(args);
            let comment = comment::edit(&mut ctx.conn, &project, id, content, &author)?;
            Ok(text(&comment))
        }
        "comment_delete" => {
            let project = ctx.project(args)?;
            let id = arg_i64(args, "id").ok_or_else(|| Error::InvalidValue {
                field: "id".to_string(),
                value: "required".to_string(),
            })?;
            let holder = arg_holder(args);
            let result = if arg_bool(args, "restore") {
                comment::restore(&mut ctx.conn, &project, id, &holder)?
            } else {
                comment::delete(
                    &mut ctx.conn,
                    &project,
                    id,
                    &holder,
                    arg_str(args, "reason"),
                )?
            };
            Ok(text(&result))
        }

        // ------------------------------------------------ заметки
        "note_save" => {
            let project = ctx.project(args)?;
            let saved = note::save(
                &mut ctx.conn,
                &project,
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
            let project = ctx.project(args)?;
            let notes = note::list(
                &ctx.conn,
                &project,
                &note::Filter {
                    note_type: arg_str(args, "note_type"),
                    node: arg_str(args, "ref"),
                    limit: arg_i64(args, "limit"),
                },
            )?;
            Ok(text(&notes))
        }
        "note_search" => {
            let project = ctx.project(args)?;
            let notes = note::search(&ctx.conn, &project, req_str(args, "query")?)?;
            Ok(text(&notes))
        }
        "note_delete" => {
            let project = ctx.project(args)?;
            let id = arg_i64(args, "id").ok_or_else(|| Error::InvalidValue {
                field: "id".to_string(),
                value: "required".to_string(),
            })?;
            note::delete(&mut ctx.conn, &project, id, &arg_holder(args))?;
            Ok(text(&json!({"removed": true})))
        }

        // ------------------------------------------------ шаблоны
        "template_save" => {
            let project = ctx.project(args)?;
            let tree = args.get("tree").ok_or_else(|| Error::InvalidValue {
                field: "tree".to_string(),
                value: "required".to_string(),
            })?;
            let saved = template::save(
                &mut ctx.conn,
                Some(&project),
                req_str(args, "name")?,
                arg_str(args, "description"),
                &serde_json::to_string(tree).unwrap_or_else(|_| "[]".to_string()),
                &arg_holder(args),
            )?;
            Ok(text(&saved))
        }
        "template_list" => {
            // Проект не требуется: без него видно всё — по проектам и общие. С проектом — свои и
            // общие, то есть ровно то, что можно применить в этом проекте.
            let templates = match &ctx.active {
                Some(project) => template::list(&ctx.conn, project)?,
                None => template::list_all(&ctx.conn)?,
            };
            Ok(text(&templates))
        }
        "template_apply" => {
            let project = ctx.project(args)?;
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
                &project,
                req_str(args, "name")?,
                arg_str(args, "parent"),
                &values,
            )?;
            Ok(text(&applied))
        }
        "template_delete" => {
            let project = ctx.project(args)?;
            template::delete(
                &mut ctx.conn,
                Some(&project),
                req_str(args, "name")?,
                &arg_holder(args),
            )?;
            Ok(text(&json!({"removed": true})))
        }

        // ------------------------------------------------ активность
        "activity_log" => {
            let project = ctx.project(args)?;
            let limit = arg_i64(args, "limit").unwrap_or(20).clamp(1, 200);
            let records = crate::activity::recent(&ctx.conn, project.id, limit)?;
            Ok(text(&records))
        }
        "session_diff" => {
            let project = ctx.project(args)?;
            let since = req_str(args, "since")?;
            let mut stmt = ctx.conn.prepare(
                "SELECT action, entity_type, COUNT(*)
                 FROM activity_log
                 WHERE project_id = ?1 AND created_at >= ?2
                 GROUP BY action, entity_type ORDER BY COUNT(*) DESC",
            )?;
            let rows = stmt.query_map(rusqlite::params![project.id, since], |row| {
                Ok(json!({
                    "action": row.get::<_, String>(0)?,
                    "entity": row.get::<_, String>(1)?,
                    "count": row.get::<_, i64>(2)?,
                }))
            })?;
            let grouped: Vec<Value> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            let records = crate::activity::recent(&ctx.conn, project.id, 50)?
                .into_iter()
                .filter(|record| record.created_at.as_str() >= since)
                .collect::<Vec<_>>();
            Ok(text(
                &json!({"since": since, "summary": grouped, "recent": records}),
            ))
        }
        "node_batch_update" => {
            let project = ctx.project(args)?;
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
                &project,
                &refs,
                status,
                arg_bool(args, "force"),
                &holder,
            )?;
            Ok(text(&result))
        }

        // ------------------------------------------------ поиск и обзоры
        "tasq_search" => {
            let query = req_str(args, "query")?;
            // Без названного проекта поиск идёт по всей базе: ответ «не найдено» на
            // существующую задачу хуже, чем находка из чужого проекта — она хотя бы правдива.
            let found = match arg_str(args, "project") {
                Some(_) => {
                    let project = ctx.project(args)?;
                    overview::search(&ctx.conn, &project, query)?
                }
                None => overview::search_all(&ctx.conn, query)?,
            };
            // Ответ едет целиком, с пометкой о качестве: агент, получивший список без
            // ранжирования, должен знать это из ответа, а не догадываться по порядку строк.
            Ok(text(&found))
        }
        "tasq_stats" => {
            // Метрики читаются из файла рядом с базой, а не из неё: сам трекер работы не пишет в
            // данные, которые отслеживает (`crate::metrics`).
            // Окно — сутки: у инструмента один вопрос и один ответ. Прune и произвольное окно
            // остаются в CLI, где стоимость схемы никого не ограничивает.
            let since = crate::metrics::parse_window(None)?;
            let report = crate::metrics::report(&ctx.db_path, &since, false)?;
            Ok(text(&report))
        }
        "tasq_dashboard" => {
            // `all` — обзор по каждому проекту: один вызов вместо обхода проектов снаружи.
            if arg_bool(args, "all") {
                let boards = overview::board_all(&ctx.conn)?;
                let mut sections = Vec::new();
                for board in boards {
                    let Some(project) = crate::project::by_key(&ctx.conn, &board.project)? else {
                        continue;
                    };
                    sections.push(json!({
                        "project": project.key,
                        "board": board,
                        "leases": lease::holdings(&ctx.conn, &project)?,
                        "next": edge::next_candidates(&ctx.conn, &project, 3)?,
                        "at_risk": crate::burnup::at_risk(&ctx.conn, &project, 0.2)?,
                    }));
                }
                return Ok(text(&sections));
            }
            let project = ctx.project(args)?;
            let board = overview::board(&ctx.conn, &project)?;
            let live = lease::holdings(&ctx.conn, &project)?;
            let next = edge::next_candidates(&ctx.conn, &project, 3)?;
            let risky = crate::burnup::at_risk(&ctx.conn, &project, 0.2)?;
            Ok(text(&json!({
                "project": project.key,
                "board": board,
                "leases": live,
                "next": next,
                "at_risk": risky,
            })))
        }
        "tasq_burnup" => {
            let project = ctx.project(args)?;
            let metric = crate::burnup::Metric::parse(arg_str(args, "metric").unwrap_or("count"))
                .ok_or_else(|| Error::InvalidValue {
                field: "metric".to_string(),
                value: "count|points|hours".to_string(),
            })?;
            let series = crate::burnup::series(
                &ctx.conn,
                &project,
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
            let project = ctx.project(args)?;
            let risky = crate::burnup::at_risk(
                &ctx.conn,
                &project,
                arg_f64(args, "threshold").unwrap_or(0.2),
            )?;
            Ok(text(&risky))
        }

        // ------------------------------------------------ перенос данных
        "tasq_export" => {
            let project = ctx.project(args)?;
            let document =
                transfer::export(&ctx.conn, &project, arg_bool(args, "include_deleted"))?;
            Ok(text(&document))
        }
        "tasq_import" => {
            let project = ctx.project(args)?;
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
                transfer::import_saga(&mut ctx.conn, &project, document, mode, &arg_holder(args))?
            } else {
                transfer::import(&mut ctx.conn, &project, document, mode, &arg_holder(args))?
            };
            Ok(text(&imported))
        }

        other => Err(Error::InvalidValue {
            field: "tool".to_string(),
            value: format!("unknown tool {other}"),
        }),
    }
}

/// Описание инструмента для `tools/list` — из единого объявления (`crate::surface`).
///
/// Здесь больше нет рукописных схем: имя, описание, аргументы и аннотации живут в таблице, и
/// `tools/list`, `docs/mcp-tools.md` и бюджет токенов строятся из неё. Схема на проводе и
/// документация перестали быть двумя копиями одного контракта — они теперь два вывода одного.
///
/// Правила бюджета (spec §13.2) остаются: описание ≤ 200 символов, описание параметра ≤ 80,
/// параметров ≤ 8, аннотации на каждом инструменте. Их проверяет `tests` внизу файла — по
/// сгенерированной схеме, то есть по тому, что реально увидит модель.
pub fn definition(name: &str) -> Option<Value> {
    let op = crate::surface::operation(name)?;
    Some(json!({
        "name": op.tool,
        "description": op.summary,
        "annotations": {
            "title": op.tool,
            "readOnlyHint": op.read_only,
            "destructiveHint": op.destructive,
            "idempotentHint": op.idempotent,
            "openWorldHint": false,
        },
        "inputSchema": schema(op),
    }))
}

/// JSON Schema вызова, собранная из объявления.
///
/// `properties` есть всегда (пустой объект, если аргументов нет), `required` — только когда есть
/// обязательные: пустой список требований ничего не добавляет, а в схеме читается как «требований
/// нет» и всё же занимает место в бюджете.
fn schema(op: &crate::surface::Operation) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required: Vec<Value> = Vec::new();
    for arg in op.args {
        let mut spec = serde_json::Map::new();
        spec.insert("type".to_string(), json!(arg.kind.json_type()));
        if let crate::surface::Kind::Choice(variants) = arg.kind {
            spec.insert("enum".to_string(), json!(variants));
        }
        if matches!(arg.kind, crate::surface::Kind::StrList) {
            spec.insert("items".to_string(), json!({"type": "string"}));
        }
        // Пустое описание не пишем вовсе: у перечислений оно не нужно (значения говорят сами),
        // а ключ с пустой строкой занимает место в бюджете схемы и выглядит как недоделка.
        if !arg.description.is_empty() {
            spec.insert("description".to_string(), json!(arg.description));
        }
        properties.insert(arg.name.to_string(), Value::Object(spec));
        if arg.required {
            required.push(json!(arg.name));
        }
    }
    // Порядок производный, а не порядок объявления: `required` — множество, и зависеть от того,
    // как расставлены `Arg` в таблице, ему незачем. Отсортированный список ещё и сравнивается
    // со снимком схемы без ложных расхождений.
    required.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
    let mut root = serde_json::Map::new();
    root.insert("type".to_string(), json!("object"));
    root.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        root.insert("required".to_string(), Value::Array(required));
    }
    Value::Object(root)
}

/// Инструменты, которые живут вне проекта: им `project` не нужен и не должен предлагаться.
///
/// Список живёт в тестах намеренно: в рабочем коде его роль играет сама таблица — `project`
/// объявлен аргументом ровно у тех операций, которым он нужен, и никакой подстановки при
/// сборке схемы нет. Здесь он нужен как независимое ожидание контракта: если кто-то уберёт
/// `project` из таблицы, тест это заметит, а не подтвердит сам себя.
#[cfg(test)]
const PROJECT_FREE: &[&str] = &[
    "tasq_init",
    // Метрики считаются по всей базе, а не по проекту: файл рядом с базой один, и вопрос
    // «сколько звали» не имеет проектного измерения. Подставить `project` значило бы обещать
    // фильтр, которого нет.
    "tasq_stats",
    "project_create",
    "project_select",
    "project_list",
    "project_update",
    "project_delete",
];

/// Инструменты, которым разрешено больше параметров, чем общий предел, — с причиной.
///
/// Предел существует ради бюджета схемы, а не ради симметрии: каждый параметр стоит токенов в
/// КАЖДОМ сеансе. Но у `node_list` это единственный способ спросить список, и разнести его
/// фильтры по двум инструментам стоило бы дороже (второй тул целиком: имя, описание, схема),
/// чем несколько параметров в уже существующем. Предел поднят адресно — одному инструменту, а не
/// всем: общий потолок, поднятый ради одного, перестаёт сдерживать остальные.
///
/// `node_create` — девятый параметр `tags`: метки пишутся при создании, а не отдельным `set`,
/// иначе первая запись журнала про узел была бы неполной, а вторая — про то, чего у него «не
/// было». Альтернатива (создать и потом `node_update`) стоила бы двух вызовов и двух записей
/// вместо одной.
#[cfg(test)]
const WIDER_THAN_USUAL: &[(&str, usize)] =
    &[("node_list", 12), ("node_create", 9), ("node_update", 9)];

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
            // Предел считает СВОИ параметры инструмента; общий селектор `project`
            // добавляется всем сразу и в этот счёт не входит.
            let own = properties
                .keys()
                .filter(|key| key.as_str() != "project")
                .count();
            let ceiling = WIDER_THAN_USUAL
                .iter()
                .find(|(wide, _)| *wide == *name)
                .map(|(_, allowed)| *allowed)
                .unwrap_or(8);
            assert!(own <= ceiling, "{name}: {own} параметров, предел {ceiling}");
            // Контракт селектора: он объявлен у всех, кто живёт внутри проекта, и отсутствует
            // у инструментов уровня базы. Иначе агент снова не узнает о возможности.
            assert_eq!(
                properties.contains_key("project"),
                !PROJECT_FREE.contains(name),
                "{name}: объявление project расходится с PROJECT_FREE"
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
