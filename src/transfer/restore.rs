//! Восстановление базы из дампа: `tasq restore`.
//!
//! Дамп — состояние, а не действие: здесь не применяется матрица переходов, не проверяются
//! аренды и не пишется журнал. Проверяется только то, что действительно может потеряться:
//! поля, которых нет в текущей схеме, строки, которые она не примет (статус вне `CHECK`,
//! висячая ссылка, дубль ключа), и нарушения внешних ключей после вставки.
//!
//! Отчёт строится до коммита: если потери есть, транзакция откатывается целиком. Половинчатая
//! база хуже отказа — на неё потом смотрят как на рабочую.

use std::collections::HashMap;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, params_from_iter};
use serde::Serialize;
use serde_json::Value;

use super::{DUMP_FORMAT, DUMP_KIND, DUMP_TABLES};
use crate::db::{SUPPORTED_SCHEMA_VERSION, write_tx};
use crate::error::{Error, Result};

/// Итог восстановления.
#[derive(Debug, Clone, Serialize)]
pub struct RestoreReport {
    /// Версия схемы, из которой снят дамп.
    pub schema_from: i64,
    /// Версия схемы, в которую заливали.
    pub schema_to: i64,
    /// Записан ли результат. `false` — потери, откат.
    pub committed: bool,
    /// По таблицам: сколько было в дампе, сколько легло, сколько не легло.
    pub tables: Vec<TableRestore>,
    /// Что именно не легло и почему.
    pub losses: Vec<RestoreLoss>,
}

/// Что перенеслось по одной таблице.
#[derive(Debug, Clone, Serialize)]
pub struct TableRestore {
    pub table: String,
    pub in_dump: usize,
    pub inserted: usize,
    pub skipped: usize,
}

/// Одна потеря: строка или поле, которые не удалось перенести.
#[derive(Debug, Clone, Serialize)]
pub struct RestoreLoss {
    pub table: String,
    /// Адрес узла, если у потери он есть (`DEV-1.2.3`); иначе — если это поле или таблица.
    pub address: Option<String>,
    pub reason: String,
}

impl RestoreReport {
    /// Потери есть — вызывающий превращает это в код выхода 3.
    pub fn has_losses(&self) -> bool {
        !self.losses.is_empty()
    }
}

