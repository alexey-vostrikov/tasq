//! Аренды с истечением: захват, продление, освобождение (README §8).
//!
//! Статус `in_progress` не хранится как решение: он выводится из живой аренды
//! (`v_node_effective`). Поэтому умерший агент не держит узел дольше своего TTL —
//! фоновая уборка нужна только для порядка в логе и в учёте времени.

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use time::{Duration, OffsetDateTime};

use crate::activity::{self, Entry};
use crate::db::{self, iso_at, now_iso};
use crate::error::{Error, Result};
use crate::node;
use crate::project::Project;
use crate::resource;

/// Значение TTL по умолчанию и допустимые границы (README §8.1).
pub const DEFAULT_TTL: i64 = 900;
pub const MIN_TTL: i64 = 60;
pub const MAX_TTL: i64 = 28_800;
/// Предел длины чекпойнта.
pub const MAX_CHECKPOINT_BYTES: usize = 8 * 1024;

/// Исход освобождения аренды.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Работа снята без результата: узел возвращается в `todo`.
    Released,
    /// Попытка провалилась: узел возвращается в `todo`, причина остаётся в чекпойнте.
    Failed,
    /// Работа завершена: узел переходит в `done`.
    Completed,
}

impl Outcome {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "released" => Some(Outcome::Released),
            "failed" => Some(Outcome::Failed),
            "completed" => Some(Outcome::Completed),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Outcome::Released => "released",
            Outcome::Failed => "failed",
            Outcome::Completed => "completed",
        }
    }
}

/// Заявка на захват узла.
pub struct ClaimRequest<'a> {
    pub holder: &'a str,
    pub ttl_seconds: i64,
    pub steal: bool,
    pub force: bool,
    pub checkpoint: Option<&'a str>,
    /// Ресурсы, которые забираются атомарно с арендой.
    pub reservations: Vec<crate::resource::Resource>,
}

impl<'a> ClaimRequest<'a> {
    pub fn new(holder: &'a str) -> Self {
        ClaimRequest {
            holder,
            ttl_seconds: DEFAULT_TTL,
            steal: false,
            force: false,
            checkpoint: None,
            reservations: Vec::new(),
        }
    }
}

/// Выданная аренда.
#[derive(Debug, Clone, Serialize)]
pub struct Lease {
    pub attempt_id: i64,
    pub attempt_no: i64,
    pub node_id: i64,
    pub dot_id: String,
    pub holder: String,
    pub token: String,
    pub ttl_seconds: i64,
    pub started_at: String,
    pub expires_at: String,
    /// Чекпойнт предыдущей попытки, если он был.
    pub inherited_checkpoint: Option<String>,
    /// Попытка, которую пришлось вытеснить (просроченную или живую при `--steal`).
    pub superseded_attempt: Option<i64>,
    /// Ресурсы, взятые атомарно с арендой.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reservations: Vec<String>,
}

/// Токен аренды: 32 случайных байта в hex.
fn new_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| Error::InvalidValue {
        field: "token".to_string(),
        value: err.to_string(),
    })?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Проверка TTL: границы из README §8.1, никакой «мягкой» подстановки.
pub fn validate_ttl(ttl_seconds: i64) -> Result<()> {
    if (MIN_TTL..=MAX_TTL).contains(&ttl_seconds) {
        return Ok(());
    }
    Err(Error::InvalidValue {
        field: "ttl_seconds".to_string(),
        value: format!("{ttl_seconds} (allowed {MIN_TTL}..={MAX_TTL})"),
    })
}

fn validate_checkpoint(checkpoint: Option<&str>) -> Result<()> {
    if let Some(value) = checkpoint
        && value.len() > MAX_CHECKPOINT_BYTES
    {
        return Err(Error::InvalidValue {
            field: "checkpoint".to_string(),
            value: format!("{} bytes (max {MAX_CHECKPOINT_BYTES})", value.len()),
        });
    }
    Ok(())
}

