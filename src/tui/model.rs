//! Модель экрана: состояние, клавиши и решения — без терминала.
//!
//! Здесь нет ни одного вызова crossterm и ни одного кадра. Причина разделения: пока разбор
//! нажатий живёт в цикле отрисовки, проверить его можно только живым терминалом, и проверяется
//! он тогда глазами. Здесь нажатие — значение, решение — значение, а терминал остаётся тонким
//! слоем: прочитать событие, отдать в `apply`, нарисовать кадр.
//!
//! Три правила, которые задают форму кода:
//!
//! * **Клавиша — данные.** Таблица «режим + клавиша → намерение» целиком проверяется тестом,
//!   потому что забытая строка — это молча неработающая клавиша, а не ошибка сборки.
//! * **Нажатие — не отпускание.** Автоповтор и отпускание клавиши нажатием не считаются:
//!   иначе удержанный `j` уезжает на десяток строк и читается как дёрганый скролл.
//! * **Выход — значением.** Решение вернуть `Quit` приходит вызывающему как `Outcome::Quit`, а
//!   не как `return` из середины обработчика: иначе ветку нельзя вынести в функцию, не меняя
//!   поведения.

/// Как показана доска: списком строк или колонками статусов.
///
/// Два вида одной и той же доски, а не два экрана: данные те же (`overview::board`), отличается
/// только раскладка. Поэтому переключение не перечитывает базу и не сбрасывает курсор.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    /// Таблица: строка на узел, статус колонкой.
    #[default]
    List,
    /// Канбан: колонки статусов рядом, узлы внутри.
    Kanban,
}

impl View {
    pub fn title(self) -> &'static str {
        match self {
            View::List => "список",
            View::Kanban => "канбан",
        }
    }
}

/// Куда направлены клавиши движения: на список узлов или на панель проектов.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Board,
    Projects,
}

/// Экран, на котором находится человек.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Доска: колонки статусов и строки узлов.
    #[default]
    Board,
    /// Карточка узла.
    Node,
    /// Выбор исхода работы: завершена, снята, провалена.
    Outcome,
    /// Выбор нового статуса из того, что разрешает матрица.
    Status,
    /// Ввод заметки или комментария к узлу.
    Note,
    /// Подтверждение опасного действия.
    Confirm,
    /// Форма узла: создание или правка.
    Form,
    /// Поиск по базе.
    Search,
    /// Справка по клавишам.
    Help,
}

impl Mode {
    /// Название для строки состояния — оно же попадает в подсказку внизу экрана.
    pub fn title(self) -> &'static str {
        match self {
            Mode::Board => "доска",
            Mode::Node => "узел",
            Mode::Outcome => "исход работы",
            Mode::Status => "новый статус",
            Mode::Note => "текст к узлу",
            Mode::Confirm => "подтверждение",
            Mode::Form => "форма",
            Mode::Search => "поиск",
            Mode::Help => "справка",
        }
    }
}

/// Клавиша в терминах модели.
///
/// Отдельный тип, а не `crossterm::event::KeyCode`: терминальная библиотека приносит вместе с
/// кодом вид события и модификаторы, и тащить её в модель значило бы проверять клавиши только
/// через живой терминал. Перевод — одна функция на границе (`key_of`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Up,
    Down,
    Left,
    Right,
    Backspace,
}

/// Что человек попросил сделать.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    MoveDown,
    MoveUp,
    ColumnLeft,
    ColumnRight,
    Open,
    Back,
    Help,
    Claim,
    Release,
    ChangeStatus,
    Create,
    Edit,
    AddNote,
    AddComment,
    Search,
    Quit,
    FieldNext,
    FieldPrev,
    Confirm,
    Cancel,
    Type(char),
    Erase,
    /// Работа завершена: узел закрывается как сделанный.
    CompleteWork,
    /// Работа снята без результата: узел возвращается в `todo`.
    ReleaseWork,
    /// Работа провалена: узел возвращается в `todo`, причина в чекпойнте.
    FailWork,
    /// Фокус на панель проектов и обратно.
    FocusNext,
    /// Другой вид доски: список или канбан.
    ToggleView,
    /// Проект выше в панели.
    ProjectUp,
    /// Проект ниже в панели.
    ProjectDown,
    /// Переключиться на выбранный проект.
    ProjectOpen,
    /// Следующая цветовая тема.
    NextTheme,
    /// Запустить поиск по введённому запросу.
    SearchRun,
    /// Открыть найденный узел.
    SearchOpen,
    /// Записать форму (создать или изменить узел).
    SubmitForm,
    /// Результат поиска выше/ниже.
    ResultDown,
    ResultUp,
    /// Выбран статус под номером: буква — это позиция в списке допустимых.
    ChooseStatus(char),
    /// Подтверждённый переход в выбранный статус.
    ApplyStatus,
}

