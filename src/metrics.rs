//! Метрики вызовов: сколько, как долго и чем кончилось.
//!
//! Зачем отдельный файл, а не таблица в базе. Метрика — это запись, а чтение базы по решению
//! TASQDEV-19 не пишет ничего: именно на этом держится обещание «сто читателей проходят сто из
//! ста». Положи метрику в основную базу — и каждая читающая команда снова берёт блокировку
//! записи, только теперь ради счётчика. Поэтому метрики живут рядом (`<имя>-metrics.db`): своя
//! блокировка, своя жизнь, и потеря этого файла ничего не значит.
//!
//! Что записывается: имя операции (команда CLI или инструмент MCP), длительность, исход (`ok` либо
//! код ошибки — те же 84 кода, что печатает `error[CODE]`), и сколько раз пришлось повторить из-за
//! занятости. Последнее — единственный способ узнать цену блокировок: сами повторы видны только в
//! коде, а их число и съеденное время не видно нигде.
//!
//! Чего здесь нет: сэмплирования и порогов. Записывается каждый вызов: их десятки-сотни в день, а
//! «редкие медленные» — ровно то, что нужно видеть, и выборка их как раз выбросила бы.
//!
//! Запись — best-effort. Сбой метрики не имеет права уронить команду: инструмент, который
//! отказывается работать из-за сломанного счётчика, хуже, чем инструмент без счётчика.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;
use serde::Serialize;

use crate::error::Result;

/// Имя файла метрик рядом с базой: `hermes.db` → `hermes-metrics.db`.
pub fn path(db_path: &Path) -> PathBuf {
    let stem = db_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "tasq".to_string());
    db_path.with_file_name(format!("{stem}-metrics.db"))
}

/// Схема: одна таблица и один индекс по времени.
///
/// Ни внешних ключей, ни ссылок на узлы: метрика не должна зависеть от того, что происходит с
/// данными, и обязана пережить удаление проекта, к которому относился вызов.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS calls (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  at          TEXT NOT NULL,
  operation   TEXT NOT NULL,
  duration_ms INTEGER NOT NULL,
  outcome     TEXT NOT NULL,
  retries     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_calls_at ON calls(at);
CREATE INDEX IF NOT EXISTS idx_calls_operation ON calls(operation, at);
";

/// Сколько дней метрики живут по умолчанию.
///
/// Месяц, а не год: вопрос, на который они отвечают, — «что происходит сейчас» (выросло ли время
/// ответа, посыпались ли отказы), и годовая давность тут только мешает сравнением с другим
/// инструментом на другой машине.
pub const KEEP_DAYS: i64 = 30;

/// Записать один вызов. Ошибки глушатся: метрика не важнее работы.
pub fn record(db_path: &Path, operation: &str, elapsed: Duration, outcome: &str, retries: u32) {
    let _ = record_checked(db_path, operation, elapsed, outcome, retries);
}

fn record_checked(
    db_path: &Path,
    operation: &str,
    elapsed: Duration,
    outcome: &str,
    retries: u32,
) -> Result<()> {
    // Основной базы может не быть вовсе (команда до `init`): тогда и метрики не нужны.
    if !db_path.is_file() {
        return Ok(());
    }
    let conn = open(&path(db_path))?;
    conn.execute(
        "INSERT INTO calls (at, operation, duration_ms, outcome, retries)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            crate::db::now_iso(),
            operation,
            elapsed.as_millis() as i64,
            outcome,
            retries as i64
        ],
    )?;
    Ok(())
}

