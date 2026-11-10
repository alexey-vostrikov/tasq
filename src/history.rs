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
