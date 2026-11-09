//! Domain errors with stable machine-readable codes (README §19).

use serde::Serialize;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Файла базы нет: работа без `tasq init` бессмысленна.
    #[error("no tasq database found (looked for .tasq/tasq.db upwards from {0})")]
    DbNotFound(String),

    /// В базе нет ни одного проекта.
    #[error("no project in the database")]
    NoProject,

    /// Команда работает в одном проекте, а селектор не задан.
    #[error("no project selected: this command works in one project (known: {known})")]
    ProjectRequired { known: String },

    /// `meta.schema_version` больше, чем знает этот бинарь.
    #[error("database schema version {found} is newer than supported {supported}")]
    SchemaTooNew { found: i64, supported: i64 },

    /// Проект с таким ключом уже есть.
    #[error("project key {0} is already used")]
    DuplicateKey(String),

    /// Проекта с указанным ключом нет в базе.
    #[error("project {key} not found (known: {known})")]
    ProjectNotFound { key: String, known: String },

    /// Удаление непустого проекта без явного согласия.
    #[error(
        "project {key} still has {nodes} live node(s); use --force to delete it with everything"
    )]
    ProjectNotEmpty { key: String, nodes: i64 },

    /// Удаление последнего проекта в базе.
    #[error(
        "project {0} is the only one in this database; use --force or delete the database file"
    )]
    LastProject(String),

    /// Ключ проекта не проходит `[A-Z0-9]{2,8}`.
    #[error("invalid project key {0}: expected 2-8 characters of A-Z and 0-9")]
    InvalidKey(String),

    /// Ссылка не разобрана: ни dot-ID, ни `#id`.
    #[error("invalid node reference {0}: expected KEY-12.3.1, 12.3.1 or #4711")]
    InvalidRef(String),

    /// Узел не найден (или удалён).
    #[error("node {0} not found")]
    NodeNotFound(String),

    /// Одна и та же ссылка подходит к узлам разных проектов.
    #[error("reference {0} is ambiguous: {1}")]
    AmbiguousId(String, String),

    /// Переход не разрешён матрицей.
    #[error("cannot move {kind} from {from} to {to}")]
    InvalidTransition {
        from: String,
        to: String,
        kind: String,
    },

    /// Переход из закрытого состояния требует `force`.
    #[error("moving from {from} to {to} requires force")]
    ForceRequired { from: String, to: String },

    /// У узла есть незавершённые дети.
    #[error("node {dot_id} has {unfinished} unfinished child(ren)")]
    ChildrenUnfinished { dot_id: String, unfinished: i64 },

    /// Незакрытые блокеры.
    #[error("node {dot_id} is blocked by {blockers}")]
    UnmetPrerequisites { dot_id: String, blockers: String },

    /// Постановка защищена от перезаписи.
    #[error("description of {dot_id} is locked")]
    DescriptionLocked { dot_id: String },

    /// `in_progress` даёт только живая аренда.
    #[error("status in_progress requires an active lease; use `tasq claim`")]
    LeaseRequired,

    /// Узел уже закрыт: захватывать и менять его нельзя.
    #[error("node {dot_id} is {status}: claim is not allowed")]
    NodeClosed { dot_id: String, status: String },

    /// На узле есть живая аренда другого исполнителя.
    #[error(
        "node {dot_id} is held by {holder} until {expires_at} (attempt {attempt_no}); \
         renew your own lease or wait for expiry"
    )]
    ActiveAttemptExists {
        dot_id: String,
        holder: String,
        expires_at: String,
        attempt_no: i64,
    },

    /// Аренда истекла: продлевать нечего, нужен новый захват.
    #[error("lease of attempt {attempt_id} has expired; claim the node again")]
    LeaseExpired { attempt_id: i64 },

    /// Токен не принадлежит живой попытке.
    #[error("token does not match an open attempt")]
    NotYourAttempt,

    /// Запрошенный ресурс занят другой попыткой.
    #[error("resource {kind}:{value} is held by {holder} on {dot_id} until {expires_at}")]
    ResourceReservationConflict {
        kind: String,
        value: String,
        holder: String,
        dot_id: String,
        expires_at: String,
    },

    /// Резервов на попытку больше предела.
    #[error("too many reservations: {count} (max {max} per attempt)")]
    TooManyReservations { count: i64, max: i64 },

    /// Путь резерва выходит за корень проекта.
    #[error("path {0} is outside the project root")]
    PathOutsideProject(String),

    /// Контрольная сумма экспорта не совпала с содержимым файла.
    #[error("export checksum mismatch: file declares {expected}, content hashes to {actual}")]
    ExportChecksumMismatch { expected: String, actual: String },

    /// Ребро зависимости соединяет узел сам с собой.
    #[error("cannot link {dot_id} to itself")]
    SelfEdge { dot_id: String },

    /// Ребро между узлами разных проектов.
    #[error("{from} and {to} belong to different projects")]
    CrossProjectEdge { from: String, to: String },

    /// Блокировка предка потомком (или наоборот) — тупик по построению.
    #[error(
        "cannot link {dot_id} -> {other}: they are in the same branch (ancestor or descendant)"
    )]
    AncestorBlockEdge { dot_id: String, other: String },

    /// Ребро создало бы цикл в графе блокировок.
    #[error("linking {from} -> {to} would create a cycle: {path}")]
    CycleDetected {
        from: String,
        to: String,
        path: String,
    },

    /// Действие допустимо только для узла, ещё не начатого работой.
    #[error("node {dot_id} is {status}: only idea, backlog or todo nodes can be removed")]
    NotRemovable { dot_id: String, status: String },

    /// Узел уже в целевом состоянии (архив/удаление).
    #[error("node {dot_id} is already {state}")]
    AlreadyInState { dot_id: String, state: String },

    /// Глубина или длина пути за пределом (64 сегмента, 200 байт).
    #[error("node path would exceed the depth limit (64 segments, 200 bytes)")]
    DepthLimit,

    /// Родитель не может быть потомком перемещаемого узла.
    #[error("cannot move {dot_id} under its own descendant {target}")]
    MoveIntoSelf { dot_id: String, target: String },

    /// Миграция оставила нарушения внешних ключей — база не переживёт пересборку таблицы.
    #[error("migration {version} left {violations} foreign key violation(s); rolled back")]
    MigrationBroken { version: i64, violations: i64 },

    /// Поле не проходит проверку.
    #[error("invalid {field} value {value}")]
    InvalidValue { field: String, value: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Error::DbNotFound(_) => "DB_NOT_FOUND",
            Error::NoProject => "NO_PROJECT",
            Error::ProjectRequired { .. } => "PROJECT_REQUIRED",
            Error::SchemaTooNew { .. } => "SCHEMA_TOO_NEW",
            Error::DuplicateKey(_) => "DUPLICATE_KEY",
            Error::ProjectNotFound { .. } => "PROJECT_NOT_FOUND",
            Error::ProjectNotEmpty { .. } => "PROJECT_NOT_EMPTY",
            Error::LastProject(_) => "LAST_PROJECT",
            Error::InvalidKey(_) => "INVALID_KEY",
            Error::InvalidRef(_) => "INVALID_REF",
            Error::NodeNotFound(_) => "NODE_NOT_FOUND",
            Error::AmbiguousId(_, _) => "AMBIGUOUS_ID",
            Error::InvalidTransition { .. } => "INVALID_TRANSITION",
            Error::ForceRequired { .. } => "FORCE_REQUIRED",
            Error::ChildrenUnfinished { .. } => "CHILDREN_UNFINISHED",
            Error::UnmetPrerequisites { .. } => "UNMET_PREREQUISITES",
            Error::DescriptionLocked { .. } => "DESCRIPTION_LOCKED",
            Error::LeaseRequired => "LEASE_REQUIRED",
            Error::NodeClosed { .. } => "NODE_CLOSED",
            Error::ActiveAttemptExists { .. } => "ACTIVE_ATTEMPT_EXISTS",
            Error::LeaseExpired { .. } => "LEASE_EXPIRED",
            Error::NotYourAttempt => "NOT_YOUR_ATTEMPT",
            Error::ResourceReservationConflict { .. } => "RESOURCE_RESERVATION_CONFLICT",
            Error::TooManyReservations { .. } => "TOO_MANY_RESERVATIONS",
            Error::PathOutsideProject(_) => "PATH_OUTSIDE_PROJECT",
            Error::ExportChecksumMismatch { .. } => "EXPORT_CHECKSUM_MISMATCH",
            Error::SelfEdge { .. } => "SELF_EDGE",
            Error::CrossProjectEdge { .. } => "CROSS_PROJECT_EDGE",
            Error::AncestorBlockEdge { .. } => "ANCESTOR_BLOCK_EDGE",
            Error::CycleDetected { .. } => "CYCLE_DETECTED",
            Error::NotRemovable { .. } => "NOT_REMOVABLE",
            Error::AlreadyInState { .. } => "ALREADY_IN_STATE",
            Error::DepthLimit => "DEPTH_LIMIT",
            Error::MoveIntoSelf { .. } => "MOVE_INTO_SELF",
            Error::MigrationBroken { .. } => "MIGRATION_BROKEN",
            Error::InvalidValue { .. } => "INVALID_VALUE",
            Error::Io(_) | Error::Sqlite(_) => "STORAGE_ERROR",
        }
    }

    pub fn hint(&self) -> &'static str {
        match self {
            Error::DbNotFound(_) => "run `tasq init` in the project root",
            Error::NoProject => "run `tasq init` to register a project",
            Error::ProjectRequired { .. } => {
                "name the project you work on: --project KEY / TASQ_PROJECT in the CLI, \
                 project_select or the `project` argument in MCP; `tasq list projects` lists the keys"
            }
            Error::SchemaTooNew { .. } => "upgrade the tasq binary; downgrades are not supported",
            Error::DuplicateKey(_) => "pick another key or reuse the existing project",
            Error::ProjectNotFound { .. } => "run `tasq list projects` to see the available keys",
            Error::ProjectNotEmpty { .. } => {
                "archive it instead, or pass --force to delete the content"
            }
            Error::LastProject(_) => "keep at least one project, or remove the database itself",
            Error::InvalidKey(_) => "use uppercase letters and digits, for example TASQ or API2",
            Error::InvalidRef(_) => "run `tasq ls` to see the available dot-notation IDs",
            Error::NodeNotFound(_) => "run `tasq ls` to see the available nodes",
            Error::AmbiguousId(_, _) => "prefix the reference with the project key",
            Error::InvalidTransition { .. } => "see the transition matrix in the README, §7",
            Error::ForceRequired { .. } => "add --force to reopen a closed node",
            Error::ChildrenUnfinished { .. } => {
                "finish or cancel the children first, or add --force"
            }
            Error::UnmetPrerequisites { .. } => {
                "unblock the referenced nodes first, or add --force"
            }
            Error::DescriptionLocked { .. } => {
                "unlock the description first (tasq set --unlock-desc)"
            }
            Error::LeaseRequired => "take a lease with `tasq claim` (M2)",
            Error::NodeClosed { .. } => "reopen the node first (tasq set --status todo --force)",
            Error::ActiveAttemptExists { .. } => {
                "wait for the lease to expire, renew your own token, or take it over with --steal"
            }
            Error::LeaseExpired { .. } => {
                "claim the node again; checkpoints are inherited automatically"
            }
            Error::NotYourAttempt => "check the token: `tasq holders` lists live leases",
            Error::ResourceReservationConflict { .. } => {
                "wait for the lease to expire, drop the overlapping reserve, or pick another node"
            }
            Error::TooManyReservations { .. } => "reserve a directory instead of its files",
            Error::PathOutsideProject(_) => "reservations are relative to the project root",
            Error::ExportChecksumMismatch { .. } => {
                "the file was edited after export; re-export it or restore the original"
            }
            Error::SelfEdge { .. } => "a node cannot depend on itself",
            Error::CrossProjectEdge { .. } => "links only work inside one project",
            Error::AncestorBlockEdge { .. } => {
                "link a sibling branch instead; a parent cannot wait for its own child"
            }
            Error::CycleDetected { .. } => "remove the opposite edge first, or pick another target",
            Error::NotRemovable { .. } => {
                "closed work keeps its history; cancel or archive it instead"
            }
            Error::AlreadyInState { .. } => "nothing to do",
            Error::DepthLimit => "flatten the tree or move the node higher",
            Error::MoveIntoSelf { .. } => "pick a parent outside the moved subtree",
            Error::MigrationBroken { .. } => {
                "restore the database from a backup: the migration was rolled back untouched"
            }
            Error::InvalidValue { .. } => "check the allowed values with --help",
            Error::Io(_) | Error::Sqlite(_) => "check file permissions and disk space",
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "error": {
                "code": self.code(),
                "message": self.to_string(),
                "hint": self.hint(),
            }
        })
    }
}

