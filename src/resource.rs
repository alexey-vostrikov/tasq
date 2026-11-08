//! Резервирование ресурсов: файлы, каталоги, glob-шаблоны и логические ключи (README §9).
//!
//! Конфликт обнаруживается до начала работы: заявка атомарна с захватом узла,
//! поэтому «взял узел — взял и файлы» либо не происходит вовсе.

use std::path::{Component, Path, PathBuf};

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::db::now_iso;
use crate::error::{Error, Result};

/// Пределы из README §9.2.
pub const MAX_PER_ATTEMPT: i64 = 64;
pub const MAX_VALUE_BYTES: usize = 512;

/// Вид ресурса.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Файл или каталог: пересечение по границе сегмента.
    Path,
    /// Шаблон `src/**/*.rs`: консервативное пересечение.
    Glob,
    /// Логический ключ: точное совпадение строки.
    Logical,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Path => "path",
            Kind::Glob => "glob",
            Kind::Logical => "logical",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "path" => Some(Kind::Path),
            "glob" => Some(Kind::Glob),
            "logical" => Some(Kind::Logical),
            _ => None,
        }
    }
}

/// Режим резерва.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Exclusive,
    Shared,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Exclusive => "exclusive",
            Mode::Shared => "shared",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "exclusive" => Some(Mode::Exclusive),
            "shared" => Some(Mode::Shared),
            _ => None,
        }
    }
}

/// Заявка на резерв.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resource {
    pub kind: Kind,
    pub value: String,
    pub mode: Mode,
}

impl Resource {
    pub fn path(value: &str) -> Self {
        Resource {
            kind: Kind::Path,
            value: value.to_string(),
            mode: Mode::Exclusive,
        }
    }

    pub fn glob(value: &str) -> Self {
        Resource {
            kind: Kind::Glob,
            value: value.to_string(),
            mode: Mode::Exclusive,
        }
    }

    pub fn logical(value: &str) -> Self {
        Resource {
            kind: Kind::Logical,
            value: value.to_string(),
            mode: Mode::Exclusive,
        }
    }

    pub fn shared(mut self) -> Self {
        self.mode = Mode::Shared;
        self
    }

    pub fn key(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.value)
    }
}

/// Нормализует путь: от корня проекта, без `.`, без выхода за корень.
///
/// Символические ссылки **не** разыменовываются: иначе конфликт зависел бы от машины.
pub fn normalize_path(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidValue {
            field: "path".to_string(),
            value: "empty".to_string(),
        });
    }
    let path = Path::new(trimmed);
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(Error::PathOutsideProject(trimmed.to_string()));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::PathOutsideProject(trimmed.to_string()));
            }
        }
    }
    if parts.is_empty() {
        return Err(Error::PathOutsideProject(trimmed.to_string()));
    }
    Ok(parts.join("/"))
}

/// Пересекаются ли два пути. `src/api` задевает `src/api/x.rs`, но не `src/apix/`.
pub fn paths_overlap(left: &str, right: &str) -> bool {
    let a = PathBuf::from(left);
    let b = PathBuf::from(right);
    a == b || a.starts_with(&b) || b.starts_with(&a)
}

/// Пересечение шаблонов — консервативно.
///
/// Доказать непересечение можно только для литеральных шаблонов без `**`/`?`/`[`;
/// во всех прочих случаях шаблоны считаются пересекающимися, кроме случая,
/// когда их литеральные префиксы уже разошлись по границе сегмента.
pub fn globs_overlap(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    let left_literal = !has_metacharacters(left);
    let right_literal = !has_metacharacters(right);
    if left_literal && right_literal {
        return paths_overlap(left, right);
    }
    // Безусловный `**` в корне пересекается со всем.
    if left.starts_with("**") || right.starts_with("**") {
        return true;
    }
    let left_prefix: Vec<&str> = left
        .split('/')
        .take_while(|s| !has_metacharacters(s))
        .collect();
    let right_prefix: Vec<&str> = right
        .split('/')
        .take_while(|s| !has_metacharacters(s))
        .collect();
    if left_prefix.is_empty() || right_prefix.is_empty() {
        return true;
    }
    // Непересечение только когда один префикс — строгое продолжение другого
    // по целым сегментам, а не «общий префикс по буквам».
    let common = left_prefix
        .iter()
        .zip(right_prefix.iter())
        .take_while(|(a, b)| a == b)
        .count();
    if common == left_prefix.len() && common == right_prefix.len() {
        return true;
    }
    if common == left_prefix.len() || common == right_prefix.len() {
        // один шаблон целиком внутри литеральной части другого
        return true;
    }
    // префиксы разошлись на каком-то сегменте
    false
}