struct OpenAttempt {
    id: i64,
    attempt_no: i64,
    holder: String,
    expires_at: String,
    checkpoint: Option<String>,
}

fn open_attempt(conn: &Connection, node_id: i64) -> Result<Option<OpenAttempt>> {
    Ok(conn
        .query_row(
            "SELECT id, attempt_no, holder, expires_at, checkpoint
             FROM attempts WHERE node_id = ?1 AND ended_at IS NULL",
            params![node_id],
            |row| {
                Ok(OpenAttempt {
                    id: row.get(0)?,
                    attempt_no: row.get(1)?,
                    holder: row.get(2)?,
                    expires_at: row.get(3)?,
                    checkpoint: row.get(4)?,
                })
            },
        )
        .optional()?)
}

fn is_expired(expires_at: &str, now: &str) -> bool {
    // Формат фиксированный, поэтому сравнение строк совпадает со сравнением моментов.
    expires_at <= now
}

fn blockers(conn: &Connection, node_id: i64) -> Result<Option<String>> {
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

/// Захватывает узел: одна транзакция, порядок проверок фиксирован (README §8.2).
pub fn claim(
    conn: &mut Connection,
    project: &Project,
    reference: &str,
    request: &ClaimRequest<'_>,
) -> Result<Lease> {
    validate_ttl(request.ttl_seconds)?;
    validate_checkpoint(request.checkpoint)?;
    if request.holder.trim().is_empty() {
        return Err(Error::InvalidValue {
            field: "holder".to_string(),
            value: "empty".to_string(),
        });
    }

    let node = node::resolve(conn, project, reference)?;
    // Освобождение мёртвых аренд этого проекта — на пути записи, а не только по команде
    // `doctor --gc`. Целевой узел исключён: ветка ниже вытесняет его сама и сообщает об этом
    // в `superseded`, а сборка отняла бы у неё эту информацию.
    release_expired(conn, project, Some(node.id))?;
    if matches!(
        node.stored_status(),
        crate::status::Status::Done | crate::status::Status::Cancelled
    ) {
        return Err(Error::NodeClosed {
            dot_id: node.dot_id.clone(),
            status: node.status.clone(),
        });
    }

    let now = OffsetDateTime::now_utc();
    let now_text = iso_at(now);
    let expires_at = iso_at(now + Duration::seconds(request.ttl_seconds));

    // Заявка нормализуется до транзакции: ошибка в пути не должна доходить до записи.
    let mut reservations = Vec::with_capacity(request.reservations.len());
    for resource in &request.reservations {
        reservations.push(resource::normalize(resource)?);
    }
    resource::check_limit(0, reservations.len() as i64)?;

    let tx = db::write_tx(conn)?;

    // Предыдущая попытка: живую вытесняем только по --steal, просроченную — всегда.
    let previous = open_attempt(&tx, node.id)?;
    let mut superseded = None;
    if let Some(attempt) = &previous {
        if !is_expired(&attempt.expires_at, &now_text) && !request.steal {
            return Err(Error::ActiveAttemptExists {
                dot_id: node.dot_id.clone(),
                holder: attempt.holder.clone(),
                expires_at: attempt.expires_at.clone(),
                attempt_no: attempt.attempt_no,
            });
        }
        tx.execute(
            "UPDATE attempts SET ended_at = ?1, end_reason = 'superseded' WHERE id = ?2",
            params![now_text, attempt.id],
        )?;
        tx.execute(
            "UPDATE reservations SET released_at = ?1
             WHERE attempt_id = ?2 AND released_at IS NULL",
            params![now_text, attempt.id],
        )?;
        superseded = Some(attempt.id);
    }

    // Чекпойнт берётся у последней закрытой попытки — в том числе у только что вытесненной.
    let inherited = match previous {
        Some(attempt) => attempt.checkpoint,
        None => tx
            .query_row(
                "SELECT checkpoint FROM attempts
                 WHERE node_id = ?1 AND ended_at IS NOT NULL AND checkpoint IS NOT NULL
                 ORDER BY attempt_no DESC LIMIT 1",
                params![node.id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten(),
    };

    if let Some(paths) = blockers(&tx, node.id)?
        && !request.force
    {
        return Err(Error::UnmetPrerequisites {
            dot_id: node.dot_id.clone(),
            blockers: paths,
        });
    }

    // Порядок проверок — по README §8.2: занятые файлы отказывают после аренды и блокеров,
    // чтобы сообщение было про то, что мешает раньше по смыслу.
    if let Some((existing, holder)) = resource::find_conflict(&tx, &reservations)? {
        return Err(Error::ResourceReservationConflict {
            kind: existing.kind.as_str().to_string(),
            value: existing.value,
            holder: holder.holder,
            dot_id: holder.dot_id,
            expires_at: holder.expires_at,
        });
    }

    let attempt_no: i64 = tx.query_row(
        "SELECT COALESCE(MAX(attempt_no), 0) + 1 FROM attempts WHERE node_id = ?1",
        params![node.id],
        |row| row.get(0),
    )?;
    let token = new_token()?;

    tx.execute(
        "INSERT INTO attempts (node_id, attempt_no, holder, token, ttl_seconds,
                               started_at, expires_at, checkpoint)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            node.id,
            attempt_no,
            request.holder,
            token,
            request.ttl_seconds,
            now_text,
            expires_at,
            request.checkpoint,
        ],
    )
    .map_err(|err| match err {
        // Гонка двух процессов: уникальный индекс живых попыток решает исход.
        rusqlite::Error::SqliteFailure(_, _) => Error::ActiveAttemptExists {
            dot_id: node.dot_id.clone(),
            holder: "another process".to_string(),
            expires_at: "-".to_string(),
            attempt_no,
        },
        other => Error::from(other),
    })?;
    let attempt_id = tx.last_insert_rowid();

    if !reservations.is_empty() {
        resource::insert_all(&tx, attempt_id, node.id, &reservations)?;
        let listed = reservations
            .iter()
            .map(|r| r.key())
            .collect::<Vec<_>>()
            .join(", ");
        activity::log(
            &tx,
            &Entry {
                project_id: project.id,
                node_id: Some(node.id),
                entity_type: "node",
                entity_id: node.id,
                action: "reserve",
                field_name: None,
                old_value: None,
                new_value: Some(listed.as_str()),
                summary: Some(node.dot_id.as_str()),
                actor: request.holder,
            },
        )?;
    }

    tx.execute(
        "UPDATE nodes SET status = 'in_progress', updated_at = ?1 WHERE id = ?2",
        params![now_text, node.id],
    )?;
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id: Some(node.id),
            entity_type: "node",
            entity_id: node.id,
            action: "claim",
            field_name: Some("status"),
            old_value: Some(node.status.as_str()),
            new_value: Some("in_progress"),
            summary: Some(node.dot_id.as_str()),
            actor: request.holder,
        },
    )?;

    tx.commit()?;

    Ok(Lease {
        attempt_id,
        attempt_no,
        node_id: node.id,
        dot_id: node.dot_id,
        holder: request.holder.to_string(),
        token,
        ttl_seconds: request.ttl_seconds,
        started_at: now_text,
        expires_at,
        inherited_checkpoint: inherited,
        superseded_attempt: superseded,
        reservations: reservations.iter().map(|r| r.key()).collect(),
    })
}