/// Намерение по режиму и клавише; `None` — клавиша здесь ничего не значит.
///
/// Таблица, а не цепочка `if`: её видно целиком и её же удобно сверять тестом по всем строкам.
/// Неизвестная клавиша молчит осознанно — сообщать о ней в строке состояния значило бы спорить
/// с человеком на каждое нажатие стрелки, которую экран и так не использует.
pub fn intent(mode: Mode, key: Key) -> Option<Intent> {
    use Intent as I;
    use Key as K;

    // В форме и поиске печатный символ — это ВВОД, поэтому общая таблица к ним не применяется:
    // `q` в поле названия — буква, а не выход.
    match mode {
        Mode::Note => {
            return match key {
                K::Char(c) => Some(I::Type(c)),
                K::Backspace => Some(I::Erase),
                K::Enter => Some(I::SubmitForm),
                K::Esc => Some(I::Cancel),
                _ => None,
            };
        }
        Mode::Form => {
            return match key {
                K::Char(c) => Some(I::Type(c)),
                K::Backspace => Some(I::Erase),
                K::Tab | K::Down => Some(I::FieldNext),
                K::BackTab | K::Up => Some(I::FieldPrev),
                K::Enter => Some(I::SubmitForm),
                K::Esc => Some(I::Cancel),
                _ => None,
            };
        }
        Mode::Search => {
            // В поиске стрелки листают РЕЗУЛЬТАТЫ, а не поля: поле здесь одно, и движение по нему
            // ничего не значит. Ввод идёт в строку запроса.
            return match key {
                K::Char(c) => Some(I::Type(c)),
                K::Backspace => Some(I::Erase),
                K::Down => Some(I::ResultDown),
                K::Up => Some(I::ResultUp),
                K::Enter => Some(I::SearchOpen),
                K::Esc => Some(I::Cancel),
                _ => None,
            };
        }
        Mode::Status => {
            // Номера, а не буквы: список допустимых переходов приходит из матрицы и меняется от
            // узла к узлу, поэтому фиксированной таблицы клавиш здесь быть не может.
            return match key {
                K::Char(c @ '1'..='9') => Some(I::ChooseStatus(c)),
                K::Esc => Some(I::Cancel),
                _ => None,
            };
        }
        Mode::Outcome => {
            // Три исхода — три клавиши, и ни одна не совпадает с теми, что двигают курсор:
            // случайное нажатие в этом режиме не должно закрывать узел.
            return match key {
                K::Char('d') | K::Char('D') => Some(I::CompleteWork),
                K::Char('x') | K::Char('X') => Some(I::ReleaseWork),
                K::Char('f') | K::Char('F') => Some(I::FailWork),
                K::Esc => Some(I::Cancel),
                _ => None,
            };
        }
        Mode::Confirm => {
            return match key {
                K::Char('y') | K::Char('Y') | K::Enter => Some(I::Confirm),
                K::Char('n') | K::Char('N') | K::Esc => Some(I::Cancel),
                _ => None,
            };
        }
        Mode::Help => {
            return match key {
                K::Esc | K::Char('q') | K::Char('?') | K::Enter => Some(I::Back),
                _ => None,
            };
        }
        Mode::Board | Mode::Node => {}
    }

    match key {
        K::Char('j') | K::Down => Some(I::MoveDown),
        K::Char('k') | K::Up => Some(I::MoveUp),
        K::Char('h') | K::Left => Some(I::ColumnLeft),
        K::Char('l') | K::Right => Some(I::ColumnRight),
        K::Enter => Some(I::Open),
        K::Esc => Some(I::Back),
        K::Char('?') => Some(I::Help),
        K::Char('c') => Some(I::Claim),
        K::Char('r') => Some(I::Release),
        K::Char('s') => Some(I::ChangeStatus),
        K::Char('a') => Some(I::Create),
        K::Char('e') => Some(I::Edit),
        K::Char('n') => Some(I::AddNote),
        K::Char('m') => Some(I::AddComment),
        K::Char('/') => Some(I::Search),
        K::Char('v') => Some(I::ToggleView),
        K::Char('T') => Some(I::NextTheme),
        K::Tab => Some(I::FocusNext),
        K::Char('q') => Some(I::Quit),
        _ => None,
    }
}

/// Клавиши панели проектов.
///
/// Отдельная таблица, а не ветка в общей: на панели `j`/`k` двигают проекты, а не узлы, и путать
/// эти два движения нельзя — иначе человек переключает проект, думая, что листает задачи.
pub fn panel_intent(key: Key) -> Option<Intent> {
    use Intent as I;
    use Key as K;
    match key {
        K::Char('j') | K::Down => Some(I::ProjectDown),
        K::Char('k') | K::Up => Some(I::ProjectUp),
        K::Enter => Some(I::ProjectOpen),
        K::Esc | K::Tab => Some(I::FocusNext),
        _ => None,
    }
}

/// Что модели нужно от вызывающего.
///
/// `Act` — не действие, а просьба: узел, аренду и статус делает библиотека, модель про них
/// ничего не знает и знать не должна.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Ничего не изменилось; перерисовывать не нужно.
    Ignored,
    /// Изменилось состояние экрана: перерисовать.
    Redraw,
    /// Спросить подтверждение (текст — что именно подтверждают).
    Ask(Intent),
    /// Выполнить просьбу модели.
    Act(Intent),
    /// Выйти из TUI.
    Quit,
}

