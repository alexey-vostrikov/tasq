//! Шаблоны: набор задач с подстановкой `{переменных}` (README §5).
//!
//! Шаблон — это дерево, а не один список: у элемента бывают дети, поэтому типовой
//! набор работ («новая ручка API») разворачивается в задачу с подзадачами.

use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::activity::{self, Entry};
use crate::db::now_iso;
use crate::error::{Error, Result};
use crate::node::{self, NewNode};
use crate::project::Project;
use crate::status::Kind;

/// Узел шаблона.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateNode {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
    /// Метки узла; в них тоже работают `{переменные}`, как в заголовке и описании.
    ///
    /// Это то, чего не хватало при переносе из предшественника: там у задачи шаблона были
    /// `tags`, а здесь поле молча отбрасывалось — и колонка `nodes.tags` оставалась мёртвой.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TemplateNode>,
}

/// Дерево шаблона: либо массив корней, либо объект с полем `nodes`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TemplateTree {
    #[serde(default)]
    pub nodes: Vec<TemplateNode>,
}

impl TemplateTree {
    /// Разбор входа: принимает и `[...]`, и `{"nodes": [...]}`.
    pub fn parse(raw: &str) -> Result<Self> {
        let value: serde_json::Value =
            serde_json::from_str(raw).map_err(|err| Error::InvalidValue {
                field: "template".to_string(),
                value: format!("not valid JSON: {err}"),
            })?;
        let tree = match value {
            serde_json::Value::Array(_) => TemplateTree {
                nodes: serde_json::from_value(value).map_err(|err| Error::InvalidValue {
                    field: "template".to_string(),
                    value: err.to_string(),
                })?,
            },
            serde_json::Value::Object(_) => {
                serde_json::from_value(value).map_err(|err| Error::InvalidValue {
                    field: "template".to_string(),
                    value: err.to_string(),
                })?
            }
            _ => {
                return Err(Error::InvalidValue {
                    field: "template".to_string(),
                    value: "expected an array or an object with \"nodes\"".to_string(),
                });
            }
        };
        if tree.nodes.is_empty() {
            return Err(Error::InvalidValue {
                field: "template".to_string(),
                value: "no nodes".to_string(),
            });
        }
        Ok(tree)
    }

    /// Сколько узлов создаст применение — вместе с детьми всех уровней.
    ///
    /// Нужно снаружи (каталог шаблонов в `templates/` сверяется по этому числу): читатель файла
    /// не должен считать дерево руками, а тест — тем более, иначе две реализации счёта разойдутся.
    pub fn count(&self) -> usize {
        fn walk(node: &TemplateNode) -> usize {
            1 + node.children.iter().map(walk).sum::<usize>()
        }
        self.nodes.iter().map(walk).sum()
    }

    /// Имена всех `{переменных}` в дереве, в порядке появления.
    pub fn variables(&self) -> Vec<String> {
        let mut found = BTreeSet::new();
        for node in &self.nodes {
            collect_variables(node, &mut found);
        }
        found.into_iter().collect()
    }
}

fn collect_variables(node: &TemplateNode, out: &mut BTreeSet<String>) {
    for text in [Some(node.title.as_str()), node.description.as_deref()]
        .into_iter()
        .flatten()
    {
        let mut rest = text;
        while let Some(start) = rest.find('{') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('}') else { break };
            let name = &after[..end];
            if !name.is_empty() && !name.contains('{') {
                out.insert(name.to_string());
            }
            rest = &after[end + 1..];
        }
    }
    for child in &node.children {
        collect_variables(child, out);
    }
}

