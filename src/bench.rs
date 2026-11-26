//! Фикстура для нагрузочных замеров: дерево известной формы и известного объёма.
//!
//! Зачем отдельный модуль, а не «нальём узлов скриптом». Замер, который нельзя повторить, — не
//! замер: число «board 12 мс» без ответа на вопрос «на какой базе» ничего не значит, а фикстура,
//! собранная руками, у каждого своя. Здесь форма задана числами (веер на уровень, глубина,
//! количество), статусы и даты выводятся из `id` детерминированно, и две сборки с одним `Spec`
//! дают побайтово одинаковую базу.
//!
//! Записи идут одним `INSERT` на пачку внутри одной транзакции, а не через `node::create`.
//! Это осознанно: `node::create` открывает свою транзакцию на узел и пишет журнал и историю —
//! на десяти тысячах узлов это минуты, а фикстура нужна как фон для ЧТЕНИЯ, а не как проверка
//! записи. Форма данных при этом та же, что у настоящей базы: пути, `seq`, FTS-триггеры, история
//! у закрытых узлов и несколько живых аренд.

use std::collections::HashMap;

use rusqlite::{Connection, params};
use time::{Duration, OffsetDateTime};

use crate::error::Result;
use crate::project::Project;

/// Форма дерева.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    /// Сколько узлов на каждом уровне под одним родителем.
    pub fanout: usize,
    /// Глубина: 1 — только корни, 4 — корни → задачи → подзадачи → микро.
    pub depth: i64,
    /// Сколько корневых эпиков. Итог — `epics * fanout^(depth-1)` узлов.
    pub epics: usize,
}

impl Default for Spec {
    fn default() -> Self {
        // Ровно 10 000 узлов глубины 4: 250 + 750 + 2250 + 6750 = 10 000.
        //
        // Веер подобран под это число, а не наоборот: при веере 10 и десяти эпиках выходит
        // 11 110, и «фикстура на 10 000» из названия перестаёт быть правдой. Дерево при этом
        // широкое у корня (250 эпиков) — так и выглядит база, которую ведут несколько команд.
        Self {
            fanout: 3,
            depth: 4,
            epics: 250,
        }
    }
}

impl Spec {
    /// Сколько узлов получится.
    pub fn nodes(&self) -> usize {
        let mut total = 0usize;
        for level in 0..self.depth {
            total += self.epics * self.fanout.pow((level) as u32);
        }
        total
    }
}

/// Что получилось: объём и то, что пригодится приёмке (сколько закрытых, сколько в аренде).
#[derive(Debug, Clone)]
pub struct Report {
    pub nodes: usize,
    pub history: usize,
    pub leases: usize,
    pub done: usize,
}

