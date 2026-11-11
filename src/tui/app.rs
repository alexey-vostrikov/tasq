//! Состояние живого экрана: доска, курсор и перевод событий терминала в намерения.
//!
//! Между моделью (`model`) и терминалом стоит этот слой, и его границы выбраны так, чтобы
//! проверяемым осталось всё, кроме самого ввода-вывода:
//!
//! * `App` хранит прочитанную доску и состояние курсора — то, что видно на экране;
//! * `step(&mut App, Event) -> Outcome` разбирает ОДНО событие терминала и ничего не рисует;
//! * цикл в `tui.rs` остаётся тонким: нарисовать кадр, прочитать событие, отдать в `step`.
//!
//! Почему событие разбирается здесь, а не в цикле: пока разбор лежит в цикле, единственный способ
//! проверить клавишу — нажать её, а проверяется тогда то, что вспомнили нажать. Здесь же событие —
//! значение, и тест подаёт его напрямую, без псевдотерминала.

use std::collections::HashMap;

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};

use super::actions;
use super::model::{self, Intent, Key, Mode, Outcome, State};
use crate::lease::Outcome as LeaseOutcome;
use crate::overview;

/// Экран целиком: данные и положение курсора.
#[derive(Debug, Clone)]
pub struct App {
    /// Доска в том виде, в каком её отдал `overview::board`.
    pub board: overview::Board,
    /// Курсор, режим и ввод.
    pub state: State,
    /// Что сказать человеку в строке состояния (отказ последнего действия).
    pub message: Option<String>,
    /// Идентификатор сессии экрана: `TASQ_SESSION`, если задан, иначе свой.
    pub session: String,
    /// Токены аренды по адресам узлов: без них `release` ответит `NOT_YOUR_ATTEMPT`.
    ///
    /// Живут в памяти и теряются при выходе — что честно: аренда всё равно истечёт, и узел
    /// освободится сам. Хранить токены в базе значило бы сделать экран единственным, кто может
    /// отпустить взятую им работу.
    pub tokens: HashMap<String, String>,
}

impl App {
    /// Собирает экран вокруг уже прочитанной доски.
    ///
    /// Число строк по колонкам берётся из доски, а не из `state.rows`: это одно и то же число, и
    /// второй его экземпляр неизбежно разошёлся бы с первым — ровно та ошибка, из-за которой
    /// `--json` и таблица когда-то показывали разные списки.
    pub fn new(board: overview::Board) -> Self {
        let rows = board
            .columns
            .iter()
            .map(|column| column.rows.len())
            .collect();
        Self {
            board,
            state: State::new(rows),
            message: None,
            session: String::new(),
            tokens: HashMap::new(),
        }
    }

    /// То же, но с готовым идентификатором сессии: тест не должен зависеть от окружения.
    pub fn with_session(board: overview::Board, session: impl Into<String>) -> Self {
        let mut app = Self::new(board);
        app.session = session.into();
        app
    }

    /// Перечитывает доску и подтягивает курсор в её новые границы.
    ///
    /// Перечитывание — отдельное действие, а не побочный эффект отрисовки: экран показывает то,
    /// что прочитано, и человек видит, когда данные обновились, а когда нет.
    pub fn refresh(
        &mut self,
        conn: &rusqlite::Connection,
        project: &crate::project::Project,
    ) -> crate::Result<()> {
        // Узел под курсором запоминается ДО перечитывания и возвращается ПОСЛЕ: колонки
        // пересобираются по статусам, и узел, взятый в работу, переезжает из `todo` в
        // `in_progress`. Курсор на прежних координатах оказался бы на соседе — а следующий `r`
        // отпустил бы не тот узел (в живом прогоне это и вышло: `NOT_YOUR_ATTEMPT` на верном
        // нажатии).
        let keep = self.selected().map(|row| row.dot_id.clone());
        self.board = overview::board(conn, project)?;
        self.state.rows = self
            .board
            .columns
            .iter()
            .map(|column| column.rows.len())
            .collect();
        // Курсор после перечитывания может оказаться за концом колонки: узлы закрывают, колонки
        // исчезают. Без подтягивания `Enter` открыл бы пустоту.
        if self.state.column >= self.state.rows.len() {
            self.state.column = self.state.rows.len().saturating_sub(1);
        }
        let here = self.state.rows_here();
        self.state.row = if here == 0 {
            0
        } else {
            self.state.row.min(here - 1)
        };
        // Узел нашёлся — курсор идёт за ним; не нашёлся (закрыт, отфильтрован) — остаётся
        // подтянутым к границам колонки.
        if let Some(reference) = keep {
            self.select_by_id(&reference);
        }
        Ok(())
    }

