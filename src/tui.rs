//! TUI: доска в терминале.
//!
//! Экран не зависит от `--project`: слева панель со всеми проектами базы, в шапке доски —
//! переключатель, и переключение не требует выхода. `--project` остался подсказкой «с какого
//! проекта открыться» — то же правило, что у остальных команд обзора.
//!
//! Два вида одной доски: список (таблица) и канбан (колонки статусов рядом). Данные у них одни
//! и те же — `overview::board`, — поэтому переключение вида не перечитывает базу.
//!
//! Источник данных — `overview::board`, то же, что у `tasq board`: экран не имеет права
//! считать статусы, аренды и просрочку по-своему, иначе доска и TUI разойдутся в числах.

pub mod actions;
pub mod app;
pub mod model;
pub mod theme;

use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::{execute, terminal};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use rusqlite::Connection;

use crate::error::Result;
use crate::overview;

/// Ширина кадра в режиме `--once`.
///
/// Фиксированная, а не по размеру терминала: у CI и у файла с картинкой размера нет, а кадр,
/// зависящий от окна, нельзя сравнить в тесте. Сто символов — столько влезает в обычный
/// терминал без переносов.
/// Ширина по умолчанию для `--once`: сто сорок, а не сто.
///
/// Слева панель проектов (24), справа сведения об узле (32), и в сотне на таблицу остаётся 44
/// знака — заголовок и адрес в неё не влезают и молча обрезаются. Применяется, только когда
/// ширину неоткуда взять: у терминала она своя (см. [`once_width`]).
pub const ONCE_WIDTH: u16 = 140;

/// Как часто перерисовывать кадр без событий.
///
/// Ноль событий — не повод крутиться в цикле на 100 % процессора, и не повод стоять часами:
/// доска меняется и без нажатий (истекает аренда, другой агент закрывает узел). Четверть
/// секунды — цена одного чтения доски на машине, где их тысячи, и заметная глазом задержка.
const TICK: Duration = Duration::from_millis(250);
/// Ширина панели проектов.
///
/// Двадцать четыре знака: ключ проекта до восьми (таков предел в схеме), остальное — счётчик
/// узлов. Шире не нужно, уже начинает резать длинные ключи.
pub const PANEL_WIDTH: u16 = 24;

/// Экран без `--project`.
///
/// Проект больше не аргумент, а состояние экрана: слева панель со всеми проектами базы, в шапке
/// доски — переключатель. `start` — необязательный НАЧАЛЬНЫЙ выбор (`--project` остался, но стал
/// подсказкой «с какого проекта открыться», а не требованием). Это то же правило, что у остальных
/// команд обзора: работа по всем проектам без селектора.
///
/// `once` — один кадр в обычный вывод и выход: так TUI проверяется в CI, где нет терминала, и так
/// же снимается картинка для документации.
///
/// Соединение берётся изменяемым: экран не только читает, но и пишет (аренда, статус), и второе
/// соединение «на запись» рядом с читающим — это два разных представления об одной базе и лишний
/// способ с ней разойтись. Запись идёт теми же транзакциями, что у CLI и MCP.
pub fn run(
    conn: &mut Connection,
    start: Option<&str>,
    theme: Option<&str>,
    once: bool,
    color: bool,
) -> Result<()> {
    let mut app = build(conn, start, theme)?;

    if once {
        // Ширина берётся у терминала, если он есть: `--once` чаще печатают руками, чем в лог, и
        // кадр под фиксированные 140 знаков рассыпается переносами посреди рамки.
        print!("{}", render_once_sized(&app, once_width(), color));
        return Ok(());
    }

    // `Error::Io` принимает `std::io::Error` через `#[from]`, поэтому `?` работает и на
    // ошибках ratatui: они того же типа. А вот отсутствие терминала — отдельный код: без tty
    // `enable_raw_mode` отдаёт `No such device or address`, и как `STORAGE_ERROR` это читалось
    // как «проблема с диском или правами на файл».
    terminal::enable_raw_mode().map_err(|err| crate::error::Error::NoTerminal {
        reason: err.to_string(),
    })?;
    let outcome = event_loop(std::io::stdout(), conn, &mut app);
    // raw mode снимается ДО возврата ошибки: иначе упавшая отрисовка оставит терминал в
    // состоянии, где Enter не отправляет строку, и человеку придётся спасать сессию `reset`.
    terminal::disable_raw_mode()?;
    outcome
}

