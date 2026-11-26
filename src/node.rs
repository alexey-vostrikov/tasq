//! Узлы дерева: создание, чтение, изменение, перенос, архив, мягкое удаление.
//!
//! Одна таблица `nodes` на все уровни (README §5); dot-ID собирается из `path` и ключа проекта.

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::Serialize;

use crate::activity::{self, Entry};
use crate::db::{self, now_iso};
use crate::error::{Error, Result};
use crate::history;
use crate::project::Project;
use crate::status::{Kind, Status, check_with_policy};

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
    /// Метки узла: то, по чему ищут и группируют («api», «dns», «срочно»).
    ///
    /// Хранятся JSON-массивом в `nodes.tags`, а читаются как набор: колонка была в схеме с
    /// первой версии и не писалась ничем, то есть обещала возможность, которой не было.
    pub tags: Vec<String>,
    /// Свободные ключ-значения узла: то, чему не нужна собственная колонка.
    ///
    /// Хранятся JSON-объектом в `nodes.metadata`. Отдельно от `tags`, потому что это разные
    /// вопросы: метка отвечает «к чему относится», поле — «какое у него свойство».
    pub metadata: std::collections::BTreeMap<String, String>,
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

/// Разбор JSON-колонки в значение.
///
/// Ошибка разбора не превращается в пустое значение: колонку пишет только этот код, поэтому
/// испорченное содержимое означает повреждение данных, а не «меток просто нет». Молчаливая
/// пустота выглядела бы как «метки потерялись при экспорте» — и искали бы их не там.
fn parse_json<T: serde::de::DeserializeOwned + Default>(
    row: &Row<'_>,
    column: &str,
) -> rusqlite::Result<T> {
    let raw: String = row.get(column)?;
    if raw.trim().is_empty() {
        return Ok(T::default());
    }
    serde_json::from_str(&raw).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            raw.len(),
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{column}: {err}"),
            )),
        )
    })
}

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
        tags: parse_json(row, "tags")?,
        metadata: parse_json(row, "metadata")?,
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

/// Разрешение ссылки с вычисленным состоянием: `incomplete` — вывод, а не запись.
///
/// Отдельная обёртка, а не правка `resolve`: пути записи обход дерева не нужен, а читатели
/// (`show`, `ctx`, ответ `release`) обязаны видеть то же, что список.
pub fn resolve_shown(
    conn: &Connection,
    project: &Project,
    reference: &str,
) -> Result<(Project, Node)> {
    let (project, mut node) = resolve_with_project(conn, project, reference)?;
    apply_derived_one(conn, &project, &mut node)?;
    Ok((project, node))
}

fn load_by_id(conn: &Connection, project: &Project, id: i64) -> Result<Node> {
    let mut node = conn
        .query_row(
            &format!("{NODE_SELECT} WHERE e.id = ?1 AND e.project_id = ?2"),
            params![id, project.id],
            |row| map_node(row, project),
        )
        .optional()?
        .ok_or_else(|| Error::NodeNotFound(format!("#{id}")))?;
    apply_derived_one(conn, project, &mut node)?;
    Ok(node)
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
    /// Метки нового узла; `None` — без меток.
    pub tags: Option<&'a [String]>,
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
    // Метки пишутся сразу: `update` после `create` дал бы в журнале две записи на один факт
    // («узел создан» и «узлу добавлены метки»), и первая из них была бы неполной.
    let tags_json = normalise_tags(req.tags.unwrap_or(&[]));
    tx.execute(
        "INSERT INTO nodes (project_id, parent_id, depth, seq, path, kind, title, description,
                            status, priority, weight, estimated_hours, due_date, branch, source_ref,
                            tags, sort_order, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?11, ?17, ?17)",
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
            tags_json,
            now,
        ],
    )?;
    let id = tx.last_insert_rowid();
    let dot_id = project.dot_id(&path);
    // Первый факт истории: узел появился в начальном статусе. Без него новый узел выглядел бы
    // «неизвестным» — история пишется на переходы, а создание переходом не является.
    history::record_at(
        &tx,
        id,
        None,
        kind.initial_status().as_str(),
        history::Cause::Created,
        &now,
        "-",
    )?;
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
    /// Метки: добавить (не заменяет набор).
    pub tag_add: &'a [String],
    /// Метки: снять.
    pub tag_remove: &'a [String],
    /// Метки: заменить набор целиком.
    ///
    /// Отдельно от `tag_add`, потому что это разные намерения: «добавь метку» и «оставь только
    /// эти». Одна команда на оба случая молча теряла бы метки при втором прочтении.
    pub tags: Option<&'a [String]>,
    /// Свободные поля: выставить (ключ перезаписывается).
    pub meta_set: &'a [(String, String)],
    /// Свободные поля: убрать ключи.
    pub meta_remove: &'a [String],
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

/// Сколько незакрытых узлов лежит ПОД каждым узлом проекта — одним обходом.
///
/// Нужен для вычисляемого `incomplete`: «закрыто, но не доведено» — это не запись в базе, а
/// вывод из состояния детей. Считать его в SQL-представлении значило бы держать второй источник
/// правды рядом с самим `nodes.status`; здесь же обход идёт один раз на выборку, а не на строку.
///
/// Считаются все потомки любого уровня: у закрытой задачи с открытой подзадачей эпик тоже
/// недоведён. `cancelled` в счёт не идёт — это законный исход, а не хвост.
pub fn unfinished_under(
    conn: &Connection,
    project_id: i64,
) -> Result<std::collections::HashMap<i64, i64>> {
    let sql = format!(
        "WITH RECURSIVE tree(ancestor_id, node_id, steps) AS (
           SELECT id, id, 0 FROM nodes WHERE project_id = ?1 AND deleted_at IS NULL
           UNION ALL
           SELECT t.ancestor_id, n.id, t.steps + 1
           FROM tree t JOIN nodes n ON n.parent_id = t.node_id
           WHERE t.steps < {max}
         )
         SELECT t.ancestor_id,
                SUM(CASE WHEN {} THEN 1 ELSE 0 END) AS unfinished
         FROM tree t
         JOIN nodes n ON n.id = t.node_id AND n.deleted_at IS NULL
         GROUP BY t.ancestor_id",
        crate::status::not_terminal_sql("n.status"),
        max = MAX_DEPTH
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![project_id], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<std::collections::HashMap<_, _>>>()?)
}

/// Подменяет эффективный статус закрытого узла на `incomplete`, если под ним есть незакрытые.
///
/// Хранимый статус остаётся `done`: это решение человека. `incomplete` — то, что читатель видит
/// про фактическое состояние, и он изменится сам, как только закроют последнюю задачу или
/// откроют новую. Именно поэтому автоперехода «переведите в done» не существует: он не нужен.
pub fn apply_derived_status(nodes: &mut [Node], unfinished: &std::collections::HashMap<i64, i64>) {
    for node in nodes.iter_mut() {
        if node.status == "done" && unfinished.get(&node.id).copied().unwrap_or(0) > 0 {
            node.effective_status = "incomplete".to_string();
        }
    }
}