    /// Ставит курсор на узел с этим адресом, если он ещё на доске.
    ///
    /// Нужно после каждого перечитывания: узел, взятый в работу, ПЕРЕЕЗЖАЕТ в другую колонку
    /// (`in_progress`), и курсор, оставленный «на тех же координатах», оказывается на соседнем
    /// узле. Дальше человек нажимает `r` — и отпускает не то, что взял, а соседа; в живом прогоне
    /// это выглядело как `NOT_YOUR_ATTEMPT` на верном нажатии.
    pub fn select_by_id(&mut self, dot_id: &str) -> bool {
        for (column_index, column) in self.board.columns.iter().enumerate() {
            if let Some(row_index) = column.rows.iter().position(|row| row.dot_id == dot_id) {
                self.state.column = column_index;
                self.state.row = row_index;
                return true;
            }
        }
        false
    }

    /// Строка под курсором, если она есть.
    pub fn selected(&self) -> Option<&overview::BoardRow> {
        self.board
            .columns
            .get(self.state.column)?
            .rows
            .get(self.state.row)
    }

    /// Значение текущей колонки в терминах статуса.
    pub fn selected_status(&self) -> Option<&str> {
        self.board
            .columns
            .get(self.state.column)
            .map(|column| column.status.as_str())
    }

    /// Название проекта — для строки состояния и заголовка.
    pub fn project(&self) -> &str {
        &self.board.project
    }

