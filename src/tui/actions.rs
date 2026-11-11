//! Действия экрана против базы: аренда, отпускание, смена статуса.
//!
//! Отдельный модуль, потому что здесь заканчивается модель и начинаются правила приложения.
//! Экран не пишет SQL и не решает, можно ли перейти из статуса в статус: он зовёт те же
//! функции, что CLI и MCP (`lease::claim_as`, `lease::release`, `node::update`). Второй набор
//! правил в интерфейсе разошёлся бы с первым на первой правке — так уже случилось с фильтром в
//! слое вывода, где `--json` и таблица показывали разные списки.
//!
//! Что здесь есть от интерфейса, а не от библиотеки:
//!
//! * **Свой срок аренды.** Экран живёт минутами, а не часами, и аренда на десять минут означает,
//!   что забытый терминал освобождает узел сам, не дожидаясь, пока человек вернётся. Это же
//!   поведение истечения, которое уже проверено в библиотеке: узел возвращается, попытка
//!   закрывается.
//! * **Идентификатор сессии.** У человека в терминале ровно одна сессия работы, и ей нужен id,
//!   чтобы `next` знал, в каком проекте он занят (`TASQ_SESSION` у CLI). Тот же id передаётся в
//!   аренду, иначе экран и CLI считали бы сессии разными.

use crate::error::{Error, Result};
use crate::lease::{self, ClaimRequest, Outcome as LeaseOutcome};
use crate::node;
use crate::project::Project;
use crate::session;
use crate::status::{Kind, Status};

/// Срок аренды, взятой из экрана.
///
/// Десять минут: столько человек проводит за одним узлом. Аренда истекает сама, и узел
/// освобождается — как и любая другая аренда в этой системе. Продлевать её экран не пытается:
/// продление — отдельное решение, а не побочный эффект просмотра.
pub const LEASE_SECONDS: i64 = 600;

/// Забирает узел в работу и возвращает токен аренды.
///
/// Токен нужен, чтобы отпустить узел: без него `release` ответит `NOT_YOUR_ATTEMPT`. Экран
/// помнит его в памяти и теряет при выходе — что честно: аренда всё равно истечёт.
pub fn claim(
    conn: &mut rusqlite::Connection,
    project: &Project,
    reference: &str,
    session_id: &str,
) -> Result<String> {
    let lease = lease::claim_as(
        conn,
        project,
        reference,
        &ClaimRequest {
            holder: "tui",
            ttl_seconds: LEASE_SECONDS,
            // Чужую живую аренду не перебиваем: `--steal` в интерфейсе — это кнопка «отобрать у
            // коллеги», и без явного намерения её быть не должно.
            steal: false,
            // `--force` в `claim` относится к предусловиям (блокеры), а не к аренде. На доске
            // человек видит, что узел заблокирован, и не должен закрывать это одним нажатием.
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        },
        Some(session_id),
    )?;
    Ok(lease.token)
}

/// Закрывает аренду с выбранным исходом.
///
/// `force` — аналог `--force` у `release`: закрыть узел с открытыми детьми человек вправе, если
/// он это подтвердил. Подтверждение спрашивает экран, поэтому здесь флаг передаётся как есть.
pub fn release(
    conn: &mut rusqlite::Connection,
    project: &Project,
    token: &str,
    outcome: LeaseOutcome,
) -> Result<()> {
    lease::release(conn, project, token, outcome, None, true)?;
    Ok(())
}

/// Меняет статус узла тем же путём, что CLI.
///
/// Проверка матрицы, `requires_force` и запись в историю — всё внутри `node::update`; здесь
/// только выбор функции. Если бы экран решал сам, «через интерфейс можно, а через CLI нет»
/// стало бы обычным делом.
pub fn set_status(
    conn: &mut rusqlite::Connection,
    project: &Project,
    reference: &str,
    status: Status,
) -> Result<()> {
    node::update(
        conn,
        project,
        reference,
        node::Patch {
            status: Some(status),
            ..node::Patch::default()
        },
    )?;
    Ok(())
}

/// Идентификатор сессии экрана: из окружения, а иначе свой.
///
/// `TASQ_SESSION` уважается, чтобы экран, запущенный из той же оболочки, что и CLI, работал с
/// той же сессией. Нет переменной — генерируется свой id: две копии экрана в двух терминалах не
/// должны считаться одной сессией, иначе `next` потянет их друг за другом.
pub fn session_id() -> Result<String> {
    match session::from_env() {
        Some(id) => Ok(id),
        None => session::new_id(),
    }
}