/// Продлевает аренду по токену.
pub fn renew(
    conn: &mut Connection,
    project: &Project,
    token: &str,
    ttl_seconds: i64,
) -> Result<Lease> {
    validate_ttl(ttl_seconds)?;
    let now = OffsetDateTime::now_utc();
    let now_text = iso_at(now);
    let expires_at = iso_at(now + Duration::seconds(ttl_seconds));

    let attempt = load_attempt(conn, token)?;
    if attempt.ended_at.is_some() {
        return Err(Error::NotYourAttempt);
    }
    if is_expired(&attempt.expires_at, &now_text) {
        return Err(Error::LeaseExpired {
            attempt_id: attempt.id,
        });
    }

    let tx = db::write_tx(conn)?;
    tx.execute(
        "UPDATE attempts SET expires_at = ?1, renewed_at = ?2, ttl_seconds = ?3 WHERE id = ?4",
        params![expires_at, now_text, ttl_seconds, attempt.id],
    )?;
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id: Some(attempt.node_id),
            entity_type: "node",
            entity_id: attempt.node_id,
            action: "renew",
            field_name: Some("expires_at"),
            old_value: Some(attempt.expires_at.as_str()),
            new_value: Some(expires_at.as_str()),
            summary: Some(attempt.dot_id.as_str()),
            actor: attempt.holder.as_str(),
        },
    )?;
    tx.commit()?;

    let reservations = resource::attempt_reservations(conn, attempt.id)?
        .iter()
        .map(|r| r.key())
        .collect();

    Ok(Lease {
        attempt_id: attempt.id,
        attempt_no: attempt.attempt_no,
        node_id: attempt.node_id,
        dot_id: attempt.dot_id,
        holder: attempt.holder,
        token: attempt.token,
        ttl_seconds,
        started_at: attempt.started_at,
        expires_at,
        inherited_checkpoint: attempt.checkpoint,
        superseded_attempt: None,
        reservations,
    })
}