    /// Применяет одно событие терминала и отвечает, что делать циклу.
    ///
    /// Отпускание и автоповтор клавиши нажатием НЕ считаются: иначе удержанный `j` уезжает на
    /// десяток строк, и это читается как дёрганый скролл. Ресайз окна — повод перерисовать,
    /// а не сообщение об ошибке.
    pub fn step(&mut self, event: Event) -> Outcome {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Press => Outcome::Ignored,
            Event::Key(key) => match key_of(key) {
                Some(key) => match model::intent(self.state.mode, key) {
                    Some(intent) => self.state.apply(intent),
                    None => Outcome::Ignored,
                },
                None => Outcome::Ignored,
            },
            Event::Resize(_, _) => Outcome::Redraw,
            _ => Outcome::Ignored,
        }
    }

    /// Показывает отказ в строке состояния.
    ///
    /// Отказ, напечатанный в stdout, сломает кадр; отказ, проглоченный молча, выглядит как
    /// неработающая клавиша. Поэтому одна строка внизу экрана — и она снимается первым же
    /// успешным действием.
    pub fn say(&mut self, text: impl Into<String>) {
        self.message = Some(text.into());
    }

    /// Снимает сообщение: следующий кадр его уже не показывает.
    pub fn clear_message(&mut self) {
        self.message = None;
    }

    /// Что подтверждают прямо сейчас, если экран просит подтверждения.
    pub fn pending(&self) -> Option<Intent> {
        if self.state.mode == Mode::Confirm {
            self.state.pending
        } else {
            None
        }
    }

    /// Выполняет просьбу модели против базы и перечитывает доску.
    ///
    /// Отказ не выходит из цикла: он попадает в строку состояния, экран остаётся живым. Причина
    /// простая — «узел занят» и «недопустимый переход» это не аварии, а обычные ответы системы, и
    /// человеку нужно увидеть их рядом с тем, что он нажимал.
    pub fn perform(
        &mut self,
        conn: &mut rusqlite::Connection,
        project: &crate::project::Project,
        intent: Intent,
    ) -> Outcome {
        self.clear_message();
        let reference = match self.selected() {
            Some(row) => row.dot_id.clone(),
            None => {
                self.say("под курсором нет узла");
                return Outcome::Redraw;
            }
        };

        let result = match intent {
            Intent::Claim => actions::claim(conn, project, &reference, &self.session)
                .map(|token| self.tokens.insert(reference.clone(), token))
                .map(|_| "аренда взята"),
            Intent::CompleteWork | Intent::ReleaseWork | Intent::FailWork => {
                let outcome = match intent {
                    Intent::CompleteWork => LeaseOutcome::Completed,
                    Intent::ReleaseWork => LeaseOutcome::Released,
                    _ => LeaseOutcome::Failed,
                };
                match self.tokens.remove(&reference) {
                    Some(token) => {
                        actions::release(conn, project, &token, outcome).map(|()| match outcome {
                            LeaseOutcome::Completed => "работа завершена",
                            LeaseOutcome::Released => "работа снята",
                            LeaseOutcome::Failed => "работа провалена",
                        })
                    }
                    None => Err(crate::error::Error::NotYourAttempt),
                }
            }
            Intent::ChangeStatus => Err(crate::error::Error::InvalidValue {
                field: "status".to_string(),
                value: "выбор статуса — отдельное действие".to_string(),
            }),
            _ => Ok(""),
        };

        match result {
            Ok(note) => {
                if !note.is_empty() {
                    self.say(note);
                }
            }
            Err(err) => {
                // Подсказка идёт рядом с отказом: в терминале нет места на два экрана, а без
                // подсказки `UNMET_PREREQUISITES` читается как «нельзя» без «что сделать».
                self.say(format!("{}: {}", err.code(), err.hint()));
            }
        }
        // Перечитывание обязательно: иначе строка аренды останется на экране после отпускания,
        // и человек нажмёт `r` второй раз по узлу, который уже свободен. Курсор после этого
        // возвращается на тот же УЗЕЛ, а не на те же координаты: действие могло переехать его в
        // другую колонку.
        if let Err(err) = self.refresh(conn, project) {
            self.say(format!("{}: {}", err.code(), err.hint()));
        } else {
            self.select_by_id(&reference);
        }
        Outcome::Redraw
    }
}