/// Восстанавливает состояние из дампа в текущую схему.
///
/// Порядок вставки — [`DUMP_TABLES`]: родители раньше детей, иначе внешние ключи не пропустят
/// строку, ссылающуюся на ещё не вставленного родителя.
pub fn restore(conn: &mut Connection, document: &Value) -> Result<RestoreReport> {
    let kind = document.get("kind").and_then(Value::as_str).unwrap_or("-");
    if kind != DUMP_KIND {
        return Err(Error::InvalidValue {
            field: "kind".to_string(),
            value: format!("{kind} (ожидался {DUMP_KIND})"),
        });
    }
    let format = document.get("format").and_then(Value::as_i64).unwrap_or(0);
    if format > DUMP_FORMAT {
        return Err(Error::InvalidValue {
            field: "format".to_string(),
            value: format!("{format} (поддерживается до {DUMP_FORMAT})"),
        });
    }
    let schema_from = document
        .get("schema_version")
        .and_then(Value::as_i64)
        .ok_or_else(|| Error::InvalidValue {
            field: "schema_version".to_string(),
            value: "не указана; без неё не ясно, что за схема в файле".to_string(),
        })?;
    if schema_from > SUPPORTED_SCHEMA_VERSION {
        return Err(Error::SchemaTooNew {
            found: schema_from,
            supported: SUPPORTED_SCHEMA_VERSION,
        });
    }
    let tables = document
        .get("tables")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::InvalidValue {
            field: "tables".to_string(),
            value: "нет объекта tables — это не дамп".to_string(),
        })?;

    let mut report = RestoreReport {
        schema_from,
        schema_to: SUPPORTED_SCHEMA_VERSION,
        committed: false,
        tables: Vec::new(),
        losses: Vec::new(),
    };
    // Ключи проектов: адрес узла собирается из ключа и пути, а не из числового id —
    // голый `node_id = 7` в отчёте ничего не говорит человеку.
    let project_keys = project_keys_from(tables);
    let mut node_addresses: HashMap<i64, String> = HashMap::new();

    let tx = write_tx(conn)?;
    // Внешние ключи откладываются до коммита. Без этого порядок строк в дампе решал бы,
    // пройдёт вставка или нет: узлы отсортированы по id, а `parent_id` вполне может быть
    // больше собственного id (узел переносили под другого родителя). Такая база цела —
    // и дамп обязан восстанавливаться, а не падать на порядке строк.
    tx.pragma_update(None, "defer_foreign_keys", true)?;
    for table in DUMP_TABLES {
        let Some(rows) = tables.get(*table).and_then(Value::as_array) else {
            continue;
        };
        let columns = table_columns(&tx, table)?;
        if columns.is_empty() {
            if rows.is_empty() {
                continue;
            }
            // Таблицы нет в текущей схеме, а строки в дампе есть: это потеря целиком.
            report.losses.push(RestoreLoss {
                table: (*table).to_string(),
                address: None,
                reason: format!(
                    "таблицы нет в схеме v{SUPPORTED_SCHEMA_VERSION}, {} строк(и) не перенесены",
                    rows.len()
                ),
            });
            report.tables.push(TableRestore {
                table: (*table).to_string(),
                in_dump: rows.len(),
                inserted: 0,
                skipped: rows.len(),
            });
            continue;
        }

        // Колонки, которые есть в дампе, но исчезли из схемы: потеря поля, а не строки.
        let dumped_columns = columns_in_dump(rows);
        let dropped: Vec<&String> = dumped_columns
            .iter()
            .filter(|column| !columns.contains(column))
            .collect();
        for column in &dropped {
            report.losses.push(RestoreLoss {
                table: (*table).to_string(),
                address: None,
                reason: format!(
                    "поля `{column}` больше нет в схеме v{SUPPORTED_SCHEMA_VERSION}: {} строк(и) без него",
                    rows.len()
                ),
            });
        }
        let usable: Vec<&String> = dumped_columns
            .iter()
            .filter(|column| columns.contains(column))
            .collect();
        let missing_required: Vec<&String> = columns
            .iter()
            .filter(|column| !usable.contains(column))
            .collect();

        let mut inserted = 0usize;
        let mut table_losses = 0usize;
        for row in rows {
            // `schema_version` переписывается миграцией: дамп со старой версией не должен
            // вернуть базу в старое состояние — схема уже новая.
            if *table == "meta" && row.get("key").and_then(Value::as_str) == Some("schema_version")
            {
                continue;
            }
            let address = row_address(table, row, &project_keys, &node_addresses);
            if !missing_required.is_empty() {
                // Без обязательного поля строку не вставить; сообщаем один раз на таблицу,
                // иначе отчёт на большой базе превращается в список одинаковых строк.
                if table_losses == 0 {
                    let names: Vec<&str> =
                        missing_required.iter().map(|name| name.as_str()).collect();
                    report.losses.push(RestoreLoss {
                        table: (*table).to_string(),
                        address: address.clone(),
                        reason: format!(
                            "в дампе нет обязательных полей: {}; {} строк(и) не перенесены",
                            names.join(", "),
                            rows.len()
                        ),
                    });
                }
                table_losses += 1;
                continue;
            }
            let sql = format!(
                "INSERT INTO {table} ({}) VALUES ({})",
                usable
                    .iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                (1..=usable.len())
                    .map(|index| format!("?{index}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let values: Vec<SqlValue> = usable
                .iter()
                .map(|column| json_to_sql(&row[*column]))
                .collect();
            let outcome = tx
                .prepare(&sql)
                .and_then(|mut stmt| stmt.execute(params_from_iter(values.iter())));
            match outcome {
                Ok(_) => {
                    inserted += 1;
                    // Адрес узла запоминается для дочерних таблиц: аренда или комментарий,
                    // которые не лягут, должны называть узел, а не его числовой id.
                    if let (Some(id), Some(addr)) = (
                        row.get("id")
                            .and_then(Value::as_i64)
                            .filter(|_| *table == "nodes"),
                        address.clone(),
                    ) {
                        node_addresses.insert(id, addr);
                    }
                }
                Err(err) => {
                    table_losses += 1;
                    report.losses.push(RestoreLoss {
                        table: (*table).to_string(),
                        address,
                        reason: classify(&err),
                    });
                }
            }
        }
        report.tables.push(TableRestore {
            table: (*table).to_string(),
            in_dump: rows.len(),
            inserted,
            skipped: rows.len() - inserted,
        });
    }

    // Отложенные ключи проверяются здесь, до коммита: `foreign_key_check` называет таблицу
    // и rowid нарушителя, поэтому потеря получает адрес узла, а не общий счётчик.
    for violation in foreign_key_violations(&tx)? {
        let address = violation_address(&tx, &violation, &project_keys, &node_addresses)?;
        report.losses.push(RestoreLoss {
            table: violation.table,
            address,
            reason: format!(
                "висячая ссылка: узел {} из таблицы {} отсутствует",
                violation.rowid, violation.parent
            ),
        });
    }

    if report.has_losses() {
        // Половинчатая база хуже отказа, а отчёт уже собран. Откат явный, хотя `Drop` у
        // транзакции сделал бы то же самое: намерение должно читаться в коде, а не
        // выводиться из времени жизни значения.
        tx.rollback()?;
        return Ok(report);
    }
    tx.commit()?;
    report.committed = true;
    Ok(report)
}

/// Колонки таблицы в текущей схеме; пусто — таблицы нет.
fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info(`{table}`)"))?;
    let mut rows = stmt.query([])?;
    let mut columns = Vec::new();
    while let Some(row) = rows.next()? {
        columns.push(row.get::<_, String>(1)?);
    }
    Ok(columns)
}

/// Имена полей, встретившиеся в строках дампа, в порядке появления.
fn columns_in_dump(rows: &[Value]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for row in rows {
        if let Some(object) = row.as_object() {
            for key in object.keys() {
                if !seen.iter().any(|name| name == key) {
                    seen.push(key.clone());
                }
            }
        }
    }
    seen
}

/// Ключи проектов из самого дампа: `id → key`.
fn project_keys_from(tables: &serde_json::Map<String, Value>) -> HashMap<i64, String> {
    let mut keys = HashMap::new();
    if let Some(rows) = tables.get("projects").and_then(Value::as_array) {
        for row in rows {
            if let (Some(id), Some(key)) = (
                row.get("id").and_then(Value::as_i64),
                row.get("key").and_then(Value::as_str),
            ) {
                keys.insert(id, key.to_string());
            }
        }
    }
    keys
}

/// Адрес строки для отчёта: `KEY-path` для узла, `KEY-path (аренда #n)` для связанной записи.
fn row_address(
    table: &str,
    row: &Value,
    project_keys: &HashMap<i64, String>,
    node_addresses: &HashMap<i64, String>,
) -> Option<String> {
    match table {
        "nodes" => {
            let key = row
                .get("project_id")
                .and_then(Value::as_i64)
                .and_then(|id| project_keys.get(&id))?;
            let path = row.get("path").and_then(Value::as_str)?;
            Some(format!("{key}-{path}"))
        }
        "projects" => row.get("key").and_then(Value::as_str).map(str::to_string),
        _ => {
            let node_id = row.get("node_id").and_then(Value::as_i64);
            match node_id.and_then(|id| node_addresses.get(&id)) {
                Some(address) => Some(address.clone()),
                // Узел не назван — называем хотя бы таблицу и её собственный id.
                None => row
                    .get("id")
                    .and_then(Value::as_i64)
                    .map(|id| format!("{table}#{id}")),
            }
        }
    }
}

/// Значение JSON → значение SQLite. Объекты и массивы (tags, metadata, tree) хранятся текстом:
/// так их и отдаёт чтение из базы, поэтому круговой путь не меняет тип.
fn json_to_sql(value: &Value) -> SqlValue {
    match value {
        Value::Null => SqlValue::Null,
        Value::Bool(flag) => SqlValue::Integer(i64::from(*flag)),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => SqlValue::Integer(integer),
            None => SqlValue::Real(number.as_f64().unwrap_or(0.0)),
        },
        Value::String(text) => SqlValue::Text(text.clone()),
        other => SqlValue::Text(other.to_string()),
    }
}