/// Собирает экран: список проектов, доска активного, виды узлов.
///
/// `start` — ключ проекта из `--project` (или `TASQ_PROJECT`). Неизвестный ключ — отказ с
/// названным полем, а не молчаливый переход на первый проект: молчание здесь означало бы «показал
/// не то, что просили».
fn build(conn: &Connection, start: Option<&str>, theme: Option<&str>) -> Result<app::App> {
    let projects = crate::project::list(conn)?;
    if projects.is_empty() {
        return Err(crate::error::Error::NoProject);
    }
    let index = match start.map(str::trim).filter(|key| !key.is_empty()) {
        Some(key) if key.eq_ignore_ascii_case("all") => 0,
        Some(key) => projects
            .iter()
            .position(|summary| summary.key == key)
            .ok_or_else(|| crate::error::Error::ProjectNotFound {
                key: key.to_string(),
                known: projects
                    .iter()
                    .map(|summary| summary.key.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            })?,
        None => 0,
    };
    let project = crate::project::select(conn, Some(&projects[index].key))?;
    let board = overview::board(conn, &project)?;
    let session = actions::session_id()?;
    let mut app = app::App::with_projects(board, session, projects, index, project.clone());
    // Неизвестное имя темы — отказ с названным полем, а не тихий переход на умолчание:
    // «я просил nord, а получил gruvbox» — это ответ, которого человек не заметит.
    if let Some(name) = theme.map(str::trim).filter(|name| !name.is_empty()) {
        app.theme =
            theme::Theme::by_name(name).ok_or_else(|| crate::error::Error::InvalidValue {
                field: "theme".to_string(),
                value: format!("{name} (allowed: {})", theme::Theme::names()),
            })?;
    }
    app.load_kinds(conn, &project)?;
    Ok(app)
}

/// Цикл: кадр → событие → решение. Тело решения в `App::step`, здесь только ввод-вывод.
fn event_loop<W>(mut out: W, conn: &mut Connection, app: &mut app::App) -> Result<()>
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
                if let Err(err) = app.refresh_active(&*conn) {
                    app.fail(app::describe(&err));
                }
                continue;
            }
            let event = event::read()?;
            match app.step(event) {
                model::Outcome::Quit => break,
                // `Act` — просьба модели. Здесь она превращается в вызов библиотеки: экран не
                // знает, что такое аренда, он знает, что человек нажал `c`.
                model::Outcome::Act(intent) => {
                    let Some(project) = app.project.clone() else {
                        app.fail("проект не загружен");
                        continue;
                    };
                    app.perform(conn, &project, intent);
                }
                model::Outcome::Redraw | model::Outcome::Ask(_) => {
                    if let Err(err) = app.refresh_active(&*conn) {
                        app.fail(app::describe(&err));
                    }
                }
                model::Outcome::Ignored => {}
            }
        }
        Ok(())
    })();

    // Выход из альтернативного экрана — до возврата ошибки: иначе терминал останется в чужом
    // буфере, и вывод упадёт поверх доски.
    execute!(terminal.backend_mut(), terminal::LeaveAlternateScreen)?;
    result
}

/// Ширина кадра для `--once`.
///
/// Три источника по порядку достоверности:
///
/// 1. **Размер терминала**, если вывод идёт в него: `--once` печатают руками чаще, чем в лог, и
///    кадр, свёрстанный под 140 знаков, «рассыпается» — человек видит переносы посреди рамки.
/// 2. **`COLUMNS`**, если терминала нет, но переменную выставила оболочка или CI: это ровно то, что
///    человек имеет в виду, когда перенаправляет вывод в файл, а потом читает его в `less -S`.
/// 3. **`ONCE_WIDTH`**, если нет ни того ни другого. Не «девяносто по умолчанию», а 140: у экрана
///    слева панель проектов (24) и справа сведения об узле (32), и при узкой ширине таблица
///    обрезает адреса и названия посреди слова.
///
/// Нижняя граница — 60 знаков: уже этого панель и доска не помещаются вместе, и усечение
/// осмысленнее сделать один раз здесь, чем показывать человеку кашу из трёх обрезков.
pub fn once_width() -> u16 {
    terminal::size()
        .ok()
        .map(|(width, _)| width)
        .or_else(|| parse_width(std::env::var("COLUMNS").ok().as_deref()))
        .unwrap_or(ONCE_WIDTH)
        .max(MIN_ONCE_WIDTH)
}

/// Разбор ширины из строки: мусор и всё, что уже минимума, читается как «не задано».
///
/// Отдельная функция, потому что `std::env` в тестах общий: проверять разбор через подмену
/// `COLUMNS` значило бы ломать соседние тесты того же бинаря.
pub fn parse_width(value: Option<&str>) -> Option<u16> {
    value
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|width| *width >= MIN_ONCE_WIDTH)
}

/// Снимает все escape-последовательности: нужен, чтобы сверить текст цветного кадра с обычным.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // ESC '[' … финальный байт в диапазоне @–~ ; всё прочее после ESC тоже пропускаем.
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

/// Меньше этого `--once` не верстает: панель проектов и доска вместе не влезают.
pub const MIN_ONCE_WIDTH: u16 = 60;

/// Кадр в виде текста, без терминала.
///
/// Рисуется в буфер памяти (`TestBackend`), а не в stdout: у `--once` нет терминала, из которого
/// можно взять размер, и ждать `tty` от CI нечего. Заодно это единственный способ проверить
/// вывод кадра тестом — сравнить строки, а не перехватывать поток.
///
/// Без цвета: так кадр сравнивается в гейте (`make smoke`, CI) строками. Цветной кадр — отдельная
/// функция, потому что сравнение с escape-последовательностями проверяло бы не экран, а
/// сериализатор.
pub fn render_once(app: &app::App) -> String {
    render_once_sized(app, ONCE_WIDTH, false)
}