/// То же для одного узла: считает незакрытых потомков на месте.
///
/// Отдельная функция, а не общий путь со списком: `show` и `ctx` читают один узел, и строить
/// карту по всему проекту ради него значило бы платить за весь проект одним запросом.
fn apply_derived_one(conn: &Connection, project: &Project, node: &mut Node) -> Result<()> {
    if node.status != "done" {
        return Ok(());
    }
    let sql = format!(
        "WITH RECURSIVE tree(node_id, steps) AS (
           SELECT ?1, 0
           UNION ALL
           SELECT n.id, t.steps + 1
           FROM tree t JOIN nodes n ON n.parent_id = t.node_id
           WHERE t.steps < {max}
         )
         SELECT COUNT(*) FROM tree t
         JOIN nodes n ON n.id = t.node_id
         WHERE t.node_id <> ?1 AND n.deleted_at IS NULL AND {}",
        crate::status::not_terminal_sql("n.status"),
        max = MAX_DEPTH
    );
    let _ = project;
    let unfinished: i64 = conn.query_row(&sql, params![node.id], |row| row.get(0))?;
    if unfinished > 0 {
        node.effective_status = "incomplete".to_string();
    }
    Ok(())
}

fn unfinished_blockers(conn: &Connection, node_id: i64) -> Result<Option<String>> {
    let sql = format!(
        "SELECT b.path FROM edges e
         JOIN nodes b ON b.id = e.to_node
         WHERE e.from_node = ?1 AND e.kind = 'blocks'
           AND b.deleted_at IS NULL AND {}
         ORDER BY b.path",
        crate::status::not_terminal_sql("b.status")
    );
    let mut stmt = conn.prepare(&sql)?;
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
        // Политика реоткрытия — из проекта, а не из кода: где-то `--force` обязателен,
        // где-то мешает. Матрица (куда можно) остаётся общей.
        check_with_policy(
            node.stored_status(),
            target,
            node.kind(),
            patch.force,
            project.reopen,
        )?;
        // Закрытие с открытыми детьми разрешено и НЕ требует `--force`: узел закрывается как
        // `done`, а недоведённость покажет вычисленный `incomplete` — пока под ним есть
        // незакрытые. Хранить вывод было бы вторым источником правды: закрыли эпик, открыли
        // задачу — и запись разошлась бы с фактом.
        if target == Status::Done
            && let Some(blockers) = unfinished_blockers(conn, node.id)?
            && !patch.force
        {
            return Err(Error::UnmetPrerequisites {
                dot_id: node.dot_id.clone(),
                blockers,
            });
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
        // Пустая строка — это «снять ветку», а не «ветка без имени»: в базе поле обязано
        // вернуться в NULL, иначе `ls --branch` пришлось бы учить отличать '' от NULL, а
        // `add --branch ""` и `set --no-branch` давали бы разные состояния одного и того же.
        let (value, new) = if branch.is_empty() {
            (Value::Null, None)
        } else {
            (Value::Text(branch.to_string()), Some(branch.to_string()))
        };
        changes.push(Change {
            column: "branch",
            value,
            old: node.branch.clone(),
            new,
        });
    }
    // Метки: замена набора, добавление и снятие — одно изменение, а не три. Промежуточные
    // состояния в журнал не попадают: «добавил и снял» в одной правке — это одна правка.
    if patch.tags.is_some() || !patch.tag_add.is_empty() || !patch.tag_remove.is_empty() {
        let mut tags: std::collections::BTreeSet<String> = node.tags.iter().cloned().collect();
        if let Some(replacement) = patch.tags {
            tags = replacement
                .iter()
                .map(|tag| tag.trim().to_string())
                .filter(|tag| !tag.is_empty())
                .collect();
        }
        for tag in patch.tag_add {
            let tag = tag.trim();
            if !tag.is_empty() {
                tags.insert(tag.to_string());
            }
        }
        for tag in patch.tag_remove {
            tags.remove(tag.trim());
        }
        let next: Vec<String> = tags.into_iter().collect();
        if next != node.tags {
            changes.push(Change {
                column: "tags",
                value: Value::Text(normalise_tags(&next)),
                old: Some(normalise_tags(&node.tags)),
                new: Some(normalise_tags(&next)),
            });
        }
    }
    if !patch.meta_set.is_empty() || !patch.meta_remove.is_empty() {
        let mut metadata = node.metadata.clone();
        for (key, value) in patch.meta_set {
            let key = key.trim();
            if key.is_empty() {
                return Err(Error::InvalidValue {
                    field: "meta".to_string(),
                    value: "empty key".to_string(),
                });
            }
            metadata.insert(key.to_string(), value.clone());
        }
        for key in patch.meta_remove {
            metadata.remove(key.trim());
        }
        if metadata != node.metadata {
            changes.push(Change {
                column: "metadata",
                value: Value::Text(normalise_metadata(&metadata)),
                old: Some(normalise_metadata(&node.metadata)),
                new: Some(normalise_metadata(&metadata)),
            });
        }
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
        // Факт перехода пишется здесь же: ручной переход — одна из причин, и он не должен
        // отличаться от остальных тем, что кто-то забыл вторую таблицу.
        if is_status {
            history::record(
                &tx,
                node.id,
                change.old.as_deref(),
                change.new.as_deref().unwrap_or("-"),
                history::Cause::Manual,
                actor,
            )?;
        }
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

/// Метки в том виде, в каком они лежат в базе: JSON-массив, без повторов, по порядку.
///
/// Порядок наводится, а не сохраняется как ввели: метки — множество, и `["api","dns"]` с
/// `["dns","api"]` обязаны быть одним и тем же значением, иначе сравнение «метки изменились»
/// срабатывало бы на перестановке и писало в журнал пустую правку.
pub fn normalise_tags(tags: &[String]) -> String {
    let unique: std::collections::BTreeSet<&str> = tags
        .iter()
        .map(|tag| tag.trim())
        .filter(|tag| !tag.is_empty())
        .collect();
    serde_json::to_string(&unique.into_iter().collect::<Vec<_>>()).expect("сериализуемо")
}

/// То же для свободных полей: JSON-объект, ключи по порядку.
fn normalise_metadata(metadata: &std::collections::BTreeMap<String, String>) -> String {
    serde_json::to_string(metadata).expect("сериализуемо")
}

/// Приводит произвольный JSON свободных полей к тому виду, в каком их хранит база.
///
/// Нужно на границе с чужими данными: импорт принимает `metadata` из дампа предшественника, где
/// значения бывают числами и флагами. Хранимое поле — объект строк, и это не педантизм: иначе
/// чтение узла зависело бы от того, чем именно записано значение, а `--meta key=value` и импорт
/// давали бы разные типы одного поля. Числа и флаги становятся текстом, вложенное — компактным
/// JSON: смысл сохраняется, тип один.
pub fn normalise_metadata_json(value: &serde_json::Value) -> String {
    let mut out = std::collections::BTreeMap::new();
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            let text = match value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            out.insert(key.clone(), text);
        }
    }
    normalise_metadata(&out)
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
    /// Только узлы этой ветки (`ls --branch`). `None` — фильтра нет.
    ///
    /// Сравнение точное: ветка — это адрес, а не ключевое слово. Префиксное совпадение
    /// смешало бы `feat/x` и `feat/x-fix`, и «что делать в этой ветке» отвечало бы про чужую.
    pub branch: Option<&'a str>,
    /// Только узлы с этой меткой (`ls --tag`).
    ///
    /// Сравнение точное: `api` и `apiv2` — разные метки, и поиск по вхождению смешивал бы их
    /// молча. Отбор идёт в SQL через `json_each`, а не разбором JSON в Rust: иначе фильтр читал
    /// бы все узлы проекта и отбрасывал их после — та же ошибка, что была с `limit`.
    pub tag: Option<&'a str>,
    pub overdue: bool,
    pub include_archived: bool,
    pub include_deleted: bool,
    pub limit: Option<i64>,
    /// Сколько узлов пропустить после сортировки (`--offset`).
    ///
    /// И `limit`, и `offset` применяются в Rust, а не в SQL, и это не небрежность: порядок
    /// списка — по числовым сегментам пути (`1.2` идёт раньше `1.10`), а SQLite строку `1.10`
    /// поставит раньше `1.2`. `LIMIT` без того же `ORDER BY` отрезал бы не первые строки, а
    /// случайные. Цена честного порядка — чтение всего списка; выигрыш от выбора полей и так
    /// в тридцать раз больше (292 КБ → 9 КБ на живой базе).
    pub offset: Option<i64>,
}