fn open(file: &Path) -> Result<Connection> {
    let conn = Connection::open(file)?;
    // Короткий таймаут: метрика — фон, и стоять в очереди за блокировкой ей незачем.
    conn.execute_batch(&format!(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = {};",
        crate::db::DEFAULT_BUSY_MS
    ))?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// Сколько раз отступали из-за занятости — процесс целиком.
///
/// Счётчик общий на процесс (`AtomicU32`), а не возвращаемое значение каждого вызова: повтор
/// случается глубоко внутри `write_tx`, и протаскивать счётчик через тридцать вызовов значило бы
/// менять их подписи ради числа в отчёте.
static RETRIES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Отметить один повтор.
pub fn count_retry() {
    RETRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Сколько повторов накопилось и обнулить счётчик.
pub fn take_retries() -> u32 {
    RETRIES.swap(0, std::sync::atomic::Ordering::Relaxed)
}

/// Счётчик по одному исходу.
#[derive(Debug, Clone, Serialize)]
pub struct OutcomeCount {
    pub outcome: String,
    pub calls: i64,
}

/// Строка по операции: сколько раз звали и как долго это было.
#[derive(Debug, Clone, Serialize)]
pub struct OperationRow {
    pub operation: String,
    pub calls: i64,
    pub p50_ms: i64,
    pub p95_ms: i64,
    pub max_ms: i64,
    pub failures: i64,
}

/// Аренды и журнал — то, что `doctor` показывает снимком, а здесь нужно числом за период.
///
/// Ни одна из этих цифр не пишется: они читаются из основной базы в режиме только-чтение, потому
/// что вопрос «сколько аренд истекает без закрытия» задаётся часто, а сто́ит одного индекса.
#[derive(Debug, Clone, Serialize)]
pub struct Leases {
    /// Аренды, которые сейчас держат: не закрыты и не просрочены.
    pub live: i64,
    /// Просроченные, но не закрытые: работа, брошенная без отчёта.
    pub expired_open: i64,
    /// Сколько аренд истекло без закрытия за окно отчёта.
    pub expired_in_window: i64,
    /// Открытых резерваций (обещаний, а не аренд).
    pub reservations: i64,
    /// Размер журнала (`-wal`) в байтах: сколько сейчас не перелито в основной файл.
    pub journal_bytes: u64,
}

/// Итог `tasq stat`.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub metrics_path: String,
    /// С какого момента считается — по нему же обрезаются старые записи.
    pub since: String,
    pub present: bool,
    pub calls: i64,
    pub failures: i64,
    pub retries: i64,
    /// Длительность всех вызовов вместе: «сколько времени ушло на трекер».
    pub total_ms: i64,
    pub by_outcome: Vec<OutcomeCount>,
    pub by_operation: Vec<OperationRow>,
    /// Аренды и журнал; `None` — базы нет (команда до `init`).
    pub leases: Option<Leases>,
    /// Сколько записей удалено как устаревшие (`--prune`).
    pub pruned: i64,
}

/// Аренды, просрочки и размер журнала — чтением, без единой записи.
///
/// Ошибка чтения здесь не роняет отчёт: метрики важнее, чем сопутствующая цифра, и отчёт без
/// раздела аренд полезнее отказа целиком.
fn lease_figures(db_path: &Path, since: &str) -> Option<Leases> {
    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    conn.busy_timeout(Duration::from_millis(u64::from(crate::db::DEFAULT_BUSY_MS)))
        .ok()?;
    let now = crate::db::now_iso();
    let live: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM attempts WHERE ended_at IS NULL AND expires_at > ?1",
            rusqlite::params![now],
            |row| row.get(0),
        )
        .ok()?;
    let expired_open: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM attempts WHERE ended_at IS NULL AND expires_at <= ?1",
            rusqlite::params![now],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let expired_in_window: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM attempts WHERE end_reason = 'expired' AND started_at >= ?1",
            rusqlite::params![since],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let reservations: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM reservations WHERE released_at IS NULL AND expires_at > ?1",
            rusqlite::params![now],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let journal_bytes = std::fs::metadata(db_path.with_extension("db-wal"))
        .map(|meta| meta.len())
        .unwrap_or(0);
    Some(Leases {
        live,
        expired_open,
        expired_in_window,
        reservations,
        journal_bytes,
    })
}

/// Начало окна отчёта: `24h`, `7d`, `30d` или готовая метка времени.
///
/// Один разбор на CLI и MCP: у MCP нет своего разбора аргументов, и второй экземпляр этой логики
/// означал бы, что `--since 7d` и `since: "7d"` когда-нибудь разойдутся. Опечатка — отказ, а не
/// тихий отчёт за сутки: отчёт не за тот период даёт неверный вывод о собственной работе.
pub fn parse_window(value: Option<&str>) -> Result<String> {
    let now = time::OffsetDateTime::now_utc();
    let Some(text) = value else {
        return Ok(crate::db::iso_at(now - time::Duration::hours(24)));
    };
    let text = text.trim();
    for (suffix, hours) in [("h", 1_i64), ("d", 24)] {
        if let Some(number) = text.strip_suffix(suffix) {
            let amount: i64 = number.parse().map_err(|_| invalid_window(text))?;
            if amount <= 0 {
                return Err(invalid_window(text));
            }
            return Ok(crate::db::iso_at(
                now - time::Duration::hours(amount * hours),
            ));
        }
    }
    time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
        .map(crate::db::iso_at)
        .map_err(|_| invalid_window(text))
}

fn invalid_window(text: &str) -> crate::error::Error {
    crate::error::Error::InvalidValue {
        field: "since".to_string(),
        value: format!("{text} (expected 24h, 7d, 30d or an RFC 3339 timestamp)"),
    }
}

