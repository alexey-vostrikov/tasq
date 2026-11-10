//! Экспорт и импорт проекта: свой формат с контрольной суммой и совместимость с saga-mcp
//! (README §16).
//!
//! Экспорт — детерминированный: одинаковые данные дают одинаковый JSON и одинаковую сумму.
//! Импорт — одна транзакция: сначала полная проверка, потом запись; при любой ошибке откат.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::activity::{self, Entry};
use crate::db::now_iso;
use crate::error::{Error, Result};
use crate::history;
use crate::project::Project;

mod restore;

pub use restore::{RestoreLoss, RestoreReport, TableRestore, restore};

/// Метка формата нашего экспорта.
pub const SCHEMA: &str = "tasq.export.v1";
/// Поддерживаемые версии экспорта saga-mcp.
pub const SAGA_VERSIONS: [&str; 4] = ["1.0", "1.1", "1.2", "1.3"];

/// Что импортировать.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Только добавить отсутствующие узлы; существующие не трогать.
    Merge,
    /// Снести данные проекта и загрузить файл целиком.
    Replace,
}

impl Mode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "merge" => Some(Mode::Merge),
            "replace" => Some(Mode::Replace),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Merge => "merge",
            Mode::Replace => "replace",
        }
    }
}

/// Итог импорта.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Imported {
    pub mode: String,
    pub nodes_created: usize,
    pub nodes_skipped: usize,
    pub edges: usize,
    pub comments: usize,
    pub notes: usize,
    pub templates: usize,
    /// Строк истории перенесено, и откуда они взяты.
    ///
    /// `None` — в файле истории нет, и по одному факту на узел не писалось (импорт saga:
    /// истории там не было и быть не могло). `Some(0)` при `history: present` означает, что
    /// переходов у узлов действительно не было.
    pub history: Option<usize>,
    pub checksum_verified: bool,
}

/// Канонический JSON: компактный, без пробелов, ключи отсортированы.
///
/// `serde_json::Map` по умолчанию держит ключи в `BTreeMap`, поэтому один и тот же набор полей
/// всегда даёт одинаковую строку — от неё и считается контрольная сумма.
fn canonical(value: &Value) -> String {
    serde_json::to_string(value).expect("serialisable")
}

/// SHA-256 строки в hex.
/// Сумма payload в том виде, в каком она попадает в документ обмена.
///
/// Вынесена наружу, потому что документ собирают и правят не только здесь: тесты,
/// скрипты миграции и внешние инструменты обязаны считать ту же сумму, иначе проверка
/// отвергнет их документ как повреждённый.
pub fn payload_checksum(data: &Value) -> String {
    checksum(&canonical(data))
}

fn checksum(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Таблицы дампа базы — в порядке вставки (родители раньше детей).
///
/// `meta` идёт первой: в ней версия схемы, по ней восстановление понимает, чего ожидать.
/// FTS-таблицы (`nodes_fts`, `notes_fts`) сюда не входят: это индексы с внешним контентом,
/// они перестраиваются из `nodes`/`notes` при восстановлении, и тащить их в дамп значило бы
/// хранить два источника правды.
pub const DUMP_TABLES: &[&str] = &[
    "meta",
    "projects",
    // `statuses` в дамп НЕ входит: справочник — часть схемы, а не данных. Он создаётся
    // миграцией и обязан совпадать с `Status::all()` (это проверяет тест), поэтому вставка
    // строк из файла была бы попыткой наложить данные на схему — ровно та ошибка, из-за
    // которой v4 и делалась. Статус, которого нет в справочнике бинаря, отбивается отчётом
    // «статус не зарегистрирован», а не молчаливой вставкой.
    //
    // Сессии — до аренд: `attempts.session_id` ссылается на них.
    "sessions",
    "nodes",
    // История — после узлов и справочника: ссылается на оба.
    "node_status_history",
    "attempts",
    "reservations",
    "edges",
    "comments",
    "notes",
    "templates",
    "activity_log",
    "burn_snapshots",
];

/// Дамп ВСЕЙ базы: все проекты и все таблицы, с версией схемы и контрольной суммой.
///
/// Отличие от [`export`] (документ одного проекта): здесь нет выбора — берётся всё, что есть
/// в файле, включая аренды, резервы и журнал. Без этого редизайн схемы не начать: экспорт
/// проекта не восстанавливает базу (нет `attempts`, `reservations`, `activity_log` и остальных
/// проектов), а `burnup`/`time` на восстановленной базе остаются пустыми.
///
/// Строки читаются универсально — по `SELECT *` и типам значений, а не по списку колонок:
/// дамп должен переживать правку схемы, иначе под каждый новый столбец нужен новый код.
/// Порядок строк — по первичному ключу (для `meta` он текстовый), поэтому две выгрузки одного
/// состояния дают одинаковую контрольную сумму.
pub fn dump(conn: &Connection) -> Result<Value> {
    let schema_version = crate::db::schema_version_of(conn).unwrap_or(0);

    let mut tables = serde_json::Map::new();
    for table in DUMP_TABLES {
        // Имя таблицы — из константы, а не из входа: подстановка пользовательских строк в SQL
        // здесь недопустима даже при том, что список закрыт.
        let mut stmt = conn.prepare(&format!("SELECT * FROM {table} ORDER BY 1"))?;
        // `column_names()` возвращает ИТЕРАТОР `&str`, а не срез: `.iter()` у него нет, а
        // `.into_iter()` clippy с `-D warnings` отбивает как лишнее преобразование.
        let columns: Vec<String> = stmt.column_names().map(str::to_string).collect();
        let mut rows_out: Vec<Value> = Vec::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let mut object = serde_json::Map::new();
            for (index, name) in columns.iter().enumerate() {
                let value = match row.get_ref(index)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(number) => Value::from(number),
                    rusqlite::types::ValueRef::Real(number) => Value::from(number),
                    rusqlite::types::ValueRef::Text(text) => {
                        Value::from(String::from_utf8_lossy(text).to_string())
                    }
                    // BLOB в схеме нет; если появится — отказ громче, чем повреждённый дамп.
                    rusqlite::types::ValueRef::Blob(bytes) => {
                        return Err(Error::InvalidValue {
                            field: format!("{table}.{name}"),
                            value: format!("blob of {} byte(s) is not dumpable", bytes.len()),
                        });
                    }
                };
                object.insert(name.clone(), value);
            }
            rows_out.push(Value::Object(object));
        }
        tables.insert(table.to_string(), Value::Array(rows_out));
    }

    let counts: serde_json::Map<String, Value> = tables
        .iter()
        .map(|(name, rows)| {
            (
                name.clone(),
                Value::from(rows.as_array().map(Vec::len).unwrap_or(0)),
            )
        })
        .collect();

    // Сумма считается по тому же набору, что и в экспорте проекта: формат, версия схемы и
    // таблицы. Момент выгрузки и версия бинаря — снаружи, иначе две выгрузки одного состояния
    // дают разные суммы.
    let payload = json!({
        "kind": DUMP_KIND,
        "format": DUMP_FORMAT,
        "schema_version": schema_version,
        "tables": Value::Object(tables.clone()),
    });
    Ok(json!({
        "kind": DUMP_KIND,
        "format": DUMP_FORMAT,
        "schema_version": schema_version,
        "version": crate::VERSION,
        "exported_at": crate::db::now_iso(),
        "counts": Value::Object(counts),
        "tables": Value::Object(tables),
        "sha256": payload_checksum(&payload),
    }))
}