/// Форма ошибки в `--json` для потребителей, которым нужен тип.
#[derive(Debug, Serialize)]
pub struct JsonError {
    pub code: &'static str,
    pub message: String,
    pub hint: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Каждый вариант ошибки с осмысленными полями. Список держится рядом с `code()`:
    /// новый вариант без строки здесь — это непроверенный код ветки, а не «мелочь».
    fn all_variants() -> Vec<Error> {
        vec![
            Error::DbNotFound("/tmp/x".to_string()),
            Error::NoProject,
            Error::ProjectRequired {
                known: "TASQ, RRL".to_string(),
            },
            Error::SchemaTooNew {
                found: 9,
                supported: 2,
            },
            Error::DuplicateKey("TASQ".to_string()),
            Error::ProjectNotFound {
                key: "NOPE".to_string(),
                known: "TASQ".to_string(),
            },
            Error::ProjectNotEmpty {
                key: "TASQ".to_string(),
                nodes: 3,
            },
            Error::LastProject("TASQ".to_string()),
            Error::InvalidKey("toolongkey".to_string()),
            Error::InvalidRef("???".to_string()),
            Error::NodeNotFound("TASQ-9".to_string()),
            Error::AmbiguousId("TASQ-1".to_string(), "2 and 3".to_string()),
            Error::InvalidTransition {
                from: "todo".to_string(),
                to: "planned".to_string(),
                kind: "task".to_string(),
            },
            Error::ForceRequired {
                from: "done".to_string(),
                to: "todo".to_string(),
            },
            Error::ChildrenUnfinished {
                dot_id: "TASQ-1".to_string(),
                unfinished: 2,
            },
            Error::UnmetPrerequisites {
                dot_id: "TASQ-1".to_string(),
                blockers: "TASQ-2".to_string(),
            },
            Error::DescriptionLocked {
                dot_id: "TASQ-1".to_string(),
            },
            Error::LeaseRequired,
            Error::NodeClosed {
                dot_id: "TASQ-1".to_string(),
                status: "done".to_string(),
            },
            Error::ActiveAttemptExists {
                dot_id: "TASQ-1".to_string(),
                holder: "agent-a".to_string(),
                expires_at: "2026-01-01T00:00:00.000Z".to_string(),
                attempt_no: 2,
            },
            Error::LeaseExpired { attempt_id: 7 },
            Error::NotYourAttempt,
            Error::ResourceReservationConflict {
                kind: "path".to_string(),
                value: "src/api".to_string(),
                holder: "agent-a".to_string(),
                dot_id: "TASQ-1".to_string(),
                expires_at: "2026-01-01T00:00:00.000Z".to_string(),
            },
            Error::TooManyReservations { count: 40, max: 32 },
            Error::PathOutsideProject("../etc".to_string()),
            Error::ExportChecksumMismatch {
                expected: "aa".to_string(),
                actual: "bb".to_string(),
            },
            Error::SelfEdge {
                dot_id: "TASQ-1".to_string(),
            },
            Error::CrossProjectEdge {
                from: "A-1".to_string(),
                to: "B-1".to_string(),
            },
            Error::AncestorBlockEdge {
                dot_id: "TASQ-1".to_string(),
                other: "TASQ-1.1".to_string(),
            },
            Error::CycleDetected {
                from: "TASQ-1".to_string(),
                to: "TASQ-2".to_string(),
                path: "TASQ-1 -> TASQ-2 -> TASQ-1".to_string(),
            },
            Error::NotRemovable {
                dot_id: "TASQ-1".to_string(),
                status: "blocked".to_string(),
            },
            Error::AlreadyInState {
                dot_id: "TASQ-1".to_string(),
                state: "todo".to_string(),
            },
            Error::DepthLimit,
            Error::MoveIntoSelf {
                dot_id: "TASQ-1".to_string(),
                target: "TASQ-1.1".to_string(),
            },
            Error::MigrationBroken {
                version: 3,
                violations: 2,
            },
            Error::InvalidValue {
                field: "priority".to_string(),
                value: "urgent".to_string(),
            },
            Error::Io(std::io::Error::other("disk on fire")),
            Error::Sqlite(rusqlite::Error::QueryReturnedNoRows),
        ]
    }