/// Кадр заданной ширины, с цветом или без.
pub fn render_once_sized(app: &app::App, width: u16, color: bool) -> String {
    let width = width.max(MIN_ONCE_WIDTH);
    let height = frame_height_for(app);
    // Буфер в памяти отказать не может: его `Error` — `Infallible`. Поэтому `render_once` не
    // возвращает `Result`: ошибка, которой нельзя случиться, в подписи была бы ложью, и
    // вызывающий писал бы `?` там, где нечего обрабатывать.
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("рисование в буфер памяти не может отказать");
    terminal
        .draw(|frame| draw(frame, app))
        .expect("рисование в буфер памяти не может отказать");
    let buffer = terminal.backend().buffer().clone();
    if color {
        buffer_ansi(&buffer, width)
    } else {
        buffer_lines(&buffer, width)
    }
}

/// Буфер кадра → текст с ANSI-цветами.
///
/// Своя сериализация, а не `CrosstermBackend`: тот спрашивает размер у терминала, а у `--once`
/// терминала может не быть вовсе. Здесь же буфер уже нарисован нужной ширины, и остаётся
/// превратить стили в escape-последовательности.
///
/// Хвостовые пробелы срезаются ДО добавления escape: иначе строка заканчивалась бы «цвет + сорок
/// пробелов + сброс», и файл с кадром весил бы втрое больше нужного.
fn buffer_ansi(buffer: &ratatui::buffer::Buffer, width: u16) -> String {
    let area = buffer.area();
    let mut text = String::new();
    for y in 0..area.height {
        // Последняя непустая клетка: дальше пробелы, которые в текст не идут.
        let last = (0..width).rev().find(|x| buffer[(*x, y)].symbol() != " ");
        let Some(last) = last else {
            text.push('\n');
            continue;
        };
        let mut current: Option<Style> = None;
        for x in 0..=last {
            let cell = &buffer[(x, y)];
            let style = cell.style();
            if current != Some(style) {
                text.push_str(&sgr(style));
                current = Some(style);
            }
            text.push_str(cell.symbol());
        }
        // Сброс в конце строки, а не в начале следующей: строка без сброса «протекает» на
        // терминал, и всё, что человек напечатает после кадра, окажется цветным.
        text.push_str("\x1b[0m");
        text.push('\n');
    }
    text
}

/// Стиль → escape-последовательности SGR.
///
/// Порядок «сброс, затем цвет, затем модификаторы» выбран потому, что так одна и та же функция
/// годится и для первой установки, и для смены стиля посреди строки: без сброса пришлось бы
/// считать разницу между стилями, а это второй способ ошибиться в том же месте.
fn sgr(style: Style) -> String {
    // `Color::Reset` означает «цвет по умолчанию», и в буфере `TestBackend` он стоит почти в
    // каждой клетке. Как код SGR он бессмысленен: сброс уже сделан первым элементом списка, и
    // от него в строке оставалось `0;0` — лишние байты в каждом кадре и шум в `git diff`.
    let fg = style.fg.filter(|color| *color != Color::Reset);
    let bg = style.bg.filter(|color| *color != Color::Reset);
    let mut codes: Vec<String> = vec!["0".to_string()];
    if let Some(color) = fg {
        codes.push(color_code(color, false));
    }
    if let Some(color) = bg {
        codes.push(color_code(color, true));
    }
    let modifier = style.add_modifier;
    for (flag, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::SLOW_BLINK, "5"),
        (Modifier::REVERSED, "7"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if modifier.contains(flag) {
            codes.push(code.to_string());
        }
    }
    format!("\x1b[{}m", codes.join(";"))
}

/// Цвет → код SGR. `background` сдвигает обычные коды на десять, как и в стандарте.
fn color_code(color: Color, background: bool) -> String {
    let base = if background { 10 } else { 0 };
    match color {
        Color::Reset => format!("{}", 39 + base),
        Color::Black => format!("{}", 30 + base),
        Color::Red => format!("{}", 31 + base),
        Color::Green => format!("{}", 32 + base),
        Color::Yellow => format!("{}", 33 + base),
        Color::Blue => format!("{}", 34 + base),
        Color::Magenta => format!("{}", 35 + base),
        Color::Cyan => format!("{}", 36 + base),
        Color::Gray => format!("{}", 37 + base),
        Color::DarkGray => format!("{}", 90 + base),
        Color::LightRed => format!("{}", 91 + base),
        Color::LightGreen => format!("{}", 92 + base),
        Color::LightYellow => format!("{}", 93 + base),
        Color::LightBlue => format!("{}", 94 + base),
        Color::LightMagenta => format!("{}", 95 + base),
        Color::LightCyan => format!("{}", 96 + base),
        Color::White => format!("{}", 97 + base),
        Color::Rgb(r, g, b) => {
            let prefix = if background { "48" } else { "38" };
            format!("{prefix};2;{r};{g};{b}")
        }
        Color::Indexed(index) => {
            let prefix = if background { "48" } else { "38" };
            format!("{prefix};5;{index}")
        }
    }
}

