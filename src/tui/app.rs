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

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};

use super::model::{self, Intent, Key, Mode, Outcome, State};
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
        }
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
        self.board = overview::board(conn, project)?;
        self.state.rows = self
            .board
            .columns
            .iter()
            .map(|column| column.rows.len())
            .collect();
        // Курсор после перечитывания может оказаться за концом колонки: узлы закрывают, колонки
        // исчезают. Без подтягивания `Enter` открыл бы пустоту.
        let here = self.state.rows_here();
        self.state.row = if here == 0 {
            0
        } else {
            self.state.row.min(here - 1)
        };
        if self.state.column >= self.state.rows.len() {
            self.state.column = self.state.rows.len().saturating_sub(1);
        }
        Ok(())
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
    /// дважды — по нажатию и по ответу.
    #[test]
    fn pending_is_visible_only_while_confirming() {
        let mut app = App::new(board(vec![("todo", vec!["DEMO-1"])]));
        assert_eq!(app.pending(), None);
        assert_eq!(
            app.step(key(KeyCode::Char('r'))),
            Outcome::Ask(Intent::Release)
        );
        assert_eq!(app.pending(), Some(Intent::Release));
        app.step(key(KeyCode::Esc));
        assert_eq!(app.pending(), None, "отмена оставила вопрос висеть");
    }
}