/// Ключ сортировки пути: числовые сегменты сравниваются как числа, а не строки.
pub fn path_sort_key(path: &str) -> Vec<u32> {
    path.split('.').filter_map(|s| s.parse().ok()).collect()
}

/// Все поля узла, которые можно выбрать флагом `--fields`.
///
/// Список закрытый и сверяется тестом с `Node`: опечатка в имени поля — отказ, а не пустой
/// столбец, который никто не заметит. Обратное тоже проверяется — поле, добавленное в `Node`,
/// обязано появиться здесь, иначе его нельзя будет выбрать.
pub const FIELDS: &[&str] = &[
    "id",
    "dot_id",
    "path",
    "project_id",
    "depth",
    "seq",
    "kind",
    "title",
    "description",
    "description_locked",
    "status",
    "effective_status",
    "priority",
    "assigned_to",
    "weight",
    "estimated_hours",
    "due_date",
    "branch",
    "source_ref",
    "tags",
    "metadata",
    "archived_at",
    "lease_holder",
    "lease_expires_at",
    "progress_done",
    "progress_total",
    "created_at",
    "updated_at",
];

/// Разбор `--fields a,b,c` в каноническом порядке `FIELDS`.
///
/// Порядок берётся из `FIELDS`, а не из строки: два вызова с теми же полями в разном порядке
/// обязаны дать одинаковый JSON, иначе сравнение двух ответов глазами превращается в угадывание.
/// Неизвестное имя — отказ со списком допустимых.
pub fn parse_fields(raw: &str) -> Result<Vec<&'static str>> {
    let mut wanted: Vec<&'static str> = Vec::new();
    for name in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let Some(field) = FIELDS.iter().find(|known| **known == name) else {
            return Err(Error::InvalidValue {
                field: "fields".to_string(),
                value: format!("{name} (allowed: {})", FIELDS.join(", ")),
            });
        };
        if !wanted.contains(field) {
            wanted.push(field);
        }
    }
    if wanted.is_empty() {
        return Err(Error::InvalidValue {
            field: "fields".to_string(),
            value: "empty (name at least one field)".to_string(),
        });
    }
    Ok(FIELDS
        .iter()
        .copied()
        .filter(|field| wanted.contains(field))
        .collect())
}