/// Высота кадра: всё, что рисуется, по строкам.
///
/// Считается здесь, а не берётся из терминала: в `--once` терминала нет. Пропущенная строка в этом
/// счёте не «обрежет» кадр на один ряд — она сдвигает раскладку, и данные вытесняются служебными
/// строками. Именно так и вышло, когда появились строки режима и подсказки: таблица ужалась до
/// рамки, и узел пропал из кадра, оставаясь в данных.
fn frame_height(board: &overview::Board) -> u16 {
    let rows: usize = board.columns.iter().map(|column| column.rows.len()).sum();
    // 4 — шапка (счётчики и переключатель), 2 — рамка таблицы, 1 — её заголовок, 2 — строка
    // режима и подсказка.
    let height = 4 + 2 + 1 + rows + 2;
    u16::try_from(height).unwrap_or(u16::MAX)
}

/// Высота кадра для того режима, который сейчас на экране.
///
/// Форма и поиск рисуются в основной области и своей высоты не требуют — им хватает места
/// таблицы; но форма из четырёх полей в кадре на три строки обрезалась бы молча. Поэтому при
/// открытой форме высота не меньше, чем нужно ей.
fn frame_height_for(app: &app::App) -> u16 {
    let base = frame_height(&app.board);
    let needed = match app.state.mode {
        model::Mode::Form | model::Mode::Note => 4 + app.state.fields.len() as u16 + 6,
        _ => base,
    };
    base.max(needed)
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

/// Кадр: панель проектов слева, доска справа, строки режима и подсказки внизу.
///
/// Пометка курсора идёт ЗДЕСЬ, а не в модели: модель про вид ничего не знает, и это правильно —
/// иначе вид стал бы частью решения.
fn draw(frame: &mut ratatui::Frame<'_>, app: &app::App) {
    let [body, mode_line, hint_line] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // Панель проектов слева, доска в центре, сведения об узле справа. Боковая панель
    // показывается только у списка: в канбане колонкам нужна вся ширина, иначе они становятся
    // в четыре знака.
    let sidebar = app.state.view == model::View::List && body.width > PANEL_WIDTH + 60;
    let [panel, main, side] = if sidebar {
        Layout::horizontal([
            Constraint::Length(PANEL_WIDTH),
            Constraint::Min(20),
            Constraint::Length(SIDEBAR_WIDTH),
        ])
        .areas(body)
    } else {
        let [panel, main] =
            Layout::horizontal([Constraint::Length(PANEL_WIDTH), Constraint::Min(20)]).areas(body);
        [panel, main, ratatui::layout::Rect::default()]
    };

    draw_panel(frame, app, panel);
    if sidebar {
        draw_sidebar(frame, app, side);
    }

    // Шапка: переключатель проекта (тот самый «дропдаун») и счётчики. Один и тот же вид у списка
    // и у канбана: переключение вида не должно менять место, где проект выбирается.
    let dropdown = format!(
        " проект: {} ▾   [Tab — панель, v — {}] ",
        app.project_key(),
        match app.state.view {
            model::View::List => "канбан",
            model::View::Kanban => "список",
        }
    );
    let counters = format!(
        "открыто {}  закрыто {}  просрочено {}  в аренде {}",
        app.board.open, app.board.done, app.board.overdue, app.board.leased
    );
    // Четыре строки, а не три: счётчики и переключатель — две строки текста внутри рамки.
    // В три они не влезают, и вторая (переключатель) просто не рисуется — кадр при этом
    // выглядит целым, поэтому заметить это можно только тестом на содержимое.
    let [header, content] =
        Layout::vertical([Constraint::Length(4), Constraint::Min(0)]).areas(main);
    let theme = app.theme;
    frame.render_widget(
        Paragraph::new(vec![
            // Счётчики приглушены, переключатель — акцентом: числа читают, когда ищут, а
            // переключатель ищут глазами всегда.
            Line::from(counters).style(theme.dim_style()),
            Line::from(dropdown).style(theme.focus_style().add_modifier(Modifier::BOLD)),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.border_style()),
        ),
        header,
    );

    // Форма, поиск и карточка занимают основную область: это временные режимы, и человек
    // смотрит в них, а не в доску. Доска остаётся под ними — Esc возвращает её же.
    match app.state.mode {
        model::Mode::Form | model::Mode::Note => draw_form(frame, app, content),
        model::Mode::Search => draw_search(frame, app, content),
        model::Mode::Node => draw_card(frame, app, content),
        _ => match app.state.view {
            model::View::List => draw_list(frame, app, content),
            model::View::Kanban => draw_kanban(frame, app, content),
        },
    }

    // Строка режима несёт отказ или подтверждение — и цвет здесь ЕДИНСТВЕННОЕ, что отличает
    // одно от другого, пока человек не прочитал текст. Отказ красный, успех зелёный, подсказка
    // приглушена: цвет — подсказка, смысл остаётся в словах.
    let (mode_style, mode_text) = match &app.message {
        Some(message) => (
            match app.level {
                app::Level::Error => theme.error_style(),
                app::Level::Ok => theme.ok_style(),
                app::Level::Info => theme.dim_style(),
            },
            message.clone(),
        ),
        None => (theme.text_style(), mode_text(app)),
    };
    frame.render_widget(
        Paragraph::new(Line::from(mode_text).style(mode_style)),
        mode_line,
    );
    frame.render_widget(
        Paragraph::new(Line::from(format!(
            "{}   j/k — строки, h/l — колонки, Enter — карточка, c/r — аренда, s — статус, v — вид, T — тема, q — выход",
            app.state.mode.title()
        )))
        .style(theme.dim_style()),
        hint_line,
    );
}

/// Строка режима: выбор статуса, подтверждение — или пусто.
fn mode_text(app: &app::App) -> String {
    match app.state.mode {
        model::Mode::Status => {
            let mut text = String::from("новый статус:  ");
            for (index, status) in app.state.statuses.iter().enumerate() {
                text.push_str(&format!("{} {status}   ", index + 1));
            }
            text.push_str("Esc — отмена");
            text
        }
        _ => match app.pending() {
            Some(intent) => format!("подтвердить {intent:?}?  y / n"),
            None => String::new(),
        },
    }
}

/// Панель проектов: ключ, счётчик узлов, пометка активного и курсора панели.
fn draw_panel(frame: &mut ratatui::Frame<'_>, app: &app::App, area: ratatui::layout::Rect) {
    let theme = app.theme;
    let mut lines: Vec<Line<'_>> = Vec::new();
    for (index, project) in app.projects.iter().enumerate() {
        let active = index == app.state.active;
        let text = format!("{:<8} {:>4}", project.key, project.nodes);
        let mut line = Line::from(text);
        if active {
            // Активный проект — акцентом; курсор панели — фоном выделения. Две разные вещи:
            // «на какой доске я сейчас» и «на что переключит Enter».
            line = line.style(theme.focus_style().add_modifier(Modifier::BOLD));
        } else {
            line = line.style(theme.dim_style());
        }
        if active && app.state.focus == model::Focus::Projects {
            line = line.style(theme.selected_style());
        }
        lines.push(line);
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_for(app, model::Focus::Projects))
                .title(" проекты "),
        ),
        area,
    );
}