struct StoredAttempt {
    id: i64,
    node_id: i64,
    attempt_no: i64,
    holder: String,
    token: String,
    started_at: String,
    expires_at: String,
    ended_at: Option<String>,
    checkpoint: Option<String>,
    dot_id: String,
}

/// Узел, на который у вызывающего есть живая аренда с этим токеном.
///
/// `None` — токен неизвестен, закрыт, просрочен или принадлежит другому проекту.
/// Нужен там, где правка узла требует аренды: наличие строки `token` в запросе
/// само по себе правом не является.
pub fn live_node(conn: &Connection, project: &Project, token: &str) -> Result<Option<i64>> {
    let now_text = now_iso();
    Ok(conn
        .query_row(
            "SELECT a.node_id FROM attempts a
             JOIN nodes n ON n.id = a.node_id
             WHERE a.token = ?1 AND a.ended_at IS NULL AND a.expires_at > ?2
               AND n.project_id = ?3 AND n.deleted_at IS NULL",
            params![token, now_text, project.id],
            |row| row.get(0),
        )
        .optional()?)
}

fn load_attempt(conn: &Connection, token: &str) -> Result<StoredAttempt> {
    conn.query_row(
        "SELECT a.id, a.node_id, a.attempt_no, a.holder, a.token, a.started_at, a.expires_at,
                a.ended_at, a.checkpoint, n.path, p.key
         FROM attempts a
         JOIN nodes n ON n.id = a.node_id
         JOIN projects p ON p.id = n.project_id
         WHERE a.token = ?1",
        params![token],
        |row| {
            let path: String = row.get(9)?;
            let key: String = row.get(10)?;
            Ok(StoredAttempt {
                id: row.get(0)?,
                node_id: row.get(1)?,
                attempt_no: row.get(2)?,
                holder: row.get(3)?,
                token: row.get(4)?,
                started_at: row.get(5)?,
                expires_at: row.get(6)?,
                ended_at: row.get(7)?,
                checkpoint: row.get(8)?,
                dot_id: format!("{key}-{path}"),
            })
        },
    )
    .optional()?
    .ok_or(Error::NotYourAttempt)
}

