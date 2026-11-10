//! Узлы дерева: создание, чтение, изменение, перенос, архив, мягкое удаление.
//!
//! Одна таблица `nodes` на все уровни (README §5); dot-ID собирается из `path` и ключа проекта.

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::Serialize;

use crate::activity::{self, Entry};
use crate::db::{self, now_iso};
use crate::error::{Error, Result};
use crate::project::Project;
use crate::status::{Kind, Status, check_transition};

/// Предел глубины дерева (README §5.3, инвариант 2).
///
/// Предел нужен не для аккуратности иерархии, а потому что адрес узла — это путь: он едет в
/// dot-ID, в аргументы CLI и в текст задач. Шестнадцать уровней — это эпик → задача →
/// подзадача → микро-задача и ещё двенадцать уровней сверху; глубже осмысленных иерархий не
/// встречается, а адрес остаётся коротким и произносимым.
///
/// Отдельного предела на длину пути нет намеренно: при 16 сегментах и номерах до семи цифр
/// путь занимает не больше 112 байт, то есть dot-ID вместе с ключом проекта укладывается в
/// 255 байт с запасом. Одно правило вместо двух, и его нельзя обойти по частям.
pub const MAX_DEPTH: i64 = 16;

/// Узел в выдаче: поля дерева плюс производные статус и прогресс.
#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub id: i64,
    pub dot_id: String,
    pub path: String,
    pub project_id: i64,
    pub depth: i64,
    pub seq: i64,
    pub kind: String,
    pub title: String,
    pub description: Option<String>,
    pub description_locked: bool,
    pub status: String,
    pub effective_status: String,
    pub priority: String,
    pub assigned_to: Option<String>,
    pub weight: f64,
    pub estimated_hours: Option<f64>,
    pub due_date: Option<String>,
    pub branch: Option<String>,
    pub source_ref: Option<String>,
    pub archived_at: Option<String>,
    pub lease_holder: Option<String>,
    pub lease_expires_at: Option<String>,
    pub progress_done: f64,
    pub progress_total: f64,
    pub created_at: String,
    pub updated_at: String,
}

impl Node {
    pub fn kind(&self) -> Kind {
        Kind::parse(&self.kind).unwrap_or(Kind::Micro)
    }

    pub fn stored_status(&self) -> Status {
        Status::parse(&self.status).unwrap_or(Status::Todo)
    }
}

/// Ссылка на узел в любой из трёх форм (README §6.2).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ref {
    Id(i64),
    Path(String),
}

fn parse_ref(project: &Project, reference: &str) -> Result<Ref> {
    let raw = reference.trim();
    if raw.is_empty() {
        return Err(Error::InvalidRef(reference.to_string()));
    }
    if let Some(rest) = raw.strip_prefix('#') {
        let id: i64 = rest
            .parse()
            .map_err(|_| Error::InvalidRef(reference.to_string()))?;
        return Ok(Ref::Id(id));
    }
    let path = match raw.split_once('-') {
        Some((prefix, rest)) if is_key_like(prefix) => {
            if prefix != project.key {
                return Err(Error::InvalidRef(reference.to_string()));
            }
            rest.to_string()
        }
        _ => raw.to_string(),
    };
    let valid = path
        .split('.')
        .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()));
    if !valid {
        return Err(Error::InvalidRef(reference.to_string()));
    }
    Ok(Ref::Path(path))
}

/// Ключ проекта, если ссылка его несёт: `DEV-1.1` → `Some("DEV")`.
///
/// Адрес узла самодостаточен, поэтому ключ в ссылке — это выбор проекта. `#12` ключа не
/// несёт: внутренний id однозначен только внутри проекта.
pub fn ref_key(reference: &str) -> Option<String> {
    let raw = reference.trim();
    if raw.starts_with('#') {
        return None;
    }
    match raw.split_once('-') {
        Some((prefix, rest)) if is_key_like(prefix) && !rest.is_empty() => Some(prefix.to_string()),
        _ => None,
    }
}

/// Проект, к которому относится ссылка: ключ из неё важнее контекста.
///
/// Возвращается владеемое значение: в вызывающем коде ссылку и соединение заимствуют
/// одновременно (`resolve(&conn, &project, …)`), и ссылка на поле контекста там не собралась бы.
fn project_for_reference(conn: &Connection, context: &Project, reference: &str) -> Result<Project> {
    match ref_key(reference) {
        Some(key) if !key.eq_ignore_ascii_case(&context.key) => crate::project::by_key(conn, &key)?
            .ok_or_else(|| Error::ProjectNotFound {
                key,
                known: crate::project::known_keys(conn).unwrap_or_else(|_| "unknown".to_string()),
            }),
        // Свой ключ (или его отсутствие) — контекст как есть.
        _ => Ok(context.clone()),
    }
}

