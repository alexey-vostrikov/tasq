//! Burn-up, прогноз и «риск срыва срока» (README §11–12).
//!
//! Первоисточник — таблицы и лог активности: ряд всегда можно восстановить заново,
//! `burn_snapshots` — только кэш для дешёвого чтения. Поэтому расхождение снимка и лога
//! лечится пересчётом, а не ручной правкой.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use time::{Date, Duration, OffsetDateTime};

use crate::error::{Error, Result};
use crate::project::Project;

/// Метрика сгорания объёма.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
    /// Штуки узлов.
    Count,
    /// Очки (`nodes.weight`).
    Points,
    /// Часы (`nodes.estimated_hours`).
    Hours,
}

impl Metric {
    pub fn as_str(self) -> &'static str {
        match self {
            Metric::Count => "count",
            Metric::Points => "points",
            Metric::Hours => "hours",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "count" => Some(Metric::Count),
            "points" => Some(Metric::Points),
            "hours" => Some(Metric::Hours),
            _ => None,
        }
    }

    /// Выражение SQL, дающее вклад узла в метрику.
    fn weight_sql(self) -> &'static str {
        match self {
            Metric::Count => "1.0",
            Metric::Points => "n.weight",
            Metric::Hours => "COALESCE(n.estimated_hours, 0.0)",
        }
    }
}

/// Точка ряда: состояние на конец дня.
#[derive(Debug, Clone, Serialize)]
pub struct Point {
    pub day: String,
    /// Весь объём, доступный на этот день (созданные минус отменённые).
    pub total: f64,
    /// Готовое к этому дню.
    pub done: f64,
    /// Сколько объёма добавили именно в этот день.
    pub added: f64,
    /// Идеальная линия: столько должно быть готово к этому дню при равномерной работе.
    pub ideal: f64,
}

/// Ряд сгорания объёма.
#[derive(Debug, Clone, Serialize)]
pub struct Series {
    pub metric: String,
    pub project: String,
    /// Итоговый объём на конец периода.
    pub scope_total: f64,
    /// Готовое на конец периода.
    pub scope_done: f64,
    /// Добавлено за период (расползание объёма).
    pub scope_added: f64,
    /// Объём узлов, по которым истории нет: про них неизвестно, закрыты они или нет.
    ///
    /// Без этого поля отчёт печатал `done 0` там, где правильный ответ — «не знаю». Импорт
    /// проекта из файла без истории давал именно такой ноль, и заметить его можно было только
    /// сравнив с другой базой.
    pub scope_unknown: f64,
    /// Сколько дней осталось до срока; `None`, если срок не задан.
    pub days_left: Option<i64>,
    /// Прогноз завершения по последним дням; `None`, если движения нет.
    pub forecast: Option<Forecast>,
    pub points: Vec<Point>,
}

/// Прогноз завершения: линейная экстраполяция, а не обещание.
#[derive(Debug, Clone, Serialize)]
pub struct Forecast {
    pub day: String,
    /// Средняя скорость в единицах метрики за день.
    pub rate_per_day: f64,
    /// На сколько дней прогноз позже срока (отрицательное — раньше).
    pub days_late: Option<i64>,
    /// Окно, по которому считалась скорость.
    pub window_days: i64,
}

/// Сумма с нормализованным нулём.
///
/// `Iterator::sum::<f64>()` на пустом итераторе даёт `-0.0`: это верная единица сложения для
/// float (`-0.0 + x == x` при любом `x`), а не ошибка реализации. Но в отчёте `-0.0` читается
/// как значащее число — «минус ноль готовой работы», — поэтому ноль приводится к обычному:
/// `-0.0 + 0.0 == 0.0`. Выглядит как пустая операция, и именно поэтому нужен комментарий.
fn sum_of(values: impl Iterator<Item = f64>) -> f64 {
    values.sum::<f64>() + 0.0
}

fn today(conn: &Connection) -> Result<Date> {
    let text: String = conn.query_row("SELECT date('now')", [], |row| row.get(0))?;
    Date::parse(&text, &time::format_description::well_known::Iso8601::DATE).map_err(|_| {
        Error::InvalidValue {
            field: "today".to_string(),
            value: text,
        }
    })
}

