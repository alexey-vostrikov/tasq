//! Архив журнала и истории статусов: вынос старого, но не удаление.
//!
//! Зачем это нужно. `node_status_history` и `activity_log` растут без предела. На фикстуре в
//! 10 000 узлов история занимает 1,85 МБ из 4,3 — вместе с индексами это 43% базы при 1,37
//! строки на узел; у живого узла переходов больше, поэтому 50 МБ из README на 100 000 узлов —
//! не потолок, а нижняя оценка. Растёт при этом не то, что читают каждый день: `board` историю не
//! трогает вовсе, а `burnup` читает её только чтобы найти первый переход в `done`.
//!
//! Что здесь устроено и почему именно так.
//!
//! **Вынос, а не удаление.** Удаление истории сломало бы `burnup` за прошлые периоды молча:
//! узлы без истории попадают в счётчик «неизвестно», то есть «работа не сделана» вместо «мы её
//! сделали год назад». Поэтому старые строки переезжают в файл рядом (`<имя>-archive.db`) и
//! остаются доступны.
//!
//! **Чтение прозрачное.** Архив подключается к соединению (`ATTACH`), и читающие запросы видят
//! объединение: основной файл плюс архив. Это единственный способ вынести данные, не сделав
//! отчёты ложными, — и цена его в том, что SQL собирается строкой (см. `history_source`).
//! Отключать архив флагом значило бы завести второй, «правильный» режим чтения, о котором никто
//! не помнит.
//!
//! **Запись — только явной командой.** `tasq compact` переносит строки старше горизонта одной
//! транзакцией и усекает основной файл (`VACUUM`), иначе освободившиеся страницы остались бы в
//! списке свободных и размер не изменился бы. Автоматически этого не происходит никогда:
//! перенос данных — решение, а не побочный эффект чтения (та же причина, что у `tasq migrate`).

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;

use crate::db;
use crate::error::Result;

/// Имя файла архива рядом с базой: `hermes.db` → `hermes-archive.db`.
pub fn path(db_path: &Path) -> PathBuf {
    let stem = db_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "tasq".to_string());
    db_path.with_file_name(format!("{stem}-archive.db"))
}

/// Сколько времени журнал и история живут в основной базе по умолчанию: год.
///
/// Год, а не месяц: `burnup` за прошлый год — обычный вопрос, и вынос данных, которые ещё
/// спрашивают, сделал бы отчёт медленнее (через архив), ничего не сэкономив. Год, а не десять
/// лет: за десять лет история перестаёт быть историей и становится мусором.
pub const DEFAULT_HORIZON_DAYS: i64 = 365;

/// Подключён ли архив к этому соединению.
pub fn is_attached(conn: &Connection) -> bool {
    // `pragma_database_list` перечисляет подключённые базы; ошибку здесь глушить не нужно —
    // если прагма недоступна, вызывающий увидит её сам.
    conn.prepare("SELECT name FROM pragma_database_list")
        .and_then(|mut stmt| {
            let names = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut found = false;
            for name in names {
                if name?.eq_ignore_ascii_case("archive") {
                    found = true;
                }
            }
            Ok(found)
        })
        .unwrap_or(false)
}

/// Подключает архив, если файл существует.
///
/// Возвращает `true`, если архив подключён. Файл создаётся только `tasq compact`: обычная
/// команда на базе без архива не должна оставлять рядом пустой файл — «есть архив» и «был
/// compact» обязаны означать одно и то же.
pub fn attach(conn: &Connection, db_path: &Path) -> Result<bool> {
    if is_attached(conn) {
        return Ok(true);
    }
    let archive = path(db_path);
    if !archive.is_file() {
        return Ok(false);
    }
    conn.execute(
        "ATTACH DATABASE ?1 AS archive",
        rusqlite::params![archive.to_string_lossy()],
    )?;
    Ok(true)
}

/// Источник строк истории для SQL: основная таблица либо объединение с архивом.
///
/// Строкой, а не представлением: представление пришлось бы создавать на каждом соединении, а
/// `TEMP VIEW` поверх подключённой базы SQLite не разрешает менять. Список столбцов выписан
/// явно — `SELECT *` в объединении молча разъедется, если в архивной таблице когда-нибудь
/// окажется другой порядок.
pub fn history_source(conn: &Connection) -> &'static str {
    if is_attached(conn) {
        "SELECT id, node_id, from_status, to_status, cause, at, actor FROM node_status_history \
         UNION ALL \
         SELECT id, node_id, from_status, to_status, cause, at, actor FROM archive.node_status_history"
    } else {
        "SELECT id, node_id, from_status, to_status, cause, at, actor FROM node_status_history"
    }
}

