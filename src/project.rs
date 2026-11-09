//! Проекты: ключ из dot-ID, выбор проекта для работы, перечисление и создание.

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::error::{Error, Result};

/// Проект в объёме, нужном для сборки dot-ID.
#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub id: i64,
    pub key: String,
    pub name: String,
    pub status: String,
}

/// Проект для списка: к нему добавлено число живых узлов.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub id: i64,
    pub key: String,
    pub name: String,
    pub status: String,
    pub nodes: i64,
    pub created_at: String,
}

/// Первый проект по порядку создания.
///
/// **Не** значение по умолчанию: проект — самостоятельная сущность иерархии, и
/// подставлять его молча нельзя (см. `select`). Функция нужна ровно там, где «любой
/// проект» — часть операции: переключение после удаления проекта сеанса, регистрация
/// проекта при `init` пустой базы, тестовые фикстуры.
pub fn first_project(conn: &Connection) -> Result<Project> {
    conn.query_row(
        "SELECT id, key, name, status FROM projects ORDER BY id LIMIT 1",
        [],
        |row| {
            Ok(Project {
                id: row.get(0)?,
                key: row.get(1)?,
                name: row.get(2)?,
                status: row.get(3)?,
            })
        },
    )
    .optional()?
    .ok_or(Error::NoProject)
}

/// Проект по ключу.
pub fn by_key(conn: &Connection, key: &str) -> Result<Option<Project>> {
    Ok(conn
        .query_row(
            "SELECT id, key, name, status FROM projects WHERE key = ?1",
            params![key],
            |row| {
                Ok(Project {
                    id: row.get(0)?,
                    key: row.get(1)?,
                    name: row.get(2)?,
                    status: row.get(3)?,
                })
            },
        )
        .optional()?)
}

/// Проект по внутреннему id.
pub fn by_id(conn: &Connection, id: i64) -> Result<Project> {
    conn.query_row(
        "SELECT id, key, name, status FROM projects WHERE id = ?1",
        params![id],
        |row| {
            Ok(Project {
                id: row.get(0)?,
                key: row.get(1)?,
                name: row.get(2)?,
                status: row.get(3)?,
            })
        },
    )
    .optional()?
    .ok_or(Error::NoProject)
}

/// Ключи всех проектов одной строкой — для сообщений об ошибке.
pub fn known_keys(conn: &Connection) -> Result<String> {
    let mut stmt = conn.prepare("SELECT key FROM projects ORDER BY id")?;
    let keys: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if keys.is_empty() {
        return Ok("none yet".to_string());
    }
    Ok(keys.join(", "))
}

/// Значение селектора, означающее «все проекты базы».
pub const ALL: &str = "ALL";

/// Что выбрал селектор: один проект или все сразу.
///
/// `--project all` (и отсутствие селектора в базе с несколькими проектами) даёт режим
/// обзора: список показывает задачи всех проектов, а dot-ID остаются однозначными, потому
/// что адрес узла начинается с ключа его проекта.
pub enum Scope {
    One(Project),
    All,
}

/// Селектор для команд обзора: `all` — все проекты, ключ — один, пусто — единственный.
pub fn scope(conn: &Connection, key: Option<&str>) -> Result<Scope> {
    match key.map(str::trim).filter(|value| !value.is_empty()) {
        Some(key) if key.eq_ignore_ascii_case(ALL) => Ok(Scope::All),
        Some(key) => by_key(conn, key)?
            .map(Scope::One)
            .ok_or_else(|| Error::ProjectNotFound {
                key: key.to_string(),
                known: known_keys(conn).unwrap_or_else(|_| "unknown".to_string()),
            }),
        None => match count(conn)? {
            0 => Err(Error::NoProject),
            1 => first_project(conn).map(Scope::One),
            // Показывать по умолчанию один из нескольких проектов — это чужие данные,
            // выданные как свои; обзорная команда по умолчанию показывает все.
            _ => Ok(Scope::All),
        },
    }
}