/// Наполняет ПУСТОЙ проект деревом по `Spec`.
///
/// Отказ, если в проекте уже есть узлы: фикстура — состояние, а не добавка, и смешать её с
/// настоящими данными значило бы измерять неизвестно что.
pub fn generate(conn: &mut Connection, project: &Project, spec: Spec) -> Result<Report> {
    let existing: i64 = conn.query_row(
        "SELECT COUNT(*) FROM nodes WHERE project_id = ?1",
        params![project.id],
        |row| row.get(0),
    )?;
    if existing > 0 {
        return Err(crate::error::Error::InvalidValue {
            field: "fixture".to_string(),
            value: format!("project {} already holds {existing} nodes", project.key),
        });
    }

    // Базовый момент фиксирован: иначе две сборки разошлись бы датами и `burnup` на них
    // отвечал бы по-разному.
    let base = OffsetDateTime::from_unix_timestamp(1_767_225_600)
        .expect("2026-01-01T00:00:00Z в секундах");
    let now = OffsetDateTime::now_utc();

    let tx = crate::db::write_tx(conn)?;

    // (parent_id, depth, path) для следующего уровня.
    let mut level: Vec<(i64, i64, String)> = Vec::with_capacity(spec.epics);
    let mut next_id: i64 = 1;
    let mut history = 0i64;
    let mut leases = 0i64;
    let mut done = 0i64;

    for index in 0..spec.epics {
        let seq = index as i64 + 1;
        let path = seq.to_string();
        let id = insert_node(
            &tx,
            project.id,
            None,
            seq,
            &path,
            1,
            &status_for(id_for(seq, 1, 0), 1),
            &created_at(base, seq as usize, 0),
            spec,
        )?;
        next_id = next_id.max(id + 1);
        level.push((id, 1, path));
    }

    for depth in 2..=spec.depth {
        let mut next: Vec<(i64, i64, String)> = Vec::new();
        for (parent_id, _parent_depth, parent_path) in &level {
            for index in 1..=spec.fanout {
                let path = format!("{parent_path}.{index}");
                let seed = id_for(*parent_id, depth, index);
                let status = status_for(seed, depth);
                let created = created_at(base, seed as usize, depth);
                let id = insert_node(
                    &tx,
                    project.id,
                    Some(*parent_id),
                    index as i64,
                    &path,
                    depth,
                    &status,
                    &created,
                    spec,
                )?;
                next_id = next_id.max(id + 1);
                // История — только у закрытых: она нужна burn-up, и писать её всем значило бы
                // раздувать фикстуру тем, что ни один отчёт не читает.
                if status == "done" {
                    done += 1;
                    tx.execute(
                        "INSERT INTO node_status_history (node_id, from_status, to_status, cause, at, actor)
                         VALUES (?1, NULL, 'idea', 'created', ?2, 'fixture'),
                                (?3, 'idea', 'done', 'manual', ?4, 'fixture')",
                        params![id, created, id, closed_at(base, seed as usize)],
                    )?;
                    history += 2;
                }
                next.push((id, depth, path));
            }
        }
        level = next;
    }

    // Живые аренды: несколько узлов держат прямо сейчас, чтобы замер видел `lease_holder`
    // и подзапрос живой попытки, а не пустоту.
    let holders = ["agent-a", "agent-b", "agent-c"];
    for (index, holder) in holders.iter().enumerate() {
        let id = next_id - 1 - index as i64;
        let expires = crate::db::iso_at(now + Duration::hours(4));
        tx.execute(
            "INSERT INTO attempts (node_id, attempt_no, holder, token, ttl_seconds,
                                   started_at, expires_at)
             VALUES (?1, 1, ?2, ?3, 14400, ?4, ?5)",
            params![
                id,
                holder,
                format!("fixture-token-{index}"),
                crate::db::iso_at(now - Duration::minutes(30)),
                expires
            ],
        )?;
        tx.execute(
            "UPDATE nodes SET status = 'in_progress', assigned_to = ?1 WHERE id = ?2",
            params![holder, id],
        )?;
        leases += 1;
    }

    // Снимок FTS строится триггерами при вставке, но `optimize` схлопывает сегменты: без него
    // первый же `find` на фикстуре мерил бы не поиск, а разбор свежего индекса.
    tx.execute("INSERT INTO nodes_fts(nodes_fts) VALUES ('optimize')", [])?;
    tx.commit()?;

    // Сверка объёма с расчётом: ошибка в формуле `Spec::nodes()` иначе прошла бы незамеченной
    // и замер шёл бы на другой базе, чем написано в задаче.
    let actual: i64 = conn.query_row(
        "SELECT COUNT(*) FROM nodes WHERE project_id = ?1",
        params![project.id],
        |row| row.get(0),
    )?;
    if actual as usize != spec.nodes() {
        return Err(crate::error::Error::InvalidValue {
            field: "fixture".to_string(),
            value: format!("вставилось {} узлов, расчёт даёт {}", actual, spec.nodes()),
        });
    }

    Ok(Report {
        nodes: spec.nodes(),
        history: history as usize,
        leases: leases as usize,
        done: done as usize,
    })
}