/// То же для журнала действий.
pub fn log_source(conn: &Connection) -> &'static str {
    if is_attached(conn) {
        "SELECT id, project_id, node_id, entity_type, entity_id, action, field_name, old_value, \
                new_value, summary, actor, attempt_id, created_at FROM activity_log \
         UNION ALL \
         SELECT id, project_id, node_id, entity_type, entity_id, action, field_name, old_value, \
                new_value, summary, actor, attempt_id, created_at FROM archive.activity_log"
    } else {
        "SELECT id, project_id, node_id, entity_type, entity_id, action, field_name, old_value, \
                new_value, summary, actor, attempt_id, created_at FROM activity_log"
    }
}

/// Итог `tasq compact`.
#[derive(Debug, Clone, Serialize)]
pub struct CompactReport {
    pub db_path: String,
    pub archive_path: String,
    /// Всё старше этого момента вынесено.
    pub before: String,
    /// Ничего не переносилось: показано только, что было бы.
    pub dry_run: bool,
    pub history_rows: i64,
    pub log_rows: i64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub archive_bytes: u64,
}

impl CompactReport {
    /// Сколько освободилось в основном файле.
    pub fn freed(&self) -> u64 {
        self.bytes_before.saturating_sub(self.bytes_after)
    }
}

/// Схема архивных таблиц.
///
/// Без внешних ключей и без `CHECK` на справочники: архив живёт отдельным файлом, и ссылаться
/// ему не на что — `nodes` и `statuses` остаются в основной базе. Типы и порядок столбцов
/// совпадают с основной таблицей побайтово, иначе `INSERT … SELECT` перепутал бы значения, а
/// объединение в SQL разъехалось бы.
const ARCHIVE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS archive.node_status_history (
  id          INTEGER PRIMARY KEY,
  node_id     INTEGER NOT NULL,
  from_status TEXT,
  to_status   TEXT NOT NULL,
  cause       TEXT NOT NULL,
  at          TEXT NOT NULL,
  actor       TEXT
);
CREATE INDEX IF NOT EXISTS archive.arch_history_node ON node_status_history(node_id, at);
CREATE INDEX IF NOT EXISTS archive.arch_history_at ON node_status_history(at);
CREATE TABLE IF NOT EXISTS archive.activity_log (
  id          INTEGER PRIMARY KEY,
  project_id  INTEGER NOT NULL,
  node_id     INTEGER,
  entity_type TEXT NOT NULL,
  entity_id   INTEGER NOT NULL,
  action      TEXT NOT NULL,
  field_name  TEXT,
  old_value   TEXT,
  new_value   TEXT,
  summary     TEXT,
  actor       TEXT,
  attempt_id  INTEGER,
  created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS archive.arch_log_project ON activity_log(project_id, created_at);
CREATE INDEX IF NOT EXISTS archive.arch_log_created ON activity_log(created_at);
";

/// Переносит строки старше `before` в архив и усекает основной файл.
///
/// `dry_run` считает то же самое, но ничего не меняет: без него решение принималось бы вслепую —
/// «сколько это даст» и «сколько это тронет» и есть весь вопрос политики хранения.
pub fn compact(
    conn: &mut Connection,
    db_path: &Path,
    before: &str,
    dry_run: bool,
) -> Result<CompactReport> {
    let archive = path(db_path);
    let bytes_before = file_size(db_path);

    let history_rows = count_older(conn, "node_status_history", "at", before)?;
    let log_rows = count_older(conn, "activity_log", "created_at", before)?;

    if dry_run {
        // Размер «после» оценивается как разница по строкам, а не нулём: обещать освобождение
        // всего файла было бы ложью — служебные страницы и индексы остаются.
        let estimate = bytes_before
            .saturating_sub(((history_rows + log_rows) as u64).saturating_mul(APPROX_ROW_BYTES));
        return Ok(CompactReport {
            db_path: db_path.display().to_string(),
            archive_path: archive.display().to_string(),
            before: before.to_string(),
            dry_run: true,
            history_rows,
            log_rows,
            bytes_before,
            bytes_after: estimate,
            archive_bytes: file_size(&archive),
        });
    }

    // Архив подключается на время переноса сам: основное соединение его не видит, если файла
    // ещё не было, а `attach` при обычном открытии чужие файлы не создаёт.
    let attached_here = !is_attached(conn);
    if attached_here {
        // `ATTACH` создаёт файл, если его нет: отдельного «создай архив» не нужно, и пустой
        // файл рядом появится только когда перенос действительно делают.
        conn.execute(
            "ATTACH DATABASE ?1 AS archive",
            rusqlite::params![archive.to_string_lossy()],
        )?;
    }

    let result = (|| -> Result<()> {
        conn.execute_batch(ARCHIVE_SCHEMA)?;
        let tx = db::write_tx(conn)?;
        // `INSERT OR IGNORE`: повторный перенос того же диапазона не должен падать на
        // совпадении `id` — архивирование идемпотентно, как и миграция.
        tx.execute(
            "INSERT OR IGNORE INTO archive.node_status_history
                 (id, node_id, from_status, to_status, cause, at, actor)
             SELECT id, node_id, from_status, to_status, cause, at, actor
             FROM node_status_history WHERE at < ?1",
            rusqlite::params![before],
        )?;
        tx.execute(
            "DELETE FROM node_status_history WHERE at < ?1",
            rusqlite::params![before],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO archive.activity_log
                 (id, project_id, node_id, entity_type, entity_id, action, field_name, old_value,
                  new_value, summary, actor, attempt_id, created_at)
             SELECT id, project_id, node_id, entity_type, entity_id, action, field_name, old_value,
                    new_value, summary, actor, attempt_id, created_at
             FROM activity_log WHERE created_at < ?1",
            rusqlite::params![before],
        )?;
        tx.execute(
            "DELETE FROM activity_log WHERE created_at < ?1",
            rusqlite::params![before],
        )?;
        tx.commit()?;
        Ok(())
    })();

    if attached_here {
        // Отключаем в любом случае: соединение остаётся живым, а `DETACH` на подключённой базе
        // с незакрытой транзакцией — ошибка, поэтому сначала транзакция, потом отключение.
        let _ = conn.execute_batch("DETACH DATABASE archive");
    }
    result?;

    // Освободившиеся страницы остаются в списке свободных, и размер файла не меняется: без
    // `VACUUM` весь смысл выноса пропадает. Ошибку не глушим — если усечь не удалось, отчёт
    // обязан это показать, а не сделать вид, что стало меньше.
    conn.execute_batch("VACUUM")?;
    // И переливка: в WAL-режиме `VACUUM` пишет через журнал, поэтому основной файл уменьшается
    // только после checkpoint. Без этого замер «стало меньше» показывал бы прежний размер, а
    // данные лежали бы в `-wal` до следующего сброса.
    crate::db::checkpoint(conn)?;

    Ok(CompactReport {
        db_path: db_path.display().to_string(),
        archive_path: archive.display().to_string(),
        before: before.to_string(),
        dry_run: false,
        history_rows,
        log_rows,
        bytes_before,
        bytes_after: file_size(db_path),
        archive_bytes: file_size(&archive),
    })
}

/// Сколько строк старше момента — тем же условием, каким их будет удалять перенос.
fn count_older(conn: &Connection, table: &str, column: &str, before: &str) -> Result<i64> {
    // Имя таблицы и столбца — из констант этого модуля, не из входа: подстановка строк в SQL
    // недопустима даже при закрытом списке.
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {column} < ?1");
    Ok(conn.query_row(&sql, rusqlite::params![before], |row| row.get(0))?)
}

/// Сколько примерно занимает одна вынесенная строка с индексами.
///
/// Нужно только для оценки в `--dry-run`; точное число даёт сам перенос. Замер на живой базе:
/// 1 651 строка журнала — 544 КБ вместе с двумя индексами, то есть около 330 байт на строку.
const APPROX_ROW_BYTES: u64 = 330;

fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// Размер журнала и истории в основной базе и в архиве — для `doctor`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ArchiveInfo {
    pub archive_path: String,
    pub present: bool,
    pub bytes: u64,
    pub history_rows: i64,
    pub log_rows: i64,
}

/// Что лежит в архиве. Пустой архив — не ошибка: так выглядит база, где `compact` ещё не звали.
pub fn info(conn: &Connection, db_path: &Path) -> ArchiveInfo {
    let archive = path(db_path);
    let mut info = ArchiveInfo {
        archive_path: archive.display().to_string(),
        present: archive.is_file(),
        bytes: file_size(&archive),
        ..ArchiveInfo::default()
    };
    if !info.present {
        return info;
    }
    if is_attached(conn) {
        info.history_rows = conn
            .query_row(
                "SELECT COUNT(*) FROM archive.node_status_history",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        info.log_rows = conn
            .query_row("SELECT COUNT(*) FROM archive.activity_log", [], |row| {
                row.get(0)
            })
            .unwrap_or(0);
    }
    info
}
