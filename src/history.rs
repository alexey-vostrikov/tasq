//! История статусов: факт перехода, отдельно от журнала (README §5.1.1).
//!
//! Зачем отдельная таблица, если есть `activity_log`. Журнал отвечает на вопрос «что
//! произошло» и пишется ВСЕМИ путями мутации; история отвечает на один вопрос — «когда узел
//! стал `done`» — и на него должен отвечать одинаково, каким бы путём статус ни сменился.
//!
//! До этого `burnup` вынимал дату закрытия из журнала по паре
//! `(field_name='status', new_value='done')`, то есть по СОВПАДЕНИЮ ТЕКСТА в местах записи.
//! Таких мест шесть, и седьмой (`import`) молча выпал из ряда: импорт проекта обнулял
//! burn-up. С отдельной таблицей путь записи не может «забыть» про историю незаметно —
//! он либо пишет факт, либо тест на этом пути падает.

use rusqlite::Connection;
use serde::Serialize;

use crate::db::now_iso;
use crate::error::Result;

/// Причина перехода. Хранится в `node_status_history.cause` и ограничена `CHECK` в схеме.
///
/// `lease_expired` отдельно от `lease_released` намеренно: истёкшая аренда — не работа,
/// доведённая до конца и не решение исполнителя. Для отчётов это разница между «сделал и
/// отпустил» и «не успел», и склеивать их значило бы врать в обе стороны.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// Узел появился: первый факт, `from_status` пуст.
    ///
    /// Создание — не переход, но без него у нового узла истории нет вовсе, и отчёт не может
    /// отличить «ничего не происходило» от «мы ничего не знаем».
    Created,
    /// Ручной переход (`tasq set`, MCP `node_update`).
    Manual,
    /// Взятие в работу: узел стал `in_progress` по аренде.
    LeaseClaimed,
    /// Аренда закрыта с исходом `completed`: узел стал `done`.
    LeaseCompleted,
    /// Аренда закрыта с исходом `released` или `failed`: узел вернулся в работу.
    LeaseReleased,
    /// Аренда истекла и была прибрана: узел вернулся в очередь.
    LeaseExpired,
    /// Узел разблокирован снятием блокирующего ребра.
    Unblock,
    /// То же, но уборкой (`doctor --gc`).
    Gc,
    /// Восстановление из файла (импорт проекта, saga).
    Import,
    /// Восстановление из дампа базы.
    Restore,
}

impl Cause {
    /// Разбор причины из файла: чужая или испорченная причина — отказ, а не «manual» по
    /// умолчанию. Молчаливая подмена исказила бы отчёты, ради которых таблица и заведена.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "created" => Cause::Created,
            "manual" => Cause::Manual,
            "lease_claimed" => Cause::LeaseClaimed,
            "lease_completed" => Cause::LeaseCompleted,
            "lease_released" => Cause::LeaseReleased,
            "lease_expired" => Cause::LeaseExpired,
            "unblock" => Cause::Unblock,
            "gc" => Cause::Gc,
            "import" => Cause::Import,
            "restore" => Cause::Restore,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Cause::Created => "created",
            Cause::Manual => "manual",
            Cause::LeaseClaimed => "lease_claimed",
            Cause::LeaseCompleted => "lease_completed",
            Cause::LeaseReleased => "lease_released",
            Cause::LeaseExpired => "lease_expired",
            Cause::Unblock => "unblock",
            Cause::Gc => "gc",
            Cause::Import => "import",
            Cause::Restore => "restore",
        }
    }
}

/// Один факт перехода в том виде, в каком его читают отчёты и человек.
#[derive(Debug, Clone, Serialize)]
pub struct Transition {
    pub at: String,
    /// Прежний статус; `None` только у первой записи узла.
    pub from: Option<String>,
    pub to: String,
    /// Причина в машинном виде (`manual`, `lease_completed`, ...).
    pub cause: String,
    pub actor: Option<String>,
    /// Та же причина человеческим словом: агент читает машинную, человек — эту.
    pub reason: &'static str,
}

/// Человеческое имя причины.
///
/// Хранится кодом (по нему сравнивают и отбирают), а показывается словом: «lease_expired»
/// человеку ничего не говорит, а «аренда истекла» отвечает на его вопрос. Неизвестная причина
/// называется как есть, а не подменяется ближайшей понятной: подмена скрыла бы порчу данных.
pub fn reason_of(cause: &str) -> &'static str {
    match cause {
        "created" => "создан",
        "manual" => "переведён вручную",
        "lease_claimed" => "взят в работу по аренде",
        "lease_completed" => "закрыт через аренду",
        "lease_released" => "аренда сдана без завершения",
        "lease_expired" => "аренда истекла",
        "unblock" => "разблокирован снятием ребра",
        "gc" => "возвращён уборкой",
        "import" => "восстановлен из файла",
        "restore" => "восстановлен из дампа",
        _ => "причина неизвестна",
    }
}

/// Полная история статусов узла, по возрастанию времени.
///
/// Читается ровно одна таблица — `node_status_history`. Журнал (`activity_log`) для этого не
/// годится: он отвечает «что менялось», а не «когда узел стал `done`», и держится на совпадении
/// текста в местах записи. Порядок — по `at`, затем по `id`: две правки в одну миллисекунду
/// обязаны читаться в том порядке, в каком были записаны.
pub fn timeline(conn: &Connection, node_id: i64) -> Result<Vec<Transition>> {
    let mut stmt = conn.prepare(
        "SELECT at, from_status, to_status, cause, actor FROM node_status_history
         WHERE node_id = ?1 ORDER BY at, id",
    )?;
    let rows = stmt.query_map(rusqlite::params![node_id], |row| {
        let cause: String = row.get(3)?;
        Ok(Transition {
            at: row.get(0)?,
            from: row.get(1)?,
            to: row.get(2)?,
            cause: cause.clone(),
            actor: row.get(4)?,
            reason: reason_of(&cause),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Пишет факт перехода. Вызывается из той же транзакции, что и сама смена статуса.
///
/// `from` — прежний статус, `None` только у первой записи узла. Переход в тот же статус не
/// пишется: это не переход, и строка с `from == to` только мешала бы отчётам.
pub fn record(
    conn: &Connection,
    node_id: i64,
    from: Option<&str>,
    to: &str,
    cause: Cause,
    actor: &str,
) -> Result<()> {
    record_at(conn, node_id, from, to, cause, &now_iso(), actor)
}

/// То же, но с моментом из файла: импорт переносит историю с её собственными временами, а не
/// переписывает всё на «сейчас» — иначе ряд burn-up съехал бы на день импорта.
pub fn record_at(
    conn: &Connection,
    node_id: i64,
    from: Option<&str>,
    to: &str,
    cause: Cause,
    at: &str,
    actor: &str,
) -> Result<()> {
    if from == Some(to) {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO node_status_history (node_id, from_status, to_status, cause, at, actor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![node_id, from, to, cause.as_str(), at, actor],
    )?;
    Ok(())
}