/// Рамка панели: в фокусе — акцент темы, без фокуса — обычная рамка.
fn border_for(app: &app::App, panel: model::Focus) -> Style {
    if app.state.focus == panel {
        app.theme.focus_style()
    } else {
        app.theme.border_style()
    }
}

/// Ширина боковой панели: цепочка контекста, заметки и комментарии.
pub const SIDEBAR_WIDTH: u16 = 32;

/// Боковая панель с тем, что относится к выбранному узлу.
///
/// Три раздела, а не один список: это разные вопросы («где я в дереве», «что решено», «что
/// сказали»), и смешивать их значило бы заставить человека разбирать строки по виду.
fn draw_sidebar(frame: &mut ratatui::Frame<'_>, app: &app::App, area: ratatui::layout::Rect) {
    let theme = app.theme;
    let mut lines: Vec<Line<'_>> = Vec::new();
    let heading = |text: &'static str| Line::from(text).style(theme.header_style());
    lines.push(heading("контекст"));
    for entry in &app.context {
        lines.push(Line::from(format!("  {entry}")).style(theme.text_style()));
    }
    lines.push(Line::from(""));
    lines.push(heading("заметки"));
    for note in &app.notes {
        lines.push(Line::from(format!("  {note}")).style(theme.dim_style()));
    }
    lines.push(Line::from(""));
    lines.push(heading("комментарии"));
    for comment in &app.comments {
        lines.push(Line::from(format!("  {comment}")).style(theme.dim_style()));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(ratatui::widgets::Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme.border_style())
                    .title(" узел "),
            ),
        area,
    );
}

/// Форма узла: поля по одному, текущее помечено.
///
/// Показываются ВСЕ поля сразу, а не одно: человек должен видеть, что он уже ввёл, иначе
/// исправление опечатки в третьем поле требует помнить первые два.
fn draw_form(frame: &mut ratatui::Frame<'_>, app: &app::App, area: ratatui::layout::Rect) {
    let theme = app.theme;
    let title = if app.state.editing.is_some() {
        " правка узла "
    } else {
        " новый узел "
    };
    let mut lines: Vec<Line<'_>> = Vec::new();
    for (index, field) in app.state.fields.iter().enumerate() {
        let marker = if index == app.state.field { ">" } else { " " };
        let mut line = Line::from(format!("{marker} {:<11} {}", field.label, field.value));
        line = if index == app.state.field {
            line.style(theme.selected_style())
        } else {
            line.style(theme.text_style())
        };
        lines.push(line);
    }
    lines.push(Line::from(""));
    lines.push(
        Line::from("Tab — следующее поле, Enter — записать, Esc — отменить")
            .style(theme.dim_style()),
    );
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.focus_style())
                .title(title),
        ),
        area,
    );
}

