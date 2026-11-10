//! Зависимости между узлами любого уровня (README §10).
//!
//! Рёбра хранятся между узлами, а не между уровнями иерархии: подзадача в одной ветви
//! может зависеть от подзадачи в другой. Блокирующим считается только `blocks`
//! и его транзитивное замыкание; остальные виды — связи для чтения.

use std::collections::{HashMap, HashSet, VecDeque};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::activity::{self, Entry};
use crate::db::now_iso;
use crate::error::{Error, Result};
use crate::history;
use crate::node::{self, Node};
use crate::project::Project;

/// Вид связи.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Жёсткая последовательность: второй узел ждёт первый.
    Blocks,
    /// Связь без порядка.
    Related,
    /// Дубликат: тот же смысл в другом месте.
    Duplicates,
    /// Найдено по ходу работы над другим узлом.
    DiscoveredFrom,
    /// Отделено от другого узла.
    SplitFrom,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Blocks => "blocks",
            Kind::Related => "related",
            Kind::Duplicates => "duplicates",
            Kind::DiscoveredFrom => "discovered_from",
            Kind::SplitFrom => "split_from",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "blocks" => Some(Kind::Blocks),
            "related" => Some(Kind::Related),
            "duplicates" => Some(Kind::Duplicates),
            "discovered_from" => Some(Kind::DiscoveredFrom),
            "split_from" => Some(Kind::SplitFrom),
            _ => None,
        }
    }

    /// Только `blocks` удерживает работу, остальные виды — пометки.
    pub fn is_blocking(self) -> bool {
        matches!(self, Kind::Blocks)
    }
}

/// Ребро в выдаче.
#[derive(Debug, Clone, Serialize)]
pub struct Edge {
    pub kind: String,
    pub from: String,
    pub to: String,
    pub created_at: String,
    pub created_by: Option<String>,
}

fn dot_of(project: &Project, path: &str) -> String {
    project.dot_id(path)
}

/// Создаёт ребро `from -> to`.
///
/// Валидация идёт до записи и до транзакции: ошибка не должна оставлять следов.
pub fn link(
    conn: &mut Connection,
    project: &Project,
    from_ref: &str,
    to_ref: &str,
    kind: Kind,
    actor: &str,
) -> Result<Edge> {
    let from = resolve_for_link(conn, project, from_ref)?;
    let to = resolve_for_link(conn, project, to_ref)?;

    if from.id == to.id {
        return Err(Error::SelfEdge {
            dot_id: from.dot_id.clone(),
        });
    }
    if from.project_id != to.project_id {
        return Err(Error::CrossProjectEdge {
            from: from.dot_id,
            to: to.dot_id,
        });
    }

    // Проверки на цикл идут ВНУТРИ транзакции, а не до неё. Причина одна и та же у всех
    // подобных проверок в проекте: между проверкой и записью другой писатель мог успеть
    // добавить встречное ребро, и оба прошли бы проверку на старом графе.
    //
    // Сценарий не теоретический: две задачи `A блокирует B` и `B блокирует A`, добавленные
    // одновременно двумя сессиями, дают замкнутую пару — ровно то, что README §5.3 запрещает.
    // Внутри `BEGIN IMMEDIATE` второй писатель видит уже вставленное первое ребро.
    let tx = crate::db::write_tx(conn)?;

    if kind.is_blocking() {
        // Пути перечитываются из соединения уже под блокировкой: узел могли перенести.
        let from_path: String = tx.query_row(
            "SELECT path FROM nodes WHERE id = ?1",
            params![from.id],
            |row| row.get(0),
        )?;
        let to_path: String = tx.query_row(
            "SELECT path FROM nodes WHERE id = ?1",
            params![to.id],
            |row| row.get(0),
        )?;
        let same_branch = to_path.starts_with(&format!("{from_path}."))
            || from_path.starts_with(&format!("{to_path}."));
        if same_branch {
            return Err(Error::AncestorBlockEdge {
                dot_id: from.dot_id.clone(),
                other: to.dot_id.clone(),
            });
        }
        // Цепочка ищется от `to` к `from`: если она есть, новое ребро замкнёт круг.
        // Поиск транзитивный, поэтому ловится любая длина — и пара из двух задач, и цепочка
        // из пяти, и круг, замкнутый через третью ветку.
        if let Some(chain) = find_path(&tx, project, to.id, from.id)? {
            // Цепочка восстановлена от `to` до `from`; круг читается как from -> to -> … -> from.
            let mut path: Vec<String> = chain.into_iter().map(|(_, dot)| dot).collect();
            path.insert(0, from.dot_id.clone());
            return Err(Error::CycleDetected {
                from: from.dot_id.clone(),
                to: to.dot_id.clone(),
                path: path.join(" -> "),
            });
        }
    }

    let now = now_iso();
    // Дубликат — не ошибка: повторная заявка того же ребра идемпотентна.
    tx.execute(
        "INSERT INTO edges (from_node, to_node, kind, created_at, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(from_node, to_node, kind) DO NOTHING",
        params![from.id, to.id, kind.as_str(), now, actor],
    )?;
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id: Some(to.id),
            entity_type: "edge",
            entity_id: to.id,
            action: "link",
            field_name: Some("kind"),
            old_value: None,
            new_value: Some(kind.as_str()),
            summary: Some(&format!("{} -> {}", from.dot_id, to.dot_id)),
            actor,
        },
    )?;
    tx.commit()?;

    Ok(Edge {
        kind: kind.as_str().to_string(),
        from: from.dot_id,
        to: to.dot_id,
        created_at: now,
        created_by: Some(actor.to_string()),
    })
}