fn parse_day(value: &str) -> Option<Date> {
    Date::parse(value, &time::format_description::well_known::Iso8601::DATE).ok()
}

fn day_key(moment: &str) -> Option<Date> {
    // created_at/ended_at в формате 2026-11-08T05:07:30.295Z
    let head = moment.get(..10)?;
    parse_day(head)
}

/// Срок проекта (дедлайн цели): ближайший незакрытый `due_date` у корневых узлов.
fn project_deadline(conn: &Connection, project: &Project) -> Result<Option<(Date, String)>> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT due_date, path FROM nodes
             WHERE project_id = ?1 AND due_date IS NOT NULL AND deleted_at IS NULL
               AND archived_at IS NULL AND status NOT IN ('done', 'cancelled')
             ORDER BY due_date LIMIT 1",
            params![project.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(row.and_then(|(due, path)| parse_day(&due).map(|day| (day, path))))
}

/// Строит ряд от `since` до `until` (по умолчанию — от первого узла до сегодня).
pub fn series(
    conn: &Connection,
    project: &Project,
    metric: Metric,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Series> {
    // Ряд строится поверх прогресса поддерева: тот же бюджет, что и у списков.
    crate::tree::check_scan_budget(conn, Some(project))?;
    let today = today(conn)?;
    let end = match until {
        Some(value) => parse_day(value).ok_or_else(|| Error::InvalidValue {
            field: "until".to_string(),
            value: value.to_string(),
        })?,
        None => today,
    };

    let weight = metric.weight_sql();

    // Создание: дата из create-записи лога, иначе — created_at самого узла.
    let mut created: BTreeMap<i64, (Date, f64)> = BTreeMap::new();
    // Готовность: первая запись перехода в done.
    let mut done_at: BTreeMap<i64, (Date, f64)> = BTreeMap::new();
    // Отмена: первая запись перехода в cancelled.
    let mut cancelled_at: BTreeMap<i64, (Date, f64)> = BTreeMap::new();
    // Объём узлов, про которые истории нет вовсе: их состояние неизвестно, и в «готово»
    // они не попадают — иначе отчёт сочинял бы данные.
    let mut unknown = 0.0_f64;

    let mut stmt = conn.prepare(&format!(
        "SELECT n.id, n.path, n.created_at, n.status, {weight} AS w
         FROM nodes n
         WHERE n.project_id = ?1 AND n.deleted_at IS NULL"
    ))?;
    let rows = stmt.query_map(params![project.id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, f64>(4)?,
        ))
    })?;

    let mut nodes: Vec<(i64, String, String, String, f64)> = Vec::new();
    for row in rows {
        nodes.push(row?);
    }

    for (id, _path, created_at, _status, w) in &nodes {
        let day = day_key(created_at).unwrap_or(today);
        created.insert(*id, (day, *w));

        // Узел без истории не считается ни закрытым, ни открытым: он попадает в отдельный
        // счётчик, и отчёт говорит об этом словами.
        match transition(conn, *id, "done")? {
            Transition::Day(day) => {
                done_at.insert(*id, (day, *w));
            }
            Transition::NotYet => {}
            Transition::Unknown => unknown += *w,
        }
        if let Transition::Day(day) = transition(conn, *id, "cancelled")? {
            cancelled_at.insert(*id, (day, *w));
        }
    }

    let start = match since {
        Some(value) => parse_day(value).ok_or_else(|| Error::InvalidValue {
            field: "since".to_string(),
            value: value.to_string(),
        })?,
        None => created.values().map(|(day, _)| *day).min().unwrap_or(today),
    };

    if start > end {
        return Err(Error::InvalidValue {
            field: "since".to_string(),
            value: format!("{start} is after {end}"),
        });
    }

    // Предел периода — год плюс запас: 900 точек достаточно для любого реального проекта,
    // а защита нужна от опечатки в дате.
    let span = (end - start).whole_days();
    if span > 900 {
        return Err(Error::InvalidValue {
            field: "since".to_string(),
            value: format!("{span} days is too long; the series is limited to 900 days"),
        });
    }

    let deadline = project_deadline(conn, project)?;
    let mut points = Vec::new();
    let mut day = start;
    let mut scope_added = 0.0;
    let mut last_total = 0.0;
    let mut last_done = 0.0;

    while day <= end {
        let total: f64 = sum_of(
            created
                .values()
                .filter(|(created_day, _)| *created_day <= day)
                .map(|(_, w)| *w),
        ) - sum_of(
            cancelled_at
                .values()
                .filter(|(cancelled_day, _)| *cancelled_day <= day)
                .map(|(_, w)| *w),
        );
        let done: f64 = sum_of(
            done_at
                .values()
                .filter(|(done_day, _)| *done_day <= day)
                .map(|(_, w)| *w),
        );
        let added: f64 = sum_of(
            created
                .values()
                .filter(|(created_day, _)| *created_day == day)
                .map(|(_, w)| *w),
        );

        let ideal = match deadline {
            Some((due, _)) if due >= start && due > start => {
                let total_days = (due - start).whole_days().max(1) as f64;
                let elapsed = (day - start).whole_days().max(0) as f64;
                (elapsed / total_days).min(1.0) * total.max(last_total)
            }
            _ => 0.0,
        };

        scope_added += added;
        last_total = total;
        last_done = done;
        points.push(Point {
            day: day.to_string(),
            total,
            done,
            added,
            ideal: (ideal * 100.0).round() / 100.0,
        });
        day += Duration::days(1);
    }

    let deadline_day = deadline.as_ref().map(|(due, _)| *due);
    let days_left = deadline.as_ref().map(|(due, _)| (*due - end).whole_days());
    let forecast = forecast(&points, deadline_day, 7);

    Ok(Series {
        metric: metric.as_str().to_string(),
        project: project.key.clone(),
        scope_total: last_total,
        scope_done: last_done,
        scope_added: (scope_added * 100.0).round() / 100.0,
        scope_unknown: (unknown * 100.0).round() / 100.0,
        days_left,
        forecast,
        points,
    })
}