/// Состояние экрана.
///
/// Размеры колонок — числа, а не данные: модели нужно только знать, докуда можно двигать курсор.
/// Сами строки живут у вызывающего, и это ещё одна причина, по которой модель проверяема без базы.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct State {
    pub mode: Mode,
    /// Сколько строк в каждой колонке — граница движения по вертикали.
    pub rows: Vec<usize>,
    pub column: usize,
    pub row: usize,
    /// Введённый текст поиска или формы.
    pub input: String,
    /// Экран, на который вернёт `Back` (из справки — туда, откуда пришли).
    pub back_to: Option<Mode>,
    /// Что подтверждают, пока экран `Confirm`.
    pub pending: Option<Intent>,
    /// Допустимые переходы из текущего статуса — их нумерует экран выбора.
    ///
    /// Заполняется вызывающим перед открытием выбора: матрица и вид узла живут в библиотеке, и
    /// второй их экземпляр в модели разошёлся бы с первым.
    pub statuses: Vec<String>,
    /// Статус, выбранный под номером и ждущий подтверждения.
    pub chosen_status: Option<String>,
    /// Вид доски: список или канбан.
    pub view: View,
    /// Что получает движение: узлы или панель проектов.
    pub focus: Focus,
    /// Сколько проектов в базе — граница движения по панели.
    pub projects: usize,
    /// Номер проекта, который показан на доске.
    pub active: usize,
    /// Поля формы: подпись и значение. Заполняется вызывающим перед открытием формы.
    pub fields: Vec<Field>,
    /// Номер поля, в которое идёт ввод.
    pub field: usize,
    /// Что делаем формой: создать узел (`None`) или править этот узел (`Some(адрес)`).
    pub editing: Option<String>,
    /// Найденные узлы: адрес и название — то, что видно в списке результатов.
    pub results: Vec<(String, String)>,
    /// Номер выбранного результата поиска.
    pub result: usize,
}

/// Поле формы: подпись для экрана и значение.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Field {
    pub label: &'static str,
    pub value: String,
}

impl State {
    /// Поле, в которое идёт ввод: у формы — текущее, у поиска — строка запроса.
    fn editing_field(&mut self) -> Option<&mut String> {
        if matches!(self.mode, Mode::Form | Mode::Note) {
            self.fields
                .get_mut(self.field)
                .map(|field| &mut field.value)
        } else {
            Some(&mut self.input)
        }
    }
}

impl State {
    /// Состояние доски с известным числом строк по колонкам.
    pub fn new(rows: Vec<usize>) -> Self {
        Self {
            rows,
            ..Self::default()
        }
    }

    /// Сколько строк в текущей колонке.
    pub fn rows_here(&self) -> usize {
        self.rows.get(self.column).copied().unwrap_or(0)
    }