/// Поиск: строка запроса и результаты.
fn draw_search(frame: &mut ratatui::Frame<'_>, app: &app::App, area: ratatui::layout::Rect) {
    let theme = app.theme;
    let [query, results] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(format!("/{}", app.state.input)).style(theme.text_style()))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme.focus_style())
                    .title(" поиск "),
            ),
        query,
    );
    let mut lines: Vec<Line<'_>> = Vec::new();
    for (index, (dot_id, title)) in app.state.results.iter().enumerate() {
        let mut line = Line::from(format!("{:<14} {title}", dot_id));
        line = if index == app.state.result {
            line.style(theme.selected_style())
        } else {
            line.style(theme.text_style())
        };
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(Line::from("  введите запрос").style(theme.dim_style()));
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.border_style())
                .title(" результат (Enter — открыть, Esc — назад) "),
        ),
        results,
    );
}

/// Карточка узла: заголовок, поля и подсказка.
fn draw_card(frame: &mut ratatui::Frame<'_>, app: &app::App, area: ratatui::layout::Rect) {
    let theme = app.theme;
    let Some(row) = app.selected() else {
        frame.render_widget(
            Paragraph::new("узел не выбран")
                .style(theme.dim_style())
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(theme.border_style())
                        .title(" карточка "),
                ),
            area,
        );
        return;
    };
    let status = app.selected_status().unwrap_or("-");
    let status_line = Line::from(format!(
        "статус {}   приоритет {}   прогресс {}",
        status, row.priority, row.progress
    ))
    .style(Style::default().fg(theme.status(status)));
    let lease = match &row.lease_holder {
        Some(holder) => {
            Line::from(format!("держатель {holder}")).style(Style::default().fg(theme.leased))
        }
        None => Line::from("держатель -").style(theme.dim_style()),
    };
    let lines = vec![
        Line::from(row.dot_id.clone()).style(theme.header_style()),
        Line::from(row.title.clone()).style(theme.text_style()),
        Line::from(""),
        status_line,
        Line::from(format!(
            "покрыто {}   срок {}",
            row.progress,
            row.due_date.as_deref().unwrap_or("-")
        ))
        .style(theme.dim_style()),
        lease,
        Line::from(""),
        Line::from("m — комментарий, n — заметка, e — правка, Esc — назад")
            .style(theme.dim_style()),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.focus_style())
                .title(" карточка "),
        ),
        area,
    );
}

/// Доска списком: строка на узел.
fn draw_list(frame: &mut ratatui::Frame<'_>, app: &app::App, area: ratatui::layout::Rect) {
    let theme = app.theme;
    let selected_flat = selected_index(&app.board, app.state.column, app.state.row);
    let mut rows: Vec<Row<'_>> = Vec::new();
    let mut offset = 0usize;
    for column in &app.board.columns {
        for (index, row) in column.rows.iter().enumerate() {
            let holder = match &row.lease_holder {
                Some(holder) => Cell::from(holder.clone()).style(Style::default().fg(theme.leased)),
                None => Cell::from("-").style(theme.dim_style()),
            };
            // Просрочка — отдельным флагом в названии: цвет строки уже занят статусом, и
            // второй цвет на той же строке читался бы как «и то, и это».
            let mut title = row.title.clone();
            if row.days_overdue > 0 {
                title.push_str(&format!("  просрочено {}", row.days_overdue));
            }
            let mut line = Row::new(vec![
                Cell::from(column.status.clone())
                    .style(Style::default().fg(theme.status(&column.status))),
                Cell::from(row.dot_id.clone()).style(theme.text_style()),
                Cell::from(row.priority.clone()).style(theme.dim_style()),
                Cell::from(row.progress.clone()).style(theme.dim_style()),
                holder,
                Cell::from(title).style(if row.days_overdue > 0 {
                    Style::default().fg(theme.overdue)
                } else {
                    theme.text_style()
                }),
            ]);
            // Зебра от фона темы: у светлых тем полосы светлее основы, у тёмных — темнее.
            line = line.style(Style::default().bg(theme.zebra(index)));
            if Some(offset + index) == selected_flat {
                line = line.style(theme.selected_style());
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
    .style(theme.header_style());

    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(11),
                Constraint::Length(14),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(12),
                Constraint::Min(10),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_for(app, model::Focus::Board))
                .title(" доска "),
        ),
        area,
    );
}