/// Что известно о переходе узла в статус.
///
/// Различие `NotYet` и `Unknown` — не педантизм. «Переходов не было» значит, что работа не
/// закрыта; «истории нет» значит, что мы про этот узел ничего не знаем (база пришла из файла
/// без истории, или узел старше таблицы). Показывать второе как первое — это тот самый
/// молчаливый ноль, из-за которого импорт проекта обнулял burn-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// Первый переход в статус состоялся в этот день.
    Day(Date),
    /// История есть, перехода в этот статус в ней нет.
    NotYet,
    /// Истории по узлу нет вовсе.
    Unknown,
}

/// Когда узел впервые перешёл в указанный статус.
///
/// Источник — `node_status_history`, а не журнал: журнал отвечает на «что произошло» и пишется
/// всеми путями мутации, поэтому дата закрытия в нём держалась на совпадении текста в шести
/// местах записи. История — факт перехода, и её пишет каждый путь смены статуса.
fn transition(conn: &Connection, node_id: i64, status: &str) -> Result<Transition> {
    let recorded: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_status_history WHERE node_id = ?1",
        params![node_id],
        |row| row.get(0),
    )?;
    if recorded == 0 {
        return Ok(Transition::Unknown);
    }
    let text: Option<String> = conn
        .query_row(
            "SELECT at FROM node_status_history
             WHERE node_id = ?1 AND to_status = ?2
             ORDER BY at, id LIMIT 1",
            params![node_id, status],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match text.as_deref().and_then(day_key) {
        Some(day) => Transition::Day(day),
        None => Transition::NotYet,
    })
}