fn is_key_like(prefix: &str) -> bool {
    (2..=8).contains(&prefix.len())
        && prefix
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

const NODE_SELECT: &str = "SELECT e.*, COALESCE(p.done_weight, 0) AS progress_done,
        COALESCE(p.total_weight, 0) AS progress_total
   FROM v_node_effective e
   LEFT JOIN v_node_progress p ON p.node_id = e.id";

fn map_node(row: &Row<'_>, project: &Project) -> rusqlite::Result<Node> {
    let path: String = row.get("path")?;
    Ok(Node {
        id: row.get("id")?,
        dot_id: project.dot_id(&path),
        project_id: row.get("project_id")?,
        path,
        depth: row.get("depth")?,
        seq: row.get("seq")?,
        kind: row.get("kind")?,
        title: row.get("title")?,
        description: row.get("description")?,
        description_locked: row.get::<_, i64>("description_locked")? != 0,
        status: row.get("status")?,
        effective_status: row.get("effective_status")?,
        priority: row.get("priority")?,
        assigned_to: row.get("assigned_to")?,
        weight: row.get("weight")?,
        estimated_hours: row.get("estimated_hours")?,
        due_date: row.get("due_date")?,
        branch: row.get("branch")?,
        source_ref: row.get("source_ref")?,
        archived_at: row.get("archived_at")?,
        lease_holder: row.get("lease_holder")?,
        lease_expires_at: row.get("lease_expires_at")?,
        progress_done: row.get("progress_done")?,
        progress_total: row.get("progress_total")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

/// Разрешает ссылку в узел проекта (без удалённых).
pub fn resolve(conn: &Connection, project: &Project, reference: &str) -> Result<Node> {
    Ok(resolve_with_project(conn, project, reference)?.1)
}

/// Разрешение ссылки вместе с проектом, к которому она относится.
///
/// Нужно там, где после разрешения идёт запись по `id` или в журнал: `load_by_id` и
/// `activity::log` фильтруют по проекту, и проект контекста («работаю в ALPHA») искал бы узел
/// чужого проекта по своему id — то есть ссылка `DEV-1.1` читалась бы, но правка падала бы
/// `NODE_NOT_FOUND #4`.
pub fn resolve_with_project(
    conn: &Connection,
    project: &Project,
    reference: &str,
) -> Result<(Project, Node)> {
    // Ссылка с ключом ведёт в свой проект: `DEV-1.1` читается из сессии ALPHA.
    let target = project_for_reference(conn, project, reference)?;
    let node = resolve_in(conn, &target, reference)?;
    Ok((target, node))
}

/// Разрешение ссылки внутри уже определённого проекта (ключ в ссылке сверяется с ним).
fn resolve_in(conn: &Connection, project: &Project, reference: &str) -> Result<Node> {
    let node = match parse_ref(project, reference)? {
        Ref::Id(id) => conn
            .query_row(
                &format!(
                    "{NODE_SELECT} WHERE e.id = ?1 AND e.project_id = ?2 AND e.deleted_at IS NULL"
                ),
                params![id, project.id],
                |row| map_node(row, project),
            )
            .optional()?,
        Ref::Path(path) => conn
            .query_row(
                &format!(
                    "{NODE_SELECT} WHERE e.project_id = ?1 AND e.path = ?2 AND e.deleted_at IS NULL"
                ),
                params![project.id, path],
                |row| map_node(row, project),
            )
            .optional()?,
    };
    node.ok_or_else(|| Error::NodeNotFound(reference.to_string()))
}

/// Разрешает ссылку, включая мягко удалённые узлы (для `restore`).
fn resolve_any(conn: &Connection, project: &Project, reference: &str) -> Result<Node> {
    // Ключ в ссылке важнее контекста и здесь: восстановить узел чужого проекта по адресу
    // можно из любой сессии.
    let target = project_for_reference(conn, project, reference)?;
    let project = &target;
    let node = match parse_ref(project, reference)? {
        Ref::Id(id) => conn
            .query_row(
                &format!("{NODE_SELECT} WHERE e.id = ?1 AND e.project_id = ?2"),
                params![id, project.id],
                |row| map_node(row, project),
            )
            .optional()?,
        Ref::Path(path) => conn
            .query_row(
                &format!("{NODE_SELECT} WHERE e.project_id = ?1 AND e.path = ?2"),
                params![project.id, path],
                |row| map_node(row, project),
            )
            .optional()?,
    };
    node.ok_or_else(|| Error::NodeNotFound(reference.to_string()))
}

fn load_by_id(conn: &Connection, project: &Project, id: i64) -> Result<Node> {
    conn.query_row(
        &format!("{NODE_SELECT} WHERE e.id = ?1 AND e.project_id = ?2"),
        params![id, project.id],
        |row| map_node(row, project),
    )
    .optional()?
    .ok_or_else(|| Error::NodeNotFound(format!("#{id}")))
}

/// Заявка на создание узла.
pub struct NewNode<'a> {
    pub parent: Option<&'a str>,
    pub title: &'a str,
    pub kind: Option<Kind>,
    pub description: Option<&'a str>,
    pub priority: Option<&'a str>,
    pub due_date: Option<&'a str>,
    pub estimated_hours: Option<f64>,
    pub weight: Option<f64>,
    pub source_ref: Option<&'a str>,
    pub branch: Option<&'a str>,
}

fn validate_path(path: &str) -> Result<()> {
    if path.split('.').count() > MAX_DEPTH as usize {
        return Err(Error::DepthLimit);
    }
    Ok(())
}

fn validate_priority(value: &str) -> Result<()> {
    match value {
        "low" | "medium" | "high" | "critical" => Ok(()),
        other => Err(Error::InvalidValue {
            field: "priority".to_string(),
            value: other.to_string(),
        }),
    }
}

/// `due_date` — календарная дата `YYYY-MM-DD` (README §5).
fn validate_due(value: &str) -> Result<()> {
    use time::Date;
    use time::format_description::well_known::Iso8601;
    Date::parse(value, &Iso8601::DATE)
        .map(|_| ())
        .map_err(|_| Error::InvalidValue {
            field: "due_date".to_string(),
            value: value.to_string(),
        })
}

/// Создаёт узел и пишет запись в лог.
pub fn create(conn: &mut Connection, project: &Project, req: NewNode<'_>) -> Result<Node> {
    if req.title.trim().is_empty() {
        return Err(Error::InvalidValue {
            field: "title".to_string(),
            value: "empty".to_string(),
        });
    }
    let priority = req.priority.unwrap_or("medium");
    validate_priority(priority)?;
    if let Some(due) = req.due_date {
        validate_due(due)?;
    }

    let tx = db::write_tx(conn)?;
    let (parent_id, depth, parent_path) = match req.parent {
        Some(reference) => {
            let parent = resolve(&tx, project, reference)?;
            (Some(parent.id), parent.depth + 1, parent.path)
        }
        None => (None, 1, String::new()),
    };
    let kind = match req.kind {
        Some(kind) => {
            let expected = Kind::from_depth(depth);
            if expected != kind {
                return Err(Error::InvalidValue {
                    field: "kind".to_string(),
                    value: format!(
                        "{} does not fit depth {depth} (expected {})",
                        kind.as_str(),
                        expected.as_str()
                    ),
                });
            }
            kind
        }
        None => Kind::from_depth(depth),
    };

    let seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM nodes WHERE project_id = ?1 AND parent_id IS ?2",
        params![project.id, parent_id],
        |row| row.get(0),
    )?;
    let path = if parent_path.is_empty() {
        seq.to_string()
    } else {
        format!("{parent_path}.{seq}")
    };
    validate_path(&path)?;

    let now = now_iso();
    tx.execute(
        "INSERT INTO nodes (project_id, parent_id, depth, seq, path, kind, title, description,
                            status, priority, weight, estimated_hours, due_date, branch, source_ref,
                            sort_order, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?11, ?16, ?16)",
        params![
            project.id,
            parent_id,
            depth,
            seq,
            path,
            kind.as_str(),
            req.title.trim(),
            req.description,
            kind.initial_status().as_str(),
            priority,
            req.weight.unwrap_or(1.0),
            req.estimated_hours,
            req.due_date,
            req.branch,
            req.source_ref,
            now,
        ],
    )?;
    let id = tx.last_insert_rowid();
    let dot_id = project.dot_id(&path);
    let summary = format!("created {dot_id}");
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id: Some(id),
            entity_type: "node",
            entity_id: id,
            action: "create",
            field_name: None,
            old_value: None,
            new_value: None,
            summary: Some(summary.as_str()),
            actor: "-",
        },
    )?;
    tx.commit()?;
    load_by_id(conn, project, id)
}