    /// Применяет намерение и отвечает, что делать дальше.
    ///
    /// Метод меняет только состояние экрана: ни база, ни терминал здесь не участвуют.
    pub fn apply(&mut self, intent: Intent) -> Outcome {
        use Intent as I;
        use Outcome as O;

        // Подтверждение разбирается первым: на экране `Confirm` любое намерение означает ответ,
        // а не новое действие — иначе `y` ушло бы в доску как обычная клавиша.
        if self.mode == Mode::Confirm {
            return match intent {
                I::Confirm => {
                    let pending = self.pending.take().unwrap_or(I::Cancel);
                    self.mode = self.back_to.unwrap_or(Mode::Board);
                    self.back_to = None;
                    O::Act(pending)
                }
                I::Cancel => {
                    self.pending = None;
                    self.mode = self.back_to.unwrap_or(Mode::Board);
                    self.back_to = None;
                    O::Redraw
                }
                _ => O::Ignored,
            };
        }

        match intent {
            I::MoveDown => self.step(1),
            I::MoveUp => self.step(-1),
            I::ColumnRight => self.shift_column(1),
            I::ColumnLeft => self.shift_column(-1),
            I::Open => match self.mode {
                Mode::Board => {
                    self.mode = Mode::Node;
                    O::Redraw
                }
                _ => O::Act(I::Open),
            },
            I::Back => match self.mode {
                Mode::Board => O::Ignored,
                Mode::Search => {
                    self.input.clear();
                    self.mode = Mode::Board;
                    O::Redraw
                }
                _ => {
                    self.mode = self.back_to.take().unwrap_or(Mode::Board);
                    O::Redraw
                }
            },
            I::Help => {
                if self.mode == Mode::Help {
                    return O::Ignored;
                }
                self.back_to = Some(self.mode);
                self.mode = Mode::Help;
                O::Redraw
            }
            I::Search => {
                self.input.clear();
                self.mode = Mode::Search;
                O::Redraw
            }
            I::Type(c) => {
                if let Some(text) = self.editing_field() {
                    text.push(c);
                }
                // В поиске ввод меняет запрос, а значит и результаты: об этом сообщается
                // вызывающему, иначе список остался бы от предыдущей буквы.
                if self.mode == Mode::Search {
                    return O::Act(I::SearchRun);
                }
                O::Redraw
            }
            I::Erase => {
                if let Some(text) = self.editing_field() {
                    text.pop();
                }
                if self.mode == Mode::Search {
                    return O::Act(I::SearchRun);
                }
                O::Redraw
            }
            I::FieldNext => {
                if self.mode == Mode::Form {
                    // По кругу: из последнего поля Tab ведёт в первое, и выйти из формы можно
                    // только Esc или записью — «Tab в никуда» читается как зависание.
                    let count = self.fields.len();
                    if count > 0 {
                        self.field = (self.field + 1) % count;
                        return O::Redraw;
                    }
                }
                O::Act(I::FieldNext)
            }
            I::FieldPrev => {
                if self.mode == Mode::Form {
                    let count = self.fields.len();
                    if count > 0 {
                        self.field = (self.field + count - 1) % count;
                        return O::Redraw;
                    }
                }
                O::Act(I::FieldPrev)
            }
            I::Quit => {
                // Незаписанный ввод терять молча нельзя: `q` в форме — это вопрос, а не выход.
                if !self.input.is_empty() {
                    self.back_to = Some(self.mode);
                    self.pending = Some(I::Quit);
                    self.mode = Mode::Confirm;
                    O::Ask(I::Quit)
                } else {
                    O::Quit
                }
            }
            I::Release => {
                // Отпускание аренды — не одно действие, а три разных, и разница между ними в
                // СТАТУСЕ узла: завершённая работа закрывает его, снятая и проваленная
                // возвращают в `todo`. Поэтому сперва выбор исхода, потом подтверждение: одно
                // нажатие не должно решать за человека, закрыта задача или нет.
                self.mode = Mode::Outcome;
                O::Redraw
            }
            I::CompleteWork | I::ReleaseWork | I::FailWork => {
                self.back_to = Some(Mode::Board);
                self.pending = Some(intent);
                self.mode = Mode::Confirm;
                O::Ask(intent)
            }
            I::ChangeStatus => {
                // Не свободный ввод, а список из матрицы: набрать недопустимый статус руками
                // нельзя, и отказ приходит не из глубины SQL.
                // Пустой список — это «отсюда никуда нельзя»: экран выбора не открывается, а
                // что сказать человеку, решает вызывающий (у модели нет строки состояния).
                if self.statuses.is_empty() {
                    return O::Ignored;
                }
                self.mode = Mode::Status;
                O::Redraw
            }
            I::ChooseStatus(digit) => {
                let index = (digit as u8).wrapping_sub(b'1') as usize;
                let Some(status) = self.statuses.get(index).cloned() else {
                    return O::Ignored;
                };
                self.chosen_status = Some(status);
                self.pending = Some(I::ApplyStatus);
                self.back_to = Some(Mode::Board);
                self.mode = Mode::Confirm;
                O::Ask(I::ApplyStatus)
            }
            I::Cancel => {
                // Отмена в выборе (исхода, статуса, формы) возвращает на доску: иначе экран
                // остаётся в режиме, из которого нет выхода, кроме выбора.
                if matches!(
                    self.mode,
                    Mode::Outcome | Mode::Status | Mode::Form | Mode::Note
                ) {
                    self.mode = Mode::Board;
                    self.fields.clear();
                    self.field = 0;
                    self.editing = None;
                    O::Redraw
                } else {
                    O::Ignored
                }
            }
            I::NextTheme => O::Act(I::NextTheme),
            I::ToggleView => {
                self.view = match self.view {
                    View::List => View::Kanban,
                    View::Kanban => View::List,
                };
                O::Redraw
            }
            I::FocusNext => {
                // Без проектов панель пуста, и фокус на ней означал бы, что клавиши движения
                // перестали двигать то, что видно.
                if self.projects == 0 {
                    return O::Ignored;
                }
                self.focus = match self.focus {
                    Focus::Board => Focus::Projects,
                    Focus::Projects => Focus::Board,
                };
                O::Redraw
            }
            I::ProjectUp => self.step_project(-1),
            I::ProjectDown => self.step_project(1),
            I::ProjectOpen => {
                self.focus = Focus::Board;
                O::Act(I::ProjectOpen)
            }
            I::SearchRun | I::SearchOpen | I::SubmitForm | I::ResultDown | I::ResultUp => {
                O::Act(intent)
            }
            I::Confirm | I::ApplyStatus => O::Ignored,
            I::Claim | I::Create | I::Edit | I::AddNote | I::AddComment => O::Act(intent),
        }
    }

    /// Движение по панели проектов.
    ///
    /// Курсор проекта НЕ подтягивается к границам молча: на панели из одного проекта движение
    /// ничего не меняет и отвечает «нечего делать», а не перерисовывает тот же кадр.
    fn step_project(&mut self, delta: isize) -> Outcome {
        if self.projects == 0 {
            return Outcome::Ignored;
        }
        let next = self.active as isize + delta;
        let clamped = next.clamp(0, self.projects as isize - 1) as usize;
        if clamped == self.active {
            Outcome::Ignored
        } else {
            self.active = clamped;
            Outcome::Redraw
        }
    }

    /// Движение по строкам текущей колонки с ограничением по её длине.
    fn step(&mut self, delta: isize) -> Outcome {
        let len = self.rows_here();
        if len == 0 {
            self.row = 0;
            return Outcome::Ignored;
        }
        let next = self.row as isize + delta;
        let clamped = next.clamp(0, len as isize - 1) as usize;
        if clamped == self.row {
            Outcome::Ignored
        } else {
            self.row = clamped;
            Outcome::Redraw
        }
    }