/// Человеческая причина отказа вставки: диагностика SQLite без номеров и префиксов.
fn classify(err: &rusqlite::Error) -> String {
    let message = err.to_string();
    if message.contains("FOREIGN KEY constraint failed") {
        return "висячая ссылка: строка, на которую ссылаются, не перенесена".to_string();
    }
    if let Some(rest) = message.split("CHECK constraint failed:").nth(1) {
        return format!(
            "недопустимое значение по CHECK{}",
            rest.trim_end_matches(' ')
        );
    }
    if message.contains("UNIQUE constraint failed") {
        return format!("дубль ключа: {message}");
    }
    if message.contains("NOT NULL constraint failed") {
        return format!("пустое обязательное поле: {message}");
    }
    message
}

/// Нарушение внешнего ключа: кто, где и на кого ссылался.
struct Violation {
    table: String,
    rowid: i64,
    parent: String,
}

/// Нарушения внешних ключей внутри транзакции.
fn foreign_key_violations(conn: &Connection) -> Result<Vec<Violation>> {
    let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
    let mut rows = stmt.query([])?;
    let mut violations = Vec::new();
    while let Some(row) = rows.next()? {
        violations.push(Violation {
            table: row.get(0)?,
            rowid: row.get(1)?,
            parent: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
        });
    }
    Ok(violations)
}

/// Адрес нарушителя: узел называется `KEY-path`, дочерняя запись — адресом своего узла.
fn violation_address(
    conn: &Connection,
    violation: &Violation,
    project_keys: &HashMap<i64, String>,
    node_addresses: &HashMap<i64, String>,
) -> Result<Option<String>> {
    // Строку перечитываем из базы, а не из дампа: нарушитель мог попасть в базу не из того
    // же файла (заливка в непустую базу с `--force`).
    let sql = format!("SELECT * FROM `{}` WHERE rowid = ?1", violation.table);
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Ok(None);
    };
    let columns: Vec<String> = stmt.column_names().map(str::to_string).collect();
    let mut rows = stmt.query(rusqlite::params![violation.rowid])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let mut json = serde_json::Map::new();
    for (index, name) in columns.iter().enumerate() {
        let value = match row.get_ref(index)? {
            rusqlite::types::ValueRef::Null => Value::Null,
            rusqlite::types::ValueRef::Integer(number) => Value::from(number),
            rusqlite::types::ValueRef::Real(number) => Value::from(number),
            rusqlite::types::ValueRef::Text(text) => {
                Value::from(String::from_utf8_lossy(text).to_string())
            }
            rusqlite::types::ValueRef::Blob(bytes) => {
                Value::from(format!("<{} bytes>", bytes.len()))
            }
        };
        json.insert(name.clone(), value);
    }
    Ok(row_address(
        &violation.table,
        &Value::Object(json),
        project_keys,
        node_addresses,
    ))
}
