//! Статусы, виды узлов и матрица переходов (README §7).

use crate::error::{Error, Result};

/// Вид узла. Определяет допустимый набор статусов и стартовое значение.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Epic,
    Task,
    Subtask,
    Micro,
}

impl Kind {
    /// Вид по глубине: 1 — эпик, 2 — задача, 3 — подзадача, глубже — микро-задача.
    pub fn from_depth(depth: i64) -> Self {
        match depth {
            1 => Kind::Epic,
            2 => Kind::Task,
            3 => Kind::Subtask,
            _ => Kind::Micro,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Epic => "epic",
            Kind::Task => "task",
            Kind::Subtask => "subtask",
            Kind::Micro => "micro",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "epic" => Some(Kind::Epic),
            "task" => Some(Kind::Task),
            "subtask" => Some(Kind::Subtask),
            "micro" => Some(Kind::Micro),
            _ => None,
        }
    }

    /// Статус при создании: эпик начинается как `planned`, остальное — `todo`.
    pub fn initial_status(self) -> Status {
        match self {
            Kind::Epic => Status::Planned,
            _ => Status::Todo,
        }
    }

    /// Глубина, соответствующая виду (для сверки при создании без родителя).
    pub fn depth(self) -> i64 {
        match self {
            Kind::Epic => 1,
            Kind::Task => 2,
            Kind::Subtask => 3,
            Kind::Micro => 4,
        }
    }
}

/// Хранимый статус узла.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Planned,
    Todo,
    InProgress,
    Blocked,
    Review,
    Done,
    Cancelled,
    Deferred,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Planned => "planned",
            Status::Todo => "todo",
            Status::InProgress => "in_progress",
            Status::Blocked => "blocked",
            Status::Review => "review",
            Status::Done => "done",
            Status::Cancelled => "cancelled",
            Status::Deferred => "deferred",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "planned" => Status::Planned,
            "todo" => Status::Todo,
            "in_progress" => Status::InProgress,
            "blocked" => Status::Blocked,
            "review" => Status::Review,
            "done" => Status::Done,
            "cancelled" => Status::Cancelled,
            "deferred" => Status::Deferred,
            _ => return None,
        })
    }

    /// Статусы, из которых виден этот статус допустим.
    fn predecessors(self) -> &'static [Status] {
        use Status::*;
        match self {
            Planned => &[],
            Todo => &[InProgress, Blocked, Deferred, Done, Cancelled],
            InProgress => &[Planned, Todo, Review, Blocked, Done],
            Blocked => &[Planned, Todo, InProgress, Review],
            Review => &[InProgress],
            Done => &[InProgress, Review],
            Cancelled => &[Planned, Todo, InProgress, Review, Blocked, Deferred],
            Deferred => &[Planned, Todo, InProgress],
        }
    }

    /// Переход требует явного `force`: возврат из закрытого состояния.
    pub fn requires_force(from: Status, to: Status) -> bool {
        matches!(
            (from, to),
            (Status::Done, Status::InProgress)
                | (Status::Done, Status::Todo)
                | (Status::Cancelled, Status::Todo)
        )
    }

    /// Разрешён ли статус для вида узла.
    pub fn allowed_for(self, kind: Kind) -> bool {
        match kind {
            Kind::Epic => self != Status::Todo,
            _ => self != Status::Planned,
        }
    }

    /// Разрешён ли переход по матрице и по виду узла.
    pub fn can_transition(from: Status, to: Status, kind: Kind) -> bool {
        if from == to {
            return true;
        }
        to.allowed_for(kind) && to.predecessors().contains(&from)
    }
}

/// Проверка перехода с выдачей предметной ошибки.
pub fn check_transition(from: Status, to: Status, kind: Kind, force: bool) -> Result<()> {
    if from == to {
        return Ok(());
    }
    if !Status::can_transition(from, to, kind) {
        return Err(Error::InvalidTransition {
            from: from.as_str().to_string(),
            to: to.as_str().to_string(),
            kind: kind.as_str().to_string(),
        });
    }
    if Status::requires_force(from, to) && !force {
        return Err(Error::ForceRequired {
            from: from.as_str().to_string(),
            to: to.as_str().to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_matches_the_specification() {
        use Status::*;
        // разрешено
        assert!(Status::can_transition(Todo, InProgress, Kind::Task));
        assert!(Status::can_transition(InProgress, Review, Kind::Task));
        assert!(Status::can_transition(Review, Done, Kind::Task));
        assert!(Status::can_transition(Blocked, Todo, Kind::Task));
        assert!(Status::can_transition(Deferred, Cancelled, Kind::Task));
        // запрещено
        assert!(!Status::can_transition(Todo, Review, Kind::Task));
        assert!(!Status::can_transition(Planned, Done, Kind::Epic));
        assert!(!Status::can_transition(Deferred, InProgress, Kind::Task));
        assert!(!Status::can_transition(Done, Cancelled, Kind::Task));
    }

    #[test]
    fn kinds_restrict_statuses() {
        assert!(!Status::Todo.allowed_for(Kind::Epic));
        assert!(Status::Planned.allowed_for(Kind::Epic));
        assert!(!Status::Planned.allowed_for(Kind::Task));
        assert_eq!(Kind::from_depth(1), Kind::Epic);
        assert_eq!(Kind::from_depth(5), Kind::Micro);
        assert_eq!(Kind::Epic.initial_status(), Status::Planned);
        assert_eq!(Kind::Task.initial_status(), Status::Todo);
    }

    #[test]
    fn reopening_needs_force() {
        assert!(Status::requires_force(Status::Done, Status::Todo));
        assert!(Status::requires_force(Status::Cancelled, Status::Todo));
        assert!(!Status::requires_force(Status::Todo, Status::InProgress));
        let err = check_transition(Status::Done, Status::Todo, Kind::Task, false).unwrap_err();
        assert_eq!(err.code(), "FORCE_REQUIRED");
        assert!(check_transition(Status::Done, Status::Todo, Kind::Task, true).is_ok());
    }
}