    /// Переход между колонками: курсор остаётся в пределах новой колонки.
    fn shift_column(&mut self, delta: isize) -> Outcome {
        if self.rows.is_empty() {
            return Outcome::Ignored;
        }
        let next = self.column as isize + delta;
        let clamped = next.clamp(0, self.rows.len() as isize - 1) as usize;
        if clamped == self.column {
            return Outcome::Ignored;
        }
        self.column = clamped;
        // Строка не переносится «как была»: в новой колонке строк может быть меньше, и курсор
        // обязан остаться внутри неё, иначе `Enter` откроет узел из чужого столбца.
        let len = self.rows_here();
        self.row = if len == 0 { 0 } else { self.row.min(len - 1) };
        Outcome::Redraw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Intent as I;
    use Key as K;

    /// Таблица клавиш проверяется ЦЕЛИКОМ: каждая строка — отдельное утверждение, иначе
    /// забытая строка (например `?` в карточке) проходит незамеченной, а на экране это клавиша,
    /// которая молчит.
    #[test]
    fn every_board_key_maps_to_its_intention() {
        let table = [
            (K::Char('j'), I::MoveDown),
            (K::Down, I::MoveDown),
            (K::Char('k'), I::MoveUp),
            (K::Up, I::MoveUp),
            (K::Char('h'), I::ColumnLeft),
            (K::Left, I::ColumnLeft),
            (K::Char('l'), I::ColumnRight),
            (K::Right, I::ColumnRight),
            (K::Enter, I::Open),
            (K::Esc, I::Back),
            (K::Char('?'), I::Help),
            (K::Char('c'), I::Claim),
            (K::Char('r'), I::Release),
            (K::Char('s'), I::ChangeStatus),
            (K::Char('a'), I::Create),
            (K::Char('e'), I::Edit),
            (K::Char('n'), I::AddNote),
            (K::Char('m'), I::AddComment),
            (K::Char('/'), I::Search),
            (K::Char('q'), I::Quit),
        ];
        for (key, expected) in table {
            assert_eq!(intent(Mode::Board, key), Some(expected), "на доске {key:?}");
            assert_eq!(
                intent(Mode::Node, key),
                Some(expected),
                "в карточке узла {key:?}"
            );
        }
    }

    /// В полях ввода печатный символ — это буква, а не команда: `q` в названии узла не выходит.
    #[test]
    fn typing_in_a_field_is_text_not_a_command() {
        for mode in [Mode::Form, Mode::Search] {
            assert_eq!(intent(mode, K::Char('q')), Some(I::Type('q')), "{mode:?}");
            assert_eq!(intent(mode, K::Char('c')), Some(I::Type('c')), "{mode:?}");
            assert_eq!(intent(mode, K::Char('?')), Some(I::Type('?')), "{mode:?}");
            assert_eq!(intent(mode, K::Backspace), Some(I::Erase), "{mode:?}");
            // Enter в форме записывает, в поиске — открывает выбранный результат: одно и то же
            // нажатие в двух режимах значит разное, и это правило живёт в таблице, а не в коде.
            let expected = if mode == Mode::Search {
                I::SearchOpen
            } else {
                I::SubmitForm
            };
            assert_eq!(intent(mode, K::Enter), Some(expected), "{mode:?}");
            assert_eq!(intent(mode, K::Esc), Some(I::Cancel), "{mode:?}");
            // Tab между полями есть только в форме: в поиске поле одно, и движение по нему
            // ничего не значит — там стрелки листают РЕЗУЛЬТАТЫ.
            if mode == Mode::Form {
                assert_eq!(intent(mode, K::Tab), Some(I::FieldNext), "{mode:?}");
                assert_eq!(intent(mode, K::BackTab), Some(I::FieldPrev), "{mode:?}");
            } else {
                assert_eq!(intent(mode, K::Tab), None, "{mode:?}");
                assert_eq!(intent(mode, K::Down), Some(I::ResultDown), "{mode:?}");
                assert_eq!(intent(mode, K::Up), Some(I::ResultUp), "{mode:?}");
            }
        }
    }

    /// Подтверждение отвечает только на `y` и `n`; остальные клавиши его не закрывают.
    #[test]
    fn confirm_answers_only_yes_and_no() {
        for key in [K::Char('y'), K::Char('Y'), K::Enter] {
            assert_eq!(intent(Mode::Confirm, key), Some(I::Confirm), "{key:?}");
        }
        for key in [K::Char('n'), K::Char('N'), K::Esc] {
            assert_eq!(intent(Mode::Confirm, key), Some(I::Cancel), "{key:?}");
        }
        for key in [K::Char('j'), K::Char('q'), K::Tab, K::Down] {
            assert_eq!(intent(Mode::Confirm, key), None, "{key:?} закрыл вопрос");
        }
    }

    /// Неизвестная клавиша молчит, а не делает что-то «по умолчанию».
    #[test]
    fn an_unbound_key_does_nothing() {
        for key in [K::Char('z'), K::Char('ы'), K::Backspace] {
            assert_eq!(intent(Mode::Board, key), None, "{key:?}");
        }
    }

    /// Панель проектов и второй вид доски привязаны к клавишам, и это проверяется отдельно:
    /// пока клавиша не привязана, о ней молчат и тест «неизвестная молчит», и тест таблицы —
    /// получается функция, которой нет.
    #[test]
    fn the_project_panel_and_the_view_switch_have_their_keys() {
        assert_eq!(intent(Mode::Board, K::Tab), Some(I::FocusNext));
        assert_eq!(intent(Mode::Board, K::Char('v')), Some(I::ToggleView));

        assert_eq!(panel_intent(K::Char('j')), Some(I::ProjectDown));
        assert_eq!(panel_intent(K::Down), Some(I::ProjectDown));
        assert_eq!(panel_intent(K::Char('k')), Some(I::ProjectUp));
        assert_eq!(panel_intent(K::Enter), Some(I::ProjectOpen));
        assert_eq!(panel_intent(K::Tab), Some(I::FocusNext));
        assert_eq!(panel_intent(K::Esc), Some(I::FocusNext));
        // На панели `q` не выходит: выход — общая клавиша, и панель её не перехватывает.
        assert_eq!(panel_intent(K::Char('q')), None);
    }

    /// `v` переключает вид и обратно, `Tab` — фокус; без проектов фокус на панель не уходит.
    #[test]
    fn the_view_switches_and_focus_needs_projects() {
        let mut state = State::new(vec![1]);
        assert_eq!(state.view, View::List);
        assert_eq!(state.apply(I::ToggleView), Outcome::Redraw);
        assert_eq!(state.view, View::Kanban);
        assert_eq!(state.apply(I::ToggleView), Outcome::Redraw);
        assert_eq!(state.view, View::List);

        // Проектов нет — панели нет, и фокус остался бы на пустоте.
        assert_eq!(state.apply(I::FocusNext), Outcome::Ignored);
        assert_eq!(state.focus, Focus::Board);

        state.projects = 3;
        assert_eq!(state.apply(I::FocusNext), Outcome::Redraw);
        assert_eq!(state.focus, Focus::Projects);
        assert_eq!(state.apply(I::FocusNext), Outcome::Redraw);
        assert_eq!(state.focus, Focus::Board);
    }

    /// Курсор панели ходит по проектам и не выходит за границы; `Enter` просит переключение.
    #[test]
    fn the_project_cursor_stays_within_the_panel() {
        let mut state = State::new(vec![1]);
        state.projects = 3;
        assert_eq!(state.apply(I::ProjectUp), Outcome::Ignored, "уже на первом");
        assert_eq!(state.apply(I::ProjectDown), Outcome::Redraw);
        assert_eq!(state.active, 1);
        assert_eq!(state.apply(I::ProjectDown), Outcome::Redraw);
        assert_eq!(state.active, 2);
        assert_eq!(state.apply(I::ProjectDown), Outcome::Ignored, "край панели");

        assert_eq!(state.apply(I::ProjectOpen), Outcome::Act(I::ProjectOpen));
        assert_eq!(state.focus, Focus::Board, "после выбора фокус не вернулся");
    }

    /// Курсор не выходит за список ни вверх, ни вниз, и на пустой колонке не двигается.
    #[test]
    fn the_cursor_stays_inside_the_list() {
        let mut state = State::new(vec![3, 0]);
        assert_eq!(
            state.apply(I::MoveUp),
            Outcome::Ignored,
            "уже на первой строке"
        );
        assert_eq!(state.apply(I::MoveDown), Outcome::Redraw);
        assert_eq!(state.row, 1);
        assert_eq!(state.apply(I::MoveDown), Outcome::Redraw);
        assert_eq!(state.row, 2);
        assert_eq!(
            state.apply(I::MoveDown),
            Outcome::Ignored,
            "на последней строке движение вниз ничего не меняет"
        );
        assert_eq!(state.row, 2, "курсор уехал за последнюю строку");
        assert_eq!(
            state.apply(I::ColumnRight),
            Outcome::Redraw,
            "вторая колонка существует"
        );
        assert_eq!(
            (state.column, state.row),
            (1, 0),
            "пустая колонка ставит курсор в 0"
        );
        assert_eq!(
            state.apply(I::MoveDown),
            Outcome::Ignored,
            "в пустой колонке двигаться некуда"
        );
        assert_eq!(state.row, 0);
    }

    /// Переход в узкую колонку подтягивает курсор: иначе `Enter` открыл бы узел чужого столбца.
    #[test]
    fn switching_columns_clamps_the_row() {
        let mut state = State::new(vec![10, 2]);
        for _ in 0..9 {
            state.apply(I::MoveDown);
        }
        assert_eq!(state.row, 9);
        state.apply(I::ColumnRight);
        assert_eq!(
            (state.column, state.row),
            (1, 1),
            "курсор обязан войти в новую колонку"
        );
    }

    /// `q` в непустом поле спрашивает, а не выходит: незаписанный ввод не теряется молча.
    #[test]
    fn quitting_asks_when_the_input_is_not_empty() {
        let mut state = State::new(vec![1]);
        state.mode = Mode::Search;
        state.input = "половина запроса".to_string();
        assert_eq!(state.apply(I::Quit), Outcome::Ask(I::Quit));
        assert_eq!(state.mode, Mode::Confirm);

        assert_eq!(state.apply(I::Cancel), Outcome::Redraw);
        assert_eq!(state.mode, Mode::Search, "отмена вернула в поиск");
        assert_eq!(state.input, "половина запроса", "отмена не стёрла ввод");

        assert_eq!(state.apply(I::Quit), Outcome::Ask(I::Quit));
        assert_eq!(
            state.apply(I::Confirm),
            Outcome::Act(I::Quit),
            "подтвердили выход"
        );
        assert_eq!(state.mode, Mode::Search, "доска ждёт решения вызывающего");
    }

    /// Пустое поле выходит сразу: спрашивать не о чем.
    #[test]
    fn quitting_with_an_empty_field_exits_at_once() {
        let mut state = State::new(vec![1]);
        assert_eq!(state.apply(I::Quit), Outcome::Quit);
    }

    /// Опасные действия спрашивают подтверждение, безопасные — нет.
    #[test]
    fn risky_actions_ask_and_safe_ones_do_not() {
        // Смена статуса открывает ВЫБОР из матрицы, а подтверждение спрашивается после выбора
        // (см. `status_choice_lists_the_matrix_and_confirms`). Пустой список допустимых —
        // «отсюда никуда», и экран не открывается.
        let mut state = State::new(vec![1]);
        assert_eq!(state.apply(I::ChangeStatus), Outcome::Ignored);
        assert_eq!(
            state.mode,
            Mode::Board,
            "открылся выбор без допустимых статусов"
        );

        state.statuses = vec!["todo".to_string(), "in_progress".to_string()];
        assert_eq!(state.apply(I::ChangeStatus), Outcome::Redraw);
        assert_eq!(state.mode, Mode::Status);
        for intent in [I::Claim, I::Create, I::Edit, I::AddNote, I::AddComment] {
            let mut state = State::new(vec![1]);
            assert_eq!(
                state.apply(intent),
                Outcome::Act(intent),
                "{intent:?} спрашивает лишнее"
            );
        }
        // `Enter` на доске — не просьба к вызывающему, а переход режима: карточка строится из
        // уже прочитанного, второй раз в базу за ней не ходят.
        let mut state = State::new(vec![1]);
        assert_eq!(state.apply(I::Open), Outcome::Redraw);
        assert_eq!(state.mode, Mode::Node);
    }

    /// Справка возвращает туда, откуда пришли, а не всегда на доску.
    #[test]
    fn help_returns_to_where_it_was_called() {
        let mut state = State::new(vec![1]);
        state.mode = Mode::Node;
        assert_eq!(state.apply(I::Help), Outcome::Redraw);
        assert_eq!(state.mode, Mode::Help);
        assert_eq!(state.apply(I::Back), Outcome::Redraw);
        assert_eq!(state.mode, Mode::Node, "справка вернула не туда");
    }

    /// Ввод и стирание: печатный символ добавляется, Backspace убирает последний, на пустом
    /// вводе стирание не паникует.
    #[test]
    fn typing_and_erasing_shape_the_input() {
        let mut state = State::new(vec![1]);
        state.mode = Mode::Search;
        // В поиске ввод ЗАПУСКАЕТ поиск: без этого список результатов остался бы от предыдущей
        // буквы, и «н» показывал бы ответ на «на».
        for c in "abc".chars() {
            assert_eq!(state.apply(I::Type(c)), Outcome::Act(I::SearchRun));
        }
        assert_eq!(state.input, "abc");
        assert_eq!(state.apply(I::Erase), Outcome::Act(I::SearchRun));
        assert_eq!(state.input, "ab");
        state.input.clear();
        assert_eq!(state.apply(I::Erase), Outcome::Act(I::SearchRun));
        assert_eq!(
            state.input, "",
            "стирание на пустом вводе ничего не добавило"
        );
    }

    /// `Esc` в поиске гасит запрос, а не оставляет его висеть на экране.
    #[test]
    fn leaving_search_clears_the_query() {
        let mut state = State::new(vec![1]);
        state.apply(I::Search);
        state.apply(I::Type('x'));
        assert_eq!(state.apply(I::Back), Outcome::Redraw);
        assert_eq!(state.mode, Mode::Board);
        assert_eq!(state.input, "");
    }

    /// Открытие карточки и возврат из неё — два перехода режима, и оба видны вызывающему.
    #[test]
    fn the_card_opens_and_closes() {
        let mut state = State::new(vec![1]);
        assert_eq!(state.apply(I::Open), Outcome::Redraw);
        assert_eq!(state.mode, Mode::Node);
        assert_eq!(state.apply(I::Back), Outcome::Redraw);
        assert_eq!(state.mode, Mode::Board);
    }

    /// Отпускание аренды спрашивает ИСХОД, и только потом подтверждение: разница между исходами
    /// в статусе узла, и одно нажатие не должно решать за человека, закрыта задача или нет.
    #[test]
    fn releasing_asks_for_the_outcome_first() {
        let mut state = State::new(vec![1]);
        assert_eq!(state.apply(I::Release), Outcome::Redraw);
        assert_eq!(state.mode, Mode::Outcome, "не открылся выбор исхода");
        assert_eq!(
            state.pending, None,
            "вопрос о исхоте поставлен раньше времени"
        );

        for (key, work) in [
            (K::Char('d'), I::CompleteWork),
            (K::Char('x'), I::ReleaseWork),
            (K::Char('f'), I::FailWork),
        ] {
            let mut state = State::new(vec![1]);
            state.apply(I::Release);
            assert_eq!(intent(Mode::Outcome, key), Some(work), "{key:?}");
            assert_eq!(state.apply(work), Outcome::Ask(work), "{work:?}");
            assert_eq!(state.pending, Some(work));
            assert_eq!(state.mode, Mode::Confirm);
            assert_eq!(
                state.apply(I::Confirm),
                Outcome::Act(work),
                "{work:?}: подтверждение не сработало"
            );
        }
    }

    /// В выборе исхода нажатие мимо не закрывает узел: только три названные клавиши.
    #[test]
    fn only_the_three_outcome_keys_do_anything() {
        for key in [K::Char('j'), K::Char('q'), K::Char('r'), K::Tab, K::Enter] {
            assert_eq!(
                intent(Mode::Outcome, key),
                None,
                "{key:?} принята как исход"
            );
        }
        assert_eq!(
            intent(Mode::Outcome, K::Esc),
            Some(I::Cancel),
            "выйти из выбора исхода нечем"
        );
    }

    /// Отмена в выборе исхода возвращает на доску, а не оставляет экран без выхода.
    #[test]
    fn cancelling_the_outcome_returns_to_the_board() {
        let mut state = State::new(vec![1]);
        state.apply(I::Release);
        assert_eq!(state.apply(I::Cancel), Outcome::Redraw);
        assert_eq!(state.mode, Mode::Board);
        assert_eq!(state.pending, None);
    }

    /// Выбор статуса: список приходит из матрицы, номер выбирает, подтверждение — после.
    #[test]
    fn status_choice_lists_the_matrix_and_confirms() {
        let mut state = State::new(vec![1]);
        state.statuses = vec![
            "todo".to_string(),
            "in_progress".to_string(),
            "done".to_string(),
        ];
        assert_eq!(state.apply(I::ChangeStatus), Outcome::Redraw);
        assert_eq!(state.mode, Mode::Status);

        assert_eq!(
            intent(Mode::Status, K::Char('2')),
            Some(I::ChooseStatus('2'))
        );
        assert_eq!(
            state.apply(I::ChooseStatus('2')),
            Outcome::Ask(I::ApplyStatus)
        );
        assert_eq!(state.chosen_status.as_deref(), Some("in_progress"));
        assert_eq!(state.mode, Mode::Confirm);

        assert_eq!(state.apply(I::Confirm), Outcome::Act(I::ApplyStatus));
        assert_eq!(
            state.mode,
            Mode::Board,
            "после подтверждения экран не вернулся на доску"
        );
        assert_eq!(
            state.chosen_status.as_deref(),
            Some("in_progress"),
            "выбранный статус потерян до записи"
        );
    }

    /// Номер за пределами списка ничего не выбирает: иначе `9` при двух допустимых переходах
    /// подставил бы несуществующий статус.
    #[test]
    fn a_number_outside_the_list_chooses_nothing() {
        let mut state = State::new(vec![1]);
        state.statuses = vec!["todo".to_string()];
        state.apply(I::ChangeStatus);
        assert_eq!(state.apply(I::ChooseStatus('5')), Outcome::Ignored);
        assert_eq!(state.chosen_status, None);
        assert_eq!(state.mode, Mode::Status, "выбор закрылся без выбора");
    }

    /// В выборе статуса работают только цифры и Esc: `q` не выходит, `j` не двигает курсор —
    /// иначе случайное нажатие сменило бы статус узла.
    #[test]
    fn only_digits_and_escape_in_the_status_chooser() {
        for key in [K::Char('q'), K::Char('j'), K::Enter, K::Tab, K::Char('0')] {
            assert_eq!(intent(Mode::Status, key), None, "{key:?} принята как выбор");
        }
        assert_eq!(intent(Mode::Status, K::Esc), Some(I::Cancel));
        assert_eq!(
            intent(Mode::Status, K::Char('1')),
            Some(I::ChooseStatus('1'))
        );
        assert_eq!(
            intent(Mode::Status, K::Char('9')),
            Some(I::ChooseStatus('9'))
        );
    }

    /// Отмена после выбранного статуса его не применяет: экран возвращается на доску, а узел
    /// остаётся как был.
    #[test]
    fn cancelling_the_confirmation_drops_the_chosen_status() {
        let mut state = State::new(vec![1]);
        state.statuses = vec!["in_progress".to_string()];
        state.apply(I::ChangeStatus);
        state.apply(I::ChooseStatus('1'));
        assert_eq!(state.mode, Mode::Confirm);
        assert_eq!(state.apply(I::Cancel), Outcome::Redraw);
        assert_eq!(state.mode, Mode::Board);
        assert_eq!(
            state.apply(I::ApplyStatus),
            Outcome::Ignored,
            "отменённый переход всё равно применился"
        );
    }
}