/// Изменение узла: пишется только то, что действительно изменилось.
#[derive(Default)]
pub struct Patch<'a> {
    pub title: Option<&'a str>,
    pub description: Option<&'a str>,
    pub status: Option<Status>,
    pub priority: Option<&'a str>,
    pub assigned_to: Option<&'a str>,
    pub due_date: Option<&'a str>,
    pub weight: Option<f64>,
    pub estimated_hours: Option<f64>,
    pub source_ref: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub force: bool,
    pub description_locked: Option<bool>,
    /// Узел, на который у вызывающего есть живая аренда. Право выставить
    /// `in_progress` есть только у неё, и только на свой узел: флаг «я с арендой»
    /// позволял ставить чужой узел в работу по любому токену.
    pub lease: Option<i64>,
    pub actor: Option<&'a str>,
}

/// Одно изменяемое поле: колонка, новое значение и пара «было/стало» для лога.
struct Change {
    column: &'static str,
    value: Value,
    old: Option<String>,
    new: Option<String>,
}

fn unfinished_children(conn: &Connection, node_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM nodes
         WHERE parent_id = ?1 AND deleted_at IS NULL AND status NOT IN ('done', 'cancelled')",
        params![node_id],
        |row| row.get(0),
    )?)
}

fn unfinished_blockers(conn: &Connection, node_id: i64) -> Result<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT b.path FROM edges e
         JOIN nodes b ON b.id = e.to_node
         WHERE e.from_node = ?1 AND e.kind = 'blocks'
           AND b.deleted_at IS NULL AND b.status NOT IN ('done', 'cancelled')
         ORDER BY b.path",
    )?;
    let paths = stmt
        .query_map(params![node_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(if paths.is_empty() {
        None
    } else {
        Some(paths.join(", "))
    })
}

/// Применяет изменения к узлу.
pub fn update(
    conn: &mut Connection,
    project: &Project,
    reference: &str,
    patch: Patch<'_>,
) -> Result<Node> {
    // Проект берётся из ссылки: правка уходит туда, куда указывает адрес узла.
    let (project, node) = resolve_with_project(conn, project, reference)?;
    if node.archived_at.is_some() {
        return Err(Error::AlreadyInState {
            dot_id: node.dot_id.clone(),
            state: "archived".to_string(),
        });
    }

    if let Some(priority) = patch.priority {
        validate_priority(priority)?;
    }
    if let Some(due) = patch.due_date {
        validate_due(due)?;
    }
    if let Some(new_desc) = patch.description
        && node.description_locked
        && node.description.as_deref() != Some(new_desc)
    {
        return Err(Error::DescriptionLocked {
            dot_id: node.dot_id.clone(),
        });
    }
    if let Some(Status::InProgress) = patch.status
        && patch.lease != Some(node.id)
    {
        return Err(Error::LeaseRequired);
    }

    if let Some(target) = patch.status {
        check_transition(node.stored_status(), target, node.kind(), patch.force)?;
        if target == Status::Done {
            let unfinished = unfinished_children(conn, node.id)?;
            if unfinished > 0 && !patch.force {
                return Err(Error::ChildrenUnfinished {
                    dot_id: node.dot_id.clone(),
                    unfinished,
                });
            }
            if let Some(blockers) = unfinished_blockers(conn, node.id)?
                && !patch.force
            {
                return Err(Error::UnmetPrerequisites {
                    dot_id: node.dot_id.clone(),
                    blockers,
                });
            }
        }
    }

    let mut changes: Vec<Change> = Vec::new();

    if let Some(title) = patch.title
        && title.trim() != node.title
    {
        changes.push(Change {
            column: "title",
            value: Value::Text(title.trim().to_string()),
            old: Some(node.title.clone()),
            new: Some(title.trim().to_string()),
        });
    }
    if let Some(description) = patch.description
        && node.description.as_deref() != Some(description)
    {
        changes.push(Change {
            column: "description",
            value: Value::Text(description.to_string()),
            old: node.description.clone(),
            new: Some(description.to_string()),
        });
    }
    if let Some(priority) = patch.priority
        && priority != node.priority
    {
        changes.push(Change {
            column: "priority",
            value: Value::Text(priority.to_string()),
            old: Some(node.priority.clone()),
            new: Some(priority.to_string()),
        });
    }
    if let Some(assigned) = patch.assigned_to
        && node.assigned_to.as_deref() != Some(assigned)
    {
        changes.push(Change {
            column: "assigned_to",
            value: Value::Text(assigned.to_string()),
            old: node.assigned_to.clone(),
            new: Some(assigned.to_string()),
        });
    }
    if let Some(due) = patch.due_date
        && node.due_date.as_deref() != Some(due)
    {
        changes.push(Change {
            column: "due_date",
            value: Value::Text(due.to_string()),
            old: node.due_date.clone(),
            new: Some(due.to_string()),
        });
    }
    if let Some(weight) = patch.weight
        && weight != node.weight
    {
        changes.push(Change {
            column: "weight",
            value: Value::Real(weight),
            old: Some(node.weight.to_string()),
            new: Some(weight.to_string()),
        });
    }
    if let Some(hours) = patch.estimated_hours
        && node.estimated_hours != Some(hours)
    {
        changes.push(Change {
            column: "estimated_hours",
            value: Value::Real(hours),
            old: node.estimated_hours.map(|v| v.to_string()),
            new: Some(hours.to_string()),
        });
    }
    if let Some(source) = patch.source_ref
        && node.source_ref.as_deref() != Some(source)
    {
        changes.push(Change {
            column: "source_ref",
            value: Value::Text(source.to_string()),
            old: node.source_ref.clone(),
            new: Some(source.to_string()),
        });
    }
    if let Some(branch) = patch.branch
        && node.branch.as_deref() != Some(branch)
    {
        changes.push(Change {
            column: "branch",
            value: Value::Text(branch.to_string()),
            old: node.branch.clone(),
            new: Some(branch.to_string()),
        });
    }
    if let Some(locked) = patch.description_locked
        && locked != node.description_locked
    {
        changes.push(Change {
            column: "description_locked",
            value: Value::Integer(i64::from(locked)),
            old: Some(node.description_locked.to_string()),
            new: Some(locked.to_string()),
        });
    }
    if let Some(target) = patch.status
        && target.as_str() != node.status
    {
        changes.push(Change {
            column: "status",
            value: Value::Text(target.as_str().to_string()),
            old: Some(node.status.clone()),
            new: Some(target.as_str().to_string()),
        });
    }

    if changes.is_empty() {
        return Ok(node);
    }

    let assignments = changes
        .iter()
        .map(|c| format!("{} = ?", c.column))
        .collect::<Vec<_>>()
        .join(", ");
    let mut values: Vec<Value> = changes.iter().map(|c| c.value.clone()).collect();
    values.push(Value::Text(now_iso()));
    values.push(Value::Integer(node.id));
    let sql = format!("UPDATE nodes SET {assignments}, updated_at = ? WHERE id = ?");

    let tx = db::write_tx(conn)?;
    tx.execute(&sql, params_from_iter(values.iter()))?;
    let actor = patch.actor.unwrap_or("-");
    for change in &changes {
        let is_status = change.column == "status";
        let entry = Entry {
            project_id: project.id,
            node_id: Some(node.id),
            entity_type: "node",
            entity_id: node.id,
            action: if is_status { "transition" } else { "update" },
            field_name: Some(change.column),
            old_value: change.old.as_deref(),
            new_value: change.new.as_deref(),
            summary: Some(node.dot_id.as_str()),
            actor,
        };
        activity::log(&tx, &entry)?;
    }
    tx.commit()?;
    load_by_id(conn, &project, node.id)
}

/// Фильтр для `ls`.
#[derive(Default)]
pub struct Filter<'a> {
    pub parent: Option<&'a str>,
    pub status: Option<&'a str>,
    /// Проекты, которые не показывать (`--except-project`). Для обзора по всем проектам
    /// исключение — второй инструмент после включения: «всё, кроме архива и стенда» пишется
    /// короче, чем перечисление нужного.
    pub exclude_projects: &'a [String],
    /// Не показывать закрытую словом `done` работу (`tasq ls -n`).
    ///
    /// Явный `status` сильнее: «покажи готовое» — осмысленный запрос, и гасить его флагом
    /// значило бы молча отдать пустоту. Отменённое этим флагом не скрывается: `cancelled`
    /// отвечает на другой вопрос («что мы решили не делать»), и путать их нельзя.
    pub exclude_done: bool,
    pub assignee: Option<&'a str>,
    pub overdue: bool,
    pub include_archived: bool,
    pub include_deleted: bool,
    pub limit: Option<i64>,
}