/// Дамп в YAML: тот же документ, но человекочитаемый и правимый руками.
///
/// Формат выбирает ВЫЗЫВАЮЩИЙ (по расширению `--out` или явному флагу), а не догадка по
/// содержимому: дамп — состояние, и молчаливое переключение формата на файле без расширения
/// было бы сюрпризом. Парсер — `serde-saphyr`: чистый Rust без C под капотом и без `unsafe`
/// в своей коде, чего `unsafe-libyaml` (зависимость `serde_yaml_ng`) дать не может.
pub fn dump_yaml(document: &Value) -> Result<String> {
    serde_saphyr::to_string(document).map_err(|err| Error::InvalidValue {
        field: "yaml".to_string(),
        value: err.to_string(),
    })
}

/// Чтение документа из YAML — обратная операция, нужна импорту и тестам на круговой путь.
pub fn read_yaml(text: &str) -> Result<Value> {
    serde_saphyr::from_str(text).map_err(|err| Error::InvalidValue {
        field: "yaml".to_string(),
        value: err.to_string(),
    })
}

/// Вид и версия документа дампа: по ним восстановление отказывает на чужом файле.
pub const DUMP_KIND: &str = "tasq-dump";
pub const DUMP_FORMAT: i64 = 1;

/// Собирает документ экспорта.
pub fn export(conn: &Connection, project: &Project, include_deleted: bool) -> Result<Value> {
    let mut nodes: Vec<Value> = Vec::new();
    {
        let mut sql = String::from(
            "SELECT id, parent_id, depth, seq, path, kind, title, description, description_locked,
                    status, priority, assigned_to, weight, estimated_hours, due_date, branch,
                    source_ref, tags, metadata, sort_order, archived_at, deleted_at,
                    created_at, updated_at
             FROM nodes WHERE project_id = ?1",
        );
        if !include_deleted {
            sql.push_str(" AND deleted_at IS NULL");
        }
        sql.push_str(" ORDER BY path");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![project.id], |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "parent_id": row.get::<_, Option<i64>>(1)?,
                "depth": row.get::<_, i64>(2)?,
                "seq": row.get::<_, i64>(3)?,
                "path": row.get::<_, String>(4)?,
                "kind": row.get::<_, String>(5)?,
                "title": row.get::<_, String>(6)?,
                "description": row.get::<_, Option<String>>(7)?,
                "description_locked": row.get::<_, i64>(8)? != 0,
                "status": row.get::<_, String>(9)?,
                "priority": row.get::<_, String>(10)?,
                "assigned_to": row.get::<_, Option<String>>(11)?,
                "weight": row.get::<_, f64>(12)?,
                "estimated_hours": row.get::<_, Option<f64>>(13)?,
                "due_date": row.get::<_, Option<String>>(14)?,
                "branch": row.get::<_, Option<String>>(15)?,
                "source_ref": row.get::<_, Option<String>>(16)?,
                "tags": row.get::<_, String>(17)?,
                "metadata": row.get::<_, String>(18)?,
                "sort_order": row.get::<_, i64>(19)?,
                "archived_at": row.get::<_, Option<String>>(20)?,
                "deleted_at": row.get::<_, Option<String>>(21)?,
                "created_at": row.get::<_, String>(22)?,
                "updated_at": row.get::<_, String>(23)?,
            }))
        })?;
        for row in rows {
            nodes.push(row?);
        }
    }

    let edges: Vec<Value> = {
        let mut stmt = conn.prepare(
            "SELECT f.path, t.path, e.kind, e.created_at, e.created_by
             FROM edges e
             JOIN nodes f ON f.id = e.from_node
             JOIN nodes t ON t.id = e.to_node
             WHERE f.project_id = ?1
             ORDER BY f.path, t.path, e.kind",
        )?;
        stmt.query_map(params![project.id], |row| {
            Ok(json!({
                "from": row.get::<_, String>(0)?,
                "to": row.get::<_, String>(1)?,
                "kind": row.get::<_, String>(2)?,
                "created_at": row.get::<_, String>(3)?,
                "created_by": row.get::<_, Option<String>>(4)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let comments: Vec<Value> = {
        let mut stmt = conn.prepare(
            "SELECT n.path, c.author, c.content, c.is_deleted, c.deleted_at, c.deleted_by,
                    c.delete_reason, c.created_at
             FROM comments c JOIN nodes n ON n.id = c.node_id
             WHERE n.project_id = ?1
             ORDER BY c.created_at, c.id",
        )?;
        stmt.query_map(params![project.id], |row| {
            Ok(json!({
                "node": row.get::<_, String>(0)?,
                "author": row.get::<_, Option<String>>(1)?,
                "content": row.get::<_, String>(2)?,
                "is_deleted": row.get::<_, i64>(3)? != 0,
                "deleted_at": row.get::<_, Option<String>>(4)?,
                "deleted_by": row.get::<_, Option<String>>(5)?,
                "delete_reason": row.get::<_, Option<String>>(6)?,
                "created_at": row.get::<_, String>(7)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let notes: Vec<Value> = {
        let mut stmt = conn.prepare(
            "SELECT x.path, n.title, n.content, n.note_type, n.tags, n.metadata,
                    n.created_at, n.updated_at
             FROM notes n LEFT JOIN nodes x ON x.id = n.node_id
             WHERE n.project_id = ?1
             ORDER BY n.created_at, n.id",
        )?;
        stmt.query_map(params![project.id], |row| {
            Ok(json!({
                "node": row.get::<_, Option<String>>(0)?,
                "title": row.get::<_, String>(1)?,
                "content": row.get::<_, String>(2)?,
                "note_type": row.get::<_, String>(3)?,
                "tags": row.get::<_, String>(4)?,
                "metadata": row.get::<_, String>(5)?,
                "created_at": row.get::<_, String>(6)?,
                "updated_at": row.get::<_, String>(7)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let templates: Vec<Value> = {
        let mut stmt = conn.prepare(
            "SELECT name, description, tree, created_at, updated_at
             FROM templates WHERE project_id = ?1 ORDER BY name",
        )?;
        stmt.query_map(params![project.id], |row| {
            Ok(json!({
                "name": row.get::<_, String>(0)?,
                "description": row.get::<_, Option<String>>(1)?,
                "tree": row.get::<_, String>(2)?,
                "created_at": row.get::<_, String>(3)?,
                "updated_at": row.get::<_, String>(4)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let snapshots: Vec<Value> = {
        let mut stmt = conn.prepare(
            "SELECT day, metric, scope_total, scope_done, scope_added, computed_at
             FROM burn_snapshots WHERE project_id = ?1 ORDER BY day, metric",
        )?;
        stmt.query_map(params![project.id], |row| {
            Ok(json!({
                "day": row.get::<_, String>(0)?,
                "metric": row.get::<_, String>(1)?,
                "scope_total": row.get::<_, f64>(2)?,
                "scope_done": row.get::<_, f64>(3)?,
                "scope_added": row.get::<_, f64>(4)?,
                "computed_at": row.get::<_, String>(5)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };

    // История переходов едет вместе с узлами: без неё импорт обнуляет burn-up, потому что
    // дата закрытия живёт только здесь (это измерено: export → import давал `done 0` при
    // закрытом узле на месте). Узлы адресуются путём, а не id: путь переносим между базами.
    let status_history: Vec<Value> = {
        let mut stmt = conn.prepare(
            "SELECT n.path, h.from_status, h.to_status, h.cause, h.at, h.actor
             FROM node_status_history h
             JOIN nodes n ON n.id = h.node_id
             WHERE n.project_id = ?1
             ORDER BY n.path, h.at, h.id",
        )?;
        stmt.query_map(params![project.id], |row| {
            Ok(json!({
                "node": row.get::<_, String>(0)?,
                "from": row.get::<_, Option<String>>(1)?,
                "to": row.get::<_, String>(2)?,
                "cause": row.get::<_, String>(3)?,
                "at": row.get::<_, String>(4)?,
                "actor": row.get::<_, Option<String>>(5)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };

    // Сумма считается по `data`, а не по документу целиком: `exported_at` и версия бинаря
    // меняются от запуска к запуску, и сумма обязана зависеть только от содержимого.
    let data = json!({
        "project": {
            "key": project.key,
            "name": project.name,
            "status": project.status,
        },
        // Маркер, а не догадка по пустому массиву: «в файле истории нет» и «у узлов не было
        // переходов» — разные факты, и импорт обязан их различать.
        "history": "present",
        "status_history": status_history,
        "nodes": nodes,
        "edges": edges,
        "comments": comments,
        "notes": notes,
        "templates": templates,
        "snapshots": snapshots,
    });

    let document = json!({
        "schema": SCHEMA,
        "version": env!("CARGO_PKG_VERSION"),
        "exported_at": now_iso(),
        "sha256": payload_checksum(&data),
        "data": data,
    });
    Ok(document)
}

/// Проверка и разбор документа: возвращает payload без поля `sha256`.
fn verify(document: &Value) -> Result<Value> {
    let schema = document
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if schema != SCHEMA {
        return Err(Error::InvalidValue {
            field: "schema".to_string(),
            value: format!("{schema} (expected {SCHEMA})"),
        });
    }
    let claimed = document
        .get("sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidValue {
            field: "sha256".to_string(),
            value: "missing".to_string(),
        })?;
    let data = document
        .get("data")
        .cloned()
        .ok_or_else(|| Error::InvalidValue {
            field: "data".to_string(),
            value: "missing".to_string(),
        })?;
    let actual = payload_checksum(&data);
    if actual != claimed {
        return Err(Error::ExportChecksumMismatch {
            expected: claimed.to_string(),
            actual,
        });
    }
    Ok(data)
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn number(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

fn integer(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_i64)
}

fn truthy(value: &Value, key: &str) -> bool {
    match value.get(key) {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_i64().unwrap_or(0) != 0,
        _ => false,
    }
}

/// Узел для записи: то, что точно нужно вставить.
struct PlanNode {
    path: String,
    parent: Option<String>,
    depth: i64,
    seq: i64,
    kind: String,
    title: String,
    description: Option<String>,
    description_locked: bool,
    status: String,
    priority: String,
    assigned_to: Option<String>,
    weight: f64,
    estimated_hours: Option<f64>,
    due_date: Option<String>,
    branch: Option<String>,
    source_ref: Option<String>,
    tags: String,
    metadata: String,
    sort_order: i64,
    archived_at: Option<String>,
    created_at: String,
    updated_at: String,
}

fn is_valid_path(path: &str) -> bool {
    !path.is_empty()
        && path
            .split('.')
            .all(|segment| !segment.is_empty() && segment.chars().all(|c| c.is_ascii_digit()))
}

/// Разбирает узлы документа и проверяет их целостность до записи.
fn plan_nodes(payload: &Value) -> Result<Vec<PlanNode>> {
    let list = payload
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidValue {
            field: "nodes".to_string(),
            value: "missing or not an array".to_string(),
        })?;

    let mut plan: Vec<PlanNode> = Vec::with_capacity(list.len());
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for raw in list {
        let path = text(raw, "path").ok_or_else(|| Error::InvalidValue {
            field: "nodes[].path".to_string(),
            value: "missing".to_string(),
        })?;
        if !is_valid_path(&path) {
            return Err(Error::InvalidValue {
                field: "nodes[].path".to_string(),
                value: path,
            });
        }
        if !seen.insert(path.clone()) {
            return Err(Error::DuplicateKey(format!("node path {path}")));
        }
        let parent = text(raw, "parent_id_path")
            .or_else(|| path.rsplit_once('.').map(|(head, _)| head.to_string()));
        let depth = integer(raw, "depth").unwrap_or_else(|| path.split('.').count() as i64);
        let now = now_iso();
        plan.push(PlanNode {
            seq: integer(raw, "seq").unwrap_or_else(|| {
                path.rsplit('.')
                    .next()
                    .and_then(|seg| seg.parse().ok())
                    .unwrap_or(1)
            }),
            parent,
            depth,
            path,
            kind: text(raw, "kind").unwrap_or_else(|| "task".to_string()),
            title: text(raw, "title").unwrap_or_default(),
            description: text(raw, "description"),
            description_locked: truthy(raw, "description_locked"),
            status: text(raw, "status").unwrap_or_else(|| "todo".to_string()),
            priority: text(raw, "priority").unwrap_or_else(|| "medium".to_string()),
            assigned_to: text(raw, "assigned_to"),
            weight: number(raw, "weight").unwrap_or(1.0),
            estimated_hours: number(raw, "estimated_hours"),
            due_date: text(raw, "due_date"),
            branch: text(raw, "branch"),
            source_ref: text(raw, "source_ref"),
            tags: text(raw, "tags").unwrap_or_else(|| "[]".to_string()),
            metadata: text(raw, "metadata").unwrap_or_else(|| "{}".to_string()),
            sort_order: integer(raw, "sort_order").unwrap_or(0),
            archived_at: text(raw, "archived_at"),
            created_at: text(raw, "created_at").unwrap_or_else(|| now.clone()),
            updated_at: text(raw, "updated_at").unwrap_or(now),
        });
    }

    // Родитель должен существовать в файле, иначе дерево не соберётся.
    for node in &plan {
        if let Some(parent) = &node.parent
            && !seen.contains(parent)
        {
            return Err(Error::InvalidValue {
                field: "nodes[].parent".to_string(),
                value: format!("{parent} is referenced by {} but missing", node.path),
            });
        }
    }
    plan.sort_by_key(|node| crate::node::path_sort_key(&node.path));
    Ok(plan)
}

/// Проверяет рёбра: ссылки существуют, граф `blocks` ацикличен.
fn plan_edges(payload: &Value, paths: &BTreeSet<String>) -> Result<Vec<(String, String, String)>> {
    let list = payload
        .get("edges")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut edges = Vec::with_capacity(list.len());
    for raw in &list {
        let from = text(raw, "from").unwrap_or_default();
        let to = text(raw, "to").unwrap_or_default();
        let kind = text(raw, "kind").unwrap_or_else(|| "blocks".to_string());
        if !paths.contains(&from) || !paths.contains(&to) {
            return Err(Error::InvalidValue {
                field: "edges".to_string(),
                value: format!("{from} -> {to}: node missing in the file"),
            });
        }
        if from == to {
            return Err(Error::SelfEdge { dot_id: from });
        }
        edges.push((from, to, kind));
    }

    // Ацикличность графа `blocks`: иначе импорт создал бы состояние, которое
    // невозможно получить через API.
    let blocks: Vec<(&str, &str)> = edges
        .iter()
        .filter(|(_, _, kind)| kind == "blocks")
        .map(|(from, to, _)| (from.as_str(), to.as_str()))
        .collect();
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for (from, to) in &blocks {
        adjacency.entry(*from).or_default().push(*to);
    }
    let mut state: HashMap<&str, u8> = HashMap::new();
    for start in adjacency.keys().copied() {
        if state.get(start).copied() == Some(2) {
            continue;
        }
        let mut stack = vec![(start, 0usize)];
        while let Some((node, index)) = stack.pop() {
            if index == 0 {
                state.insert(node, 1);
            }
            let next = adjacency
                .get(node)
                .and_then(|list| list.get(index))
                .copied();
            match next {
                Some(child) => {
                    stack.push((node, index + 1));
                    match state.get(child).copied() {
                        Some(1) => {
                            return Err(Error::CycleDetected {
                                from: start.to_string(),
                                to: child.to_string(),
                                path: format!("{start} -> {child}"),
                            });
                        }
                        Some(2) => {}
                        _ => stack.push((child, 0)),
                    }
                }
                None => {
                    state.insert(node, 2);
                }
            }
        }
    }
    Ok(edges)
}

/// Импорт документа нашего формата.
pub fn import(
    conn: &mut Connection,
    project: &Project,
    document: &Value,
    mode: Mode,
    actor: &str,
) -> Result<Imported> {
    let payload = verify(document)?;

    // 1. Проверки — до первой записи.
    let plan = plan_nodes(&payload)?;
    let paths: BTreeSet<String> = plan.iter().map(|node| node.path.clone()).collect();
    let edges = plan_edges(&payload, &paths)?;
    let comments = payload
        .get("comments")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let notes = payload
        .get("notes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let templates = payload
        .get("templates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let snapshots = payload
        .get("snapshots")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for note in &notes {
        if let Some(reference) = text(note, "node")
            && !paths.contains(&reference)
        {
            return Err(Error::InvalidValue {
                field: "notes[].node".to_string(),
                value: format!("{reference} missing in the file"),
            });
        }
    }
    for comment in &comments {
        let reference = text(comment, "node").unwrap_or_default();
        if !paths.contains(&reference) {
            return Err(Error::InvalidValue {
                field: "comments[].node".to_string(),
                value: format!("{reference} missing in the file"),
            });
        }
    }

    // 2. Запись — одна транзакция.
    let tx = crate::db::write_tx(conn)?;

    let existing: HashMap<String, i64> = {
        let mut stmt =
            tx.prepare("SELECT path, id FROM nodes WHERE project_id = ?1 AND deleted_at IS NULL")?;
        stmt.query_map(params![project.id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?
    };

    let mut cleared = 0usize;
    if mode == Mode::Replace {
        cleared = tx.execute(
            "DELETE FROM nodes WHERE project_id = ?1",
            params![project.id],
        )?;
        tx.execute(
            "DELETE FROM notes WHERE project_id = ?1",
            params![project.id],
        )?;
        tx.execute(
            "DELETE FROM templates WHERE project_id = ?1",
            params![project.id],
        )?;
        tx.execute(
            "DELETE FROM burn_snapshots WHERE project_id = ?1",
            params![project.id],
        )?;
    }

    let mut id_by_path: HashMap<String, i64> = if mode == Mode::Replace {
        HashMap::new()
    } else {
        existing.clone()
    };
    let mut created = 0usize;
    let mut skipped = 0usize;
    // Созданные узлы: нужны для синтетической истории, когда файл её не несёт.
    let mut created_nodes: Vec<(i64, String, String)> = Vec::new();

    for node in &plan {
        if mode == Mode::Merge
            && let Some(_id) = id_by_path.get(&node.path)
        {
            skipped += 1;
            continue;
        }
        let parent_id = match &node.parent {
            Some(parent) => match id_by_path.get(parent) {
                Some(id) => Some(*id),
                None => {
                    return Err(Error::InvalidValue {
                        field: "nodes[].parent".to_string(),
                        value: format!("{parent} is not present after merge"),
                    });
                }
            },
            None => None,
        };
        tx.execute(
            "INSERT INTO nodes (project_id, parent_id, depth, seq, path, kind, title, description,
                                description_locked, status, priority, assigned_to, weight,
                                estimated_hours, due_date, branch, source_ref, tags, metadata,
                                sort_order, archived_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                     ?18, ?19, ?20, ?21, ?22, ?23)",
            params![
                project.id,
                parent_id,
                node.depth,
                node.seq,
                node.path,
                node.kind,
                node.title,
                node.description,
                i64::from(node.description_locked),
                node.status,
                node.priority,
                node.assigned_to,
                node.weight,
                node.estimated_hours,
                node.due_date,
                node.branch,
                node.source_ref,
                node.tags,
                node.metadata,
                node.sort_order,
                node.archived_at,
                node.created_at,
                node.updated_at,
            ],
        )?;
        let node_id = tx.last_insert_rowid();
        id_by_path.insert(node.path.clone(), node_id);
        created_nodes.push((node_id, node.status.clone(), node.created_at.clone()));
        created += 1;
    }

    // История переходов: переносим, если файл её несёт; иначе пишем по одному факту на узел.
    //
    // Различие принципиальное. Файл нашего формата несёт историю, и она — наша: переносим как
    // есть, с её временами, иначе burn-up съедет на день импорта. Файл saga истории не несёт и
    // не может: там нет такого понятия, поэтому единственный честный факт — «узел доехал»,
    // с причиной `import` и временем создания узла, а не моментом импорта.
    let history_present = payload.get("history").and_then(Value::as_str) == Some("present");
    let mut history_rows = 0usize;
    if history_present {
        let rows = payload
            .get("status_history")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for row in &rows {
            let Some(path) = text(row, "node") else {
                continue;
            };
            let Some(node_id) = id_by_path.get(&path).copied() else {
                // Узел не перенесён (вырезан из файла или не легёл) — история без него не нужна
                // и не может ссылаться в пустоту.
                continue;
            };
            let cause_text = text(row, "cause").unwrap_or_default();
            let cause = history::Cause::parse(&cause_text).ok_or_else(|| Error::InvalidValue {
                field: "status_history[].cause".to_string(),
                value: format!("{cause_text} (unknown cause)"),
            })?;
            let to = text(row, "to").unwrap_or_default();
            history::record_at(
                &tx,
                node_id,
                text(row, "from").as_deref(),
                &to,
                cause,
                &text(row, "at").unwrap_or_else(now_iso),
                text(row, "actor").as_deref().unwrap_or(actor),
            )?;
            history_rows += 1;
        }
    } else {
        for (node_id, status, created_at) in &created_nodes {
            history::record_at(
                &tx,
                *node_id,
                None,
                status,
                history::Cause::Import,
                created_at,
                actor,
            )?;
            history_rows += 1;
        }
    }

    let mut edge_count = 0usize;
    for (from, to, kind) in &edges {
        let (Some(from_id), Some(to_id)) = (id_by_path.get(from), id_by_path.get(to)) else {
            continue;
        };
        let changed = tx.execute(
            "INSERT INTO edges (from_node, to_node, kind, created_at, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(from_node, to_node, kind) DO NOTHING",
            params![from_id, to_id, kind, now_iso(), actor],
        )?;
        edge_count += changed;
    }

    let mut comment_count = 0usize;
    for comment in &comments {
        let Some(node_id) = text(comment, "node").and_then(|path| id_by_path.get(&path).copied())
        else {
            continue;
        };
        tx.execute(
            "INSERT INTO comments (node_id, author, content, is_deleted, deleted_at, deleted_by,
                                   delete_reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                node_id,
                text(comment, "author"),
                text(comment, "content").unwrap_or_default(),
                i64::from(truthy(comment, "is_deleted")),
                text(comment, "deleted_at"),
                text(comment, "deleted_by"),
                text(comment, "delete_reason"),
                text(comment, "created_at").unwrap_or_else(now_iso),
            ],
        )?;
        comment_count += 1;
    }

    let mut note_count = 0usize;
    for note in &notes {
        let node_id = text(note, "node").and_then(|path| id_by_path.get(&path).copied());
        tx.execute(
            "INSERT INTO notes (project_id, node_id, title, content, note_type, tags, metadata,
                                created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                project.id,
                node_id,
                text(note, "title").unwrap_or_default(),
                text(note, "content").unwrap_or_default(),
                text(note, "note_type").unwrap_or_else(|| "general".to_string()),
                text(note, "tags").unwrap_or_else(|| "[]".to_string()),
                text(note, "metadata").unwrap_or_else(|| "{}".to_string()),
                text(note, "created_at").unwrap_or_else(now_iso),
                text(note, "updated_at").unwrap_or_else(now_iso),
            ],
        )?;
        note_count += 1;
    }

    let mut template_count = 0usize;
    for template in &templates {
        let Some(name) = text(template, "name") else {
            continue;
        };
        tx.execute(
            "INSERT INTO templates (project_id, name, description, tree, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(project_id, name) DO UPDATE SET
               description = excluded.description,
               tree = excluded.tree,
               updated_at = excluded.updated_at",
            params![
                project.id,
                name,
                text(template, "description"),
                text(template, "tree").unwrap_or_else(|| "{\"nodes\":[]}".to_string()),
                text(template, "created_at").unwrap_or_else(now_iso),
                text(template, "updated_at").unwrap_or_else(now_iso),
            ],
        )?;
        template_count += 1;
    }

    for snapshot in &snapshots {
        let (Some(day), Some(metric)) = (text(snapshot, "day"), text(snapshot, "metric")) else {
            continue;
        };
        tx.execute(
            "INSERT INTO burn_snapshots (project_id, day, metric, scope_total, scope_done,
                                         scope_added, computed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(project_id, day, metric) DO UPDATE SET
               scope_total = excluded.scope_total,
               scope_done = excluded.scope_done,
               scope_added = excluded.scope_added,
               computed_at = excluded.computed_at",
            params![
                project.id,
                day,
                metric,
                number(snapshot, "scope_total").unwrap_or(0.0),
                number(snapshot, "scope_done").unwrap_or(0.0),
                number(snapshot, "scope_added").unwrap_or(0.0),
                text(snapshot, "computed_at").unwrap_or_else(now_iso),
            ],
        )?;
    }

    let summary = format!(
        "import {}: {} node(s) created, {} skipped{}",
        mode.as_str(),
        created,
        skipped,
        if cleared > 0 {
            format!(", {cleared} removed before load")
        } else {
            String::new()
        }
    );
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id: None,
            entity_type: "import",
            entity_id: 0,
            action: "import",
            field_name: None,
            old_value: None,
            new_value: None,
            summary: Some(summary.as_str()),
            actor,
        },
    )?;
    tx.commit()?;

    Ok(Imported {
        mode: mode.as_str().to_string(),
        nodes_created: created,
        nodes_skipped: skipped,
        edges: edge_count,
        comments: comment_count,
        notes: note_count,
        templates: template_count,
        history: Some(history_rows),
        checksum_verified: true,
    })
}

/// Импорт экспорта saga-mcp (`tracker_export`).
///
/// Маппинг: `project.epics[] → узлы глубины 1`, `tasks[] → 2`, `subtasks[] → 3`,
/// `task.dependencies → рёбра blocks`, `comments/notes` — как есть. Числовые id saga
/// уходят в `metadata.saga_id`, dot-ID строится заново. Шаблоны saga не экспортирует,
/// поэтому их в импорте нет — это ограничение исходного формата, а не потеря.
pub fn import_saga(
    conn: &mut Connection,
    project: &Project,
    document: &Value,
    mode: Mode,
    actor: &str,
) -> Result<Imported> {
    let version = document
        .get("format_version")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !SAGA_VERSIONS.contains(&version) {
        return Err(Error::InvalidValue {
            field: "format_version".to_string(),
            value: format!("{version} (expected one of {})", SAGA_VERSIONS.join(", ")),
        });
    }
    let project_data = document.get("project").ok_or_else(|| Error::InvalidValue {
        field: "project".to_string(),
        value: "missing".to_string(),
    })?;
    if text(project_data, "name").is_none() {
        return Err(Error::InvalidValue {
            field: "project.name".to_string(),
            value: "missing".to_string(),
        });
    }

    let epics = project_data
        .get("epics")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // Статусы saga → статусы tasq.
    //
    // Явно перечислены ВСЕ значения обеих схем: `done` уже совпадает по имени, и пропущенная
    // ветка молча превращала готовую подзадачу в `todo` (поймано импортом реальной фикстуры).
    let map_status = |status: &str, epic: bool| -> String {
        match status {
            "completed" | "done" => "done".to_string(),
            "cancelled" => "cancelled".to_string(),
            "blocked" => "blocked".to_string(),
            "review" => "review".to_string(),
            "deferred" => "deferred".to_string(),
            "in_progress" => "in_progress".to_string(),
            "todo" => "todo".to_string(),
            "planned" | "on_hold" => "planned".to_string(),
            // Воронка tasq: у saga таких состояний нет, но собственный экспорт
            // проходит через эту же таблицу, и без явных ветвей идея превращалась
            // бы в `planned`/`todo` при каждом импорте.
            "idea" => "idea".to_string(),
            "backlog" => "backlog".to_string(),
            _ if epic => "planned".to_string(),
            _ => "todo".to_string(),
        }
    };

    /// Узел внутри плоского плана импорта.
    struct Flat {
        path: String,
        parent: Option<String>,
        depth: i64,
        seq: i64,
        kind: String,
        title: String,
        description: Option<String>,
        status: String,
        priority: String,
        assigned_to: Option<String>,
        estimated_hours: Option<f64>,
        due_date: Option<String>,
        branch: Option<String>,
        source_ref: Option<String>,
        description_locked: bool,
        tags: String,
        metadata: String,
        saga_id: Option<i64>,
        created_at: String,
        updated_at: String,
    }

    let mut flat: Vec<Flat> = Vec::new();
    let mut task_path_by_saga: HashMap<i64, String> = HashMap::new();
    let mut subtask_path_by_saga: HashMap<i64, String> = HashMap::new();
    let mut comments_by_path: Vec<(String, Value)> = Vec::new();
    let now = now_iso();

    for (epic_index, epic) in epics.iter().enumerate() {
        let epic_seq = integer(epic, "sort_order").unwrap_or(epic_index as i64) + 1;
        let epic_path = epic_seq.to_string();
        let epic_saga_id = integer(epic, "_original_id");
        flat.push(Flat {
            path: epic_path.clone(),
            parent: None,
            depth: 1,
            seq: epic_seq,
            kind: "epic".to_string(),
            title: text(epic, "name").unwrap_or_else(|| "Epic".to_string()),
            description: text(epic, "description"),
            status: map_status(&text(epic, "status").unwrap_or_default(), true),
            priority: text(epic, "priority").unwrap_or_else(|| "medium".to_string()),
            assigned_to: None,
            estimated_hours: None,
            due_date: None,
            branch: text(epic, "branch"),
            source_ref: None,
            description_locked: false,
            tags: text(epic, "tags").unwrap_or_else(|| "[]".to_string()),
            metadata: text(epic, "metadata").unwrap_or_else(|| "{}".to_string()),
            saga_id: epic_saga_id,
            created_at: text(epic, "created_at").unwrap_or_else(|| now.clone()),
            updated_at: text(epic, "updated_at").unwrap_or_else(|| now.clone()),
        });

        let tasks = epic
            .get("tasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for (task_index, task) in tasks.iter().enumerate() {
            let task_seq = integer(task, "sort_order").unwrap_or(task_index as i64) + 1;
            let task_path = format!("{epic_path}.{task_seq}");
            if let Some(saga_id) = integer(task, "_original_id") {
                task_path_by_saga.insert(saga_id, task_path.clone());
            }
            flat.push(Flat {
                path: task_path.clone(),
                parent: Some(epic_path.clone()),
                depth: 2,
                seq: task_seq,
                kind: "task".to_string(),
                title: text(task, "title").unwrap_or_else(|| "Task".to_string()),
                description: text(task, "description"),
                status: map_status(&text(task, "status").unwrap_or_default(), false),
                priority: text(task, "priority").unwrap_or_else(|| "medium".to_string()),
                assigned_to: text(task, "assigned_to"),
                estimated_hours: number(task, "estimated_hours"),
                due_date: text(task, "due_date"),
                branch: None,
                source_ref: text(task, "source_ref"),
                description_locked: truthy(task, "description_locked"),
                tags: text(task, "tags").unwrap_or_else(|| "[]".to_string()),
                metadata: text(task, "metadata").unwrap_or_else(|| "{}".to_string()),
                saga_id: integer(task, "_original_id"),
                created_at: text(task, "created_at").unwrap_or_else(|| now.clone()),
                updated_at: text(task, "updated_at").unwrap_or_else(|| now.clone()),
            });

            let subtasks = task
                .get("subtasks")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for (sub_index, subtask) in subtasks.iter().enumerate() {
                let sub_seq = integer(subtask, "sort_order").unwrap_or(sub_index as i64) + 1;
                let sub_path = format!("{task_path}.{sub_seq}");
                if let Some(saga_id) = integer(subtask, "_original_id") {
                    subtask_path_by_saga.insert(saga_id, sub_path.clone());
                }
                flat.push(Flat {
                    path: sub_path.clone(),
                    parent: Some(task_path.clone()),
                    depth: 3,
                    seq: sub_seq,
                    kind: "subtask".to_string(),
                    title: text(subtask, "title").unwrap_or_else(|| "Subtask".to_string()),
                    description: None,
                    status: map_status(&text(subtask, "status").unwrap_or_default(), false),
                    priority: "medium".to_string(),
                    assigned_to: None,
                    estimated_hours: None,
                    due_date: None,
                    branch: None,
                    source_ref: None,
                    description_locked: false,
                    tags: "[]".to_string(),
                    metadata: "{}".to_string(),
                    saga_id: None,
                    created_at: now.clone(),
                    updated_at: now.clone(),
                });
            }

            if let Some(comments) = task.get("comments").and_then(Value::as_array) {
                for comment in comments {
                    comments_by_path.push((task_path.clone(), comment.clone()));
                }
            }
        }
    }

    if flat.is_empty() {
        return Err(Error::InvalidValue {
            field: "project.epics".to_string(),
            value: "no epics to import".to_string(),
        });
    }

    // Рёбра: saga хранит `task.depends_on` списком id — задача ждёт перечисленные.
    let mut edges: Vec<(String, String)> = Vec::new();
    for epic in &epics {
        for task in epic
            .get("tasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let Some(from_path) =
                integer(&task, "_original_id").and_then(|id| task_path_by_saga.get(&id).cloned())
            else {
                continue;
            };
            for dependency in task
                .get("depends_on")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                let Some(id) = dependency.as_i64() else {
                    continue;
                };
                if let Some(to_path) = task_path_by_saga.get(&id) {
                    // Самоссылка в исходном файле — порча данных, а не повод упасть на
                    // CHECK-ограничении схемы с честной ошибкой хранилища. Отказываем
                    // тем же кодом, что и импорт родного формата.
                    if to_path == &from_path {
                        return Err(Error::SelfEdge {
                            dot_id: from_path.clone(),
                        });
                    }
                    edges.push((from_path.clone(), to_path.clone()));
                }
            }
        }
    }

    // Запись.
    let tx = crate::db::write_tx(conn)?;
    // `replace` обязан очистить проект: без этого повторный импорт падал на
    // `UNIQUE constraint failed: nodes.project_id, nodes.path`, то есть режим
    // молча не работал, а вызывающий видел ошибку хранилища.
    if mode == Mode::Replace {
        tx.execute(
            "DELETE FROM nodes WHERE project_id = ?1",
            params![project.id],
        )?;
        tx.execute(
            "DELETE FROM notes WHERE project_id = ?1",
            params![project.id],
        )?;
        tx.execute(
            "DELETE FROM templates WHERE project_id = ?1",
            params![project.id],
        )?;
    }
    let mut id_by_path: HashMap<String, i64> = HashMap::new();
    // Сколько фактов истории написано синтетически (по одному на узел).
    let mut history_rows = 0usize;
    for node in &flat {
        let parent_id = node
            .parent
            .as_ref()
            .and_then(|p| id_by_path.get(p).copied());
        let mut metadata: Value =
            serde_json::from_str(&node.metadata).unwrap_or_else(|_| json!({}));
        if let Some(saga_id) = node.saga_id
            && let Some(object) = metadata.as_object_mut()
        {
            object.insert("saga_id".to_string(), json!(saga_id));
        }
        tx.execute(
            "INSERT INTO nodes (project_id, parent_id, depth, seq, path, kind, title, description,
                                description_locked, status, priority, assigned_to, weight,
                                estimated_hours, due_date, branch, source_ref, tags, metadata,
                                sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1.0, ?13, ?14, ?15, ?16,
                     ?17, ?18, 0, ?19, ?20)",
            params![
                project.id,
                parent_id,
                node.depth,
                node.seq,
                node.path,
                node.kind,
                node.title,
                node.description,
                i64::from(node.description_locked),
                node.status,
                node.priority,
                node.assigned_to,
                node.estimated_hours,
                node.due_date,
                node.branch,
                node.source_ref,
                node.tags,
                serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".to_string()),
                node.created_at,
                node.updated_at,
            ],
        )?;
        let node_id = tx.last_insert_rowid();
        id_by_path.insert(node.path.clone(), node_id);
        // Saga не несёт истории: честный факт — «узел доехал», с временем его создания, а не
        // моментом импорта. Узел не появился сейчас, он сейчас доехал.
        history::record_at(
            &tx,
            node_id,
            None,
            &node.status,
            history::Cause::Import,
            &node.created_at,
            actor,
        )?;
        history_rows += 1;
    }

    let mut edge_count = 0usize;
    for (from, to) in &edges {
        let (Some(from_id), Some(to_id)) = (id_by_path.get(from), id_by_path.get(to)) else {
            continue;
        };
        edge_count += tx.execute(
            "INSERT INTO edges (from_node, to_node, kind, created_at, created_by)
             VALUES (?1, ?2, 'blocks', ?3, ?4)
             ON CONFLICT(from_node, to_node, kind) DO NOTHING",
            params![from_id, to_id, now, actor],
        )?;
    }

    let mut comment_count = 0usize;
    for (path, comment) in &comments_by_path {
        let Some(node_id) = id_by_path.get(path).copied() else {
            continue;
        };
        tx.execute(
            "INSERT INTO comments (node_id, author, content, is_deleted, deleted_at, deleted_by,
                                   delete_reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                node_id,
                text(comment, "author"),
                text(comment, "content").unwrap_or_default(),
                i64::from(truthy(comment, "is_deleted")),
                text(comment, "deleted_at"),
                text(comment, "deleted_by"),
                text(comment, "delete_reason"),
                text(comment, "created_at").unwrap_or_else(|| now.clone()),
            ],
        )?;
        comment_count += 1;
    }

    // Заметки saga ссылаются на сущности по числовым id: восстанавливаем связь по картам.
    let mut note_count = 0usize;
    for note in document
        .get("notes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let entity_type = text(&note, "related_entity_type").unwrap_or_default();
        let entity_id = integer(&note, "_original_related_entity_id");
        let node_id = match (entity_type.as_str(), entity_id) {
            ("task", Some(id)) => task_path_by_saga
                .get(&id)
                .and_then(|path| id_by_path.get(path).copied()),
            ("epic", Some(id)) => {
                let path = epics
                    .iter()
                    .find(|epic| integer(epic, "_original_id") == Some(id))
                    .map(|epic| (integer(epic, "sort_order").unwrap_or(0) + 1).to_string());
                path.and_then(|path| id_by_path.get(&path).copied())
            }
            _ => subtask_path_by_saga
                .get(&entity_id.unwrap_or_default())
                .and_then(|path| id_by_path.get(path).copied()),
        };
        tx.execute(
            "INSERT INTO notes (project_id, node_id, title, content, note_type, tags, metadata,
                                created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![
                project.id,
                node_id,
                text(&note, "title").unwrap_or_default(),
                text(&note, "content").unwrap_or_default(),
                text(&note, "note_type").unwrap_or_else(|| "general".to_string()),
                text(&note, "tags").unwrap_or_else(|| "[]".to_string()),
                text(&note, "metadata").unwrap_or_else(|| "{}".to_string()),
                now,
            ],
        )?;
        note_count += 1;
    }

    let summary = format!(
        "imported from saga {}: {} node(s), {} link(s), {} comment(s), {} note(s)",
        version,
        flat.len(),
        edge_count,
        comment_count,
        note_count
    );
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id: None,
            entity_type: "import",
            entity_id: 0,
            action: "import",
            field_name: Some("format_version"),
            old_value: None,
            new_value: Some(version),
            summary: Some(summary.as_str()),
            actor,
        },
    )?;
    tx.commit()?;

    Ok(Imported {
        mode: format!("saga {}", mode.as_str()),
        nodes_created: flat.len(),
        nodes_skipped: 0,
        edges: edge_count,
        comments: comment_count,
        notes: note_count,
        templates: 0,
        // Saga истории не несёт: ниже на каждый узел пишется одна запись `import`.
        history: Some(history_rows),
        checksum_verified: false,
    })
}

/// Проверка формата документа без записи: для `--dry-run`.
pub fn dry_run(conn: &Connection, project: &Project, document: &Value) -> Result<Imported> {
    let payload = verify(document)?;
    let plan = plan_nodes(&payload)?;
    let paths: BTreeSet<String> = plan.iter().map(|node| node.path.clone()).collect();
    let edges = plan_edges(&payload, &paths)?;
    let existing: BTreeSet<String> = {
        let mut stmt =
            conn.prepare("SELECT path FROM nodes WHERE project_id = ?1 AND deleted_at IS NULL")?;
        stmt.query_map(params![project.id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<BTreeSet<_>>>()?
    };
    let skipped = plan
        .iter()
        .filter(|node| existing.contains(&node.path))
        .count();
    Ok(Imported {
        mode: "dry-run".to_string(),
        nodes_created: plan.len() - skipped,
        nodes_skipped: skipped,
        edges: edges.len(),
        comments: payload
            .get("comments")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0),
        notes: payload
            .get("notes")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0),
        templates: payload
            .get("templates")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0),
        // Пробный прогон не пишет: история будет перенесена (или синтезирована) столько же
        // раз, сколько узлов, — столько и называем.
        history: Some(
            payload
                .get("status_history")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(plan.len()),
        ),
        checksum_verified: true,
    })
}

/// Сводка по документу без базы: используется в тестах и для сообщений.
pub fn describe(document: &Value) -> BTreeMap<String, usize> {
    let data = document.get("data").unwrap_or(document);
    let mut out = BTreeMap::new();
    for key in [
        "nodes",
        "edges",
        "comments",
        "notes",
        "templates",
        "snapshots",
    ] {
        out.insert(
            key.to_string(),
            data.get(key)
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0),
        );
    }
    out
}