fn has_metacharacters(segment: &str) -> bool {
    segment.contains('*') || segment.contains('?') || segment.contains('[') || segment.contains('{')
}

/// Пересекаются ли два резерва по смыслу.
pub fn conflicts(left: &Resource, right: &Resource) -> bool {
    if left.mode == Mode::Shared && right.mode == Mode::Shared {
        return false;
    }
    match (left.kind, right.kind) {
        (Kind::Logical, Kind::Logical) => left.value == right.value,
        // Логический ключ не пересекается с путями: это разные пространства.
        (Kind::Logical, _) | (_, Kind::Logical) => false,
        (Kind::Path, Kind::Path) => paths_overlap(&left.value, &right.value),
        // glob с путём: путь внутри литеральной части шаблона даёт конфликт
        (Kind::Glob, Kind::Path) => globs_overlap(&left.value, &right.value),
        (Kind::Path, Kind::Glob) => globs_overlap(&right.value, &left.value),
        (Kind::Glob, Kind::Glob) => globs_overlap(&left.value, &right.value),
    }
}

/// Проверка и нормализация одной заявки.
pub fn normalize(resource: &Resource) -> Result<Resource> {
    if resource.value.len() > MAX_VALUE_BYTES {
        return Err(Error::InvalidValue {
            field: "reservation".to_string(),
            value: format!("{} bytes (max {MAX_VALUE_BYTES})", resource.value.len()),
        });
    }
    let value = match resource.kind {
        Kind::Path => normalize_path(&resource.value)?,
        Kind::Glob => {
            let trimmed = resource.value.trim();
            if trimmed.is_empty() {
                return Err(Error::InvalidValue {
                    field: "glob".to_string(),
                    value: "empty".to_string(),
                });
            }
            // `..` в шаблоне не поддерживается: разбор шаблона и разбор пути
            // разошлись бы, и конфликт зависел бы от порядка сегментов.
            if trimmed.split('/').any(|segment| segment == "..") {
                return Err(Error::PathOutsideProject(trimmed.to_string()));
            }
            // Префикс `./` снимается до проверки на пустоту: иначе `./` превращался
            // бы в пустой шаблон, который уже не проходит повторную нормализацию.
            let without_prefix = trimmed.trim_start_matches("./");
            if without_prefix.is_empty() {
                return Err(Error::InvalidValue {
                    field: "glob".to_string(),
                    value: "empty".to_string(),
                });
            }
            without_prefix.to_string()
        }
        Kind::Logical => {
            let trimmed = resource.value.trim();
            if trimmed.is_empty() {
                return Err(Error::InvalidValue {
                    field: "logical".to_string(),
                    value: "empty".to_string(),
                });
            }
            trimmed.to_string()
        }
    };
    Ok(Resource {
        kind: resource.kind,
        value,
        mode: resource.mode,
    })
}

/// Проверяет заявку против всех живых резервов; возвращает первого виновника.
pub fn find_conflict(
    conn: &Connection,
    requested: &[Resource],
) -> Result<Option<(Resource, Holder)>> {
    find_conflict_excluding(conn, requested, 0)
}

/// То же, но резервы собственной попытки не считаются конфликтом.
///
/// Внутри одной заявки пересечения допустимы: заявки одной попытки держит один исполнитель,
/// и требовать от них взаимной непересекаемости незачем (README §9.2).
pub fn find_conflict_excluding(
    conn: &Connection,
    requested: &[Resource],
    own_attempt: i64,
) -> Result<Option<(Resource, Holder)>> {
    let live = live_reservations(conn)?;
    for want in requested {
        for (existing, holder) in &live {
            if own_attempt != 0 && holder.attempt_id == own_attempt {
                continue;
            }
            if conflicts(want, existing) {
                return Ok(Some((existing.clone(), holder.clone())));
            }
        }
    }
    Ok(None)
}

/// Держатель живого резерва: нужен, чтобы назвать виновника отказа.
#[derive(Clone, Debug, Serialize)]
pub struct Holder {
    pub attempt_id: i64,
    pub holder: String,
    pub expires_at: String,
    pub dot_id: String,
}