    /// Код ошибки — часть контракта: по нему агент и скрипт принимают решение, поэтому
    /// он есть у каждого варианта, пишется заглавными и не повторяется. Два варианта
    /// делят один код осознанно (`Io` и `Sqlite` — обе беды хранилища).
    #[test]
    fn every_error_has_a_unique_machine_code() {
        let variants = all_variants();
        let mut codes: Vec<&'static str> = Vec::new();
        for error in &variants {
            let code = error.code();
            assert!(!code.is_empty(), "пустой код у {error}");
            assert!(
                code.chars().all(|c| c.is_ascii_uppercase() || c == '_'),
                "код {code} не в верхнем регистре с подчёркиваниями"
            );
            codes.push(code);
        }

        let shared_storage = codes
            .iter()
            .filter(|code| **code == "STORAGE_ERROR")
            .count();
        assert_eq!(
            shared_storage, 2,
            "STORAGE_ERROR делят ровно Io и Sqlite, иначе код перестал быть различителем"
        );

        // Все прочие коды различны: два разных отказа с одним кодом невозможно
        // разобрать ни агенту, ни скрипту.
        let mut seen: Vec<&'static str> = Vec::new();
        let mut duplicates: Vec<&'static str> = Vec::new();
        for code in &codes {
            if *code == "STORAGE_ERROR" {
                continue;
            }
            if seen.contains(code) {
                duplicates.push(code);
            }
            seen.push(code);
        }
        assert!(duplicates.is_empty(), "коды повторяются: {duplicates:?}");
    }

    /// Подсказка есть у каждой ошибки: сообщение без «что делать» заставляет агента
    /// угадывать, а CLI — печатать пустую строку.
    #[test]
    fn every_error_carries_a_hint() {
        for error in all_variants() {
            let hint = error.hint();
            assert!(!hint.is_empty(), "нет подсказки у {error}");
            assert!(
                hint.len() > 8,
                "подсказка «{hint}» слишком коротка, чтобы помочь"
            );
            assert!(
                !hint.ends_with('.'),
                "подсказка не заканчивается точкой: её печатают отдельной строкой — «{hint}»"
            );
        }
    }
}