/// Прогноз по средней скорости за последние `window` дней.
fn forecast(points: &[Point], deadline: Option<Date>, window: i64) -> Option<Forecast> {
    if points.len() < 2 {
        return None;
    }
    let window = window.clamp(1, points.len() as i64) as usize;
    let slice = &points[points.len() - window..];
    let first = slice.first()?;
    let last = slice.last()?;
    let days = (points.len() - 1).min(window - 1).max(1) as f64;
    let rate = (last.done - first.done) / days;
    if rate <= 0.0 {
        return None;
    }
    let remaining = (last.total - last.done).max(0.0);
    let days_needed = (remaining / rate).ceil() as i64;
    let finish = parse_day(&last.day)? + Duration::days(days_needed.max(0));
    let days_late = deadline.map(|due| (finish - due).whole_days());
    Some(Forecast {
        day: finish.to_string(),
        rate_per_day: (rate * 100.0).round() / 100.0,
        days_late,
        window_days: window as i64,
    })
}

/// Материализует снимок за день: кэш поверх лога.
pub fn snapshot(
    conn: &mut Connection,
    project: &Project,
    metric: Metric,
    day: &str,
) -> Result<Point> {
    let parsed = parse_day(day).ok_or_else(|| Error::InvalidValue {
        field: "day".to_string(),
        value: day.to_string(),
    })?;
    let series = series(
        conn,
        project,
        metric,
        Some(&parsed.to_string()),
        Some(&parsed.to_string()),
    )?;
    let point = series
        .points
        .last()
        .cloned()
        .ok_or_else(|| Error::InvalidValue {
            field: "day".to_string(),
            value: day.to_string(),
        })?;

    let tx = crate::db::write_tx(conn)?;
    tx.execute(
        "INSERT INTO burn_snapshots (project_id, day, metric, scope_total, scope_done, scope_added, computed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(project_id, day, metric) DO UPDATE SET
           scope_total = excluded.scope_total,
           scope_done = excluded.scope_done,
           scope_added = excluded.scope_added,
           computed_at = excluded.computed_at",
        params![
            project.id,
            point.day,
            metric.as_str(),
            point.total,
            point.done,
            point.added,
            crate::db::now_iso()
        ],
    )?;
    tx.commit()?;
    Ok(point)
}

/// Пишет снимки за каждый день ряда — для истории и для дешёвого чтения.
pub fn snapshot_all(conn: &mut Connection, project: &Project, metric: Metric) -> Result<usize> {
    let series = series(conn, project, metric, None, None)?;
    let tx = crate::db::write_tx(conn)?;
    let now = crate::db::now_iso();
    let mut written = 0;
    for point in &series.points {
        tx.execute(
            "INSERT INTO burn_snapshots (project_id, day, metric, scope_total, scope_done, scope_added, computed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(project_id, day, metric) DO UPDATE SET
               scope_total = excluded.scope_total,
               scope_done = excluded.scope_done,
               scope_added = excluded.scope_added,
               computed_at = excluded.computed_at",
            params![
                project.id,
                point.day,
                metric.as_str(),
                point.total,
                point.done,
                point.added,
                now
            ],
        )?;
        written += 1;
    }
    tx.commit()?;
    Ok(written)
}

/// ASCII-график: объём, готовое и линия цели.
///
/// Рисуется по строкам сверху вниз: `·` — объём, `#` — готовое, `|` — идеальная линия.
pub fn ascii(series: &Series, width: usize) -> String {
    let points = &series.points;
    if points.is_empty() {
        return "нет данных".to_string();
    }
    let height = 12usize;
    let width = width.clamp(20, 200);
    let total = points
        .iter()
        .map(|p| p.total.max(p.ideal))
        .fold(0.0_f64, f64::max)
        .max(1.0);

    // Разрежение по дням: если дней больше ширины, берём каждый n-й.
    let step = (points.len() as f64 / width as f64).ceil().max(1.0) as usize;
    let sampled: Vec<&Point> = points.iter().step_by(step).collect();

    let mut grid = vec![vec![' '; sampled.len()]; height];
    let place = |column: usize, value: f64, symbol: char, grid: &mut Vec<Vec<char>>| {
        let ratio = (value / total).clamp(0.0, 1.0);
        let row = ((height - 1) as f64 * (1.0 - ratio)).round() as usize;
        let row = row.min(height - 1);
        // готовое перекрывает объём, чтобы линия прогресса была видна
        grid[row][column] = symbol;
    };

    for (column, point) in sampled.iter().enumerate() {
        place(column, point.total, '·', &mut grid);
        place(column, point.done, '#', &mut grid);
        if point.ideal > 0.0 {
            place(column, point.ideal, '|', &mut grid);
        }
    }

    let mut out = String::new();
    for (row, cells) in grid.iter().enumerate() {
        let value = total * (1.0 - row as f64 / (height - 1) as f64);
        out.push_str(&format!("{:>7.1} |", value));
        out.extend(cells.iter());
        out.push('\n');
    }
    out.push_str(&format!("{:>7} +", ""));
    out.push_str(&"-".repeat(sampled.len()));
    out.push('\n');

    let first = sampled.first().map(|p| p.day.as_str()).unwrap_or("");
    let last = sampled.last().map(|p| p.day.as_str()).unwrap_or("");
    out.push_str(&format!("        {first} … {last}\n"));
    out.push_str(&format!(
        "        · объём  # готово  | линия цели  (метрика {}, шаг {} дн.)\n",
        series.metric, step
    ));
    out
}