/// Клавиша терминала → клавиша модели.
///
/// `None` для всего, что модели не нужно: функциональные клавиши, Home/End, Ctrl-сочетания.
/// Отдельная функция, а не `match` в цикле: таблицу переводов проверяют тестом целиком, и
/// забытая строка видна сразу.
pub fn key_of(key: KeyEvent) -> Option<Key> {
    // Ctrl-C — обычный выход из терминального приложения; он ведёт себя как `q`, а не как
    // сигнал: raw mode выключил обработку сигналов, и без этой строки Ctrl-C просто не работает.
    if key
        .modifiers
        .contains(ratatui::crossterm::event::KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return Some(Key::Char('q'));
    }
    match key.code {
        KeyCode::Char(c) => Some(Key::Char(c)),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Esc => Some(Key::Esc),
        KeyCode::Tab => Some(Key::Tab),
        KeyCode::BackTab => Some(Key::BackTab),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        KeyCode::Left => Some(Key::Left),
        KeyCode::Right => Some(Key::Right),
        KeyCode::Backspace => Some(Key::Backspace),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overview::{Board, BoardColumn, BoardRow};
    use ratatui::crossterm::event::{KeyModifiers, MouseEventKind};

    fn row(id: &str) -> BoardRow {
        BoardRow {
            dot_id: id.to_string(),
            title: format!("узел {id}"),
            priority: "medium".to_string(),
            due_date: None,
            days_overdue: 0,
            lease_holder: None,
            progress: "0%".to_string(),
        }
    }

    fn board(columns: Vec<(&str, Vec<&str>)>) -> Board {
        Board {
            project: "DEMO".to_string(),
            columns: columns
                .into_iter()
                .map(|(status, ids)| BoardColumn {
                    status: status.to_string(),
                    count: ids.len(),
                    rows: ids.into_iter().map(row).collect(),
                })
                .collect(),
            overdue: 0,
            leased: 0,
            open: 0,
            done: 0,
        }
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// Курсор доски указывает на настоящую строку и на настоящий статус колонки.
    #[test]
    fn the_cursor_points_at_a_real_row_and_status() {
        let mut app = App::new(board(vec![
            ("todo", vec!["DEMO-1", "DEMO-2"]),
            ("done", vec![]),
        ]));
        assert_eq!(app.selected().map(|r| r.dot_id.as_str()), Some("DEMO-1"));
        assert_eq!(app.selected_status(), Some("todo"));

        assert_eq!(app.step(key(KeyCode::Char('j'))), Outcome::Redraw);
        assert_eq!(app.selected().map(|r| r.dot_id.as_str()), Some("DEMO-2"));

        assert_eq!(app.step(key(KeyCode::Char('l'))), Outcome::Redraw);
        assert!(
            app.selected().is_none(),
            "в пустой колонке строки нет — и экран обязан это пережить"
        );
        assert_eq!(app.selected_status(), Some("done"));
    }

    /// Отпускание и автоповтор клавиши не двигают курсор: иначе удержание `j` уезжает на десяток
    /// строк.
    #[test]
    fn key_release_and_repeat_are_not_presses() {
        let mut app = App::new(board(vec![("todo", vec!["DEMO-1", "DEMO-2", "DEMO-3"])]));
        for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
            let event = Event::Key(KeyEvent {
                kind,
                ..KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)
            });
            assert_eq!(
                app.step(event),
                Outcome::Ignored,
                "{kind:?} сдвинуло курсор"
            );
        }
        assert_eq!(app.selected().map(|r| r.dot_id.as_str()), Some("DEMO-1"));
    }

    /// Ресайз перерисовывает кадр, мышь игнорируется: мыши в первой версии нет, и событие о ней
    /// не должно выглядеть как нажатие.
    #[test]
    fn resize_redraws_and_mouse_is_ignored() {
        let mut app = App::new(board(vec![("todo", vec!["DEMO-1"])]));
        assert_eq!(app.step(Event::Resize(120, 40)), Outcome::Redraw);
        assert_eq!(
            app.step(Event::Mouse(ratatui::crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            })),
            Outcome::Ignored
        );
    }

    /// Ctrl-C работает как `q`: raw mode выключает обработку сигналов, и без перевода сочетания
    /// в обычную клавишу выход из приложения не работает вовсе.
    #[test]
    fn ctrl_c_is_the_usual_way_out() {
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let mut app = App::new(board(vec![("todo", vec!["DEMO-1"])]));
        assert_eq!(app.step(ctrl_c), Outcome::Quit);
    }

    /// Таблица переводов проверяется целиком: забытая строка — это клавиша, которая молча не
    /// работает, и компилятор об этом не скажет.
    #[test]
    fn every_modelled_key_is_translated() {
        let table = [
            (KeyCode::Enter, Key::Enter),
            (KeyCode::Esc, Key::Esc),
            (KeyCode::Tab, Key::Tab),
            (KeyCode::BackTab, Key::BackTab),
            (KeyCode::Up, Key::Up),
            (KeyCode::Down, Key::Down),
            (KeyCode::Left, Key::Left),
            (KeyCode::Right, Key::Right),
            (KeyCode::Backspace, Key::Backspace),
            (KeyCode::Char('ж'), Key::Char('ж')),
            (KeyCode::Char('/'), Key::Char('/')),
        ];
        for (code, expected) in table {
            assert_eq!(
                key_of(KeyEvent::new(code, KeyModifiers::NONE)),
                Some(expected),
                "{code:?}"
            );
        }
        for code in [KeyCode::Home, KeyCode::End, KeyCode::F(5), KeyCode::Delete] {
            assert_eq!(
                key_of(KeyEvent::new(code, KeyModifiers::NONE)),
                None,
                "{code:?}"
            );
        }
    }

    /// После перечитывания курсор остаётся внутри колонки, даже если узлов стало меньше.
    #[test]
    fn refresh_clamps_the_cursor_into_the_new_board() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-app-{nanos}"));
        let outcome = crate::db::init(&dir, "APP", "app").expect("init");
        let conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let project = crate::project::first_project(&conn).expect("project");

        let mut app = App::new(board(vec![
            ("todo", vec!["A", "B", "C"]),
            ("done", vec!["D"]),
        ]));
        app.state.row = 2;
        app.state.column = 1;
        app.refresh(&conn, &project).expect("refresh");
        // В новой базе узлов нет вовсе: курсор обязан вернуться в ноль, а не остаться на третьей
        // строке несуществующей колонки.
        assert_eq!(
            (app.state.column, app.state.row),
            (0, 0),
            "курсор вышел за пустую доску"
        );
        assert!(app.selected().is_none());
    }

    /// Сообщение об отказе живёт до первого успешного действия и не копится.
    #[test]
    fn a_message_is_shown_until_it_is_cleared() {
        let mut app = App::new(board(vec![("todo", vec!["DEMO-1"])]));
        assert!(app.message.is_none());
        app.say("INVALID_TRANSITION: из done только в todo");
        assert_eq!(
            app.message.as_deref(),
            Some("INVALID_TRANSITION: из done только в todo")
        );
        app.clear_message();
        assert!(app.message.is_none());
    }

    /// Подтверждение видно вызывающему только в режиме подтверждения: иначе действие выполнится
    /// дважды — по нажатию и по ответу. И `r` сперва спрашивает ИСХОД, а не подтверждение:
    /// разница между «завершено» и «снято» — это статус узла.
    #[test]
    fn pending_is_visible_only_while_confirming() {
        let mut app = App::new(board(vec![("todo", vec!["DEMO-1"])]));
        assert_eq!(app.pending(), None);

        assert_eq!(app.step(key(KeyCode::Char('r'))), Outcome::Redraw);
        assert_eq!(app.state.mode, Mode::Outcome, "не открылся выбор исхода");
        assert_eq!(
            app.pending(),
            None,
            "подтверждение спрошено до выбора исхода"
        );

        assert_eq!(
            app.step(key(KeyCode::Char('d'))),
            Outcome::Ask(Intent::CompleteWork)
        );
        assert_eq!(app.pending(), Some(Intent::CompleteWork));

        app.step(key(KeyCode::Esc));
        assert_eq!(app.pending(), None, "отмена оставила вопрос висеть");
        assert_ne!(app.state.mode, Mode::Confirm);
    }

    /// Полный круг через экран: взять узел, увидеть аренду на доске, завершить работу и увидеть
    /// закрытый узел. Проверяется не «функции вернули Ok», а состояние БАЗЫ и строка состояния:
    /// именно это видит человек.
    #[test]
    fn claim_and_complete_through_the_screen() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-act-app-{nanos}"));
        let outcome = crate::db::init(&dir, "ACT", "act").expect("init");
        let mut conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let project = crate::project::first_project(&conn).expect("project");
        crate::node::create(
            &mut conn,
            &project,
            crate::node::NewNode {
                parent: None,
                title: "узел для экрана",
                kind: None,
                description: None,
                priority: None,
                due_date: None,
                estimated_hours: None,
                weight: None,
                source_ref: None,
                branch: None,
            },
        )
        .expect("node");

        let board = crate::overview::board(&conn, &project).expect("board");
        let mut app = App::with_session(board, "sess-tui");
        assert!(
            app.selected().is_some(),
            "в доске нет узла — фикстура пуста"
        );
        let reference = app.selected().expect("строка").dot_id.clone();

        // Взяли.
        assert_eq!(
            app.perform(&mut conn, &project, Intent::Claim),
            Outcome::Redraw
        );
        assert_eq!(app.message.as_deref(), Some("аренда взята"));
        assert!(
            app.tokens.contains_key(&reference),
            "токен аренды не запомнен: отпустить узел будет нечем"
        );
        let holder: Option<String> = conn
            .query_row(
                "SELECT lease_holder FROM v_node_effective e JOIN nodes n ON n.id = e.id
                  WHERE n.title = 'узел для экрана'",
                [],
                |row| row.get(0),
            )
            .expect("чтение аренды");
        assert_eq!(holder.as_deref(), Some("tui"), "аренда не записана в базу");

        // Завершили.
        assert_eq!(
            app.perform(&mut conn, &project, Intent::CompleteWork),
            Outcome::Redraw
        );
        assert_eq!(app.message.as_deref(), Some("работа завершена"));
        assert!(
            !app.tokens.contains_key(&reference),
            "токен остался после отпускания — второй `r` пошлёт его повторно"
        );
        let status: String = conn
            .query_row(
                "SELECT status FROM nodes WHERE title = 'узел для экрана'",
                [],
                |row| row.get(0),
            )
            .expect("статус");
        assert_eq!(status, "done", "завершение не закрыло узел");
    }

    /// Отпускание без токена — названный отказ в строке состояния, а не паника и не тишина.
    #[test]
    fn releasing_without_a_token_explains_itself() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-act-nokey-{nanos}"));
        let outcome = crate::db::init(&dir, "NOK", "nok").expect("init");
        let mut conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let project = crate::project::first_project(&conn).expect("project");
        crate::node::create(
            &mut conn,
            &project,
            crate::node::NewNode {
                parent: None,
                title: "чужой узел",
                kind: None,
                description: None,
                priority: None,
                due_date: None,
                estimated_hours: None,
                weight: None,
                source_ref: None,
                branch: None,
            },
        )
        .expect("node");
        let board = crate::overview::board(&conn, &project).expect("board");
        let mut app = App::with_session(board, "sess-tui");

        app.perform(&mut conn, &project, Intent::CompleteWork);
        let message = app.message.clone().unwrap_or_default();
        assert!(
            message.starts_with("NOT_YOUR_ATTEMPT"),
            "отказ не назван кодом: {message}"
        );
        assert!(
            message.len() > "NOT_YOUR_ATTEMPT".len() + 2,
            "у отказа нет подсказки: {message}"
        );
    }

    /// Действие при пустой доске не падает и не пишет в базу.
    #[test]
    fn acting_without_a_selected_node_says_so() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-act-empty-{nanos}"));
        let outcome = crate::db::init(&dir, "EMP", "emp").expect("init");
        let mut conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let project = crate::project::first_project(&conn).expect("project");
        let board = crate::overview::board(&conn, &project).expect("board");
        let mut app = App::with_session(board, "sess-tui");
        assert!(app.selected().is_none(), "фикстура не пуста");

        assert_eq!(
            app.perform(&mut conn, &project, Intent::Claim),
            Outcome::Redraw
        );
        assert_eq!(app.message.as_deref(), Some("под курсором нет узла"));
        let attempts: i64 = conn
            .query_row("SELECT COUNT(*) FROM attempts", [], |row| row.get(0))
            .expect("count");
        assert_eq!(attempts, 0, "действие без узла записало аренду");
    }

    /// После действия курсор остаётся на ТОМ ЖЕ УЗЛЕ, а не на тех же координатах.
    ///
    /// Взятый в работу узел переезжает в колонку `in_progress`, и курсор, оставленный на прежних
    /// координатах, оказывается на соседе. В живом прогоне это выглядело так: человек нажимал `r`
    /// сразу после `c` и получал `NOT_YOUR_ATTEMPT` — отпускался не тот узел.
    #[test]
    fn the_cursor_follows_the_node_after_it_moves() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-act-follow-{nanos}"));
        let outcome = crate::db::init(&dir, "FLW", "flw").expect("init");
        let mut conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let project = crate::project::first_project(&conn).expect("project");
        for title in ["первый", "второй"] {
            crate::node::create(
                &mut conn,
                &project,
                crate::node::NewNode {
                    parent: None,
                    title,
                    kind: None,
                    description: None,
                    priority: None,
                    due_date: None,
                    estimated_hours: None,
                    weight: None,
                    source_ref: None,
                    branch: None,
                },
            )
            .expect("node");
        }

        let board = crate::overview::board(&conn, &project).expect("board");
        let mut app = App::with_session(board, "sess-tui");
        // Курсор на ПЕРВЫЙ узел списка.
        app.state.column = 0;
        app.state.row = 0;
        let chosen = app.selected().expect("строка").dot_id.clone();

        app.perform(&mut conn, &project, Intent::Claim);
        assert_eq!(
            app.selected().map(|row| row.dot_id.clone()),
            Some(chosen.clone()),
            "курсор уехал на другой узел после взятия в работу"
        );
        assert!(
            app.tokens.contains_key(&chosen),
            "токен записан под другим адресом"
        );

        // И отпускание работает без повторного поиска узла.
        app.perform(&mut conn, &project, Intent::CompleteWork);
        assert_eq!(app.message.as_deref(), Some("работа завершена"));
        let status: String = conn
            .query_row(
                "SELECT status FROM nodes WHERE id = (SELECT node_id FROM attempts ORDER BY id DESC LIMIT 1)",
                [],
                |row| row.get(0),
            )
            .expect("статус");
        assert_eq!(status, "done", "завершение не закрыло узел");
    }

    /// Цепочка «клавиша → намерение → действие» целиком, как в цикле: `c`, `r`, `d`, `y`.
    ///
    /// Тесты модели и тесты действий по отдельности зелёные, а связка проверяется только здесь:
    /// именно в ней жил дефект — после взятия в работу узел переезжает в другую колонку, и
    /// отпускание уходило по адресу СОСЕДА (`NOT_YOUR_ATTEMPT` на верном нажатии).
    #[test]
    fn the_key_chain_claims_and_completes_the_same_node() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-act-chain-{nanos}"));
        let outcome = crate::db::init(&dir, "CHN", "chain").expect("init");
        let mut conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let project = crate::project::first_project(&conn).expect("project");
        for title in ["первый", "второй", "третий"] {
            crate::node::create(
                &mut conn,
                &project,
                crate::node::NewNode {
                    parent: None,
                    title,
                    kind: None,
                    description: None,
                    priority: None,
                    due_date: None,
                    estimated_hours: None,
                    weight: None,
                    source_ref: None,
                    branch: None,
                },
            )
            .expect("node");
        }
        let board = crate::overview::board(&conn, &project).expect("board");
        let mut app = App::with_session(board, "sess-chain");
        app.state.column = 0;
        app.state.row = 1;
        let chosen = app.selected().expect("строка").dot_id.clone();

        // Клавиша `c`.
        assert_eq!(
            app.step(key(KeyCode::Char('c'))),
            Outcome::Act(Intent::Claim)
        );
        app.perform(&mut conn, &project, Intent::Claim);
        assert_eq!(
            app.selected().map(|row| row.dot_id.clone()),
            Some(chosen.clone()),
            "после взятия курсор ушёл с узла"
        );

        // `r` — выбор исхода, `d` — завершение, `y` — подтверждение.
        assert_eq!(app.step(key(KeyCode::Char('r'))), Outcome::Redraw);
        let ask = app.step(key(KeyCode::Char('d')));
        assert_eq!(ask, Outcome::Ask(Intent::CompleteWork));
        let act = app.step(key(KeyCode::Char('y')));
        assert_eq!(act, Outcome::Act(Intent::CompleteWork));
        app.perform(&mut conn, &project, Intent::CompleteWork);

        assert_eq!(
            app.message.as_deref(),
            Some("работа завершена"),
            "цепочка не дошла до завершения"
        );
        let status: String = conn
            .query_row(
                "SELECT status FROM nodes WHERE title = (SELECT title FROM nodes WHERE path = ?1)",
                [&chosen.split('-').next_back().expect("путь")],
                |row| row.get(0),
            )
            .unwrap_or_else(|_| {
                conn.query_row(
                    "SELECT status FROM nodes WHERE id =
                       (SELECT node_id FROM attempts WHERE token IS NOT NULL
                         ORDER BY id DESC LIMIT 1)",
                    [],
                    |row| row.get(0),
                )
                .expect("статус")
            });
        assert_eq!(status, "done", "узел не закрыт: {status}");
    }
}