/// Узел в объёме, нужном для рёбер; `project_id` берётся из строки, а не из ссылки,
/// чтобы поймать попытку связать узлы разных проектов.
fn resolve_for_link(conn: &Connection, project: &Project, reference: &str) -> Result<Node> {
    match node::resolve(conn, project, reference) {
        Ok(node) => Ok(node),
        Err(Error::NodeNotFound(_)) => {
            // ссылка вида `#id` может указывать в другой проект: различаем эти случаи
            if let Some(rest) = reference.strip_prefix('#')
                && let Ok(id) = rest.parse::<i64>()
                && let Some((path, key, node_project)) = conn
                    .query_row(
                        "SELECT n.path, p.key, n.project_id FROM nodes n
                         JOIN projects p ON p.id = n.project_id WHERE n.id = ?1",
                        params![id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, i64>(2)?,
                            ))
                        },
                    )
                    .optional()?
                && node_project != project.id
            {
                return Err(Error::CrossProjectEdge {
                    from: project.dot_id("?"),
                    to: format!("{key}-{path}"),
                });
            }
            Err(Error::NodeNotFound(reference.to_string()))
        }
        Err(other) => Err(other),
    }
}

/// Путь по рёбрам `blocks` от `start` до `goal` (без старта), если он есть.
fn find_path(
    conn: &Connection,
    project: &Project,
    start: i64,
    goal: i64,
) -> Result<Option<Vec<(i64, String)>>> {
    let mut queue: VecDeque<i64> = VecDeque::new();
    let mut came_from: HashMap<i64, i64> = HashMap::new();
    let mut seen: HashSet<i64> = HashSet::new();
    queue.push_back(start);
    seen.insert(start);

    while let Some(current) = queue.pop_front() {
        let mut stmt = conn.prepare(
            "SELECT e.to_node FROM edges e
             JOIN nodes n ON n.id = e.to_node
             WHERE e.from_node = ?1 AND e.kind = 'blocks' AND n.deleted_at IS NULL",
        )?;
        let next: Vec<i64> = stmt
            .query_map(params![current], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for neighbour in next {
            if !seen.insert(neighbour) {
                continue;
            }
            came_from.insert(neighbour, current);
            if neighbour == goal {
                let mut chain = Vec::new();
                let mut cursor = goal;
                while cursor != start {
                    chain.push((cursor, dot_of(project, &path_of(conn, cursor)?)));
                    cursor = came_from[&cursor];
                }
                // старт тоже часть пути: иначе круг в сообщении получается разорванным
                chain.push((start, dot_of(project, &path_of(conn, start)?)));
                chain.reverse();
                return Ok(Some(chain));
            }
            queue.push_back(neighbour);
        }
    }
    Ok(None)
}

fn path_of(conn: &Connection, id: i64) -> Result<String> {
    Ok(
        conn.query_row("SELECT path FROM nodes WHERE id = ?1", params![id], |row| {
            row.get(0)
        })?,
    )
}

/// Снимает ребро. Возвращает `true`, если ребро было.
pub fn unlink(
    conn: &mut Connection,
    project: &Project,
    from_ref: &str,
    to_ref: &str,
    kind: Kind,
    actor: &str,
) -> Result<bool> {
    let from = node::resolve(conn, project, from_ref)?;
    let to = node::resolve(conn, project, to_ref)?;

    let tx = crate::db::write_tx(conn)?;
    let removed = tx.execute(
        "DELETE FROM edges WHERE from_node = ?1 AND to_node = ?2 AND kind = ?3",
        params![from.id, to.id, kind.as_str()],
    )?;
    if removed == 0 {
        tx.commit()?;
        return Ok(false);
    }
    activity::log(
        &tx,
        &Entry {
            project_id: project.id,
            node_id: Some(to.id),
            entity_type: "edge",
            entity_id: to.id,
            action: "unlink",
            field_name: Some("kind"),
            old_value: Some(kind.as_str()),
            new_value: None,
            summary: Some(&format!("{} -/-> {}", from.dot_id, to.dot_id)),
            actor,
        },
    )?;

    // Снятие последнего блокера возвращает узел в работу: статус хранимый, поэтому
    // его надо перевести явно, иначе `blocked` останется в колонке навсегда.
    // Ждёт всегда `from`, поэтому проверяется именно он.
    if kind.is_blocking() {
        let remaining: i64 = tx.query_row(
            "SELECT COUNT(*) FROM edges e
             JOIN nodes b ON b.id = e.to_node
             WHERE e.from_node = ?1 AND e.kind = 'blocks' AND b.deleted_at IS NULL
               AND b.status NOT IN ('done', 'cancelled')",
            params![from.id],
            |row| row.get(0),
        )?;
        if remaining == 0 {
            let changed = tx.execute(
                "UPDATE nodes SET status = 'todo', updated_at = ?1
                 WHERE id = ?2 AND status = 'blocked' AND deleted_at IS NULL",
                params![now_iso(), from.id],
            )?;
            if changed > 0 {
                history::record(
                    &tx,
                    from.id,
                    Some("blocked"),
                    "todo",
                    history::Cause::Unblock,
                    actor,
                )?;
                activity::log(
                    &tx,
                    &Entry {
                        project_id: project.id,
                        node_id: Some(from.id),
                        entity_type: "node",
                        entity_id: from.id,
                        action: "unblock",
                        field_name: Some("status"),
                        old_value: Some("blocked"),
                        new_value: Some("todo"),
                        summary: Some(from.dot_id.as_str()),
                        actor,
                    },
                )?;
            }
        }
    }
    tx.commit()?;
    Ok(true)
}

/// Все рёбра проекта.
pub fn list(conn: &Connection, project: &Project, kind: Option<Kind>) -> Result<Vec<Edge>> {
    let mut sql = String::from(
        "SELECT e.kind, f.path, t.path, e.created_at, e.created_by
         FROM edges e
         JOIN nodes f ON f.id = e.from_node
         JOIN nodes t ON t.id = e.to_node
         WHERE f.project_id = ?1 AND f.deleted_at IS NULL AND t.deleted_at IS NULL",
    );
    if kind.is_some() {
        sql.push_str(" AND e.kind = ?2");
    }
    sql.push_str(" ORDER BY f.path, t.path");
    let mut stmt = conn.prepare(&sql)?;
    let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Edge> {
        let from: String = row.get(1)?;
        let to: String = row.get(2)?;
        Ok(Edge {
            kind: row.get(0)?,
            from,
            to,
            created_at: row.get(3)?,
            created_by: row.get(4)?,
        })
    };
    let rows: Vec<Edge> = match kind {
        Some(kind) => stmt
            .query_map(params![project.id, kind.as_str()], map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        None => stmt
            .query_map(params![project.id], map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?,
    };
    Ok(rows
        .into_iter()
        .map(|mut edge| {
            edge.from = project.dot_id(&edge.from);
            edge.to = project.dot_id(&edge.to);
            edge
        })
        .collect())
}

/// Окрестность узла в графе.
#[derive(Debug, Clone, Serialize)]
pub struct Graph {
    pub node: String,
    /// Кого узел блокирует (его завершение разблокирует их).
    pub blocks: Vec<Neighbour>,
    /// Что держит узел.
    pub blocked_by: Vec<Neighbour>,
    /// Транзитивные блокеры, ближайшие первыми.
    pub transitive_blockers: Vec<Neighbour>,
    /// Сколько узлов ждут этого узла прямо и через цепочки.
    pub unblocks_total: i64,
    /// Самая длинная цепочка «этот узел блокирует … блокирует …».
    pub critical_chain: Vec<String>,
    /// Прочие связи (related, duplicates, …).
    pub related: Vec<Edge>,
}

/// Сосед в графе: dot-ID, заголовок, статус и глубина по отношению к исходному узлу.
#[derive(Debug, Clone, Serialize)]
pub struct Neighbour {
    pub dot_id: String,
    pub title: String,
    pub status: String,
    pub distance: i64,
    /// Узел вне дерева исходного проекта-ветви — только для чтения.
    pub terminal: bool,
}

fn neighbours(
    conn: &Connection,
    project: &Project,
    node_id: i64,
    kind: Kind,
) -> Result<Vec<Neighbour>> {
    let mut stmt = conn.prepare(
        "SELECT n.path, n.title, n.status FROM edges e
         JOIN nodes n ON n.id = e.to_node
         WHERE e.from_node = ?1 AND e.kind = ?2 AND n.deleted_at IS NULL
         ORDER BY n.path",
    )?;
    let rows = stmt.query_map(params![node_id, kind.as_str()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (path, title, status) = row?;
        out.push(Neighbour {
            dot_id: dot_of(project, &path),
            title,
            status: status.clone(),
            distance: 1,
            terminal: matches!(status.as_str(), "done" | "cancelled"),
        });
    }
    Ok(out)
}

/// Окрестность узла с транзитивными блокерами и цепочкой зависимых.
///
/// Направление ребра в БД: `from -> to` означает «from ждёт to» (to блокирует from).
/// Так его читают `v_node_effective`, `node.rs` и `lease.rs`, поэтому здесь
/// `blocked_by` — исходящие рёбра узла, а `blocks` — входящие.
pub fn graph(conn: &Connection, project: &Project, reference: &str, depth: i64) -> Result<Graph> {
    let node = node::resolve(conn, project, reference)?;
    let depth = depth.clamp(1, 32);

    let blocked_by = neighbours(conn, project, node.id, Kind::Blocks)?;
    let blocks = incoming(conn, project, node.id, Kind::Blocks)?;

    // Транзитивные блокеры: обход в ширину по исходящим рёбрам `blocks`.
    let mut transitive: Vec<Neighbour> = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    seen.insert(node.id);
    let mut frontier: Vec<(i64, i64)> = vec![(node.id, 0)];
    while let Some((current, distance)) = frontier.pop() {
        if distance >= depth {
            continue;
        }
        let mut stmt = conn.prepare(
            "SELECT b.id, b.path, b.title, b.status FROM edges e
             JOIN nodes b ON b.id = e.to_node
             WHERE e.from_node = ?1 AND e.kind = 'blocks' AND b.deleted_at IS NULL",
        )?;
        let rows = stmt.query_map(params![current], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (id, path, title, status) = row?;
            if !seen.insert(id) {
                continue;
            }
            transitive.push(Neighbour {
                dot_id: dot_of(project, &path),
                title,
                status: status.clone(),
                distance: distance + 1,
                terminal: matches!(status.as_str(), "done" | "cancelled"),
            });
            frontier.push((id, distance + 1));
        }
    }

    let unblocks_total = dependents(conn, node.id)?.len() as i64;
    let critical_chain = longest_chain(conn, project, node.id, depth)?;

    let mut related = Vec::new();
    for kind in [
        Kind::Related,
        Kind::Duplicates,
        Kind::DiscoveredFrom,
        Kind::SplitFrom,
    ] {
        let mut edges = list(conn, project, Some(kind))?;
        related.append(&mut edges);
    }
    let node_dot = node.dot_id.clone();
    related.retain(|edge| edge.from == node_dot || edge.to == node_dot);

    Ok(Graph {
        node: node_dot,
        blocks,
        blocked_by,
        transitive_blockers: transitive,
        unblocks_total,
        critical_chain,
        related,
    })
}

fn incoming(
    conn: &Connection,
    project: &Project,
    node_id: i64,
    kind: Kind,
) -> Result<Vec<Neighbour>> {
    let mut stmt = conn.prepare(
        "SELECT n.path, n.title, n.status FROM edges e
         JOIN nodes n ON n.id = e.from_node
         WHERE e.to_node = ?1 AND e.kind = ?2 AND n.deleted_at IS NULL
         ORDER BY n.path",
    )?;
    let rows = stmt.query_map(params![node_id, kind.as_str()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (path, title, status) = row?;
        out.push(Neighbour {
            dot_id: dot_of(project, &path),
            title,
            status: status.clone(),
            distance: 1,
            terminal: matches!(status.as_str(), "done" | "cancelled"),
        });
    }
    Ok(out)
}

/// Все узлы, которые ждут этот (транзитивно).
fn dependents(conn: &Connection, node_id: i64) -> Result<Vec<i64>> {
    let mut out: Vec<i64> = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    let mut queue: VecDeque<i64> = VecDeque::new();
    queue.push_back(node_id);
    seen.insert(node_id);
    while let Some(current) = queue.pop_front() {
        let mut stmt = conn.prepare(
            "SELECT n.id FROM edges e
             JOIN nodes n ON n.id = e.from_node
             WHERE e.to_node = ?1 AND e.kind = 'blocks' AND n.deleted_at IS NULL",
        )?;
        let rows = stmt.query_map(params![current], |row| row.get::<_, i64>(0))?;
        for row in rows {
            let id = row?;
            if seen.insert(id) {
                out.push(id);
                queue.push_back(id);
            }
        }
    }
    Ok(out)
}

/// Самая длинная цепочка «блокирует … блокирует …» от узла вверх.
fn longest_chain(
    conn: &Connection,
    project: &Project,
    node_id: i64,
    max_depth: i64,
) -> Result<Vec<String>> {
    fn walk(
        conn: &Connection,
        project: &Project,
        id: i64,
        depth: i64,
        budget: i64,
    ) -> Result<Vec<String>> {
        let mut chain = vec![dot_of(project, &path_of(conn, id)?)];
        if depth >= budget {
            return Ok(chain);
        }
        let mut stmt = conn.prepare(
            "SELECT n.id FROM edges e
             JOIN nodes n ON n.id = e.from_node
             WHERE e.to_node = ?1 AND e.kind = 'blocks' AND n.deleted_at IS NULL",
        )?;
        let next: Vec<i64> = stmt
            .query_map(params![id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut best: Vec<String> = Vec::new();
        for child in next {
            let candidate = walk(conn, project, child, depth + 1, budget)?;
            if candidate.len() > best.len() {
                best = candidate;
            }
        }
        chain.extend(best);
        Ok(chain)
    }
    walk(conn, project, node_id, 1, max_depth)
}

impl Graph {
    /// Mermaid-описание для вставки в документацию.
    pub fn to_mermaid(&self) -> String {
        let mut out = String::from("graph LR\n");
        out.push_str(&format!("  n0[\"{}\"]\n", self.node));
        let mut index = 1;
        for neighbour in &self.blocked_by {
            out.push_str(&format!(
                "  n{index}[\"{}\"] -->|blocks| n0\n",
                neighbour.dot_id
            ));
            index += 1;
        }
        for neighbour in &self.blocks {
            out.push_str(&format!(
                "  n0 -->|blocks| n{index}[\"{}\"]\n",
                neighbour.dot_id
            ));
            index += 1;
        }
        for edge in &self.related {
            out.push_str(&format!(
                "  r{}[\"{}\"] -.->|{}| r{}[\"{}\"]\n",
                index,
                edge.from,
                edge.kind,
                index + 1,
                edge.to
            ));
            index += 2;
        }
        out
    }
}

/// Переводит в `todo` узлы, у которых больше нет незавершённых блокеров (README §10).
///
/// Хранимый `blocked` мог остаться после ручных правок: этот проход выравнивает его
/// с фактическим графом. Вызывается из `doctor --gc`.
pub fn clear_stale_blocked(conn: &mut Connection, project: &Project) -> Result<usize> {
    let stale: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT n.id, n.path FROM nodes n
             WHERE n.project_id = ?1 AND n.status = 'blocked' AND n.deleted_at IS NULL
               AND NOT EXISTS (
                 SELECT 1 FROM edges e JOIN nodes b ON b.id = e.to_node
                 WHERE e.from_node = n.id AND e.kind = 'blocks' AND b.deleted_at IS NULL
                   AND b.status NOT IN ('done', 'cancelled'))",
        )?;
        stmt.query_map(params![project.id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if stale.is_empty() {
        return Ok(0);
    }
    let now = now_iso();
    let tx = crate::db::write_tx(conn)?;
    for (id, path) in &stale {
        tx.execute(
            "UPDATE nodes SET status = 'todo', updated_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        let dot = project.dot_id(path);
        history::record(&tx, *id, Some("blocked"), "todo", history::Cause::Gc, "gc")?;
        activity::log(
            &tx,
            &Entry {
                project_id: project.id,
                node_id: Some(*id),
                entity_type: "node",
                entity_id: *id,
                action: "unblock",
                field_name: Some("status"),
                old_value: Some("blocked"),
                new_value: Some("todo"),
                summary: Some(dot.as_str()),
                actor: "gc",
            },
        )?;
    }
    tx.commit()?;
    Ok(stale.len())
}

/// Готовность узла к работе: чем меньше число, тем раньше его брать.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub dot_id: String,
    pub title: String,
    pub priority: String,
    pub due_date: Option<String>,
    pub days_overdue: i64,
    /// Сколько узлов разблокируется, когда этот будет готов.
    pub unblocks: i64,
    pub score: i64,
    pub reason: String,
}

/// Что брать дальше: узлы без живой аренды, без блокеров и незакрытые.
///
/// Порядок: приоритет, затем просрочка, затем срок, затем «разблокирует больше других»,
/// затем глубина (листовая работа раньше контейнера).
pub fn next_candidates(
    conn: &Connection,
    project: &Project,
    limit: usize,
) -> Result<Vec<Candidate>> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.path, e.title, e.effective_status, e.priority, e.due_date, e.depth,
                COALESCE((SELECT COUNT(*) FROM edges x JOIN nodes b ON b.id = x.to_node
                          WHERE x.from_node = e.id AND x.kind = 'blocks'
                            AND b.deleted_at IS NULL AND b.status NOT IN ('done','cancelled')), 0)
                    AS blockers,
                CAST(julianday(date('now')) - julianday(e.due_date) AS INTEGER) AS overdue
         FROM v_node_effective e
         WHERE e.project_id = ?1
           AND e.deleted_at IS NULL
           AND e.archived_at IS NULL
           AND e.lease_holder IS NULL
           AND e.effective_status IN ('todo', 'planned', 'review')
           AND NOT EXISTS (
             SELECT 1 FROM nodes c WHERE c.parent_id = e.id AND c.deleted_at IS NULL
               AND c.status NOT IN ('done', 'cancelled'))",
    )?;
    let rows = stmt.query_map(params![project.id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, Option<i64>>(8)?,
        ))
    })?;

    let mut candidates = Vec::new();
    for row in rows {
        let (id, path, title, priority, due_date, depth, blockers, overdue) = row?;
        if blockers > 0 {
            continue;
        }
        let unblocks = dependents(conn, id)?.len() as i64;
        let priority_score = match priority.as_str() {
            "critical" => 400,
            "high" => 300,
            "medium" => 200,
            _ => 100,
        };
        let overdue_days = overdue.unwrap_or(0).max(0);
        let due_score = if overdue_days > 0 {
            200 + overdue_days.min(100)
        } else if due_date.is_some() {
            50
        } else {
            0
        };
        let depth_score = depth.min(10) * 5;
        let unblocks_score = unblocks.min(20) * 10;
        let score = priority_score + due_score + depth_score + unblocks_score;

        let mut reasons = vec![format!("priority {priority}")];
        if overdue_days > 0 {
            reasons.push(format!("overdue by {overdue_days}d"));
        } else if let Some(due) = &due_date {
            reasons.push(format!("due {due}"));
        }
        if unblocks > 0 {
            reasons.push(format!("unblocks {unblocks}"));
        }
        if depth >= 3 {
            reasons.push("leaf work".to_string());
        }

        candidates.push(Candidate {
            dot_id: project.dot_id(&path),
            title,
            priority,
            due_date,
            days_overdue: overdue_days,
            unblocks,
            score,
            reason: reasons.join(", "),
        });
    }
    candidates.sort_by(|a, b| b.score.cmp(&a.score).then(a.dot_id.cmp(&b.dot_id)));
    candidates.truncate(limit);
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip() {
        for kind in [
            Kind::Blocks,
            Kind::Related,
            Kind::Duplicates,
            Kind::DiscoveredFrom,
            Kind::SplitFrom,
        ] {
            assert_eq!(Kind::parse(kind.as_str()), Some(kind));
        }
        assert!(Kind::Blocks.is_blocking());
        assert!(!Kind::Related.is_blocking());
        assert_eq!(Kind::parse("nope"), None);
    }
}