/// Освобождает аренду: закрывает попытку, снимает резервы, пишет статус узла.
pub fn release(
    conn: &mut Connection,
    project: &Project,
    token: &str,
    outcome: Outcome,
    checkpoint: Option<&str>,
    force: bool,
) -> Result<node::Node> {
    validate_checkpoint(checkpoint)?;
    let now_text = now_iso();
    let attempt = load_attempt(conn, token)?;
    if attempt.ended_at.is_some() {
        return Err(Error::NotYourAttempt);
    }

    let key = attempt
        .dot_id
        .split_once('-')
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_else(|| attempt.dot_id.clone());
    let node = node::resolve(conn, project, &format!("#{}", attempt.node_id))
        .or_else(|_| node::resolve(conn, project, &key))?;
    if outcome == Outcome::Completed && node.stored_status() != crate::status::Status::Done {
        let unfinished: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes
             WHERE parent_id = ?1 AND deleted_at IS NULL AND status NOT IN ('done', 'cancelled')",
            params![node.id],
            |row| row.get(0),
        )?;
        if unfinished > 0 && !force {
            return Err(Error::ChildrenUnfinished {
                dot_id: node.dot_id.clone(),
                unfinished,
            });
        }
        if let Some(paths) = blockers(conn, node.id)?
            && !force
        {
            return Err(Error::UnmetPrerequisites {
                dot_id: node.dot_id.clone(),
                blockers: paths,
            });
        }
    }

    let tx = db::write_tx(conn)?;
    tx.execute(
        "UPDATE attempts SET ended_at = ?1, end_reason = ?2, checkpoint = COALESCE(?3, checkpoint)
         WHERE id = ?4",
        params![now_text, outcome.as_str(), checkpoint, attempt.id],
    )?;
    tx.execute(
        "UPDATE reservations SET released_at = ?1
         WHERE attempt_id = ?2 AND released_at IS NULL",
        params![now_text, attempt.id],
    )?;

    let target = match outcome {
        Outcome::Completed => crate::status::Status::Done,
        Outcome::Released | Outcome::Failed => crate::status::Status::Todo,
    };
    tx.execute(
        "UPDATE nodes SET status = ?1, updated_at = ?2 WHERE id = ?3",
        params![target.as_str(), now_text, node.id],
    )?;
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id: Some(node.id),
            entity_type: "node",
            entity_id: node.id,
            action: outcome.as_str(),
            field_name: Some("status"),
            old_value: Some(node.status.as_str()),
            new_value: Some(target.as_str()),
            summary: Some(node.dot_id.as_str()),
            actor: attempt.holder.as_str(),
        },
    )?;
    tx.commit()?;
    node::resolve(conn, project, &format!("#{}", node.id))
}

/// Закрывает просроченные попытки и снимает их резервы (README §8.3).
///
/// Явная команда `doctor --gc`. Тот же механизм работает и сам — [`release_expired`]
/// вызывается на путях записи, поэтому аренды мёртвых сессий не накапливаются.
pub fn gc(conn: &mut Connection, project: &Project) -> Result<usize> {
    release_expired(conn, project, None)
}