/// Детерминированный «шум» из трех чисел: не генератор случайных чисел, а перемешивание
/// битов, чтобы соседние узлы не получали одинаковый статус и одинаковую дату.
fn id_for(parent: i64, depth: i64, index: usize) -> u64 {
    let mix = (parent as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add((depth as u64) << 32)
        .wrapping_add(index as u64)
        .wrapping_mul(0xBF58_476D_1CE4_E5B9);
    mix ^ (mix >> 29)
}

/// Статус по уровню и зерну.
///
/// Наборы у уровней разные не случайно: эпик не бывает `todo`, задача не бывает `planned`, и
/// фикстура, нарушающая правило вида, уронила бы первую же проверку матрицы — то есть мерила бы
/// не то, что бывает в жизни.
fn status_for(seed: u64, depth: i64) -> String {
    let pick = seed % 100;
    let status = if depth == 1 {
        match pick {
            0..=69 => "done",
            70..=89 => "planned",
            90..=94 => "blocked",
            _ => "cancelled",
        }
    } else {
        match pick {
            0..=69 => "done",
            70..=84 => "todo",
            85..=92 => "review",
            93..=96 => "blocked",
            _ => "cancelled",
        }
    };
    status.to_string()
}

/// Дата создания: равномерно по девяноста дням от базы, со сдвигом по узлу.
fn created_at(base: OffsetDateTime, seed: usize, depth: i64) -> String {
    let minutes = (seed % 129_600) as i64 + depth * 7;
    crate::db::iso_at(base + Duration::minutes(minutes))
}

/// Дата закрытия: не раньше создания и не позже «сейчас» — фикстура не должна врать отчётам
/// будущими датами.
fn closed_at(base: OffsetDateTime, seed: usize) -> String {
    let minutes = (seed % 129_600) as i64 + 60;
    crate::db::iso_at(base + Duration::minutes(minutes))
}

#[allow(clippy::too_many_arguments)]
fn insert_node(
    tx: &rusqlite::Transaction<'_>,
    project_id: i64,
    parent_id: Option<i64>,
    seq: i64,
    path: &str,
    depth: i64,
    status: &str,
    created: &str,
    _spec: Spec,
) -> Result<i64> {
    let kind = crate::status::Kind::from_depth(depth).as_str();
    // Описание даётся каждому десятому узлу: на живой базе описания занимают две трети ответа
    // `ls --json`, и фикстура без них занижала бы цену полного списка в разы.
    let description = if seq % 10 == 0 {
        Some(format!(
            "Описание узла {path}: достаточно длинный текст, чтобы список без выбора полей \
             мерился на честном объёме, а не на пустых строках."
        ))
    } else {
        None
    };
    tx.execute(
        "INSERT INTO nodes (project_id, parent_id, depth, seq, path, kind, title, description,
                            status, priority, weight, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?11)",
        params![
            project_id,
            parent_id,
            depth,
            seq,
            path,
            kind,
            format!("Узел {path}"),
            description,
            status,
            priority_for(seq, depth),
            created
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

/// Приоритет по зерну: без него `next` сортировал бы по одному адресу, и замер выбора не мерил.
fn priority_for(seq: i64, depth: i64) -> &'static str {
    match (seq + depth) % 20 {
        0..=1 => "critical",
        2..=8 => "high",
        9..=17 => "medium",
        _ => "low",
    }
}

/// Сводка по проекту: сколько узлов, сколько закрытых и сколько в аренде.
///
/// Нужна тестам и `make perf`, чтобы падение бюджета можно было объяснить числами, а не
/// догадкой «стало медленнее».
pub fn summary(conn: &Connection, project: &Project) -> Result<HashMap<String, i64>> {
    let mut out = HashMap::new();
    out.insert(
        "nodes".to_string(),
        conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE project_id = ?1",
            params![project.id],
            |row| row.get(0),
        )?,
    );
    out.insert(
        "done".to_string(),
        conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE project_id = ?1 AND status = 'done'",
            params![project.id],
            |row| row.get(0),
        )?,
    );
    out.insert(
        "leases".to_string(),
        conn.query_row(
            "SELECT COUNT(*) FROM attempts a JOIN nodes n ON n.id = a.node_id
             WHERE n.project_id = ?1 AND a.ended_at IS NULL",
            params![project.id],
            |row| row.get(0),
        )?,
    );
    Ok(out)
}
