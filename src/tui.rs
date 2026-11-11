//! TUI: доска в терминале.
//!
//! Здесь пока только кадр: экран целиком и решения по клавишам — предмет следующих шагов
//! (`TASQDEV-6.3`…`6.5`). Файл заведён на шаге замера (`TASQDEV-6.2`), потому что цена
//! ratatui в артефакте измеряется только настоящим кодом, а не оценкой по документации.
//!
//! Источник данных — `overview::board`, то же, что у `tasq board`: экран не имеет права
//! считать статусы, аренды и просрочку по-своему, иначе доска и TUI разойдутся в числах.

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::{execute, terminal};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use rusqlite::Connection;

use crate::error::Result;
use crate::overview;
use crate::project::Project;

/// Рисует доску и возвращает управление.
///
/// `once` — один кадр в обычный вывод и выход: так TUI проверяется в CI, где нет терминала,
/// и так же снимается скриншот для документации. Без `once` экран занимает терминал целиком
/// и ждёт клавишу `q`; полноценный цикл появится на шаге 6.5.
pub fn run(conn: &Connection, project: &Project, once: bool) -> Result<()> {
    let board = overview::board(conn, project)?;
    let mut out = std::io::stdout();

    if once {
        // Без альтернативного экрана и без raw mode: вывод остаётся текстом, который можно
        // перенаправить в файл и сравнить в тесте.
        let backend = CrosstermBackend::new(&mut out);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| draw(frame, &board))?;
    } else {
        // `Error::Io` принимает `std::io::Error` через `#[from]`, поэтому `?` работает и на
        // ошибках ratatui: они того же типа.
        terminal::enable_raw_mode()?;
        let mut backend_out = std::io::stdout();
        execute!(backend_out, terminal::EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(backend_out);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| draw(frame, &board))?;
        terminal::disable_raw_mode()?;
    }
    Ok(())
}

/// Кадр: строка состояния и таблица открытых узлов по статусам.
fn draw(frame: &mut ratatui::Frame<'_>, board: &overview::Board) {
    let rows: Vec<Row<'_>> = board
        .columns
        .iter()
        .flat_map(|column| {
            column.rows.iter().map(move |row| {
                let holder = row.lease_holder.as_deref().unwrap_or("-");
                Row::new(vec![
                    Cell::from(column.status.clone()),
                    Cell::from(row.dot_id.clone()),
                    Cell::from(row.priority.clone()),
                    Cell::from(row.progress.clone()),
                    Cell::from(holder.to_string()),
                    Cell::from(row.title.clone()),
                ])
            })
        })
        .collect();

    let header = Row::new(vec![
        "status",
        "id",
        "prio",
        "покрыто",
        "держатель",
        "название",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let [top, body] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(frame.area());

    frame.render_widget(
        Paragraph::new(Line::from(format!(
            "{}  открыто {}  закрыто {}  просрочено {}  в аренде {}",
            board.project, board.open, board.done, board.overdue, board.leased
        )))
        .block(Block::default().borders(Borders::ALL)),
        top,
    );
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(11),
                Constraint::Length(14),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(12),
                Constraint::Min(20),
            ],
        )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" доска ")),
        body,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Кадр действительно содержит доску: ключ проекта, шапку и строку узла.
    ///
    /// Проверяется по буферу, а не по пикселям: раскладку смотреть глазами, а утверждать надо
    /// то, что легко потерять молча, — что таблица вообще нарисована и что в неё попали
    /// данные, а не пустой набор.
    #[test]
    fn the_frame_carries_the_board() {
        let board = overview::Board {
            project: "DEMO".to_string(),
            columns: vec![overview::BoardColumn {
                status: "todo".to_string(),
                count: 1,
                rows: vec![overview::BoardRow {
                    dot_id: "DEMO-1".to_string(),
                    title: "первая задача".to_string(),
                    priority: "high".to_string(),
                    due_date: None,
                    days_overdue: 0,
                    lease_holder: Some("hermes".to_string()),
                    progress: "0%".to_string(),
                }],
            }],
            overdue: 0,
            leased: 1,
            open: 1,
            done: 0,
        };

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("terminal");
        terminal.draw(|frame| draw(frame, &board)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();

        assert!(text.contains("DEMO-1"), "строки узла нет в кадре: {text}");
        assert!(text.contains("hermes"), "держатель не показан: {text}");
        assert!(text.contains("открыто 1"), "счётчики не показаны: {text}");
        assert!(text.contains("доска"), "заголовок таблицы потерян: {text}");
    }

    /// Пустая доска рисуется без строк и без паники: `TestBackend` 1×1 — самый узкий случай.
    #[test]
    fn an_empty_board_draws_in_a_one_cell_terminal() {
        let board = overview::Board {
            project: "DEMO".to_string(),
            columns: Vec::new(),
            overdue: 0,
            leased: 0,
            open: 0,
            done: 0,
        };
        let mut terminal = Terminal::new(TestBackend::new(1, 1)).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &board))
            .expect("кадр 1x1 должен рисоваться, а не падать на усечении");
    }
}