/// Узел, отстающий от своего срока.
#[derive(Debug, Clone, Serialize)]
pub struct AtRisk {
    pub dot_id: String,
    pub title: String,
    pub due_date: String,
    pub days_left: i64,
    /// Доля прошедшего времени от окна «создан → срок».
    pub elapsed: f64,
    /// Доля готовности (по весу детей и своему статусу).
    pub done: f64,
    /// Насколько отставание больше допустимого (0.2 из README §11).
    pub gap: f64,
}

/// Узлы с риском срыва: времени прошло заметно больше, чем сделано.
pub fn at_risk(conn: &Connection, project: &Project, threshold: f64) -> Result<Vec<AtRisk>> {
    let today = today(conn)?;
    let mut stmt = conn.prepare(
        "SELECT e.path, e.title, e.due_date, e.created_at, e.status, e.effective_status,
                COALESCE(p.done_weight, 0), COALESCE(p.total_weight, 0)
         FROM v_node_effective e
         LEFT JOIN v_node_progress p ON p.node_id = e.id
         WHERE e.project_id = ?1 AND e.deleted_at IS NULL AND e.archived_at IS NULL
           AND e.due_date IS NOT NULL
           AND e.effective_status NOT IN ('done', 'cancelled')",
    )?;
    let rows = stmt.query_map(params![project.id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, f64>(6)?,
            row.get::<_, f64>(7)?,
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (path, title, due, created_at, status, done_weight, total_weight) = row?;
        let Some(due_day) = parse_day(&due) else {
            continue;
        };
        let created_day = day_key(&created_at).unwrap_or(today);
        let window = (due_day - created_day).whole_days().max(1) as f64;
        let elapsed = ((today - created_day).whole_days() as f64 / window).clamp(0.0, 2.0);
        let done = if total_weight > 0.0 {
            (done_weight / total_weight).clamp(0.0, 1.0)
        } else if status == "done" {
            1.0
        } else {
            0.0
        };
        let gap = elapsed - done;
        if gap > threshold {
            out.push(AtRisk {
                dot_id: project.dot_id(&path),
                title,
                due_date: due,
                days_left: (due_day - today).whole_days(),
                elapsed: (elapsed * 100.0).round() / 100.0,
                done: (done * 100.0).round() / 100.0,
                gap: (gap * 100.0).round() / 100.0,
            });
        }
    }
    out.sort_by(|a, b| {
        b.gap
            .partial_cmp(&a.gap)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}

/// Проверка, что «сегодня» внутри окна проекта: используется в `doctor`.
pub fn deadline_in_past(conn: &Connection, project: &Project) -> Result<bool> {
    let today = today(conn)?;
    Ok(project_deadline(conn, project)?
        .map(|(due, _)| due < today)
        .unwrap_or(false))
}

/// Время в UTC — для тестов, которым нужен детерминизм.
pub fn utc_today() -> String {
    OffsetDateTime::now_utc().date().to_string()
}