/// Ключ сортировки пути: числовые сегменты сравниваются как числа, а не строки.
pub fn path_sort_key(path: &str) -> Vec<u32> {
    path.split('.').filter_map(|s| s.parse().ok()).collect()
}

/// Список узлов по фильтру, в порядке дерева.
pub fn list(conn: &Connection, project: &Project, filter: &Filter<'_>) -> Result<Vec<Node>> {
    // Исключить проект, в котором и спрашивают, можно — ответ тогда пуст. Игнорировать флаг
    // было бы хуже: скрипт «покажи всё, кроме X» молча получил бы X.
    if filter
        .exclude_projects
        .iter()
        .any(|key| key.eq_ignore_ascii_case(&project.key))
    {
        return Ok(Vec::new());
    }
    let mut sql = format!("{NODE_SELECT} WHERE e.project_id = ?");
    let mut values: Vec<Value> = vec![Value::Integer(project.id)];

    if !filter.include_deleted {
        sql.push_str(" AND e.deleted_at IS NULL");
    }
    if !filter.include_archived {
        sql.push_str(" AND e.archived_at IS NULL");
    }
    if let Some(parent) = filter.parent {
        let root = resolve(conn, project, parent)?;
        sql.push_str(" AND (e.path = ? OR e.path GLOB ?)");
        values.push(Value::Text(root.path.clone()));
        values.push(Value::Text(format!("{}.*", root.path)));
    }
    if let Some(status) = filter.status {
        sql.push_str(" AND e.effective_status = ?");
        values.push(Value::Text(status.to_string()));
    } else if filter.exclude_done {
        sql.push_str(" AND e.effective_status <> 'done'");
    }
    if let Some(assignee) = filter.assignee {
        sql.push_str(" AND e.assigned_to = ?");
        values.push(Value::Text(assignee.to_string()));
    }
    if filter.overdue {
        sql.push_str(
            " AND e.due_date IS NOT NULL AND e.due_date < date('now')
               AND e.effective_status NOT IN ('done', 'cancelled')",
        );
    }

    let mut stmt = conn.prepare(&sql)?;
    let mut nodes = stmt
        .query_map(params_from_iter(values.iter()), |row| {
            map_node(row, project)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    nodes.sort_by_key(|node| path_sort_key(&node.path));
    if let Some(limit) = filter.limit {
        nodes.truncate(limit.max(0) as usize);
    }
    Ok(nodes)
}

/// Список узлов по всем проектам базы, в порядке «проект, путь».
///
/// dot-ID собирается из ключа СВОЕГО проекта (`pr.key || '-' || e.path`): адрес узла
/// самодостаточен, поэтому список по всем проектам остаётся однозначным. Фильтр `parent`
/// здесь не поддерживается — ссылка-родитель живёт внутри проекта, и молча игнорировать её
/// значило бы выдать не то, что попросили.
pub fn list_all(conn: &Connection, filter: &Filter<'_>) -> Result<Vec<Node>> {
    if filter.parent.is_some() {
        return Err(Error::InvalidValue {
            field: "parent".to_string(),
            value: "needs a project; `--parent` does not work across projects".to_string(),
        });
    }
    let mut sql = String::from(
        "SELECT e.*, pr.key AS project_key, COALESCE(p.done_weight, 0) AS progress_done,
                COALESCE(p.total_weight, 0) AS progress_total
           FROM v_node_effective e
           JOIN projects pr ON pr.id = e.project_id
           LEFT JOIN v_node_progress p ON p.node_id = e.id
          WHERE 1 = 1",
    );
    let mut values: Vec<Value> = Vec::new();
    if !filter.include_deleted {
        sql.push_str(" AND e.deleted_at IS NULL");
    }
    if !filter.include_archived {
        sql.push_str(" AND e.archived_at IS NULL");
    }
    if let Some(status) = filter.status {
        sql.push_str(" AND e.effective_status = ?");
        values.push(Value::Text(status.to_string()));
    } else if filter.exclude_done {
        sql.push_str(" AND e.effective_status <> 'done'");
    }
    if let Some(assignee) = filter.assignee {
        sql.push_str(" AND e.assigned_to = ?");
        values.push(Value::Text(assignee.to_string()));
    }
    if !filter.exclude_projects.is_empty() {
        // Плейсхолдеры по числу ключей: подстановка строки в SQL недопустима даже здесь.
        let marks = vec!["?"; filter.exclude_projects.len()].join(", ");
        sql.push_str(&format!(" AND pr.key NOT IN ({marks})"));
        for key in filter.exclude_projects {
            values.push(Value::Text(key.to_uppercase()));
        }
    }
    if filter.overdue {
        sql.push_str(
            " AND e.due_date IS NOT NULL AND e.due_date < date('now')
               AND e.effective_status NOT IN ('done', 'cancelled')",
        );
    }
    sql.push_str(" ORDER BY pr.key, e.depth, e.path");

    let mut stmt = conn.prepare(&sql)?;
    let mut nodes = stmt
        .query_map(params_from_iter(values.iter()), |row| {
            let path: String = row.get("path")?;
            let key: String = row.get("project_key")?;
            let mut node = map_node(
                row,
                &Project {
                    // map_node пишет dot_id из переданного проекта; здесь он сразу заменяется
                    // на адрес своего проекта, поэтому заглушка не видна снаружи.
                    id: 0,
                    key: String::new(),
                    name: String::new(),
                    status: String::new(),
                },
            )?;
            node.dot_id = format!("{key}-{path}");
            Ok(node)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Порядок уже задан SQL (проект, затем путь); сортировка в коде не нужна.
    if let Some(limit) = filter.limit {
        nodes.truncate(limit.max(0) as usize);
    }
    Ok(nodes)
}

/// Поддерево узла в порядке дерева (сам узел первым).
pub fn subtree(conn: &Connection, project: &Project, reference: &str) -> Result<Vec<Node>> {
    let root = resolve(conn, project, reference)?;
    list(
        conn,
        project,
        &Filter {
            parent: Some(root.dot_id.as_str()),
            include_archived: true,
            ..Filter::default()
        },
    )
}

/// Перенос узла под другого родителя; пути узла и потомков пересчитываются.
pub fn move_node(
    conn: &mut Connection,
    project: &Project,
    reference: &str,
    new_parent: Option<&str>,
    renumber: bool,
) -> Result<Node> {
    let (project, node) = resolve_with_project(conn, project, reference)?;
    let (parent_id, new_depth, parent_path) = match new_parent {
        Some(reference) => {
            let parent = resolve(conn, &project, reference)?;
            if parent.id == node.id || parent.path.starts_with(&format!("{}.", node.path)) {
                return Err(Error::MoveIntoSelf {
                    dot_id: node.dot_id.clone(),
                    target: parent.dot_id.clone(),
                });
            }
            (Some(parent.id), parent.depth + 1, parent.path)
        }
        None => (None, 1, String::new()),
    };

    let tx = db::write_tx(conn)?;
    // Номер сохраняется, если он свободен у нового родителя: тогда dot-ID не меняется.
    // Если занят — берётся следующий свободный. Молча падать на `UNIQUE constraint` нельзя:
    // перенос в уже занятую ветку — обычная операция, а не ошибка вызывающего.
    let next_free = |tx: &rusqlite::Transaction<'_>| -> Result<i64> {
        Ok(tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM nodes WHERE project_id = ?1 AND parent_id IS ?2",
            params![project.id, parent_id],
            |row| row.get(0),
        )?)
    };
    let candidate = |seq: i64| -> String {
        if parent_path.is_empty() {
            seq.to_string()
        } else {
            format!("{parent_path}.{seq}")
        }
    };
    let taken = |tx: &rusqlite::Transaction<'_>, path: &str| -> Result<bool> {
        Ok(tx.query_row(
            "SELECT COUNT(*) FROM nodes WHERE project_id = ?1 AND path = ?2",
            params![project.id, path],
            |row| row.get::<_, i64>(0),
        )? > 0)
    };

    // `--renumber` и занятый номер ведут к одному и тому же: берём следующий свободный.
    let seq: i64 = if renumber || taken(&tx, &candidate(node.seq))? {
        next_free(&tx)?
    } else {
        node.seq
    };
    let new_path = candidate(seq);
    validate_path(&new_path)?;

    let deepest: i64 = tx.query_row(
        "SELECT COALESCE(MAX(depth), ?1) FROM nodes
         WHERE project_id = ?2 AND (path = ?3 OR path GLOB ?4)",
        params![
            node.depth,
            project.id,
            node.path,
            format!("{}.*", node.path)
        ],
        |row| row.get(0),
    )?;
    let delta = new_depth - node.depth;
    if deepest + delta > MAX_DEPTH {
        return Err(Error::DepthLimit);
    }

    let descendants: Vec<(i64, String, i64)> = {
        let mut stmt = tx.prepare(
            "SELECT id, path, depth FROM nodes
             WHERE project_id = ?1 AND path GLOB ?2 ORDER BY path",
        )?;
        stmt.query_map(params![project.id, format!("{}.*", node.path)], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let now = now_iso();
    tx.execute(
        "UPDATE nodes SET parent_id = ?1, depth = ?2, seq = ?3, path = ?4, updated_at = ?5
         WHERE id = ?6",
        params![parent_id, new_depth, seq, new_path, now, node.id],
    )?;
    tx.execute(
        "UPDATE nodes SET path = ?1 || substr(path, ?2), depth = depth + ?3, updated_at = ?4
         WHERE project_id = ?5 AND (path = ?6 OR path GLOB ?7)",
        params![
            new_path,
            node.path.len() as i64 + 1,
            delta,
            now,
            project.id,
            node.path,
            format!("{}.*", node.path)
        ],
    )?;

    let new_dot = project.dot_id(&new_path);
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id: Some(node.id),
            entity_type: "node",
            entity_id: node.id,
            action: "move",
            field_name: Some("path"),
            old_value: Some(node.path.as_str()),
            new_value: Some(new_path.as_str()),
            summary: Some(new_dot.as_str()),
            actor: "-",
        },
    )?;
    for (id, old_path, old_depth) in &descendants {
        let suffix = &old_path[node.path.len()..];
        let moved = format!("{new_path}{suffix}");
        let depth_note = format!("depth {old_depth} -> {}", old_depth + delta);
        activity::log(
            &tx,
            &Entry {
                project_id: project.id,
                node_id: Some(*id),
                entity_type: "node",
                entity_id: *id,
                action: "move",
                field_name: Some("path"),
                old_value: Some(old_path.as_str()),
                new_value: Some(moved.as_str()),
                summary: Some(depth_note.as_str()),
                actor: "-",
            },
        )?;
    }
    tx.commit()?;
    load_by_id(conn, &project, node.id)
}

/// Задаёт порядок сиблингов.
pub fn reorder(conn: &mut Connection, project: &Project, references: &[String]) -> Result<()> {
    let tx = db::write_tx(conn)?;
    let now = now_iso();
    let mut ids = Vec::new();
    for reference in references {
        let node = resolve(&tx, project, reference)?;
        ids.push(node.id);
    }
    for (index, id) in ids.iter().enumerate() {
        tx.execute(
            "UPDATE nodes SET sort_order = ?1, updated_at = ?2 WHERE id = ?3",
            params![index as i64, now, id],
        )?;
    }
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id: None,
            entity_type: "node",
            entity_id: ids.first().copied().unwrap_or_default(),
            action: "reorder",
            field_name: Some("sort_order"),
            old_value: None,
            new_value: None,
            summary: Some("siblings reordered"),
            actor: "-",
        },
    )?;
    tx.commit()?;
    Ok(())
}