/// Подставляет значения переменных; неизвестная переменная — ошибка, а не «{name}» в тексте.
pub fn render(text: &str, values: &[(String, String)]) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            // Хвост без закрывающей скобки — обычный текст. Дописываем остаток от самой
            // скобки: `rest` уже включает напечатанный префикс, и он попал бы дважды.
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let name = &after[..end];
        match values.iter().find(|(key, _)| key == name) {
            Some((_, value)) => out.push_str(value),
            None => {
                return Err(Error::InvalidValue {
                    field: "template".to_string(),
                    value: format!("no value for variable {{{name}}}"),
                });
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Шаблон в выдаче.
#[derive(Debug, Clone, Serialize)]
pub struct Template {
    pub id: i64,
    /// Ключ проекта, которому шаблон принадлежит; `None` — шаблон общий для всей базы.
    ///
    /// Общий шаблон — это запись процесса, а не данные проекта: каталог `templates/` описывает,
    /// как ведётся работа, и ведётся она одинаково во всех проектах. Поле нужно читателю, чтобы
    /// ответ «без проекта» был однозначным: имя шаблона уникально внутри проекта, а не в базе,
    /// поэтому два проекта могут держать одноимённые шаблоны.
    pub project: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub tree: TemplateTree,
    pub variables: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Проект ключом, а не номером: ответ читает человек, и `project: 3` ему ничего не говорит.
/// `LEFT JOIN`, а не `JOIN`: у общего шаблона проекта нет, и внутреннее соединение выбросило бы
/// его из списка — то есть ровно тот шаблон, который мы хотим показать.
const SELECT: &str = "SELECT t.id, t.name, t.description, t.tree, t.created_at, t.updated_at, p.key
     FROM templates t LEFT JOIN projects p ON p.id = t.project_id";

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Template> {
    let raw: String = row.get(3)?;
    let tree = TemplateTree::parse(&raw).unwrap_or_default();
    Ok(Template {
        id: row.get(0)?,
        project: row.get(6)?,
        name: row.get(1)?,
        description: row.get(2)?,
        variables: tree.variables(),
        tree,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

/// Создаёт шаблон или заменяет существующий с тем же именем.
///
/// `project: None` — шаблон общий для всей базы. В журнал (`activity_log`) такая запись не
/// попадает: `activity_log.project_id` объявлен обязательным, а у общего шаблона проекта нет —
/// приписать ему чужой значило бы положить в аудит ложь. Видно это и по `updated_at`, и по
/// каталогу `templates/`, откуда общие шаблоны и берутся.
pub fn save(
    conn: &mut Connection,
    project: Option<&Project>,
    name: &str,
    description: Option<&str>,
    raw_tree: &str,
    actor: &str,
) -> Result<Template> {
    let project_id = project.map(|project| project.id);
    if name.trim().is_empty() {
        return Err(Error::InvalidValue {
            field: "name".to_string(),
            value: "empty".to_string(),
        });
    }
    let tree = TemplateTree::parse(raw_tree)?;
    validate(&tree)?;
    let now = now_iso();
    let stored = serde_json::to_string(&tree).expect("serialisable");

    let tx = crate::db::write_tx(conn)?;
    tx.execute(
        "INSERT INTO templates (project_id, name, description, tree, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)
         ON CONFLICT(project_id, name) DO UPDATE SET
           description = excluded.description,
           tree = excluded.tree,
           updated_at = excluded.updated_at",
        params![project_id, name.trim(), description, stored, now],
    )?;
    // Поиск идёт по `IS`, а не `=`: у общего шаблона `project_id IS NULL`, и `= NULL` не совпал
    // бы ни с одной строкой — сохранение отвечало бы «шаблона нет» сразу после успешной записи.
    let id: i64 = tx.query_row(
        "SELECT id FROM templates WHERE project_id IS ?1 AND name = ?2",
        params![project_id, name.trim()],
        |row| row.get(0),
    )?;
    if let Some(project) = project {
        activity::log(
            &tx,
            &Entry {
                project_id: project.id,
                node_id: None,
                entity_type: "template",
                entity_id: id,
                action: "save",
                field_name: Some("name"),
                old_value: None,
                new_value: Some(name.trim()),
                summary: None,
                actor,
            },
        )?;
    }
    tx.commit()?;
    by_id(conn, id)
}

/// Проверяет, что виды и приоритеты в шаблоне допустимы.
/// Проверяет, что виды и приоритеты в шаблоне допустимы и не противоречат друг другу.
///
/// Про «не противоречат»: шаблон не знает, на какой глубине его применят, поэтому вид в нём —
/// объявление СВОЕГО корня, а не абсолютной глубины. «Задача с подзадачей» — это корень на
/// глубине 2 и ребёнок на 3, и такое объявление внутренне согласовано, даже если применят его
/// в корень проекта. Противоречие — другое: ребёнок объявлен эпиком под корнем-задачей, то
/// есть автор заявил две несовместимые вещи, и какая из них верна, знает только он.
///
/// Ловится это при сохранении, а не при применении: шаблон пишется один раз, а применяется
/// много, и отказ в момент применения приходит против уже выбранного родителя.
fn validate(tree: &TemplateTree) -> Result<()> {
    // Базовый сдвиг задаёт первое объявление: корень-задача сдвигает всю шкалу на единицу.
    let base = declared_base(tree);
    fn walk(node: &TemplateNode, depth: i64, base: i64) -> Result<()> {
        if node.title.trim().is_empty() {
            return Err(Error::InvalidValue {
                field: "template".to_string(),
                value: "empty title".to_string(),
            });
        }
        if let Some(kind_text) = &node.kind {
            let kind = Kind::parse(kind_text).ok_or_else(|| Error::InvalidValue {
                field: "kind".to_string(),
                value: kind_text.clone(),
            })?;
            let expected = Kind::from_depth(depth + base);
            if kind != expected {
                return Err(Error::InvalidValue {
                    field: "kind".to_string(),
                    value: format!(
                        "«{}» declared {} but on its level the kind is {} — a template's kinds \
                         must agree with each other",
                        node.title,
                        kind.as_str(),
                        expected.as_str()
                    ),
                });
            }
        }
        if let Some(priority) = &node.priority
            && !matches!(priority.as_str(), "low" | "medium" | "high" | "critical")
        {
            return Err(Error::InvalidValue {
                field: "priority".to_string(),
                value: priority.clone(),
            });
        }
        // Тот же предел, что и у живой иерархии: шаблон, который нельзя применить, не
        // должен и сохраняться. Раньше здесь стояло 32 — второе число на то же правило,
        // и оно расходилось с настоящим пределом молча.
        if depth > crate::node::MAX_DEPTH {
            return Err(Error::DepthLimit);
        }
        for child in &node.children {
            walk(child, depth + 1, base)?;
        }
        Ok(())
    }
    for node in &tree.nodes {
        walk(node, 1, base)?;
    }
    Ok(())
}

/// Сдвиг шкалы видов: на сколько объявленный вид глубже собственной глубины узла.
///
/// Берётся первое объявление в порядке обхода, потому что порядок в шаблоне — это порядок
/// появления: автор пишет корень, потом его детей. Ноль, если вид не объявлен нигде, — тогда
/// проверять нечего.
fn declared_base(tree: &TemplateTree) -> i64 {
    fn find(node: &TemplateNode, depth: i64) -> Option<i64> {
        if let Some(kind) = node.kind.as_deref().and_then(Kind::parse) {
            return Some(kind.depth() - depth);
        }
        node.children
            .iter()
            .find_map(|child| find(child, depth + 1))
    }
    tree.nodes
        .iter()
        .find_map(|node| find(node, 1))
        .unwrap_or(0)
}

/// Шаблон по идентификатору. Проект уточнять не нужно: `id` уникален в базе.
pub fn by_id(conn: &Connection, id: i64) -> Result<Template> {
    conn.query_row(&format!("{SELECT} WHERE t.id = ?1"), params![id], map_row)
        .optional()?
        .ok_or_else(|| Error::InvalidValue {
            field: "template id".to_string(),
            value: format!("{id} not found"),
        })
}

/// Шаблон по имени: сначала свой, потом общий.
///
/// Порядок — не мелочь: проект вправе переопределить общий шаблон под свои условия, и
/// переопределение обязано побеждать. Обратный порядок означал бы, что загруженный из каталога
/// шаблон нельзя поправить в проекте.
pub fn by_name(conn: &Connection, project: Option<&Project>, name: &str) -> Result<Template> {
    if let Some(project) = project
        && let Some(found) = conn
            .query_row(
                &format!("{SELECT} WHERE t.name = ?1 AND t.project_id = ?2"),
                params![name, project.id],
                map_row,
            )
            .optional()?
    {
        return Ok(found);
    }
    conn.query_row(
        &format!("{SELECT} WHERE t.name = ?1 AND t.project_id IS NULL"),
        params![name],
        map_row,
    )
    .optional()?
    .ok_or_else(|| Error::InvalidValue {
        field: "template".to_string(),
        value: format!("{name} not found"),
    })
}

/// Шаблоны проекта вместе с общими.
///
/// Общие показываются всегда: они доступны в любом проекте (их и применяет `by_name`), и молча
/// скрыть их значило бы врать о том, что можно применить.
pub fn list(conn: &Connection, project: &Project) -> Result<Vec<Template>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT} WHERE t.project_id = ?1 OR t.project_id IS NULL ORDER BY t.name"
    ))?;
    let rows = stmt.query_map(params![project.id], map_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Все шаблоны базы, без выбора проекта: по проекту и общие.
///
/// Нужен там, где проект не назван. Ответ обязан называть проект у каждого шаблона — иначе
/// список из нескольких проектов выглядит как один, а имя шаблона уникально только внутри
/// проекта, и два одноимённых стали бы неразличимы.
pub fn list_all(conn: &Connection) -> Result<Vec<Template>> {
    let mut stmt = conn.prepare(&format!("{SELECT} ORDER BY COALESCE(p.key, ''), t.name"))?;
    let rows = stmt.query_map([], map_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Удаляет шаблон.
/// Удаляет шаблон. Отсутствующий шаблон — отказ из `by_name`, поэтому
/// «удалено ли что-то» не возвращается: строка после успешного поиска есть всегда.
/// Удаляет шаблон — свой или общий (когда проект не назван).
///
/// Общий шаблон из проекта не удаляется: он принадлежит не проекту, а всей базе, и удаление
/// «отсюда» было бы удалением того, чем пользуются соседи. Такой отказ называет причину, а не
/// «не найдено»: иначе человек ищет опечатку в имени, которого не допустил.
pub fn delete(
    conn: &mut Connection,
    project: Option<&Project>,
    name: &str,
    actor: &str,
) -> Result<()> {
    let template = by_name(conn, project, name)?;
    if template.project.is_none() && project.is_some() {
        return Err(Error::GlobalTemplate {
            name: name.to_string(),
        });
    }
    let tx = crate::db::write_tx(conn)?;
    tx.execute("DELETE FROM templates WHERE id = ?1", params![template.id])?;
    if let Some(project) = project {
        activity::log(
            &tx,
            &Entry {
                project_id: project.id,
                node_id: None,
                entity_type: "template",
                entity_id: template.id,
                action: "delete",
                field_name: Some("name"),
                old_value: Some(template.name.as_str()),
                new_value: None,
                summary: None,
                actor,
            },
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Результат применения шаблона.
#[derive(Debug, Clone, Serialize)]
pub struct Applied {
    pub template: String,
    pub parent: String,
    pub created: Vec<String>,
    /// Сколько объявленных в шаблоне видов не совпало с видом по глубине и было заменено.
    ///
    /// Число, а не молчание: `kind` в шаблоне — подсказка автора о том, что он имел в виду, а
    /// место в дереве решает. Шаблон «задача с подзадачей», применённый в корень, даёт эпик с
    /// задачей — и об этом надо сказать, иначе человек ищет, почему вид не тот, что он написал.
    pub rekinded: usize,
}

/// Разворачивает шаблон под указанным родителем.
///
/// Все значения переменных обязательны: без них шаблон молча создаст задачи с `{name}`
/// в заголовке, и это выяснится позже. Поэтому проверка идёт до первой записи.
///
/// Имя исполнителя сюда не передаётся: каждый узел логируется через `node::create`,
/// а он пока пишет актора `-`. Сквозная передача актора появится вместе с MCP (M8).
pub fn apply(
    conn: &mut Connection,
    project: &Project,
    name: &str,
    parent_ref: Option<&str>,
    values: &[(String, String)],
) -> Result<Applied> {
    // Проект обязателен: применение создаёт узлы, а узлы живут в проекте. Общий шаблон при этом
    // применим — `by_name` находит его, когда своего с таким именем нет.
    let template = by_name(conn, Some(project), name)?;
    let missing: Vec<String> = template
        .variables
        .iter()
        .filter(|variable| !values.iter().any(|(key, _)| key == *variable))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(Error::InvalidValue {
            field: "template".to_string(),
            value: format!("missing values for: {}", missing.join(", ")),
        });
    }

    let mut created: Vec<String> = Vec::new();
    let mut rekinded = 0usize;
    let parent_dot = match parent_ref {
        Some(reference) => Some(node::resolve(conn, project, reference)?.dot_id),
        None => None,
    };

    for item in &template.tree.nodes {
        spawn(
            conn,
            project,
            item,
            parent_dot.as_deref(),
            values,
            &mut created,
            &mut rekinded,
        )?;
    }
    // Без родителя узлы создаются в корне проекта, и «родитель» — ключ проекта.
    // `dot_id("-")` давал бы «TASQ--»: такая строка выглядит как ссылка на узел,
    // но ни на что не указывает.
    let parent = parent_dot.unwrap_or_else(|| project.key.clone());
    // Каждый созданный узел уже попал в лог активности из node::create;
    // отдельная запись «apply» дублировала бы её без пользы.
    Ok(Applied {
        template: template.name,
        parent,
        created,
        rekinded,
    })
}

fn spawn(
    conn: &mut Connection,
    project: &Project,
    item: &TemplateNode,
    parent: Option<&str>,
    values: &[(String, String)],
    created: &mut Vec<String>,
    rekinded: &mut usize,
) -> Result<String> {
    let title = render(&item.title, values)?;
    let description = match &item.description {
        Some(text) => Some(render(text, values)?),
        None => None,
    };
    // Метки рендерятся как любой текст: `{name}` в метке — обычное дело («api-{name}»).
    let tags = item
        .tags
        .iter()
        .map(|tag| render(tag, values))
        .collect::<Result<Vec<_>>>()?;
    // Вид НЕ передаётся: он выводится из глубины самим `node::create`. Объявленный в шаблоне
    // вид — подсказка о замысле, а не требование к месту: шаблон, задуманный как «задача с
    // подзадачей», обязан работать и в корень проекта (там он станет эпиком с задачей).
    // Раньше он отказывал — и отказ называл глубину, то есть человек чинил не то.
    let node = node::create(
        conn,
        project,
        NewNode {
            parent,
            title: &title,
            kind: None,
            description: description.as_deref(),
            priority: item.priority.as_deref(),
            due_date: None,
            estimated_hours: item.estimate,
            weight: item.weight,
            source_ref: None,
            branch: None,
            tags: (!tags.is_empty()).then_some(tags.as_slice()),
        },
    )?;
    if let Some(declared) = &item.kind
        && declared != &node.kind
    {
        *rekinded += 1;
    }
    created.push(node.dot_id.clone());
    for child in &item.children {
        spawn(
            conn,
            project,
            child,
            Some(&node.dot_id),
            values,
            created,
            rekinded,
        )?;
    }
    Ok(node.dot_id)
}