/// Автоматическое освобождение узлов с истёкшей арендой.
///
/// Зачем это нужно, если представление и так считает истёкшую аренду недействительной:
/// представление чинит ЧТЕНИЕ, а хранимый `nodes.status` остаётся `in_progress`, попытка —
/// открытой, резервы — не снятыми. Пока их никто не закрыл, `doctor` показывает «просроченные
/// попытки», `time` считает часы по незакрытой аренде, а `uq_attempts_live` держит строку,
/// которую никто не обслуживает. Поэтому сборка мусора идёт на путях записи: аренды мёртвых
/// сессий убираются тогда, когда система всё равно пишет.
///
/// `exclude_node` — узел, который вызывающий разбирает сам. Нужен `claim`-у: он вытесняет
/// истёкшую попытку целевого узла отдельной веткой и сообщает об этом в `superseded`, а
/// предварительная сборка отняла бы у него эту информацию.
///
/// Возвращает число закрытых попыток.
pub fn release_expired(
    conn: &mut Connection,
    project: &Project,
    exclude_node: Option<i64>,
) -> Result<usize> {
    let now_text = now_iso();
    let expired: Vec<(i64, i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT a.id, a.node_id, a.holder FROM attempts a
             JOIN nodes n ON n.id = a.node_id
             WHERE n.project_id = ?1 AND a.ended_at IS NULL AND a.expires_at <= ?2
               AND (?3 IS NULL OR a.node_id <> ?3)",
        )?;
        stmt.query_map(params![project.id, now_text, exclude_node], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if expired.is_empty() {
        return Ok(0);
    }

    let tx = db::write_tx(conn)?;
    for (attempt_id, node_id, holder) in &expired {
        tx.execute(
            "UPDATE attempts SET ended_at = ?1, end_reason = 'expired' WHERE id = ?2",
            params![now_text, attempt_id],
        )?;
        tx.execute(
            "UPDATE reservations SET released_at = ?1
             WHERE attempt_id = ?2 AND released_at IS NULL",
            params![now_text, attempt_id],
        )?;
        // Узел возвращается в очередь, только если его не закрыли руками за это время.
        //
        // Статус выбирается по виду узла: у эпика `todo` не бывает (§7), его место — `planned`.
        // То же правило, что в `status::allowed_for`; согласованность проверяется тестом.
        tx.execute(
            "UPDATE nodes SET status = CASE WHEN kind = 'epic' THEN 'planned' ELSE 'todo' END,
                              updated_at = ?1
             WHERE id = ?2 AND status = 'in_progress'
               AND NOT EXISTS (SELECT 1 FROM attempts WHERE node_id = ?2 AND ended_at IS NULL)",
            params![now_text, node_id],
        )?;
        activity::log(
            &tx,
            &Entry {
                project_id: project.id,
                node_id: Some(*node_id),
                entity_type: "node",
                entity_id: *node_id,
                action: "expire",
                field_name: Some("expires_at"),
                old_value: None,
                new_value: Some(now_text.as_str()),
                summary: Some("lease expired"),
                actor: holder.as_str(),
            },
        )?;
    }
    tx.commit()?;
    Ok(expired.len())
}

/// Живые аренды проекта: для дашборда и `ls --leased`.
#[derive(Debug, Clone, Serialize)]
pub struct Holding {
    pub dot_id: String,
    pub holder: String,
    pub attempt_no: i64,
    pub started_at: String,
    pub expires_at: String,
    pub ttl_seconds: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reservations: Vec<String>,
}

pub fn holdings(conn: &Connection, project: &Project) -> Result<Vec<Holding>> {
    let now_text = now_iso();
    let mut stmt = conn.prepare(
        "SELECT n.path, a.holder, a.attempt_no, a.started_at, a.expires_at, a.ttl_seconds, a.id
         FROM attempts a
         JOIN nodes n ON n.id = a.node_id
         WHERE n.project_id = ?1 AND a.ended_at IS NULL AND a.expires_at > ?2
         ORDER BY a.expires_at",
    )?;
    // Строки собираются вместе с id попытки: по нему досчитываются резервы.
    let rows: Vec<(i64, Holding)> = stmt
        .query_map(params![project.id, now_text], |row| {
            let path: String = row.get(0)?;
            let attempt_id: i64 = row.get(6)?;
            Ok((
                attempt_id,
                Holding {
                    dot_id: project.dot_id(&path),
                    holder: row.get(1)?,
                    attempt_no: row.get(2)?,
                    started_at: row.get(3)?,
                    expires_at: row.get(4)?,
                    ttl_seconds: row.get(5)?,
                    reservations: Vec::new(),
                },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut holdings = Vec::with_capacity(rows.len());
    for (attempt_id, mut holding) in rows {
        holding.reservations = resource::attempt_reservations(conn, attempt_id)?
            .iter()
            .map(|r| r.key())
            .collect();
        holdings.push(holding);
    }
    Ok(holdings)
}

/// Добавляет резервы живой попытке по токену.
pub fn reserve(
    conn: &mut Connection,
    project: &Project,
    token: &str,
    requested: &[resource::Resource],
) -> Result<Vec<String>> {
    if requested.is_empty() {
        return Err(Error::InvalidValue {
            field: "reservation".to_string(),
            value: "nothing requested".to_string(),
        });
    }
    let mut normalized = Vec::with_capacity(requested.len());
    for item in requested {
        normalized.push(resource::normalize(item)?);
    }

    let now_text = now_iso();
    let attempt = load_attempt(conn, token)?;
    if attempt.ended_at.is_some() {
        return Err(Error::NotYourAttempt);
    }
    if is_expired(&attempt.expires_at, &now_text) {
        return Err(Error::LeaseExpired {
            attempt_id: attempt.id,
        });
    }

    let existing = resource::attempt_reservations(conn, attempt.id)?;
    // Уже взятое тем же исполнителем не дублируется.
    let fresh: Vec<resource::Resource> = normalized
        .into_iter()
        .filter(|want| !existing.contains(want))
        .collect();
    resource::check_limit(existing.len() as i64, fresh.len() as i64)?;

    let tx = db::write_tx(conn)?;
    if let Some((held, holder)) = resource::find_conflict_excluding(&tx, &fresh, attempt.id)? {
        return Err(Error::ResourceReservationConflict {
            kind: held.kind.as_str().to_string(),
            value: held.value,
            holder: holder.holder,
            dot_id: holder.dot_id,
            expires_at: holder.expires_at,
        });
    }
    resource::insert_all(&tx, attempt.id, attempt.node_id, &fresh)?;
    if !fresh.is_empty() {
        let listed = fresh.iter().map(|r| r.key()).collect::<Vec<_>>().join(", ");
        activity::log(
            &tx,
            &Entry {
                project_id: project.id,
                node_id: Some(attempt.node_id),
                entity_type: "node",
                entity_id: attempt.node_id,
                action: "reserve",
                field_name: None,
                old_value: None,
                new_value: Some(listed.as_str()),
                summary: Some(attempt.dot_id.as_str()),
                actor: attempt.holder.as_str(),
            },
        )?;
    }
    tx.commit()?;

    Ok(resource::attempt_reservations(conn, attempt.id)?
        .iter()
        .map(|r| r.key())
        .collect())
}

/// Снимает конкретные резервы живой попытки; `None` — снять все.
pub fn unreserve(
    conn: &mut Connection,
    project: &Project,
    token: &str,
    requested: Option<&[resource::Resource]>,
) -> Result<usize> {
    let attempt = load_attempt(conn, token)?;
    if attempt.ended_at.is_some() {
        return Err(Error::NotYourAttempt);
    }

    let tx = db::write_tx(conn)?;
    let removed = match requested {
        None => resource::release_all(&tx, attempt.id)?,
        Some(items) => {
            let mut total = 0;
            for item in items {
                let normalized = resource::normalize(item)?;
                total += tx.execute(
                    "UPDATE reservations SET released_at = ?1
                     WHERE attempt_id = ?2 AND released_at IS NULL
                       AND kind = ?3 AND value = ?4",
                    params![
                        now_iso(),
                        attempt.id,
                        normalized.kind.as_str(),
                        normalized.value
                    ],
                )?;
            }
            total
        }
    };
    if removed > 0 {
        let removed_text = removed.to_string();
        activity::log(
            &tx,
            &Entry {
                project_id: project.id,
                node_id: Some(attempt.node_id),
                entity_type: "node",
                entity_id: attempt.node_id,
                action: "unreserve",
                field_name: None,
                old_value: None,
                new_value: Some(removed_text.as_str()),
                summary: Some(attempt.dot_id.as_str()),
                actor: attempt.holder.as_str(),
            },
        )?;
    }
    tx.commit()?;
    Ok(removed)
}