/// Проект, в котором идёт работа.
///
/// Проект — самостоятельная сущность иерархии, поэтому «первый по порядку создания» не
/// подставляется никогда: в базе с несколькими проектами такой выбор — это чужие данные,
/// в которые запись уходит молча (ровно так теряется работа). Правило:
///
/// - ключ задан (`--project`, `TASQ_PROJECT`) — работаем в нём;
/// - ключа нет, проект в базе один — выбора нет, берём его (не дефолт среди вариантов,
///   а единственная возможность; существующие скрипты и однопроектные базы живут как жили);
/// - ключа нет, проектов несколько — отказ `PROJECT_REQUIRED` со списком ключей. Какой
///   проект твой, знает только тот, кто ставит задачу.
pub fn select(conn: &Connection, key: Option<&str>) -> Result<Project> {
    match key.map(str::trim).filter(|value| !value.is_empty()) {
        Some(key) => by_key(conn, key)?.ok_or_else(|| Error::ProjectNotFound {
            key: key.to_string(),
            known: known_keys(conn).unwrap_or_else(|_| "unknown".to_string()),
        }),
        None => {
            let mut stmt =
                conn.prepare("SELECT id, key, name, status FROM projects ORDER BY id")?;
            let projects: Vec<Project> = stmt
                .query_map([], |row| {
                    Ok(Project {
                        id: row.get(0)?,
                        key: row.get(1)?,
                        name: row.get(2)?,
                        status: row.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            match projects.len() {
                0 => Err(Error::NoProject),
                1 => Ok(projects.into_iter().next().expect("один проект")),
                _ => Err(Error::ProjectRequired {
                    known: projects
                        .iter()
                        .map(|project| project.key.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                }),
            }
        }
    }
}

/// Единственный проект базы — или `None`, если их несколько.
///
/// Для операций, где «любой проект» возникает сам: сессия потеряла свой проект удалением,
/// и переезд возможен только тогда, когда переезжать некуда — проект один.
pub fn first_project_if_single(conn: &Connection) -> Result<Option<Project>> {
    let mut stmt = conn.prepare("SELECT id, key, name, status FROM projects ORDER BY id")?;
    let projects: Vec<Project> = stmt
        .query_map([], |row| {
            Ok(Project {
                id: row.get(0)?,
                key: row.get(1)?,
                name: row.get(2)?,
                status: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(match projects.len() {
        1 => projects.into_iter().next(),
        _ => None,
    })
}

/// Все проекты базы, в порядке создания, с числом живых узлов.
pub fn list(conn: &Connection) -> Result<Vec<Summary>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.key, p.name, p.status, p.created_at,
                (SELECT COUNT(*) FROM nodes n
                  WHERE n.project_id = p.id AND n.deleted_at IS NULL) AS nodes
           FROM projects p
          ORDER BY p.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Summary {
            id: row.get(0)?,
            key: row.get(1)?,
            name: row.get(2)?,
            status: row.get(3)?,
            created_at: row.get(4)?,
            nodes: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Создаёт проект в существующей базе.
///
/// Ключ приводится к верхнему регистру: dot-ID всё равно пишется заглавными, а
/// `tasq create project prj` с отказом «ожидалось A-Z» раздражает без пользы.
pub fn create(conn: &mut Connection, key: &str, name: Option<&str>) -> Result<Project> {
    let key = key.trim().to_uppercase();
    crate::db::validate_key(&key)?;
    if by_key(conn, &key)?.is_some() {
        return Err(Error::DuplicateKey(key));
    }
    let name = name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&key)
        .to_string();
    let now = crate::db::now_iso();
    let tx = crate::db::write_tx(conn)?;
    tx.execute(
        "INSERT INTO projects (key, name, status, timezone, tags, metadata, created_at, updated_at)
         VALUES (?1, ?2, 'active', 'UTC', '[]', '{}', ?3, ?3)",
        params![key, name, now],
    )?;
    let id = tx.last_insert_rowid();
    tx.commit()?;
    Ok(Project {
        id,
        key,
        name,
        status: "active".to_string(),
    })
}

impl Project {
    /// Собирает dot-ID из пути: `TASQ-12.3.1`.
    pub fn dot_id(&self, path: &str) -> String {
        format!("{}-{}", self.key, path)
    }
}

/// Что можно изменить у проекта. `None` — не трогать.
///
/// Ключ здесь тоже меняется: `dot_id` собирается из него на чтении, поэтому переименование
/// ключа меняет все dot-ID проекта разом и не требует правки строк узлов.
#[derive(Debug, Clone, Default)]
pub struct Patch<'a> {
    pub name: Option<&'a str>,
    pub description: Option<&'a str>,
    pub status: Option<&'a str>,
    pub key: Option<&'a str>,
}

/// Допустимые статусы проекта — те же, что в `CHECK` схемы.
pub const STATUSES: [&str; 4] = ["active", "on_hold", "completed", "archived"];

/// Изменяет проект, выбранный по ключу.
///
/// Возвращает проект и его прежний ключ — он нужен, чтобы сообщить, что dot-ID сместились.
pub fn update(
    conn: &mut Connection,
    key: Option<&str>,
    patch: &Patch<'_>,
) -> Result<(Project, String)> {
    let current = select(conn, key)?;
    let previous_key = current.key.clone();

    let mut changes: Vec<&str> = Vec::new();
    let mut values: Vec<String> = Vec::new();

    if let Some(name) = patch.name.map(str::trim).filter(|value| !value.is_empty()) {
        changes.push("name = ?");
        values.push(name.to_string());
    }
    if let Some(description) = patch.description {
        changes.push("description = ?");
        values.push(description.to_string());
    }
    if let Some(status) = patch
        .status
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !STATUSES.contains(&status) {
            return Err(Error::InvalidValue {
                field: "status".to_string(),
                value: format!("{status} (allowed: {})", STATUSES.join(", ")),
            });
        }
        changes.push("status = ?");
        values.push(status.to_string());
    }
    if let Some(raw_key) = patch.key.map(str::trim).filter(|value| !value.is_empty()) {
        let new_key = raw_key.to_uppercase();
        if new_key != previous_key {
            crate::db::validate_key(&new_key)?;
            if by_key(conn, &new_key)?.is_some() {
                return Err(Error::DuplicateKey(new_key));
            }
            changes.push("key = ?");
            values.push(new_key);
        }
    }

    if changes.is_empty() {
        return Err(Error::InvalidValue {
            field: "project".to_string(),
            value: "nothing to change".to_string(),
        });
    }

    changes.push("updated_at = ?");
    values.push(crate::db::now_iso());
    values.push(current.id.to_string());

    let tx = crate::db::write_tx(conn)?;
    tx.execute(
        &format!("UPDATE projects SET {} WHERE id = ?", changes.join(", ")),
        rusqlite::params_from_iter(values.iter()),
    )?;
    tx.commit()?;

    // Перечитываем по id, а не по ключу: ключ мог измениться в этом же UPDATE.
    let updated = by_id(conn, current.id)?;
    Ok((updated, previous_key))
}

/// Сколько живых узлов в проекте: удаление непустого проекта требует явного согласия.
pub fn live_nodes(conn: &Connection, project: &Project) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM nodes WHERE project_id = ?1 AND deleted_at IS NULL",
        params![project.id],
        |row| row.get(0),
    )?)
}

/// Сколько всего проектов в базе.
pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))?)
}

/// Удаляет проект вместе со всем содержимым (каскад объявлен в схеме).
///
/// Два отказа по умолчанию, потому что оба случая — это потеря работы, а не опечатка:
/// непустой проект требует `force`, последний проект в базе — тоже.
pub fn delete(conn: &mut Connection, key: Option<&str>, force: bool) -> Result<Project> {
    let project = select(conn, key)?;
    let nodes = live_nodes(conn, &project)?;
    if nodes > 0 && !force {
        return Err(Error::ProjectNotEmpty {
            key: project.key,
            nodes,
        });
    }
    if count(conn)? <= 1 && !force {
        return Err(Error::LastProject(project.key));
    }

    let tx = crate::db::write_tx(conn)?;
    tx.execute("DELETE FROM projects WHERE id = ?1", params![project.id])?;
    tx.commit()?;
    Ok(project)
}
