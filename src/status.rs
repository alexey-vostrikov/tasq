//! Статусы, виды узлов и матрица переходов (README §7).

use serde::Serialize;

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
    /// Работа прекращена, но не доведена: эпик закрыт с открытыми детьми.
    ///
    /// Отдельный статус, а не `done` с флагом, потому что это разные ответы на разные вопросы:
    /// «работа по узлу закончена» (`terminal`) — да, «работа сделана» (`counts_done`) — нет.
    /// До него единственным исходом закрытия с хвостами был отказ или ложное `done`.
    Incomplete,
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
            Status::Incomplete => "incomplete",
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
            "incomplete" => Status::Incomplete,
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
            Status::Incomplete,
        ]
    }

    /// Закрыт ли узел: работа по нему прекращена (справочник `statuses.terminal`).
    ///
    /// Закрытость и выполненность — разные вопросы, и до v11 они были одним: закрытым считалось
    /// `done` или `cancelled`, а «засчитать как сделанное» не спрашивал никто. С появлением
    /// `incomplete` это перестало работать: он закрыт, но не выполнен.
    pub fn is_terminal(self) -> bool {
        matches!(self, Status::Done | Status::Cancelled | Status::Incomplete)
    }

    /// Засчитывается ли узел как выполненный (справочник `statuses.counts_done`).
    ///
    /// Только `done`: `cancelled` и `incomplete` — исходы закрытия без выполненной работы.
    /// По этому признаку считается прогресс и строится burn-up, иначе эпик, прекращённый с
    /// четырьмя открытыми задачами, показывал бы 100 %.
    pub fn counts_as_done(self) -> bool {
        self == Status::Done
    }

    /// Статусы, из которых виден этот статус допустим.
    fn predecessors(self) -> &'static [Status] {
        use Status::*;
        match self {
            // В идею возвращают сырую мысль, которую рано держать в очереди: из очереди
            // (`backlog`, `planned`, `todo`) и из отложенного — то есть из любого состояния
            // ДО начала работы, но не из закрытого.
            Idea => &[Backlog, Planned, Todo, Deferred],
            Backlog => &[
                Idea, Planned, Todo, InProgress, Blocked, Review, Deferred, Incomplete,
            ],
            Planned => &[Idea, Backlog, Deferred, Incomplete],
            Todo => &[
                Idea, Backlog, InProgress, Blocked, Deferred, Done, Cancelled, Incomplete,
            ],
            InProgress => &[
                Idea, Backlog, Planned, Todo, Review, Blocked, Done, Incomplete,
            ],
            Blocked => &[Idea, Backlog, Planned, Todo, InProgress, Review],
            Review => &[InProgress, Incomplete],
            // `incomplete` здесь — не реоткрытие: узел закрыт, и его можно довести до `done`,
            // когда хвосты убраны. Это единственный «прямой» путь из закрытого в выполненное,
            // и он не требует `--force`: человек не отменяет решение, а доводит его.
            Done => &[InProgress, Review, Incomplete],
            Cancelled => &[
                Idea, Backlog, Planned, Todo, InProgress, Review, Blocked, Deferred, Incomplete,
            ],
            Deferred => &[Idea, Backlog, Planned, Todo, InProgress],
            // Закрыть с хвостами можно из любого состояния, в котором работа уже начата (или
            // признано, что её не будет): из очереди `todo`, из `planned` (эпик, до которого
            // руки не дошли) и из работы. Из `idea` — только с `--force`: там ещё ничего не
            // решено, и `incomplete` был бы преждевременным итогом.
            Incomplete => &[
                Backlog, Planned, Todo, InProgress, Review, Blocked, Deferred,
            ],
        }
    }

    /// Переход требует явного `force`: возврат из закрытого состояния.
    /// Нужен ли `--force`, чтобы вернуть закрытый узел в работу.
    ///
    /// Само правило — структура (какие пары «закрытый → в работу» существуют), но его
    /// СТРОГОСТЬ — политика проекта: у одного проекта реоткрытие требует явного подтверждения,
    /// у другого (где закрывают по ошибке часто) — нет. Поэтому функция отвечает только на
    /// «есть ли пара», а нужен ли флаг — решает [`Reopen`].
    ///
    /// `incomplete` в списке отсутствует намеренно: это «закрыто, но не закончено», и вернуть
    /// такой узел в работу — не исключение из правила, а его прямой смысл. Требовать `--force`
    /// значило бы наказывать за честную запись.
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

/// Политика реоткрытия закрытого узла — свойство проекта, а не кода.
///
/// Это первое, что действительно хочется менять без релиза: в проекте, где задачи закрывают
/// по ошибке, `--force` на каждое реоткрытие только мешает; в проекте с аудитом — наоборот,
/// обязателен. К матрице переходов политика не относится: она не меняет, КУДА можно, она
/// меняет, нужен ли явный флаг.
// `Default` выводится, а не пишется руками: значение по умолчанию — часть описания типа, и
// держать его отдельным блоком значит давать повод разойтись.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reopen {
    /// По умолчанию: вернуть `done`/`cancelled` в работу можно только с `--force`.
    #[default]
    ForceRequired,
    /// Реоткрытие разрешено без флага (проект сам так решил).
    Allowed,
}

