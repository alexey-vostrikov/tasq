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

    // Справочник статусов текущей схемы: с v4 `nodes.status` — ссылка на него, а не список в
    // CHECK. Поэтому статус из чужого или старого дампа отбивается не общим «FOREIGN KEY
    // constraint failed», а названным статусом — иначе в отчёте не видно, что именно не так.
    let known_statuses = known_statuses(conn)?;

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
            if *table == "nodes" {
                let status = row
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !known_statuses.contains(status) {
                    table_losses += 1;
                    report.losses.push(RestoreLoss {
                        table: (*table).to_string(),
                        address,
                        reason: format!(
                            "статус `{status}` не зарегистрирован в справочнике схемы \
                             v{SUPPORTED_SCHEMA_VERSION}"
                        ),
                    });
                    continue;
                }
            }
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

/// Имена статусов из справочника текущей схемы.
fn known_statuses(conn: &Connection) -> Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT name FROM statuses")?;
    let mut rows = stmt.query([])?;
    let mut names = std::collections::HashSet::new();
    while let Some(row) = rows.next()? {
        names.insert(row.get::<_, String>(0)?);
    }
    Ok(names)
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
    classify_message(&err.to_string())
}

/// Разбор текста ошибки SQLite — отдельно от её типа.
///
/// Отдельно потому, что `rusqlite::Error` с произвольным текстом не собрать: четыре ветки
/// распознавания иначе проверяются только теми ошибками, которые удалось вызвать на живой
/// базе, то есть обычно одной.
fn classify_message(message: &str) -> String {
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
    message.to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn db_conn() -> Connection {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-restore-{nanos}"));
        let outcome = crate::db::init(&dir, "RST", "restore").expect("init");
        crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open")
    }

    /// Минимальный дамп: пустой набор таблиц, текущая схема.
    fn empty_dump() -> Value {
        json!({
            "kind": DUMP_KIND,
            "format": DUMP_FORMAT,
            "schema_version": SUPPORTED_SCHEMA_VERSION,
            "tables": {},
        })
    }

    /// Чужой документ отвергается по полю `kind`, а не падением на разборе.
    #[test]
    fn a_document_of_another_kind_is_refused() {
        let mut conn = db_conn();
        let err =
            restore(&mut conn, &json!({"kind": "something-else"})).expect_err("чужой kind принят");
        assert_eq!(err.code(), "INVALID_VALUE");
        assert!(err.to_string().contains("kind"), "поле не названо: {err}");
    }

    /// Дамп новее нашего формата отвергается: разбирать его наугад нельзя.
    #[test]
    fn a_newer_format_is_refused() {
        let mut conn = db_conn();
        let doc = json!({
            "kind": DUMP_KIND,
            "format": DUMP_FORMAT + 1,
            "schema_version": SUPPORTED_SCHEMA_VERSION,
            "tables": {},
        });
        let err = restore(&mut conn, &doc).expect_err("формат из будущего принят");
        assert_eq!(err.code(), "INVALID_VALUE");
        assert!(err.to_string().contains("format"), "поле не названо: {err}");
    }

    /// Без версии схемы дамп не читается: неизвестно, что в нём за схема.
    #[test]
    fn a_dump_without_a_schema_version_is_refused() {
        let mut conn = db_conn();
        let doc = json!({"kind": DUMP_KIND, "format": DUMP_FORMAT, "tables": {}});
        let err = restore(&mut conn, &doc).expect_err("дамп без версии схемы принят");
        assert_eq!(err.code(), "INVALID_VALUE");
        assert!(
            err.to_string().contains("schema_version"),
            "поле не названо: {err}"
        );
    }

    /// Схема из будущего — отдельный код отказа, по нему решают «обнови tasq».
    #[test]
    fn a_newer_schema_is_refused_with_its_own_code() {
        let mut conn = db_conn();
        let mut doc = empty_dump();
        doc["schema_version"] = json!(SUPPORTED_SCHEMA_VERSION + 1);
        let err = restore(&mut conn, &doc).expect_err("схема из будущего принята");
        assert_eq!(err.code(), "SCHEMA_TOO_NEW", "другой код отказа: {err}");
    }

    /// Документ без `tables` — не дамп, и это сказано прямо.
    #[test]
    fn a_document_without_tables_is_refused() {
        let mut conn = db_conn();
        let doc = json!({
            "kind": DUMP_KIND,
            "format": DUMP_FORMAT,
            "schema_version": SUPPORTED_SCHEMA_VERSION,
        });
        let err = restore(&mut conn, &doc).expect_err("документ без tables принят");
        assert_eq!(err.code(), "INVALID_VALUE");
        assert!(err.to_string().contains("tables"), "поле не названо: {err}");
    }

    /// Текущий формат и текущая схема — ГРАНИЦА, которая обязана приниматься.
    ///
    /// Без этого утверждения мутация `>` → `>=` не отличима от верного кода: дамп «из будущего»
    /// отвергается в обоих случаях, и красными становятся все тесты сразу — то есть граница
    /// проверяется не там, где она записана.
    #[test]
    fn the_current_format_and_schema_are_accepted() {
        let mut conn = db_conn();
        let doc = json!({
            "kind": DUMP_KIND,
            "format": DUMP_FORMAT,
            "schema_version": SUPPORTED_SCHEMA_VERSION,
            "tables": {},
        });
        let report = restore(&mut conn, &doc).expect("ровно текущая версия отвергнута");
        assert!(report.committed, "дамп текущей версии не закоммичен");
        assert_eq!(report.schema_from, SUPPORTED_SCHEMA_VERSION);
        assert_eq!(report.schema_to, SUPPORTED_SCHEMA_VERSION);
    }

    /// Пустой дамп проходит и коммитится: восстанавливать нечего — не отказ.
    #[test]
    fn an_empty_dump_commits() {
        let mut conn = db_conn();
        let report = restore(&mut conn, &empty_dump()).expect("пустой дамп");
        assert!(report.committed, "пустой дамп не закоммичен");
        assert!(report.tables.is_empty(), "таблицы без строк попали в отчёт");
        assert!(!report.has_losses(), "у пустого дампа нашлись потери");
    }

    /// Таблица, которой больше нет в схеме, попадает в ПОТЕРИ с числом строк.
    ///
    /// Имя берётся из `DUMP_TABLES` и таблица УДАЛЯЕТСЯ из базы: разбор идёт по закрытому
    /// списку, поэтому выдуманное имя («ghost_table») до этой ветки не доходит вовсе — оно
    /// молча пропускается, и тест на нём проверял бы пустоту.
    #[test]
    fn a_table_unknown_to_the_schema_is_reported_as_a_loss() {
        let mut conn = db_conn();
        conn.execute_batch("DROP TABLE burn_snapshots")
            .expect("drop");
        let mut doc = empty_dump();
        doc["tables"] = json!({"burn_snapshots": [{"id": 1}, {"id": 2}]});
        let report = restore(&mut conn, &doc).expect("дамп с исчезнувшей таблицей");
        assert!(!report.committed, "дамп с потерями закоммичен");
        assert_eq!(report.losses.len(), 1, "потеря не записана: {report:?}");
        let loss = &report.losses[0];
        assert_eq!(loss.table, "burn_snapshots");
        assert!(
            loss.reason.contains("2 строк"),
            "в причине нет числа строк: {}",
            loss.reason
        );
        let table = report
            .tables
            .iter()
            .find(|t| t.table == "burn_snapshots")
            .expect("таблица не попала в отчёт");
        assert_eq!((table.in_dump, table.inserted, table.skipped), (2, 0, 2));
    }

    /// Таблица из схемы без строк в дампе попадает в отчёт нулями и потерей НЕ считается.
    ///
    /// Дамп перечисляет все таблицы, включая пустые, поэтому «ноль строк» — обычное состояние,
    /// а не потеря; отличить «таблица была и пуста» от «таблицы в дампе нет» отчёт обязан.
    #[test]
    fn an_empty_table_is_reported_with_zeros_and_is_not_a_loss() {
        let mut conn = db_conn();
        let mut doc = empty_dump();
        doc["tables"] = json!({"nodes": []});
        let report = restore(&mut conn, &doc).expect("дамп с пустой таблицей");
        assert!(report.committed, "пустая таблица считается потерей");
        assert!(
            !report.has_losses(),
            "у пустой таблицы нашлись потери: {report:?}"
        );
        let table = report
            .tables
            .iter()
            .find(|t| t.table == "nodes")
            .unwrap_or_else(|| panic!("пустая таблица выпала из отчёта: {report:?}"));
        assert_eq!(
            (table.in_dump, table.inserted, table.skipped),
            (0, 0, 0),
            "пустая таблица описана не нулями"
        );
    }

    /// Значения JSON ложатся в SQLite тем типом, которым их потом читают: флаг — числом,
    /// объект — текстом. Иначе круговой путь «экспорт → восстановление» меняет тип поля.
    #[test]
    fn json_values_map_to_the_types_the_reader_expects() {
        let num = |n: serde_json::Number| Value::Number(n);
        assert!(matches!(json_to_sql(&Value::Null), SqlValue::Null));
        assert!(matches!(
            json_to_sql(&Value::Bool(true)),
            SqlValue::Integer(1)
        ));
        assert!(matches!(
            json_to_sql(&Value::Bool(false)),
            SqlValue::Integer(0)
        ));
        assert!(matches!(
            json_to_sql(&num(serde_json::Number::from(7))),
            SqlValue::Integer(7)
        ));
        match json_to_sql(&num(serde_json::Number::from_f64(1.5).expect("float"))) {
            SqlValue::Real(value) => assert!((value - 1.5).abs() < f64::EPSILON),
            other => panic!("дробное стало {other:?}"),
        }
        match json_to_sql(&Value::String("текст".to_string())) {
            SqlValue::Text(text) => assert_eq!(text, "текст"),
            other => panic!("строка стала {other:?}"),
        }
        match json_to_sql(&json!(["a", "b"])) {
            SqlValue::Text(text) => assert_eq!(text, r#"["a","b"]"#),
            other => panic!("массив стал {other:?}"),
        }
        match json_to_sql(&json!({"k": 1})) {
            SqlValue::Text(text) => assert_eq!(text, r#"{"k":1}"#),
            other => panic!("объект стал {other:?}"),
        }
    }

    /// Четыре вида отказа вставки называются по-разному: иначе в отчёте они неразличимы.
    #[test]
    fn every_constraint_violation_gets_its_own_wording() {
        assert_eq!(
            classify_message("FOREIGN KEY constraint failed"),
            "висячая ссылка: строка, на которую ссылаются, не перенесена"
        );
        assert_eq!(
            classify_message("CHECK constraint failed: status "),
            "недопустимое значение по CHECK status",
            "текст CHECK-отказа изменился: пробел по краю срезается, двоеточия не остаётся"
        );
        let unique = classify_message("UNIQUE constraint failed: nodes.project_id, nodes.path");
        assert!(
            unique.starts_with("дубль ключа: UNIQUE constraint failed"),
            "уникальность названа не по-своему: {unique}"
        );
        let null = classify_message("NOT NULL constraint failed: nodes.title");
        assert!(
            null.starts_with("пустое обязательное поле: NOT NULL"),
            "пустое поле названо не по-своему: {null}"
        );
        assert_eq!(
            classify_message("database is locked"),
            "database is locked",
            "незнакомая ошибка переписана"
        );
    }
}