/// Архив и распаковка поддерева: скрывает из выдач, ничего не удаляя.
pub fn archive(
    conn: &mut Connection,
    project: &Project,
    reference: &str,
    undo: bool,
) -> Result<usize> {
    let (project, node) = resolve_with_project(conn, project, reference)?;
    if !undo && node.archived_at.is_some() {
        return Err(Error::AlreadyInState {
            dot_id: node.dot_id.clone(),
            state: "archived".to_string(),
        });
    }
    if undo && node.archived_at.is_none() {
        return Err(Error::AlreadyInState {
            dot_id: node.dot_id.clone(),
            state: "not archived".to_string(),
        });
    }

    let now = now_iso();
    let tx = db::write_tx(conn)?;
    let affected = tx.execute(
        "UPDATE nodes SET archived_at = ?1, updated_at = ?2
         WHERE project_id = ?3 AND (path = ?4 OR path GLOB ?5)",
        params![
            if undo { None } else { Some(now.clone()) },
            now,
            project.id,
            node.path,
            format!("{}.*", node.path)
        ],
    )?;
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id: Some(node.id),
            entity_type: "node",
            entity_id: node.id,
            action: if undo { "unarchive" } else { "archive" },
            field_name: Some("archived_at"),
            old_value: None,
            new_value: if undo { None } else { Some(now.as_str()) },
            summary: Some(node.dot_id.as_str()),
            actor: "-",
        },
    )?;
    tx.commit()?;
    Ok(affected)
}

