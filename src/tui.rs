//! TUI: доска в терминале.
//!
//! Здесь пока только кадр: экран целиком и решения по клавишам — предмет следующих шагов
//! (`TASQDEV-6.3`…`6.5`). Файл заведён на шаге замера (`TASQDEV-6.2`), потому что цена
//! ratatui в артефакте измеряется только настоящим кодом, а не оценкой по документации.
//!
//! Источник данных — `overview::board`, то же, что у `tasq board`: экран не имеет права
//! считать статусы, аренды и просрочку по-своему, иначе доска и TUI разойдутся в числах.

pub mod app;
pub mod model;

use std::time::Duration;

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

/// Как часто перерисовывать кадр без событий.
///
/// Ноль событий — не повод крутиться в цикле на 100 % процессора, и не повод стоять часами:
/// доска меняется и без нажатий (истекает аренда, другой агент закрывает узел). Четверть
/// секунды — цена одного чтения доски на машине, где их тысячи, и заметная глазом задержка.
const TICK: Duration = Duration::from_millis(250);

/// Рисует доску и возвращает управление.
///
/// `once` — один кадр в обычный вывод и выход: так TUI проверяется в CI, где нет терминала,
/// и так же снимается картинка для документации. Без `once` экран занимает терминал целиком и
/// живёт в цикле до `q`.
pub fn run(conn: &Connection, project: &Project, once: bool) -> Result<()> {
    let board = overview::board(conn, project)?;

    if once {
        print!("{}", render_once(&board));
        return Ok(());
    }

    let mut app = app::App::new(board);
    // `Error::Io` принимает `std::io::Error` через `#[from]`, поэтому `?` работает и на
    // ошибках ratatui: они того же типа. А вот отсутствие терминала — отдельный код: без tty
    // `enable_raw_mode` отдаёт `No such device or address`, и как `STORAGE_ERROR` это читалось
    // как «проблема с диском или правами на файл».
    terminal::enable_raw_mode().map_err(|err| crate::error::Error::NoTerminal {
        reason: err.to_string(),
    })?;
    let outcome = event_loop(std::io::stdout(), conn, project, &mut app);
    // raw mode снимается ДО возврата ошибки: иначе упавшая отрисовка оставит терминал в
    // состоянии, где Enter не отправляет строку, и человеку придётся спасать сессию `reset`.
    terminal::disable_raw_mode()?;
    outcome
}

