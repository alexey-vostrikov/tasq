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

    /// Статус при создании: любой узел начинается как `idea`.
    ///
    /// Воронка общая для всех уровней (idea → backlog → todo/planned), поэтому в
    /// `planned`/`todo` узел попадает осознанным переходом, а не по факту создания.
    pub fn initial_status(self) -> Status {
        Status::Idea
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
    /// Сырая мысль, ещё не оформленная работа. Стартовый статус любого узла.
    Idea,
    /// Принято в очередь, но не готово к захвату.
    Backlog,
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
            Status::Idea => "idea",
            Status::Backlog => "backlog",
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
            "idea" => Status::Idea,
            "backlog" => Status::Backlog,
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

    /// Все статусы в порядке воронки: используется в подсказках и в колонках доски.
    pub fn all() -> &'static [Status] {
        &[
            Status::Idea,
            Status::Backlog,
            Status::Planned,
            Status::Todo,
            Status::InProgress,
            Status::Blocked,
            Status::Review,
            Status::Deferred,
            Status::Done,
            Status::Cancelled,
        ]
    }

    /// Статусы, из которых виден этот статус допустим.
    fn predecessors(self) -> &'static [Status] {
        use Status::*;
        match self {
            // В идею возвращают сырую мысль, которую рано держать в очереди: из очереди
            // (`backlog`, `planned`, `todo`) и из отложенного — то есть из любого состояния
            // ДО начала работы, но не из закрытого.
            Idea => &[Backlog, Planned, Todo, Deferred],
            Backlog => &[Idea, Planned, Todo, InProgress, Blocked, Review, Deferred],
            Planned => &[Idea, Backlog, Deferred],
            Todo => &[
                Idea, Backlog, InProgress, Blocked, Deferred, Done, Cancelled,
            ],
            InProgress => &[Idea, Backlog, Planned, Todo, Review, Blocked, Done],
            Blocked => &[Idea, Backlog, Planned, Todo, InProgress, Review],
            Review => &[InProgress],
            Done => &[InProgress, Review],
            Cancelled => &[
                Idea, Backlog, Planned, Todo, InProgress, Review, Blocked, Deferred,
            ],
            Deferred => &[Idea, Backlog, Planned, Todo, InProgress],
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
        assert!(Status::can_transition(Idea, Backlog, Kind::Task));
        assert!(Status::can_transition(Backlog, Todo, Kind::Task));
        assert!(Status::can_transition(Idea, Planned, Kind::Epic));
        assert!(Status::can_transition(Todo, InProgress, Kind::Task));
        assert!(Status::can_transition(InProgress, Review, Kind::Task));
        assert!(Status::can_transition(Review, Done, Kind::Task));
        assert!(Status::can_transition(Blocked, Todo, Kind::Task));
        assert!(Status::can_transition(Deferred, Cancelled, Kind::Task));
        assert!(Status::can_transition(Todo, Idea, Kind::Task));
        // запрещено
        assert!(!Status::can_transition(Todo, Review, Kind::Task));
        assert!(!Status::can_transition(Planned, Done, Kind::Epic));
        assert!(!Status::can_transition(Deferred, InProgress, Kind::Task));
        assert!(!Status::can_transition(Done, Cancelled, Kind::Task));
        assert!(!Status::can_transition(Idea, Done, Kind::Task));
        // вид узла важнее матрицы: задача не бывает `planned`, эпик — `todo`
        assert!(!Status::can_transition(Idea, Planned, Kind::Task));
        assert!(!Status::can_transition(Idea, Todo, Kind::Epic));
    }

    #[test]
    fn kinds_restrict_statuses() {
        assert!(!Status::Todo.allowed_for(Kind::Epic));
        assert!(Status::Planned.allowed_for(Kind::Epic));
        assert!(!Status::Planned.allowed_for(Kind::Task));
        assert!(Status::Idea.allowed_for(Kind::Epic));
        assert!(Status::Idea.allowed_for(Kind::Micro));
        assert!(Status::Backlog.allowed_for(Kind::Task));
        assert_eq!(Kind::from_depth(1), Kind::Epic);
        assert_eq!(Kind::from_depth(5), Kind::Micro);
        assert_eq!(Kind::Epic.initial_status(), Status::Idea);
        assert_eq!(Kind::Task.initial_status(), Status::Idea);
    }

    /// Список статусов — единственный источник и для колонок доски, и для подсказок:
    /// пропущенный `parse`/`as_str` делает статус невидимым в одном из мест.
    #[test]
    fn every_status_round_trips_and_is_listed_once() {
        let mut seen = Vec::new();
        for status in Status::all() {
            assert_eq!(Status::parse(status.as_str()), Some(*status));
            assert!(
                !seen.contains(&status.as_str()),
                "{} listed twice",
                status.as_str()
            );
            seen.push(status.as_str());
        }
        assert_eq!(seen.len(), 10, "добавили статус — добавьте его в all()");
    }

    /// `deferred` — вход из любого состояния ДО начала работы и из `in_progress`, выход в
    /// `todo`/`planned`/`backlog`/`idea`/`cancelled`. Раньше эта ветка матрицы не проверялась
    /// ни одним тестом: строка `Deferred => &[…]` оставалась непокрытой, и подмена списка
    /// проходила молча.
    #[test]
    fn deferred_is_reachable_from_open_states_and_leaves_them() {
        use Status::*;
        for from in [Idea, Backlog, Todo] {
            assert!(
                Status::can_transition(from, Deferred, Kind::Task),
                "{from:?} -> deferred должно быть разрешено"
            );
        }
        assert!(Status::can_transition(InProgress, Deferred, Kind::Task));
        assert!(Status::can_transition(Planned, Deferred, Kind::Epic));
        // Обратно: из отложенного — только в очередь, а не сразу в работу.
        for to in [Idea, Backlog, Todo] {
            assert!(
                Status::can_transition(Deferred, to, Kind::Task),
                "deferred -> {to:?} должно быть разрешено"
            );
        }
        assert!(Status::can_transition(Deferred, Cancelled, Kind::Task));
        // Закрытая работа не «откладывается»: сначала reopen, и это отдельное решение.
        for closed in [Done, Cancelled] {
            assert!(
                !Status::can_transition(closed, Deferred, Kind::Task),
                "{closed:?} -> deferred не должно быть разрешено"
            );
        }
    }

    /// «Оставить как есть» — не переход: матрица отвечает «да» на `from == to`, и это
    /// проверяется отдельно, потому что ранний возврат обходит и проверку вида узла.
    #[test]
    fn the_same_status_is_always_allowed() {
        for status in Status::all() {
            assert!(
                Status::can_transition(*status, *status, Kind::Task),
                "{status:?} -> {status:?} должно быть no-op"
            );
            assert!(check_transition(*status, *status, Kind::Task, false).is_ok());
        }
    }

    #[test]
    fn reopening_needs_force() {
        assert!(Status::requires_force(Status::Done, Status::Todo));
        assert!(Status::requires_force(Status::Cancelled, Status::Todo));
        assert!(!Status::requires_force(Status::Todo, Status::InProgress));
        assert!(!Status::requires_force(Status::Idea, Status::Backlog));
        let err = check_transition(Status::Done, Status::Todo, Kind::Task, false).unwrap_err();
        assert_eq!(err.code(), "FORCE_REQUIRED");
        assert!(check_transition(Status::Done, Status::Todo, Kind::Task, true).is_ok());
    }
}