/// Статус, в который ведёт клавиша, если матрица этот переход разрешает.
///
/// Экран показывает не свободный ввод статуса, а СПИСОК допустимых переходов: `s` открывает
/// выбор из того, что разрешает матрица (`status::allowed_next`). Здесь — проверка, что
/// запрошенный переход существует, чтобы отказ был названным и с подсказкой, а не пришёл
/// невнятным текстом из глубины SQL.
pub fn next_status(from: &str, kind: Kind, to: &str) -> Result<Status> {
    let to = Status::parse(to).ok_or_else(|| Error::InvalidValue {
        field: "status".to_string(),
        value: to.to_string(),
    })?;
    let from = Status::parse(from).ok_or_else(|| Error::InvalidValue {
        field: "status".to_string(),
        value: from.to_string(),
    })?;
    if !Status::can_transition(from, to, kind) {
        return Err(Error::InvalidTransition {
            from: from.as_str().to_string(),
            to: to.as_str().to_string(),
            kind: kind.as_str().to_string(),
        });
    }
    Ok(to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NewNode;

    fn base(tag: &str) -> (rusqlite::Connection, Project) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-act-{tag}-{nanos}"));
        let outcome = crate::db::init(&dir, "ACT", tag).expect("init");
        let conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let project = crate::project::first_project(&conn).expect("project");
        (conn, project)
    }

    fn task(conn: &mut rusqlite::Connection, project: &Project, title: &str) -> String {
        node::create(
            conn,
            project,
            NewNode {
                parent: None,
                title,
                kind: None,
                description: None,
                priority: None,
                due_date: None,
                estimated_hours: None,
                weight: None,
                source_ref: None,
                branch: None,
            },
        )
        .expect("node")
        .dot_id
    }

    /// Взятая из экрана аренда видна в базе: держатель `tui`, срок — экранный.
    #[test]
    fn a_claim_from_the_screen_is_recorded_with_its_own_holder_and_ttl() {
        let (mut conn, project) = base("claim");
        let reference = task(&mut conn, &project, "узел");
        let token = claim(&mut conn, &project, &reference, "sess-1").expect("claim");
        assert!(!token.is_empty(), "пустой токен аренды");

        let held: (String, i64) = conn
            .query_row(
                "SELECT holder, CAST((julianday(expires_at) - julianday(started_at)) * 86400 AS INTEGER)
                   FROM attempts WHERE token = ?1",
                [&token],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("строка аренды");
        assert_eq!(held.0, "tui", "держатель не назван экраном");
        assert!(
            (held.1 - LEASE_SECONDS).abs() <= 2,
            "срок аренды {} не равен экранному {LEASE_SECONDS}",
            held.1
        );
    }

    /// Аренда привязана к сессии экрана: иначе `next` не знает, в каком проекте экран занят.
    #[test]
    fn the_claim_is_bound_to_the_screen_session() {
        let (mut conn, project) = base("session");
        let reference = task(&mut conn, &project, "узел");
        claim(&mut conn, &project, &reference, "sess-42").expect("claim");
        let bound: String = conn
            .query_row(
                "SELECT s.id FROM sessions s JOIN attempts a ON a.session_id = s.id",
                [],
                |row| row.get(0),
            )
            .expect("сессия аренды");
        assert_eq!(bound, "sess-42", "аренда привязана не к той сессии");
    }

    /// Живую чужую аренду экран не перебивает: `steal` включается осознанно, а не по нажатию.
    #[test]
    fn a_live_lease_of_someone_else_is_not_taken_over() {
        let (mut conn, project) = base("steal");
        let reference = task(&mut conn, &project, "узел");
        lease::claim(
            &mut conn,
            &project,
            &reference,
            &ClaimRequest::new("другой агент"),
        )
        .expect("чужая аренда");
        let err = claim(&mut conn, &project, &reference, "sess-1")
            .expect_err("экран отобрал живую аренду");
        assert_eq!(err.code(), "ACTIVE_ATTEMPT_EXISTS", "другой отказ: {err}");
    }

    /// Завершение закрывает узел, снятие возвращает в `todo` — разница исходов видна в статусе.
    #[test]
    fn the_outcome_decides_the_node_status() {
        for (outcome, expected) in [
            (LeaseOutcome::Completed, "done"),
            (LeaseOutcome::Released, "todo"),
            (LeaseOutcome::Failed, "todo"),
        ] {
            let (mut conn, project) = base("outcome");
            let reference = task(&mut conn, &project, "узел");
            let token = claim(&mut conn, &project, &reference, "sess-1").expect("claim");
            release(&mut conn, &project, &token, outcome).expect("release");
            let status: String = conn
                .query_row(
                    "SELECT status FROM nodes WHERE path =
                       (SELECT path FROM nodes ORDER BY id DESC LIMIT 1)",
                    [],
                    |row| row.get(0),
                )
                .expect("статус узла");
            assert_eq!(status, expected, "{outcome:?} дал статус {status}");
        }
    }

    /// Отпустить чужую аренду нельзя: токен принадлежит тому, кто взял.
    #[test]
    fn releasing_without_the_token_is_refused() {
        let (mut conn, project) = base("token");
        let reference = task(&mut conn, &project, "узел");
        claim(&mut conn, &project, &reference, "sess-1").expect("claim");
        let err = release(&mut conn, &project, "чужой-токен", LeaseOutcome::Completed)
            .expect_err("отпустили чужую аренду");
        assert_eq!(err.code(), "NOT_YOUR_ATTEMPT", "другой отказ: {err}");
    }

    /// Переход берётся из матрицы статусов, а не из введённой строки: недопустимый назван.
    #[test]
    fn only_transitions_from_the_matrix_are_accepted() {
        assert_eq!(
            next_status("todo", Kind::Task, "in_progress").expect("todo → in_progress"),
            Status::InProgress
        );
        let err = next_status("todo", Kind::Task, "bogus").expect_err("мусор принят статусом");
        assert_eq!(err.code(), "INVALID_VALUE");

        // `todo → done` матрица не разрешает: между ними работа, а не одно нажатие. Отказ
        // обязан назвать допустимые переходы, иначе человек не поймёт, какого шага не хватает.
        let err = next_status("todo", Kind::Task, "done").expect_err("todo закрыт в done");
        assert_eq!(err.code(), "INVALID_TRANSITION", "другой отказ: {err}");
        assert!(
            err.hint().contains("in_progress"),
            "отказ не называет допустимые переходы: {}",
            err.hint()
        );

        // А `done → todo` матрица разрешает — это переоткрытие, и его цена не в матрице, а в
        // `requires_force`; решение о силе принимает вызывающий, а не эта функция.
        assert_eq!(
            next_status("done", Kind::Task, "todo").expect("переоткрытие разрешено матрицей"),
            Status::Todo
        );
    }
}