/// Мягкое удаление: только узлы до начала работы — `idea`, `backlog`, `todo`
/// (README §5.3, инвариант 10).
pub fn delete(conn: &mut Connection, project: &Project, reference: &str) -> Result<()> {
    let (project, node) = resolve_with_project(conn, project, reference)?;
    if !matches!(
        node.stored_status(),
        Status::Idea | Status::Backlog | Status::Todo
    ) {
        return Err(Error::NotRemovable {
            dot_id: node.dot_id.clone(),
            status: node.status.clone(),
        });
    }
    let now = now_iso();
    let tx = db::write_tx(conn)?;
    tx.execute(
        "UPDATE nodes SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2",
        params![now, node.id],
    )?;
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id: Some(node.id),
            entity_type: "node",
            entity_id: node.id,
            action: "delete",
            field_name: Some("deleted_at"),
            old_value: None,
            new_value: Some(now.as_str()),
            summary: Some(node.dot_id.as_str()),
            actor: "-",
        },
    )?;
    tx.commit()?;
    Ok(())
}

/// Возвращает мягко удалённый узел.
pub fn restore(conn: &mut Connection, project: &Project, reference: &str) -> Result<Node> {
    let target = project_for_reference(conn, project, reference)?;
    let project = &target;
    let node = resolve_any(conn, project, reference)?;
    let deleted: Option<String> = conn.query_row(
        "SELECT deleted_at FROM nodes WHERE id = ?1",
        params![node.id],
        |row| row.get(0),
    )?;
    let Some(deleted_at) = deleted else {
        return Err(Error::AlreadyInState {
            dot_id: node.dot_id.clone(),
            state: "present".to_string(),
        });
    };
    let tx = db::write_tx(conn)?;
    tx.execute(
        "UPDATE nodes SET deleted_at = NULL, updated_at = ?1 WHERE id = ?2",
        params![now_iso(), node.id],
    )?;
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id: Some(node.id),
            entity_type: "node",
            entity_id: node.id,
            action: "restore",
            field_name: Some("deleted_at"),
            old_value: Some(deleted_at.as_str()),
            new_value: None,
            summary: Some(node.dot_id.as_str()),
            actor: "-",
        },
    )?;
    tx.commit()?;
    load_by_id(conn, project, node.id)
}