/// Узел в виде JSON с выбранными полями.
///
/// Пустой выбор означает «все поля»: вид ответа без новых флагов обязан остаться прежним.
/// `description` — главная причина, по которой выбор вообще нужен: на живой базе описание
/// занимает 197 КБ из 292 КБ в списке из 151 узла, то есть две трети ответа, который агент
/// читает ради адреса и статуса.
pub fn project_json(node: &Node, fields: &[&'static str]) -> serde_json::Value {
    let full = serde_json::to_value(node).expect("Node is serialisable");
    if fields.is_empty() {
        return full;
    }
    let serde_json::Value::Object(all) = full else {
        return serde_json::Value::Null;
    };
    let mut out = serde_json::Map::new();
    for field in fields {
        if let Some(value) = all.get(*field) {
            out.insert((*field).to_string(), value.clone());
        }
    }
    serde_json::Value::Object(out)
}

/// Список узлов по фильтру, в порядке дерева.
pub fn list(conn: &Connection, project: &Project, filter: &Filter<'_>) -> Result<Vec<Node>> {
    // Прогресс поддерева берётся из представления, а оно перечисляет пары «предок — потомок».
    // На широкой базе это гигабайты временного файла, поэтому объём проверяется до чтения.
    crate::tree::check_scan_budget(conn, Some(project))?;
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
        // Поддерево — обходом по структуре: `path GLOB` сканирует все узлы проекта (замер и
        // объяснение — в `crate::tree`).
        sql.push_str(" AND e.id IN (");
        sql.push_str(&crate::tree::subtree_sql());
        sql.push(')');
        values.push(Value::Integer(root.id));
    }
    // `status` и `exclude_done` НЕ переносятся в SQL: `incomplete` вычисляется в коде, и
    // представление о нём не знает. Отбор идёт ниже, после `apply_derived_status`, — иначе
    // `ls --status incomplete` молча вернул бы пустоту, а `ls -n` спрятал бы недоведённое.
    if let Some(assignee) = filter.assignee {
        sql.push_str(" AND e.assigned_to = ?");
        values.push(Value::Text(assignee.to_string()));
    }
    if let Some(branch) = filter.branch {
        sql.push_str(" AND e.branch = ?");
        values.push(Value::Text(branch.to_string()));
    }
    if let Some(tag) = filter.tag {
        // `json_each` по колонке меток: сравнение в SQL, а не разбор JSON в Rust — иначе фильтр
        // читал бы все узлы проекта и отбрасывал их после, ровно как это было с `limit`.
        sql.push_str(" AND EXISTS (SELECT 1 FROM json_each(e.tags) WHERE json_each.value = ?)");
        values.push(Value::Text(tag.to_string()));
    }
    if filter.overdue {
        sql.push_str(&format!(
            " AND e.due_date IS NOT NULL AND e.due_date < date('now')
               AND {}",
            crate::status::not_terminal_sql("e.effective_status")
        ));
    }

    let mut stmt = conn.prepare(&sql)?;
    let mut nodes = stmt
        .query_map(params_from_iter(values.iter()), |row| {
            map_node(row, project)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    nodes.sort_by_key(|node| path_sort_key(&node.path));
    // Вычисляемый `incomplete` ставится до отбора и обрезки: отбор по нему — часть контракта.
    let unfinished = unfinished_under(conn, project.id)?;
    apply_derived_status(&mut nodes, &unfinished);
    if let Some(status) = filter.status {
        nodes.retain(|node| node.effective_status == status);
    } else if filter.exclude_done {
        nodes.retain(|node| node.effective_status != "done");
    }
    if let Some(offset) = filter.offset {
        let skip = offset.max(0) as usize;
        if skip >= nodes.len() {
            nodes.clear();
        } else {
            nodes.drain(..skip);
        }
    }
    if let Some(limit) = filter.limit {
        nodes.truncate(limit.max(0) as usize);
    }
    Ok(nodes)
}

/// Узлы проекта по их идентификаторам — без фильтров, с вычисленным `incomplete`.
///
/// Нужен экрану для инкрементального обновления: после изменения в базе он перечитывает не доску
/// целиком, а только затронутые узлы и их предков. Отбор здесь намеренно не применяется —
/// фильтр доски живёт в одном месте (`overview::build_board`), и второй отбор по тем же правилам
/// разошёлся бы с ним на первом же новом фильтре.
///
/// Вычисляемый статус считается по одному узлу (`apply_derived_one`), а не картой по всему
/// проекту: у нас на руках единицы узлов, и карта на десять тысяч ради них была бы той же
/// работой, что и полное чтение.
pub fn by_ids(conn: &Connection, project: &Project, ids: &[i64]) -> Result<Vec<Node>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = (0..ids.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
    // Прогресс считается ОТДЕЛЬНЫМ запросом по нужным узлам, а не джойном с представлением.
    // Причина измерена: `LEFT JOIN v_node_progress` заставляет SQLite материализовать всё
    // представление (пары «предок — потомок» по всему проекту), даже когда в `IN` три
    // идентификатора. На фикстуре 10 000 узлов это 38 мс против 0,01 мс у самого чтения строк —
    // то есть вся дельта состояла из работы, которую она и должна была отменить.
    let sql = format!(
        // Нули на месте прогресса — заглушка для общей проекции строки: настоящие значения
        // приходят из `progress_of` ниже, и второй способ читать одно и то же здесь не заводится.
        "SELECT e.*, 0 AS progress_done, 0 AS progress_total FROM v_node_effective e
         WHERE e.project_id = ? AND e.deleted_at IS NULL AND e.id IN ({placeholders})"
    );
    let mut values: Vec<Value> = Vec::with_capacity(ids.len() + 1);
    values.push(Value::Integer(project.id));
    values.extend(ids.iter().map(|id| Value::Integer(*id)));
    let mut stmt = conn.prepare(&sql)?;
    let mut nodes = stmt
        .query_map(params_from_iter(values.iter()), |row| {
            map_node(row, project)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let progress = progress_of(conn, ids)?;
    for node in &mut nodes {
        if let Some((done, total)) = progress.get(&node.id) {
            node.progress_done = *done;
            node.progress_total = *total;
        }
    }
    for node in &mut nodes {
        apply_derived_one(conn, project, node)?;
    }
    nodes.sort_by_key(|node| path_sort_key(&node.path));
    Ok(nodes)
}

/// Прогресс поддерева для названных узлов — обходом только их поддеревьев.
///
/// Тот же смысл, что у представления `v_node_progress`, но с корнями, названными явно: обход
/// начинается с них, а не со всех узлов базы. Считается одним запросом на весь набор, а не по
/// одному: у дельты на руках единицы узлов, и N запросов стоили бы N раз столько же.
fn progress_of(
    conn: &Connection,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, (f64, f64)>> {
    let placeholders = (0..ids.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "WITH RECURSIVE tree(root_id, node_id, steps) AS (
           SELECT id, id, 0 FROM nodes WHERE id IN ({placeholders})
           UNION ALL
           SELECT t.root_id, n.id, t.steps + 1
           FROM tree t JOIN nodes n ON n.parent_id = t.node_id
           WHERE t.steps < {max}
         )
         SELECT t.root_id,
                SUM(CASE WHEN n.status = 'done' THEN n.weight ELSE 0 END) AS done_weight,
                SUM(n.weight) AS total_weight
         FROM tree t
         JOIN nodes n ON n.id = t.node_id AND n.deleted_at IS NULL
         GROUP BY t.root_id",
        max = MAX_DEPTH
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(ids.iter()), |row| {
        Ok((row.get::<_, i64>(0)?, (row.get(1)?, row.get(2)?)))
    })?;
    Ok(rows.collect::<rusqlite::Result<std::collections::HashMap<_, _>>>()?)
}

/// Список узлов по всем проектам базы, в порядке «проект, путь».
///
/// dot-ID собирается из ключа СВОЕГО проекта (`pr.key || '-' || e.path`): адрес узла
/// самодостаточен, поэтому список по всем проектам остаётся однозначным. Фильтр `parent`
/// здесь не поддерживается — ссылка-родитель живёт внутри проекта, и молча игнорировать её
/// значило бы выдать не то, что попросили.
pub fn list_all(conn: &Connection, filter: &Filter<'_>) -> Result<Vec<Node>> {
    crate::tree::check_scan_budget(conn, None)?;
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
    // `status` и `exclude_done` НЕ переносятся в SQL: `incomplete` вычисляется в коде, и
    // представление о нём не знает. Отбор идёт ниже, после `apply_derived_status`, — иначе
    // `ls --status incomplete` молча вернул бы пустоту, а `ls -n` спрятал бы недоведённое.
    if let Some(assignee) = filter.assignee {
        sql.push_str(" AND e.assigned_to = ?");
        values.push(Value::Text(assignee.to_string()));
    }
    if let Some(branch) = filter.branch {
        sql.push_str(" AND e.branch = ?");
        values.push(Value::Text(branch.to_string()));
    }
    if let Some(tag) = filter.tag {
        // `json_each` по колонке меток: сравнение в SQL, а не разбор JSON в Rust — иначе фильтр
        // читал бы все узлы проекта и отбрасывал их после, ровно как это было с `limit`.
        sql.push_str(" AND EXISTS (SELECT 1 FROM json_each(e.tags) WHERE json_each.value = ?)");
        values.push(Value::Text(tag.to_string()));
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
        sql.push_str(&format!(
            " AND e.due_date IS NOT NULL AND e.due_date < date('now')
               AND {}",
            crate::status::not_terminal_sql("e.effective_status")
        ));
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
                    // Политика тут не нужна: заглушка живёт только внутри `map_node`, который
                    // строит dot-ID и не проверяет переходы.
                    reopen: crate::status::Reopen::default(),
                },
            )?;
            node.dot_id = format!("{key}-{path}");
            Ok(node)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Порядок уже задан SQL (проект, затем путь); сортировка в коде не нужна.
    //
    // Вычисляемый `incomplete` — до отбора и обрезки, по карте на каждый проект из ответа:
    // отбор по этому состоянию (`ls --status incomplete`, `ls -n`) должен видеть его, а не
    // только хранимые статусы.
    let mut projects: Vec<i64> = nodes.iter().map(|node| node.project_id).collect();
    projects.sort_unstable();
    projects.dedup();
    for project_id in &projects {
        let unfinished = unfinished_under(conn, *project_id)?;
        for node in nodes
            .iter_mut()
            .filter(|node| node.project_id == *project_id)
        {
            if node.status == "done" && unfinished.get(&node.id).copied().unwrap_or(0) > 0 {
                node.effective_status = "incomplete".to_string();
            }
        }
    }
    if let Some(status) = filter.status {
        nodes.retain(|node| node.effective_status == status);
    } else if filter.exclude_done {
        nodes.retain(|node| node.effective_status != "done");
    }
    if let Some(offset) = filter.offset {
        let skip = offset.max(0) as usize;
        if skip >= nodes.len() {
            nodes.clear();
        } else {
            nodes.drain(..skip);
        }
    }
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

/// Сделает ли перенос `node_id` под `target` цикл в дереве.
///
/// Обход идёт вверх от целевого родителя к корню: если на этом пути встречается сам
/// переносимый узел, он является предком цели — перенос замкнул бы дерево на себя.
///
/// Обход ограничен `MAX_DEPTH + 1` шагов, а не «пока не кончится»: в испорченной базе
/// (цикл уже есть, база записана другой версией бинаря или руками) безусловный подъём повесил
/// бы команду. Ограничение — страховка от чужой ошибки, а не правило.
fn creates_cycle(conn: &Connection, node_id: i64, target: i64) -> Result<bool> {
    let mut current = Some(target);
    for _ in 0..=MAX_DEPTH {
        let Some(id) = current else {
            return Ok(false);
        };
        if id == node_id {
            return Ok(true);
        }
        current = conn
            .query_row(
                "SELECT parent_id FROM nodes WHERE id = ?1",
                params![id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
    }
    Ok(false)
}

/// dot-ID узла по id — для сообщения об ошибке.
fn parent_dot_id(conn: &Connection, project: &Project, id: i64) -> Result<String> {
    let path: Option<String> = conn
        .query_row("SELECT path FROM nodes WHERE id = ?1", params![id], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(path.map_or_else(|| format!("#{id}"), |path| project.dot_id(&path)))
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
    // Проверка повторяется ВНУТРИ транзакции, и уже обходом по `parent_id`, а не по префиксу
    // пути. Причин две.
    //
    // 1. Гонка. Проверка выше идёт до `BEGIN IMMEDIATE`, поэтому между ней и записью другой
    //    писатель мог успеть перенести узел — и цикл в дереве образовался бы уже после
    //    проверки. Цикл здесь не косметика: `path` потом считается рекурсивно, и дерево
    //    перестаёт читаться.
    // 2. Зависимость от хранимого пути. Префикс работает, пока `path` — колонка. В v4 путь
    //    становится вычисляемым, и проверка обязана опираться на структуру (`parent_id`),
    //    иначе тихо исчезнет вместе с колонкой.
    if let Some(target) = parent_id
        && creates_cycle(&tx, node.id, target)?
    {
        return Err(Error::MoveIntoSelf {
            dot_id: node.dot_id.clone(),
            target: parent_dot_id(&tx, &project, target)?,
        });
    }
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

    // Глубина считается по структуре: `depth` узла плюс самый длинный путь вниз от него.
    // Два запроса, а не один с подзапросом: у внешнего и внутреннего `?` общий счётчик, и
    // смешивать их в одном тексте — способ перепутать параметры при правке.
    let relative_deepest: i64 = tx
        .query_row(
            &crate::tree::subtree_max_depth_sql(),
            params![node.id],
            |row| row.get::<_, Option<i64>>(0),
        )?
        .unwrap_or(0);
    let deepest = node.depth + relative_deepest;
    let delta = new_depth - node.depth;
    if deepest + delta > MAX_DEPTH {
        return Err(Error::DepthLimit);
    }

    let descendants: Vec<(i64, String, i64)> = {
        let mut stmt = tx.prepare(&format!(
            "SELECT id, path, depth FROM nodes WHERE id IN ({}) ORDER BY path",
            crate::tree::descendants_sql()
        ))?;
        stmt.query_map(params![node.id], |row| {
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
    // Потомки переезжают по структуре: `substr(path, …)` остаётся, потому что `path` хранится,
    // но ОТБОР идёт обходом, а не сравнением строк.
    tx.execute(
        &format!(
            "UPDATE nodes SET path = ? || substr(path, ?), depth = depth + ?, updated_at = ?
             WHERE id IN ({})",
            crate::tree::descendants_sql()
        ),
        params![new_path, node.path.len() as i64 + 1, delta, now, node.id],
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
    // Тот же пересчёт, что и при переносе в проект: `mv` внутри проекта тоже меняет глубину, а
    // значит и вид узла. Без этого задача, перенесённая в корень, оставалась «задачей» глубины 1.
    renormalise_after_move(&tx, project.id, node.id)?;

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

/// Что получилось при переносе узла в другой проект.
///
/// Отчёт, а не просто узел: перенос меняет адрес, и вызывающий обязан увидеть все три следствия —
/// новый адрес, число переписанных ссылок и то, что комментарий о прежнем месте добавлен. Молча
/// вернуть другой `dot_id` значило бы заставить человека догадываться, что ещё изменилось.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MoveReport {
    pub dot_id: String,
    pub previous_dot_id: String,
    pub project: String,
    pub previous_project: String,
    /// Узлов переехало (сам узел и его потомки).
    pub nodes: usize,
    /// Сколько записей получили новый адрес: узлы, комментарии, заметки.
    pub references: ReferenceReport,
    /// Узлам сменялся вид или статус: перенос меняет глубину, а вид — её следствие.
    pub rekinded: usize,
}

/// Кого коснулось переписывание ссылок.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ReferenceReport {
    pub nodes: usize,
    pub comments: usize,
    pub notes: usize,
}

impl ReferenceReport {
    pub fn total(&self) -> usize {
        self.nodes + self.comments + self.notes
    }
}

/// Переносит узел с поддеревом в другой проект.
///
/// Что решено и почему (решение владельца, TASQDEV-16.1):
///
///   * адрес перестраивается: `RRL-3.1` становится `<новый ключ>-<seq>`. Алиасы не хранятся —
///     второй способ разрешить ту же ссылку разошёлся бы с первым, и `NODE_NOT_FOUND` начал бы
///     зависеть от того, какой базе верить;
///   * ссылки переписываются во всех НЕархивных узлах базы: в заголовках, описаниях,
///     `source_ref`, в комментариях к ним и в заметках. Архив — это застывшая история, и править
///     в нём текст задним числом значит переписывать прошлое; архивные записи остаются с прежним
///     адресом;
///   * в перенесённый узел добавляется комментарий о прежнем месте: адрес сменился, и тот, кто
///     откроет узел через полгода, должен видеть, откуда он приехал и почему в старых записях
///     другой адрес.
///
/// История, аренды и комментарии переезжают сами: они ссылаются на `node_id`, а не на проект.
pub fn move_to_project(
    conn: &mut Connection,
    project: &Project,
    reference: &str,
    target_key: &str,
    new_parent: Option<&str>,
    renumber: bool,
) -> Result<MoveReport> {
    let (source, node) = resolve_with_project(conn, project, reference)?;
    let target =
        crate::project::by_key(conn, target_key)?.ok_or_else(|| Error::ProjectNotFound {
            key: target_key.to_string(),
            known: crate::project::known_keys(conn).unwrap_or_default(),
        })?;
    if target.id == source.id {
        return Err(Error::InvalidValue {
            field: "to_project".to_string(),
            value: format!(
                "{} is already the node's project; use `mv` without --to-project",
                target.key
            ),
        });
    }

    let (parent_id, new_depth, parent_path) = match new_parent {
        Some(reference) => {
            // Родитель ищется В ЦЕЛЕВОМ проекте: переносить узел под родителя чужого проекта
            // нельзя, а разрешать ссылку в старом значило бы принять `RRL-1` за адрес в новом.
            let parent = resolve(conn, &target, reference)?;
            (Some(parent.id), parent.depth + 1, parent.path)
        }
        None => (None, 1, String::new()),
    };

    let snapshot: Vec<(i64, String, i64)> = {
        let mut stmt = conn.prepare(&format!(
            "SELECT id, path, depth FROM nodes WHERE id IN ({}) ORDER BY path",
            crate::tree::descendants_sql()
        ))?;
        stmt.query_map(params![node.id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let relative_deepest = snapshot
        .iter()
        .map(|(_, _, depth)| *depth - node.depth)
        .max()
        .unwrap_or(0);
    let deepest = node.depth + relative_deepest;
    let delta = new_depth - node.depth;
    if deepest + delta > MAX_DEPTH {
        return Err(Error::DepthLimit);
    }

    let tx = db::write_tx(conn)?;
    // Номер в целевом проекте: тот же номер, если он свободен, иначе следующий свободный.
    // Молча падать на `UNIQUE (project_id, path)` нельзя — перенос в занятую ветку обычная
    // операция, а не ошибка вызывающего.
    let next_free: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM nodes WHERE project_id = ?1 AND parent_id IS ?2",
        params![target.id, parent_id],
        |row| row.get(0),
    )?;
    let taken = |seq: i64| -> Result<bool> {
        let path = if parent_path.is_empty() {
            seq.to_string()
        } else {
            format!("{parent_path}.{seq}")
        };
        Ok(tx.query_row(
            "SELECT COUNT(*) FROM nodes WHERE project_id = ?1 AND path = ?2",
            params![target.id, path],
            |row| row.get::<_, i64>(0),
        )? > 0)
    };
    let seq = if renumber || taken(node.seq)? {
        next_free
    } else {
        node.seq
    };
    let new_path = if parent_path.is_empty() {
        seq.to_string()
    } else {
        format!("{parent_path}.{seq}")
    };
    validate_path(&new_path)?;

    let now = now_iso();
    tx.execute(
        "UPDATE nodes SET project_id = ?1, parent_id = ?2, depth = ?3, seq = ?4, path = ?5,
                          updated_at = ?6
         WHERE id = ?7",
        params![target.id, parent_id, new_depth, seq, new_path, now, node.id],
    )?;
    let moved = tx.execute(
        &format!(
            "UPDATE nodes SET project_id = ?1, path = ?2 || substr(path, ?3), depth = depth + ?4,
                              updated_at = ?5
             WHERE id IN ({})",
            crate::tree::descendants_sql()
        ),
        params![
            target.id,
            new_path,
            node.path.len() as i64 + 1,
            delta,
            now,
            node.id
        ],
    )? + 1;

    // Вид подчиняется глубине: задача, поставленная в корень чужого проекта, становится эпиком,
    // эпик, опущенный под задачу, — подзадачей, и дети едут вместе с ним.
    let rekinded = renormalise_after_move(&tx, target.id, node.id)?;

    let previous_dot = source.dot_id(&node.path);
    let new_dot = target.dot_id(&new_path);

    // Ссылки переписываются ПОСЛЕ смены адресов: старый адрес известен, новый посчитан, и обе
    // записи уже в базе — если что-то упадёт, транзакция откатит всё вместе.
    let references = rewrite_references(&tx, &previous_dot, &new_dot)?;

    // Комментарий о прежнем месте: адрес сменился, и человек, открывший узел через полгода,
    // обязан видеть, откуда он приехал — иначе архивная запись с другим адресом выглядит
    // ошибкой, а не историей.
    tx.execute(
        "INSERT INTO comments (node_id, author, content, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            node.id,
            "tasq",
            format!(
                "Перенесён из проекта {} в {}: {previous_dot} → {new_dot}. Ссылки в неархивных \
                 записях обновлены; архивные хранят прежний адрес намеренно.",
                source.key, target.key
            ),
            now
        ],
    )?;

    // Сводка собирается заранее: `Entry` держит `&str`, и временная строка внутри литерала
    // структуры жила бы короче самого вызова.
    let summary = format!(
        "{} -> {}, {} node(s), {} reference(s)",
        source.key,
        target.key,
        moved,
        references.total()
    );
    activity::log(
        &tx,
        &Entry {
            project_id: target.id,
            node_id: Some(node.id),
            entity_type: "node",
            entity_id: node.id,
            action: "move_project",
            field_name: Some("project_id"),
            old_value: Some(previous_dot.as_str()),
            new_value: Some(new_dot.as_str()),
            summary: Some(summary.as_str()),
            actor: "-",
        },
    )?;
    tx.commit()?;

    Ok(MoveReport {
        dot_id: new_dot,
        previous_dot_id: previous_dot,
        project: target.key.clone(),
        previous_project: source.key.clone(),
        nodes: moved,
        references,
        rekinded,
    })
}

/// Переписывает ссылки на старый адрес в текстах, которые читает человек.
///
/// Область — всё, что не архив: заголовок, описание и `source_ref` узла, комментарии к таким
/// узлам, заметки (привязанные к неархивным узлам или ни к чему не привязанные). Архив
/// намеренно не трогается: это застывшая история, и правка текста в нём задним числом
/// переписала бы прошлое.
///
/// `updated_at` у узлов НЕ обновляется при правке текста: дата последней работы над задачей —
/// это не дата переезда соседа, и поднимать её значило бы врать в отчётах о застое.
fn rewrite_references(
    tx: &rusqlite::Transaction<'_>,
    previous: &str,
    current: &str,
) -> Result<ReferenceReport> {
    let mut report = ReferenceReport::default();

    // 1. Узлы: заголовок, описание, source_ref.
    let rows: Vec<(i64, String, Option<String>, Option<String>)> = {
        let mut stmt = tx.prepare(
            "SELECT id, title, description, source_ref FROM nodes
             WHERE archived_at IS NULL
               AND (title LIKE ?1 OR description LIKE ?1 OR source_ref LIKE ?1)",
        )?;
        // `LIKE` — только грубый отбор: точное совпадение проверяет `refs::rewrite`, иначе
        // `RRL-3.1` совпал бы с `RRL-3.10` ещё на этом шаге.
        stmt.query_map(params![format!("%{previous}%")], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, title, description, source_ref) in rows {
        let new_title = crate::refs::rewrite(&title, previous, current);
        let new_description = description
            .as_deref()
            .and_then(|text| crate::refs::rewrite(text, previous, current));
        let new_source = source_ref
            .as_deref()
            .and_then(|text| crate::refs::rewrite(text, previous, current));
        if new_title.is_none() && new_description.is_none() && new_source.is_none() {
            continue;
        }
        // Пустое значение — «оставить как есть»: без этого правка `title` затирала бы описание.
        tx.execute(
            "UPDATE nodes SET title = COALESCE(?1, title), description = COALESCE(?2, description),
                              source_ref = COALESCE(?3, source_ref)
             WHERE id = ?4",
            params![new_title, new_description, new_source, id],
        )?;
        report.nodes += 1;
    }

    // 2. Комментарии к неархивным узлам.
    let comments: Vec<(i64, String)> = {
        let mut stmt = tx.prepare(
            "SELECT c.id, c.content FROM comments c JOIN nodes n ON n.id = c.node_id
             WHERE n.archived_at IS NULL AND c.is_deleted = 0 AND c.content LIKE ?1",
        )?;
        stmt.query_map(params![format!("%{previous}%")], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, content) in comments {
        if let Some(updated) = crate::refs::rewrite(&content, previous, current) {
            tx.execute(
                "UPDATE comments SET content = ?1 WHERE id = ?2",
                params![updated, id],
            )?;
            report.comments += 1;
        }
    }

    // 3. Заметки: привязанные к неархивным узлам и свободные. Заметка проекта — тоже текст,
    // который читают, и ссылка в ней стареет так же.
    let notes: Vec<(i64, String, String)> = {
        let mut stmt = tx.prepare(
            "SELECT nt.id, nt.title, nt.content FROM notes nt
             LEFT JOIN nodes n ON n.id = nt.node_id
             WHERE (nt.node_id IS NULL OR n.archived_at IS NULL)
               AND (nt.title LIKE ?1 OR nt.content LIKE ?1)",
        )?;
        stmt.query_map(params![format!("%{previous}%")], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, title, content) in notes {
        let new_title = crate::refs::rewrite(&title, previous, current);
        let new_content = crate::refs::rewrite(&content, previous, current);
        if new_title.is_none() && new_content.is_none() {
            continue;
        }
        tx.execute(
            "UPDATE notes SET title = COALESCE(?1, title), content = COALESCE(?2, content),
                              updated_at = ?3
             WHERE id = ?4",
            params![new_title, new_content, now_iso(), id],
        )?;
        report.notes += 1;
    }

    Ok(report)
}

/// Пересчитывает вид узлов поддерева по их НОВОЙ глубине и нормализует статус под вид.
///
/// Вид узла — не свободное поле, а следствие глубины (`Kind::from_depth`). Задача, поставленная
/// в корень проекта, становится эпиком; эпик, опущенный под задачу, — подзадачей; её дети
/// сдвигаются на уровень вниз вместе с ней. До этой правки перенос менял `depth` и `path`, но не
/// `kind`, и в базе оказывался «эпик» глубины 3: набор допустимых статусов и подсказка матрицы
/// начинали зависеть от того, каким путём узел сюда попал.
///
/// Статус нормализуется вместе с видом, потому что наборы статусов у видов разные: эпику запрещён
/// `todo`, остальным — `planned`. Обе стороны НЕтерминальны, поэтому burn-up и отчёты о закрытии
/// от такой замены не меняются — и строки в истории статусов здесь нет намеренно: история
/// отвечает «когда узел стал done», а `todo ↔ planned` — не решение о работе, а следствие места
/// в дереве. В журнал запись идёт: он отвечает «что произошло».
///
/// Возвращает число узлов, которым сменился вид или статус.
fn renormalise_after_move(
    tx: &rusqlite::Transaction<'_>,
    project_id: i64,
    root_id: i64,
) -> Result<usize> {
    let rows: Vec<(i64, i64, String, String)> = {
        let mut stmt = tx.prepare(&format!(
            "SELECT id, depth, kind, status FROM nodes WHERE id IN ({})",
            crate::tree::subtree_sql()
        ))?;
        stmt.query_map(params![root_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut changed = 0;
    for (id, depth, kind, status) in rows {
        let want_kind = Kind::from_depth(depth);
        let want_status = normalise_status_for(&status, want_kind);
        let kind_changed = kind != want_kind.as_str();
        let status_changed = status != want_status;
        if !kind_changed && !status_changed {
            continue;
        }
        tx.execute(
            "UPDATE nodes SET kind = ?1, status = ?2 WHERE id = ?3",
            params![want_kind.as_str(), want_status, id],
        )?;
        if kind_changed {
            activity::log(
                tx,
                &Entry {
                    project_id,
                    node_id: Some(id),
                    entity_type: "node",
                    entity_id: id,
                    action: "move",
                    field_name: Some("kind"),
                    old_value: Some(kind.as_str()),
                    new_value: Some(want_kind.as_str()),
                    summary: Some("вид пересчитан по новой глубине"),
                    actor: "-",
                },
            )?;
        }
        if status_changed {
            activity::log(
                tx,
                &Entry {
                    project_id,
                    node_id: Some(id),
                    entity_type: "node",
                    entity_id: id,
                    action: "move",
                    field_name: Some("status"),
                    old_value: Some(status.as_str()),
                    new_value: Some(want_status.as_str()),
                    summary: Some("статус приведён к набору нового вида"),
                    actor: "-",
                },
            )?;
        }
        changed += 1;
    }
    Ok(changed)
}

/// Статус, допустимый для вида: у эпика не бывает `todo`, у остальных — `planned`.
///
/// Замена, а не отказ: перенос — это структурное действие, и требовать от человека «сначала
/// поменяй статус, потом переноси» значило бы перекладывать следствие на него. Обе пары
/// нетерминальны, поэтому замена ничего не значит для отчётов.
fn normalise_status_for(status: &str, kind: Kind) -> String {
    match Status::parse(status) {
        Some(parsed) if parsed.allowed_for(kind) => status.to_string(),
        Some(Status::Todo) => Status::Planned.as_str().to_string(),
        Some(Status::Planned) => Status::Todo.as_str().to_string(),
        _ => status.to_string(),
    }
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
        &format!(
            "UPDATE nodes SET archived_at = ?, updated_at = ? WHERE id IN ({})",
            crate::tree::subtree_sql()
        ),
        params![if undo { None } else { Some(now.clone()) }, now, node.id],
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

/// Итог массового архива.
#[derive(Debug, Clone, Serialize)]
pub struct ArchiveBatch {
    /// Ключ проекта, в котором работали.
    pub project: String,
    /// Адреса выбранных узлов — САМЫХ ВЕРХНИХ в своём поддереве.
    pub refs: Vec<String>,
    /// Сколько узлов изменилось (или изменилось бы при `dry_run`).
    pub affected: usize,
    /// Ничего не записано: только показано, что было бы.
    pub dry_run: bool,
    /// Что делали: архив или распаковка.
    pub archived: bool,
}

/// Архив (или распаковка) всех узлов проекта в заданном статусе.
///
/// Выбираются только САМЫЕ ВЕРХНИЕ узлы множества: если родитель тоже в множестве, ребёнок не
/// берётся — родитель накрывает его поддеревом. Без этого правила второй проход упирается в
/// `ALREADY_IN_STATE` на каждом вложенном узле, а первый пишет по строке журнала на каждый из
/// них вместо одного на поддерево.
///
/// Статус берётся ЭФФЕКТИВНЫЙ — тот, который человек видит в `ls` и `board`. Разница видна на
/// производных статусах: `in_progress` появляется от живой аренды, `blocked` — от незакрытого
/// блокера, и по хранимому статусу это были бы другие узлы. Для `done` разницы нет: представление
/// отдаёт `done` только тогда, когда хранимый статус и есть `done` (`cancelled` идёт отдельно).
///
/// Поддерево уходит целиком, включая детей в других статусах: это поведение `archive` для одной
/// ссылки, и второй набор правил здесь разошёлся бы с ним.
pub fn archive_batch(
    conn: &mut Connection,
    project: &Project,
    status: &Status,
    undo: bool,
    dry_run: bool,
) -> Result<ArchiveBatch> {
    let roots = topmost_in_status(conn, project, status, undo)?;
    let affected = count_in_subtrees(conn, &roots)?;
    let report = ArchiveBatch {
        project: project.key.clone(),
        refs: roots.iter().map(|(_, dot_id)| dot_id.clone()).collect(),
        affected,
        dry_run,
        archived: !undo,
    };
    if dry_run || roots.is_empty() {
        return Ok(report);
    }

    // Одна транзакция на весь пакет: половина заархивированного пакета — это состояние, которого
    // никто не просил, и его пришлось бы разбирать руками.
    let now = now_iso();
    let tx = db::write_tx(conn)?;
    for (id, dot_id) in &roots {
        tx.execute(
            &format!(
                "UPDATE nodes SET archived_at = ?, updated_at = ? WHERE id IN ({})",
                crate::tree::subtree_sql()
            ),
            params![if undo { None } else { Some(now.clone()) }, now, id],
        )?;
        activity::log(
            &tx,
            &Entry {
                project_id: project.id,
                node_id: Some(*id),
                entity_type: "node",
                entity_id: *id,
                action: if undo { "unarchive" } else { "archive" },
                field_name: Some("archived_at"),
                old_value: None,
                new_value: if undo { None } else { Some(now.as_str()) },
                summary: Some(dot_id.as_str()),
                actor: "-",
            },
        )?;
    }
    tx.commit()?;
    Ok(report)
}

/// Самые верхние узлы проекта в заданном статусе: `(id, dot_id)`.
///
/// `undo` переворачивает признак архива: распаковываются те, что УЖЕ архивны, и верхним считается
/// тот, чей родитель не архивный, — иначе распаковка родителя и ребёнка шла бы дважды.
fn topmost_in_status(
    conn: &Connection,
    project: &Project,
    status: &Status,
    undo: bool,
) -> Result<Vec<(i64, String)>> {
    // Признак архива вынесен в литерал SQL, а не в параметр: `IS ?` с NULL не работает, а
    // `= NULL` не совпадает никогда — на этом легко получить пустой список вместо отказа.
    let archived = if undo { "IS NOT NULL" } else { "IS NULL" };
    let sql = format!(
        "SELECT e.id, e.path FROM v_node_effective e
          WHERE e.project_id = ?1
            AND e.deleted_at IS NULL
            AND e.effective_status = ?2
            AND e.archived_at {archived}
            AND NOT EXISTS (
              SELECT 1 FROM v_node_effective p
               WHERE p.id = e.parent_id
                 AND p.deleted_at IS NULL
                 AND p.effective_status = ?2
                 AND p.archived_at {archived})
          ORDER BY e.path"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![project.id, status.as_str()], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut found = Vec::new();
    for row in rows {
        let (id, path) = row?;
        found.push((id, project.dot_id(&path)));
    }
    Ok(found)
}

/// Сколько узлов накрывают выбранные поддеревья — с повторами не считая.
///
/// Именно размер поддерева, а не число выбранных узлов: человеку важно, сколько строк исчезнет из
/// выдач, а не сколько ссылок было напечатано.
fn count_in_subtrees(conn: &Connection, roots: &[(i64, String)]) -> Result<usize> {
    let mut total = 0usize;
    for (id, _) in roots {
        let count: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM nodes WHERE id IN ({})",
                crate::tree::subtree_sql()
            ),
            [id],
            |row| row.get(0),
        )?;
        total += usize::try_from(count).unwrap_or(0);
    }
    Ok(total)
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