impl Reopen {
    /// Разбор значения из `projects.metadata.reopen`.
    ///
    /// Неизвестное значение — ошибка, а не «по умолчанию»: опечатка в настройке политики
    /// молча вернула бы строгий режим, и человек не понял бы, почему флаг всё ещё нужен.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "force_required" | "force" => Some(Reopen::ForceRequired),
            "allow" | "allowed" => Some(Reopen::Allowed),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Reopen::ForceRequired => "force_required",
            Reopen::Allowed => "allow",
        }
    }

    /// Нужен ли флаг для этой пары при этой политике.
    fn needs_force(self, from: Status, to: Status) -> bool {
        match self {
            Reopen::ForceRequired => Status::requires_force(from, to),
            Reopen::Allowed => false,
        }
    }
}

/// SQL-условие «статус в этой колонке не закрыт».
///
/// Закрытость до v11 была литералом `IN ('done','cancelled')` и жила в девятнадцати запросах
/// и четырёх представлениях. С появлением `incomplete` каждый такой литерал стал бы ошибкой:
/// забытое место показало бы прекращённый эпик работающим. Здесь одно место, из которого
/// условие берётся, — и новый статус в справочнике меняет поведение всех запросов сразу.
///
/// Имя колонки подставляется вызывающим и всегда литерально из кода: параметром SQL имя
/// колонки не передать, поэтому это единственный безопасный способ не дублировать условие.
pub fn not_terminal_sql(column: &str) -> String {
    format!("{column} NOT IN (SELECT name FROM statuses WHERE terminal = 1)")
}

/// То же для «выполнено» — по признаку `counts_done`, а не по закрытости.
pub fn counts_done_sql(column: &str) -> String {
    format!("{column} IN (SELECT name FROM statuses WHERE counts_done = 1)")
}

/// Куда можно перейти из этого статуса у узла этого вида.
///
/// Нужен подсказке об отказе: «см. матрицу в README» для человека — лишний шаг, а для
/// MCP-агента — тупик, потому что README он не читает. Список строится из той же матрицы,
/// поэтому не может с ней разойтись.
pub fn allowed_next(from: Status, kind: Kind) -> Vec<Status> {
    Status::all()
        .iter()
        .copied()
        .filter(|to| *to != from && Status::can_transition(from, *to, kind))
        .collect()
}

/// Проверка перехода с выдачей предметной ошибки.
pub fn check_transition(from: Status, to: Status, kind: Kind, force: bool) -> Result<()> {
    check_with_policy(from, to, kind, force, Reopen::default())
}

/// То же, но с политикой реоткрытия проекта.
///
/// Отдельная функция, а не лишний параметр у `check_transition`: матрица переходов — правило
/// кода, политика реоткрытия — настройка проекта, и склеивать их в одной сигнатуре значило бы
/// заставлять каждого вызывающего знать про политику, даже когда он её не спрашивает.
pub fn check_with_policy(
    from: Status,
    to: Status,
    kind: Kind,
    force: bool,
    policy: Reopen,
) -> Result<()> {
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
    if policy.needs_force(from, to) && !force {
        return Err(Error::ForceRequired {
            from: from.as_str().to_string(),
            to: to.as_str().to_string(),
            reopen: policy.as_str().to_string(),
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
        assert_eq!(seen.len(), 11, "добавили статус — добавьте его в all()");
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

    /// `incomplete` — третий исход закрытия: закрыт, но не выполнен. Оба признака проверяются
    /// здесь, потому что на них держится всё остальное: прогресс, burn-up, списки.
    #[test]
    fn incomplete_is_closed_but_not_done() {
        assert!(Status::Incomplete.is_terminal(), "закрыт");
        assert!(!Status::Incomplete.counts_as_done(), "не выполнен");
        assert!(Status::Done.is_terminal() && Status::Done.counts_as_done());
        assert!(Status::Cancelled.is_terminal() && !Status::Cancelled.counts_as_done());
        assert!(!Status::Todo.is_terminal() && !Status::Todo.counts_as_done());
    }

    /// Из недоведённого эпика можно вернуться в работу без `--force`: смысл статуса в том, что
    /// решение уже принято и признано неполным, а не в том, чтобы наказывать за честность.
    #[test]
    fn incomplete_reopens_without_force() {
        use Status::*;
        assert!(check_transition(Incomplete, Todo, Kind::Task, false).is_ok());
        assert!(check_transition(Incomplete, InProgress, Kind::Task, false).is_ok());
        assert!(check_transition(Incomplete, Planned, Kind::Epic, false).is_ok());
        // Довести до `done`, когда хвосты убраны, — тоже без флага.
        assert!(check_transition(Incomplete, Done, Kind::Epic, false).is_ok());
        assert!(!Status::requires_force(Incomplete, Todo));
        assert!(!Status::requires_force(Incomplete, InProgress));
    }

    /// Закрыть с хвостами можно из очереди и из работы, но не из `idea`: там ещё ничего не
    /// решено, и `incomplete` был бы преждевременным итогом.
    #[test]
    fn incomplete_is_reachable_from_queue_and_work_only() {
        use Status::*;
        for from in [
            Backlog, Planned, Todo, InProgress, Review, Blocked, Deferred,
        ] {
            assert!(
                Status::can_transition(from, Incomplete, Kind::Epic)
                    || Status::can_transition(from, Incomplete, Kind::Task),
                "{from:?} -> incomplete должно быть разрешено хотя бы одному виду"
            );
        }
        assert!(!Status::can_transition(Idea, Incomplete, Kind::Epic));
        assert!(!Status::can_transition(Idea, Incomplete, Kind::Task));
        // Обратно в идею из недоведённого не возвращают: это уже не сырая мысль.
        assert!(!Status::can_transition(Incomplete, Idea, Kind::Task));
    }
}