fn live_reservations(conn: &Connection) -> Result<Vec<(Resource, Holder)>> {
    let now_text = now_iso();
    let mut stmt = conn.prepare(
        "SELECT r.kind, r.value, r.mode, a.id, a.holder, a.expires_at, n.path, p.key
         FROM reservations r
         JOIN attempts a ON a.id = r.attempt_id
         JOIN nodes n ON n.id = a.node_id
         JOIN projects p ON p.id = n.project_id
         WHERE r.released_at IS NULL AND a.ended_at IS NULL AND a.expires_at > ?1",
    )?;
    let rows = stmt.query_map(params![now_text], |row| {
        let kind: String = row.get(0)?;
        let mode: String = row.get(2)?;
        let path: String = row.get(6)?;
        let key: String = row.get(7)?;
        Ok((
            Resource {
                kind: Kind::parse(&kind).unwrap_or(Kind::Path),
                value: row.get(1)?,
                mode: Mode::parse(&mode).unwrap_or(Mode::Exclusive),
            },
            Holder {
                attempt_id: row.get(3)?,
                holder: row.get(4)?,
                expires_at: row.get(5)?,
                dot_id: format!("{key}-{path}"),
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Живые резервы попытки — в том числе для проверки лимита.
pub fn attempt_reservations(conn: &Connection, attempt_id: i64) -> Result<Vec<Resource>> {
    let mut stmt = conn.prepare(
        "SELECT kind, value, mode FROM reservations
         WHERE attempt_id = ?1 AND released_at IS NULL
         ORDER BY id",
    )?;
    let rows = stmt.query_map(params![attempt_id], |row| {
        let kind: String = row.get(0)?;
        let mode: String = row.get(2)?;
        Ok(Resource {
            kind: Kind::parse(&kind).unwrap_or(Kind::Path),
            value: row.get(1)?,
            mode: Mode::parse(&mode).unwrap_or(Mode::Exclusive),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Записывает резервы попытки (вызывается внутри транзакции захвата).
pub fn insert_all(
    conn: &Connection,
    attempt_id: i64,
    node_id: i64,
    resources: &[Resource],
) -> Result<()> {
    let now_text = now_iso();
    for resource in resources {
        conn.execute(
            "INSERT INTO reservations (attempt_id, node_id, kind, value, mode, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                attempt_id,
                node_id,
                resource.kind.as_str(),
                resource.value,
                resource.mode.as_str(),
                now_text
            ],
        )?;
    }
    Ok(())
}

/// Снимает резервы попытки (release, истечение, gc).
pub fn release_all(conn: &Connection, attempt_id: i64) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE reservations SET released_at = ?1 WHERE attempt_id = ?2 AND released_at IS NULL",
        params![now_iso(), attempt_id],
    )?)
}

/// Проверка лимита на попытку.
pub fn check_limit(existing: i64, adding: i64) -> Result<()> {
    if existing + adding > MAX_PER_ATTEMPT {
        return Err(Error::TooManyReservations {
            count: existing + adding,
            max: MAX_PER_ATTEMPT,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_overlap_respects_segment_boundaries() {
        assert!(paths_overlap("src/api", "src/api/handlers.rs"));
        assert!(paths_overlap("src/api/handlers.rs", "src/api"));
        assert!(paths_overlap("src/api", "src/api"));
        assert!(!paths_overlap("src/api", "src/apix/other.rs"));
        assert!(!paths_overlap("src/api", "src/web"));
    }

    #[test]
    fn normalization_rejects_escapes() {
        assert_eq!(normalize_path("./src/api/").expect("ok"), "src/api");
        assert_eq!(normalize_path("src/a/../b").expect("ok"), "src/b");
        assert!(matches!(
            normalize_path("../outside"),
            Err(Error::PathOutsideProject(_))
        ));
        assert!(matches!(
            normalize_path("/etc/passwd"),
            Err(Error::PathOutsideProject(_))
        ));
    }

    #[test]
    fn literal_globs_do_not_overlap_when_prefixes_differ() {
        assert!(!globs_overlap("src/api/*.rs", "src/web/*.rs"));
        assert!(globs_overlap("src/**/*.rs", "src/web/main.rs"));
        assert!(globs_overlap("src/*.rs", "src/*.toml"));
        assert!(globs_overlap("**/*.rs", "src/web/main.rs"));
        assert!(globs_overlap(
            "migrations/*.sql",
            "migrations/0001_init.sql"
        ));
    }

    #[test]
    fn shared_never_conflicts_with_shared() {
        let left = Resource::path("src/api").shared();
        let right = Resource::path("src/api").shared();
        assert!(!conflicts(&left, &right));
        let exclusive = Resource::path("src/api");
        assert!(conflicts(&left, &exclusive));
        assert!(conflicts(&exclusive, &left));
    }

    #[test]
    fn logical_keys_are_their_own_namespace() {
        let port = Resource::logical("port:8080");
        let other = Resource::logical("port:8081");
        assert!(!conflicts(&port, &other));
        assert!(conflicts(&port, &Resource::logical("port:8080")));
        assert!(!conflicts(&port, &Resource::path("port:8080")));
    }
}