/// Цикл: кадр → событие → решение. Тело решения в `App::step`, здесь только ввод-вывод.
fn event_loop<W>(mut out: W, conn: &Connection, project: &Project, app: &mut app::App) -> Result<()>
where
    W: std::io::Write,
{
    use ratatui::crossterm::event;

    execute!(out, terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let result = (|| -> Result<()> {
        loop {
            terminal.draw(|frame| draw(frame, app))?;
            if !event::poll(TICK)? {
                // Пауза без событий: доска могла измениться извне, поэтому перечитываем её, а не
                // перерисовываем тот же снимок. Ошибка чтения не выходит из цикла — она
                // показывается в строке состояния: база, запертая другим процессом, не повод
                // терять экран.
                if let Err(err) = app.refresh(conn, project) {
                    app.say(err.to_string());
                }
                continue;
            }
            let event = event::read()?;
            match app.step(event) {
                model::Outcome::Quit => break,
                model::Outcome::Redraw => {
                    if let Err(err) = app.refresh(conn, project) {
                        app.say(err.to_string());
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })();

    // Выход из альтернативного экрана — до возврата ошибки: иначе терминал останется в чужом
    // буфере, и вывод упадёт поверх доски.
    execute!(terminal.backend_mut(), terminal::LeaveAlternateScreen)?;
    result
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
    // Через `App`, а не через доску: кадр в CI обязан быть ТЕМ ЖЕ кадром, что видит человек,
    // включая пометку курсора и строку подсказки. Иначе `--once` проверяет другой экран.
    let app = app::App::new(board.clone());
    terminal
        .draw(|frame| draw(frame, &app))
        .expect("рисование в буфер памяти не может отказать");
    let buffer = terminal.backend().buffer().clone();
    buffer_lines(&buffer, ONCE_WIDTH)
}

/// Высота кадра: всё, что рисуется, по строкам.
///
/// Считается здесь, а не берётся из терминала: в `--once` терминала нет. Пропущенная строка в этом
/// счёте не «обрежет» кадр на один ряд — она сдвигает раскладку, и данные вытесняются служебными
/// строками. Именно так и вышло, когда появились строки режима и подсказки: таблица ужалась до
/// рамки, и узел пропал из кадра, оставаясь в данных.
fn frame_height(board: &overview::Board) -> u16 {
    let rows: usize = board.columns.iter().map(|column| column.rows.len()).sum();
    // 3 — блок счётчиков, 2 — рамка таблицы, 1 — её заголовок, 2 — строка режима и подсказка.
    let height = 3 + 2 + 1 + rows + 2;
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

/// Кадр: строка состояния, таблица узлов по статусам и строка подсказки.
///
/// Строка под курсором помечена реверсом. Пометка идёт ЗДЕСЬ, а не в модели: модель про вид
/// ничего не знает, и это правильно — иначе вид стал бы частью решения.
fn draw(frame: &mut ratatui::Frame<'_>, app: &app::App) {
    let board = &app.board;
    // Строка под курсором — её номер в ПЛОСКОМ списке всех строк, а не в колонке: таблица
    // рисуется одним списком, и номер колонки ей ничего не говорит. Считается по индексам, а не
    // счётчиком в замыкании: `map(move |…| …)` копирует `flat` в каждое замыкание, и счётчик
    // молча перестаёт расти — тогда подсвечена либо каждая строка, либо ни одна.
    let selected_flat = selected_index(board, app.state.column, app.state.row);
    let mut rows: Vec<Row<'_>> = Vec::new();
    let mut offset = 0usize;
    for column in &board.columns {
        for (index, row) in column.rows.iter().enumerate() {
            let holder = row.lease_holder.as_deref().unwrap_or("-");
            let mut line = Row::new(vec![
                Cell::from(column.status.clone()),
                Cell::from(row.dot_id.clone()),
                Cell::from(row.priority.clone()),
                Cell::from(row.progress.clone()),
                Cell::from(holder.to_string()),
                Cell::from(row.title.clone()),
            ]);
            if Some(offset + index) == selected_flat {
                line = line.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            rows.push(line);
        }
        offset += column.rows.len();
    }

    let header = Row::new(vec![
        "status",
        "id",
        "prio",
        "покрыто",
        "держатель",
        "название",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let status_line = match &app.message {
        Some(message) => message.to_string(),
        None => format!(
            "{}  открыто {}  закрыто {}  просрочено {}  в аренде {}",
            board.project, board.open, board.done, board.overdue, board.leased
        ),
    };
    let hint = format!(
        "{}   j/k — строки, h/l — колонки, Enter — карточка, c/r — аренда, s — статус, q — выход",
        app.state.mode.title()
    );

    // Четыре полосы: счётчики (3), таблица (всё остальное), строка режима и подсказка (по 1).
    let [top, body, mode_line, hint_line] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    frame.render_widget(
        Paragraph::new(Line::from(status_line)).block(Block::default().borders(Borders::ALL)),
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
    frame.render_widget(Paragraph::new(Line::from(hint)), hint_line);
    frame.render_widget(
        Paragraph::new(Line::from(match app.pending() {
            Some(intent) => format!("подтвердить {intent:?}?  y / n"),
            None => String::new(),
        })),
        mode_line,
    );
}

/// Номер строки в плоском списке всех строк доски.
///
/// `None`, если колонки или строки с такими номерами нет: тогда ни одна строка не помечена, и
/// это верно — помечать «первую попавшуюся» вместо несуществующей значит показать курсор там,
/// где его нет.
fn selected_index(board: &overview::Board, column: usize, row: usize) -> Option<usize> {
    let body = board.columns.get(column)?;
    if row >= body.rows.len() {
        return None;
    }
    let before: usize = board
        .columns
        .iter()
        .take(column)
        .map(|c| c.rows.len())
        .sum();
    Some(before + row)
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
        let app = app::App::new(board.clone());
        terminal.draw(|frame| draw(frame, &app)).expect("draw");
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
        let app = app::App::new(board.clone());
        terminal
            .draw(|frame| draw(frame, &app))
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

    /// Курсор ВИДЕН: строка под ним помечена реверсом, и помечена ровно одна.
    ///
    /// Проверяется по буферу кадра, а не по коду: подсветка — это то, что человек видит, и
    /// ошибка здесь не ломает ни один тест модели. Именно так и было, пока счётчик строк считался
    /// в замыкании `map(move ...)`: он копировался в каждое замыкание, и подсветка пропадала
    /// целиком при зелёных тестах.
    #[test]
    fn exactly_one_row_is_highlighted_and_it_follows_the_cursor() {
        let mut board = overview::Board {
            project: "DEMO".to_string(),
            columns: vec![
                overview::BoardColumn {
                    status: "todo".to_string(),
                    count: 3,
                    rows: (1..=3)
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
                },
                overview::BoardColumn {
                    status: "done".to_string(),
                    count: 2,
                    rows: (4..=5)
                        .map(|n| overview::BoardRow {
                            dot_id: format!("DEMO-{n}"),
                            title: format!("задача {n}"),
                            priority: "medium".to_string(),
                            due_date: None,
                            days_overdue: 0,
                            lease_holder: None,
                            progress: "100%".to_string(),
                        })
                        .collect(),
                },
            ],
            overdue: 0,
            leased: 0,
            open: 3,
            done: 2,
        };

        let highlighted = |board: &overview::Board, column: usize, row: usize| -> Option<String> {
            let mut app = app::App::new(board.clone());
            app.state.column = column;
            app.state.row = row;
            let height = frame_height(board);
            let mut terminal =
                Terminal::new(TestBackend::new(ONCE_WIDTH, height)).expect("terminal");
            terminal.draw(|frame| draw(frame, &app)).expect("draw");
            let buffer = terminal.backend().buffer().clone();
            let mut found: Vec<String> = Vec::new();
            for y in 0..buffer.area().height {
                let mut line = String::new();
                let mut reversed = false;
                for x in 0..ONCE_WIDTH {
                    let cell = &buffer[(x, y)];
                    line.push_str(cell.symbol());
                    if cell.style().add_modifier.contains(Modifier::REVERSED) {
                        reversed = true;
                    }
                }
                if reversed {
                    found.push(line);
                }
            }
            assert!(found.len() <= 1, "помечено больше одной строки: {found:?}");
            found.pop()
        };

        // Первая строка первой колонки.
        let first = highlighted(&board, 0, 0).expect("курсор не помечен вовсе");
        assert!(first.contains("DEMO-1"), "помечена не та строка: {first}");

        // Третья строка первой колонки.
        let third = highlighted(&board, 0, 2).expect("курсор не помечен");
        assert!(third.contains("DEMO-3"), "помечена не та строка: {third}");

        // Первая строка ВТОРОЙ колонки: счёт идёт по плоскому списку, и здесь легко ошибиться
        // на всю длину предыдущей колонки.
        let other = highlighted(&board, 1, 0).expect("курсор не помечен");
        assert!(other.contains("DEMO-4"), "помечена не та строка: {other}");

        // Пустой доска: помечать нечего, и это не паника.
        board.columns[1].rows.clear();
        board.columns[1].count = 0;
        assert!(
            highlighted(&board, 1, 0).is_none(),
            "в пустой колонке что-то помечено"
        );
    }
}