/// Отчёт по метрикам.
///
/// `since` — начало окна в ISO; `prune` удаляет записи старше окна хранения (не окна отчёта):
/// иначе таблица растёт вечно — ровно та беда, которую мы лечим в 20.7, только на счётчике.
pub fn report(db_path: &Path, since: &str, prune: bool) -> Result<Report> {
    let file = path(db_path);
    let mut report = Report {
        metrics_path: file.display().to_string(),
        since: since.to_string(),
        present: file.is_file(),
        calls: 0,
        failures: 0,
        retries: 0,
        total_ms: 0,
        by_outcome: Vec::new(),
        by_operation: Vec::new(),
        leases: lease_figures(db_path, since),
        pruned: 0,
    };
    if !report.present {
        return Ok(report);
    }
    let conn = open(&file)?;

    if prune {
        let cutoff =
            crate::db::iso_at(time::OffsetDateTime::now_utc() - time::Duration::days(KEEP_DAYS));
        report.pruned =
            conn.execute("DELETE FROM calls WHERE at < ?1", rusqlite::params![cutoff])? as i64;
    }

    let window = "at >= ?1";
    report.calls = conn.query_row(
        &format!("SELECT COUNT(*) FROM calls WHERE {window}"),
        rusqlite::params![since],
        |row| row.get(0),
    )?;
    report.total_ms = conn.query_row(
        &format!("SELECT COALESCE(SUM(duration_ms), 0) FROM calls WHERE {window}"),
        rusqlite::params![since],
        |row| row.get(0),
    )?;
    report.retries = conn.query_row(
        &format!("SELECT COALESCE(SUM(retries), 0) FROM calls WHERE {window}"),
        rusqlite::params![since],
        |row| row.get(0),
    )?;
    report.failures = conn.query_row(
        &format!("SELECT COUNT(*) FROM calls WHERE {window} AND outcome <> 'ok'"),
        rusqlite::params![since],
        |row| row.get(0),
    )?;

    let mut stmt = conn.prepare(&format!(
        "SELECT outcome, COUNT(*) FROM calls WHERE {window} GROUP BY outcome ORDER BY 2 DESC"
    ))?;
    report.by_outcome = stmt
        .query_map(rusqlite::params![since], |row| {
            Ok(OutcomeCount {
                outcome: row.get(0)?,
                calls: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    report.by_operation = per_operation(&conn, since)?;
    Ok(report)
}

/// Перцентили по операциям.
///
/// Считаются не в SQL, а выборкой длительностей: у SQLite нет функции перцентиля, а выражение
/// вида `LIMIT 1 OFFSET (COUNT(*)*0.95)` требует двух запросов на каждую строку и на сотнях
/// операций становится медленнее самого отчёта. Список операций мал, поэтому честнее прочитать
/// числа и посчитать их в Rust — там формула видна и проверяема тестом.
fn per_operation(conn: &Connection, since: &str) -> Result<Vec<OperationRow>> {
    let mut names: Vec<String> = conn
        .prepare("SELECT DISTINCT operation FROM calls WHERE at >= ?1 ORDER BY 1")?
        .query_map(rusqlite::params![since], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    names.sort();

    let mut rows = Vec::with_capacity(names.len());
    for name in names {
        let mut durations: Vec<i64> = conn
            .prepare("SELECT duration_ms FROM calls WHERE operation = ?1 AND at >= ?2")?
            .query_map(rusqlite::params![name, since], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        durations.sort_unstable();
        let failures: i64 = conn.query_row(
            "SELECT COUNT(*) FROM calls WHERE operation = ?1 AND at >= ?2 AND outcome <> 'ok'",
            rusqlite::params![name, since],
            |row| row.get(0),
        )?;
        rows.push(OperationRow {
            operation: name,
            calls: durations.len() as i64,
            p50_ms: percentile(&durations, 50.0),
            p95_ms: percentile(&durations, 95.0),
            max_ms: durations.last().copied().unwrap_or(0),
            failures,
        });
    }
    Ok(rows)
}

/// Перцентиль по отсортированному ряду, методом ближайшего ранга.
///
/// Ранг, а не интерполяция: длительность — целое число миллисекунд, и «p95 = 42,7 мс» было бы
/// точностью, которой у замера нет. Пустой ряд даёт ноль — так вызывающий не обязан проверять.
fn percentile(sorted: &[i64], percent: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((percent / 100.0) * sorted.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[index]
}

/// Размер файла метрик — для `doctor`.
pub fn size(db_path: &Path) -> u64 {
    fs::metadata(path(db_path))
        .map(|meta| meta.len())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Границы перцентиля: ранг, а не интерполяция, и пустой ряд не паника.
    #[test]
    fn percentiles_take_the_nearest_rank() {
        assert_eq!(percentile(&[], 50.0), 0);
        assert_eq!(percentile(&[7], 95.0), 7);
        assert_eq!(percentile(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 50.0), 5);
        assert_eq!(percentile(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 95.0), 10);
        // Двадцать значений: p95 — девятнадцатое, а не двадцатое и не интерполяция.
        let twenty: Vec<i64> = (1..=20).collect();
        assert_eq!(percentile(&twenty, 95.0), 19);
    }
}
