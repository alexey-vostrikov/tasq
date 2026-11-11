//! TUI: доска в терминале.
//!
//! Здесь пока только кадр: экран целиком и решения по клавишам — предмет следующих шагов
//! (`TASQDEV-6.3`…`6.5`). Файл заведён на шаге замера (`TASQDEV-6.2`), потому что цена
//! ratatui в артефакте измеряется только настоящим кодом, а не оценкой по документации.
//!
//! Источник данных — `overview::board`, то же, что у `tasq board`: экран не имеет права
//! считать статусы, аренды и просрочку по-своему, иначе доска и TUI разойдутся в числах.

pub mod model;

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

/// Ширина кадра в режиме `--once`.
///
/// Фиксированная, а не по размеру терминала: у CI и у файла с картинкой размера нет, а кадр,
/// зависящий от окна, нельзя сравнить в тесте. Сто символов — столько влезает в обычный
/// терминал без переносов.
pub const ONCE_WIDTH: u16 = 100;

/// Рисует доску и возвращает управление.
///
/// `once` — один кадр в обычный вывод и выход: так TUI проверяется в CI, где нет терминала,
/// и так же снимается картинка для документации. Без `once` экран занимает терминал целиком;
/// цикл событий появится на шаге 6.5, сейчас рисуется один кадр и в этом режиме.
pub fn run(conn: &Connection, project: &Project, once: bool) -> Result<()> {
    let board = overview::board(conn, project)?;

    if once {
        print!("{}", render_once(&board));
        return Ok(());
    }

    // `Error::Io` принимает `std::io::Error` через `#[from]`, поэтому `?` работает и на
    // ошибках ratatui: они того же типа.
    terminal::enable_raw_mode()?;
    let outcome = draw_on_terminal(std::io::stdout(), &board);
    // raw mode снимается ДО возврата ошибки: иначе упавшая отрисовка оставит терминал в
    // состоянии, где Enter не отправляет строку, и человеку придётся спасать сессию `reset`.
    terminal::disable_raw_mode()?;
    outcome
}

/// Рисует кадр на настоящем терминале (альтернативный экран).
fn draw_on_terminal<W>(mut out: W, board: &overview::Board) -> Result<()>
where
    W: std::io::Write,
{
    execute!(out, terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| draw(frame, board))?;
    Ok(())
}

/// Кадр в виде текста, без терминала.
///
/// Рисуется в буфер памяти (`TestBackend`), а не в stdout: у `--once` нет терминала, из которого
/// можно взять размер, и ждать `tty` от CI нечего. Заодно это единственный способ проверить
/// вывод кадра тестом — сравнить строки, а не перехватывать поток.
pub fn render_once(board: &overview::Board) -> String {
    let height = frame_height(board);
    // Буфер в памяти отказать не может: его `Error` — `Infallible`. Поэтому `render_once` не
    // возвращает `Result`: ошибка, которой нельзя случиться, в подписи была бы ложью, и
    // вызывающий писал бы `?` там, где нечего обрабатывать.
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(ONCE_WIDTH, height))
        .expect("рисование в буфер памяти не может отказать");
    terminal
        .draw(|frame| draw(frame, board))
        .expect("рисование в буфер памяти не может отказать");
    let buffer = terminal.backend().buffer().clone();
    buffer_lines(&buffer, ONCE_WIDTH)
}

/// Высота кадра: шапка со счётчиками (3 строки) плюс таблица с рамкой и строкой заголовка.
fn frame_height(board: &overview::Board) -> u16 {
    let rows: usize = board.columns.iter().map(|column| column.rows.len()).sum();
    // 3 — блок счётчиков, 2 — рамка таблицы, 1 — её заголовок.
    let height = 3 + 2 + 1 + rows;
    u16::try_from(height).unwrap_or(u16::MAX)
}

/// Буфер кадра → текст: по строке на ряд, хвостовые пробелы срезаны.
///
/// Срезать обязательно: рамка рисуется на всю ширину, и без этого каждая строка несёт десятки
/// пробелов, которые ломают сравнение в тесте и `git diff` картинки в документации.
fn buffer_lines(buffer: &ratatui::buffer::Buffer, width: u16) -> String {
    let area = buffer.area();
    let mut text = String::new();
    for y in 0..area.height {
        let mut line = String::new();
        for x in 0..width {
            line.push_str(buffer[(x, y)].symbol());
        }
        text.push_str(line.trim_end());
        text.push('\n');
    }
    text
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

    /// Кадр из настоящей базы: чтение → доска → текст. Проверяется сквозной путь, а не
    /// собранная руками структура, — иначе тест не поймал бы расхождение между `overview::board`
    /// и тем, что экран показывает.
    #[test]
    fn the_once_frame_comes_from_the_database() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-tui-{nanos}"));
        let outcome = crate::db::init(&dir, "SMOKE", "smoke").expect("init");
        let mut conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let project = crate::project::first_project(&conn).expect("project");
        crate::node::create(
            &mut conn,
            &project,
            crate::node::NewNode {
                parent: None,
                title: "виден на доске",
                kind: None,
                description: None,
                priority: Some("high"),
                due_date: None,
                estimated_hours: None,
                weight: None,
                source_ref: None,
                branch: None,
            },
        )
        .expect("node");

        let board = overview::board(&conn, &project).expect("board");
        let frame = render_once(&board);

        assert!(
            frame.contains("SMOKE"),
            "ключ проекта не попал в кадр:\n{frame}"
        );
        assert!(frame.contains("SMOKE-1"), "узел не попал в кадр:\n{frame}");
        assert!(
            frame.contains("виден на доске"),
            "название не попало в кадр:\n{frame}"
        );
        assert!(
            !frame.lines().any(|line| line.ends_with(' ')),
            "в кадре остались хвостовые пробелы — сравнение и diff будут шуметь"
        );
    }

    /// Высота кадра вмещает все строки доски: ряд, не влезший в буфер, — это молча потерянный
    /// узел, а не «слишком длинный экран».
    #[test]
    fn the_frame_height_fits_every_row() {
        let mut board = overview::Board {
            project: "DEMO".to_string(),
            columns: Vec::new(),
            overdue: 0,
            leased: 0,
            open: 0,
            done: 0,
        };
        board.columns.push(overview::BoardColumn {
            status: "todo".to_string(),
            count: 5,
            rows: (1..=5)
                .map(|n| overview::BoardRow {
                    dot_id: format!("DEMO-{n}"),
                    title: format!("задача {n}"),
                    priority: "medium".to_string(),
                    due_date: None,
                    days_overdue: 0,
                    lease_holder: None,
                    progress: "0%".to_string(),
                })
                .collect(),
        });
        let frame = render_once(&board);
        for n in 1..=5 {
            assert!(
                frame.contains(&format!("DEMO-{n}")),
                "строка DEMO-{n} не влезла в кадр:\n{frame}"
            );
        }
    }

    /// `run(.., once = true)` — путь, которым пользуется CI и документация: доска читается из
    /// базы, кадр печатается, функция возвращается без терминала. Без этого теста сам вход в
    /// режим не исполнялся ни разу: `--once` проверялся на живом бинаре, а код — нет.
    #[test]
    fn run_once_reads_the_board_and_returns() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-tui-run-{nanos}"));
        let outcome = crate::db::init(&dir, "RUN", "run").expect("init");
        let conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let project = crate::project::first_project(&conn).expect("project");
        run(&conn, &project, true).expect("один кадр в обычный вывод");
    }
}