/// Доска канбаном: колонки статусов рядом, узлы внутри колонки.
///
/// Тот же курсор, что у списка: `h`/`l` ходят по колонкам, `j`/`k` — по строкам внутри. На
/// нескольких колонках это единственное осмысленное соответствие, и оно делает переключение вида
/// безболезненным — курсор остаётся на том же узле.
fn draw_kanban(frame: &mut ratatui::Frame<'_>, app: &app::App, area: ratatui::layout::Rect) {
    let theme = app.theme;
    if app.board.columns.is_empty() {
        frame.render_widget(
            Paragraph::new("в проекте нет узлов")
                .style(theme.dim_style())
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(theme.border_style())
                        .title(" канбан "),
                ),
            area,
        );
        return;
    }
    let constraints: Vec<Constraint> = app
        .board
        .columns
        .iter()
        .map(|_| Constraint::Ratio(1, app.board.columns.len() as u32))
        .collect();
    let chunks = Layout::horizontal(constraints).split(area);

    for (column_index, column) in app.board.columns.iter().enumerate() {
        let mut lines: Vec<Line<'_>> = Vec::new();
        for (row_index, row) in column.rows.iter().enumerate() {
            let holder = match row.lease_holder.as_deref() {
                Some(holder) => format!(" [{holder}]"),
                None => String::new(),
            };
            let selected = column_index == app.state.column && row_index == app.state.row;
            let mut line = Line::from(format!("{}{holder}", row.dot_id));
            line = if selected {
                line.style(theme.selected_style())
            } else {
                line.style(theme.text_style())
            };
            lines.push(line);
            // Название — второй строкой: в колонке шириной в десять знаков оно не влезает рядом
            // с адресом, а обрезать название до «TUI: инт…» бессмысленно.
            let mut title = Line::from(format!("  {}", row.title));
            title = if selected {
                title.style(theme.selected_style())
            } else if row.days_overdue > 0 {
                title.style(Style::default().fg(theme.overdue))
            } else {
                title.style(theme.dim_style())
            };
            lines.push(title);
        }
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(if column_index == app.state.column {
                        Style::default().fg(theme.status(&column.status))
                    } else {
                        theme.border_style()
                    })
                    .title(Span::styled(
                        format!(" {} ({}) ", column.status, column.count),
                        Style::default()
                            .fg(theme.status(&column.status))
                            .add_modifier(Modifier::BOLD),
                    )),
            ),
            chunks[column_index],
        );
    }
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
        let frame = render_once(&app::App::new(board.clone()));

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
        let frame = render_once(&app::App::new(board.clone()));
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
        let mut conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        run(&mut conn, None, None, true, false).expect("один кадр в обычный вывод без --project");
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
            // Выделение задаёт ТЕМА (цвет текста и фона), а не модификатор: иначе на терминале
            // без поддержки реверса курсор был бы невидим, а на цветной теме — неотличим от
            // обычной строки. Поэтому ищем строки с фоном выделения.
            let selected_bg = app.theme.selected_bg;
            for y in 0..buffer.area().height {
                let mut line = String::new();
                let mut marked = false;
                for x in 0..ONCE_WIDTH {
                    let cell = &buffer[(x, y)];
                    line.push_str(cell.symbol());
                    if cell.style().bg == Some(selected_bg) {
                        marked = true;
                    }
                }
                if marked {
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

    /// Кадр без `--project`: панель со ВСЕМИ проектами и переключатель в шапке.
    ///
    /// Проверяется по буферу: ключи обоих проектов, метка активного и слово «проект» в шапке —
    /// то, без чего человек не поймёт, что можно переключиться.
    #[test]
    fn the_frame_shows_the_panel_and_the_switcher() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-panel-{nanos}"));
        let outcome = crate::db::init(&dir, "FIRST", "first").expect("init");
        let mut conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let first = crate::project::first_project(&conn).expect("project");
        let second = crate::project::create(&mut conn, "SECOND", Some("второй")).expect("create");
        for (project, title) in [(&first, "узел первого"), (&second, "узел второго")]
        {
            crate::node::create(
                &mut conn,
                project,
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

        let projects = crate::project::list(&conn).expect("projects");
        let board = overview::board(&conn, &first).expect("board");
        let app = app::App::with_projects(board, "sess", projects, 0, first);
        let frame = render_once(&app);

        assert!(frame.contains("проекты"), "панели проектов нет:\n{frame}");
        assert!(
            frame.contains("FIRST"),
            "первый проект не в панели:\n{frame}"
        );
        assert!(
            frame.contains("SECOND"),
            "второй проект не в панели — переключаться не на что:\n{frame}"
        );
        assert!(
            frame.contains("проект: FIRST"),
            "в шапке нет переключателя проекта:\n{frame}"
        );
        assert!(
            frame.contains("узел первого") && !frame.contains("узел второго"),
            "на доске узлы чужого проекта:\n{frame}"
        );
    }

    /// Канбан: колонки статусов рядом, узлы внутри, курсор на выбранном узле.
    #[test]
    fn the_kanban_view_draws_one_column_per_status() {
        let board = overview::Board {
            project: "DEMO".to_string(),
            columns: vec![
                overview::BoardColumn {
                    status: "todo".to_string(),
                    count: 2,
                    rows: vec![
                        overview::BoardRow {
                            dot_id: "DEMO-1".to_string(),
                            title: "первая".to_string(),
                            priority: "medium".to_string(),
                            due_date: None,
                            days_overdue: 0,
                            lease_holder: None,
                            progress: "0%".to_string(),
                        },
                        overview::BoardRow {
                            dot_id: "DEMO-2".to_string(),
                            title: "вторая".to_string(),
                            priority: "medium".to_string(),
                            due_date: None,
                            days_overdue: 0,
                            lease_holder: None,
                            progress: "0%".to_string(),
                        },
                    ],
                },
                overview::BoardColumn {
                    status: "done".to_string(),
                    count: 1,
                    rows: vec![overview::BoardRow {
                        dot_id: "DEMO-3".to_string(),
                        title: "третья".to_string(),
                        priority: "medium".to_string(),
                        due_date: None,
                        days_overdue: 0,
                        lease_holder: None,
                        progress: "100%".to_string(),
                    }],
                },
            ],
            overdue: 0,
            leased: 0,
            open: 2,
            done: 1,
        };
        let mut app = app::App::new(board);
        app.state.view = model::View::Kanban;
        // Канбан выше списка: каждая строка занимает две строки (адрес и название).
        let height = frame_height(&app.board) + 4;
        let mut terminal = Terminal::new(TestBackend::new(ONCE_WIDTH, height)).expect("terminal");
        terminal.draw(|frame| draw(frame, &app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area().height {
            for x in 0..ONCE_WIDTH {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }

        assert!(
            text.contains("todo (2)"),
            "нет заголовка колонки todo:\n{text}"
        );
        assert!(
            text.contains("done (1)"),
            "нет заголовка колонки done:\n{text}"
        );
        for id in ["DEMO-1", "DEMO-3"] {
            assert!(text.contains(id), "узел {id} не попал в канбан:\n{text}");
        }
        assert!(
            text.contains("первая") && text.contains("третья"),
            "названия не показаны вторыми строками:\n{text}"
        );
    }

    /// Ширина: `COLUMNS` уважается, мусор в ней читается как «не задана», и ниже минимума кадр
    /// не верстается.
    ///
    /// Проверяется на функции с явными входами, а не на окружении процесса: `std::env` в тестах
    /// общий, и подмена `COLUMNS` ломала бы соседние тесты того же бинаря.
    #[test]
    fn the_width_comes_from_the_environment_when_valid() {
        assert_eq!(parse_width(Some("120")), Some(120));
        assert_eq!(parse_width(Some(" 100 ")), Some(100));
        assert_eq!(parse_width(Some("")), None, "пустая — это не ширина");
        assert_eq!(parse_width(Some("широко")), None, "мусор — это не ширина");
        assert_eq!(parse_width(Some("0")), None, "ноль колонок — не ширина");
        assert_eq!(
            parse_width(Some("20")),
            None,
            "уже минимума: панель и доска не влезают вместе"
        );
        assert_eq!(parse_width(None), None);
    }

    /// Цветной кадр содержит escape-последовательности, а обычный — нет, и оба несут один текст.
    ///
    /// Утверждение про ТЕКСТ важно: цветной кадр с другим содержимым означал бы, что гейт
    /// (`make smoke`, CI) проверяет не тот экран, который видит человек.
    #[test]
    fn the_coloured_frame_carries_the_same_text_and_adds_escapes() {
        let board = overview::Board {
            project: "DEMO".to_string(),
            columns: vec![overview::BoardColumn {
                status: "todo".to_string(),
                count: 1,
                rows: vec![overview::BoardRow {
                    dot_id: "DEMO-1".to_string(),
                    title: "задача".to_string(),
                    priority: "high".to_string(),
                    due_date: None,
                    days_overdue: 3,
                    lease_holder: Some("hermes".to_string()),
                    progress: "0%".to_string(),
                }],
            }],
            overdue: 1,
            leased: 1,
            open: 1,
            done: 0,
        };
        let app = app::App::new(board);
        let plain = render_once_sized(&app, 120, false);
        let coloured = render_once_sized(&app, 120, true);

        assert!(
            !plain.contains("\x1b["),
            "в обычном кадре появились escape-последовательности"
        );
        assert!(
            coloured.contains("\x1b["),
            "цветной кадр нарисован без цвета"
        );
        // Снимаем escape и сверяем текст построчно: цвет не должен менять содержимое.
        let stripped = strip_ansi(&coloured);
        assert_eq!(
            stripped, plain,
            "цветной кадр отличается текстом от обычного"
        );
        assert!(stripped.contains("DEMO-1"), "узла нет в кадре");
    }

    /// Каждая строка цветного кадра заканчивается сбросом: без него цвет «протекает» на всё, что
    /// человек напечатает после кадра.
    #[test]
    fn every_coloured_line_resets_the_style() {
        let board = overview::Board {
            project: "DEMO".to_string(),
            columns: Vec::new(),
            overdue: 0,
            leased: 0,
            open: 0,
            done: 0,
        };
        let app = app::App::new(board);
        let coloured = render_once_sized(&app, 100, true);
        for (number, line) in coloured.lines().enumerate() {
            if !line.contains("\x1b[") {
                continue;
            }
            assert!(
                line.ends_with("\x1b[0m"),
                "строка {} не сбрасывает стиль: {line:?}",
                number + 1
            );
        }
    }

    /// Ширина кадра уважается: узкая не паникует, широкая не добавляет пустых знаков в конце.
    #[test]
    fn the_frame_honours_the_requested_width() {
        let board = overview::Board {
            project: "DEMO".to_string(),
            columns: Vec::new(),
            overdue: 0,
            leased: 0,
            open: 0,
            done: 0,
        };
        let app = app::App::new(board);
        for width in [MIN_ONCE_WIDTH, 80, 140, 200] {
            let frame = render_once_sized(&app, width, false);
            for line in frame.lines() {
                let length = line.chars().count();
                assert!(
                    length <= usize::from(width),
                    "строка шире кадра ({length} > {width}): {line:?}"
                );
                assert!(
                    !line.ends_with(' '),
                    "в строке остались хвостовые пробелы: {line:?}"
                );
            }
        }
    }
}
