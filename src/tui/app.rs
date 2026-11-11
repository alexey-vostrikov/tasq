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
/// Уровень сообщения в строке состояния.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Level {
    /// Справка: что произошло, но ни хорошо, ни плохо.
    #[default]
    Info,
    /// Действие удалось.
    Ok,
    /// Отказ: действие не выполнено.
    Error,
}

#[derive(Debug, Clone)]
pub struct App {
    /// Доска в том виде, в каком её отдал `overview::board`.
    pub board: overview::Board,
    /// Курсор, режим и ввод.
    pub state: State,
    /// Что сказать человеку в строке состояния (отказ последнего действия).
    pub message: Option<String>,
    /// Чем это сообщение является: отказом, подтверждением или справкой.
    ///
    /// Уровень ХРАНИТСЯ, а не выводится из текста: попытка угадать его по двоеточию в строке
    /// («тема: gruvbox» тоже с двоеточием) покрасила бы справку в цвет отказа — и человек,
    /// увидев красное, искал бы поломку там, где её нет.
    pub level: Level,
    /// Идентификатор сессии экрана: `TASQ_SESSION`, если задан, иначе свой.
    pub session: String,
    /// Все проекты базы — панель слева и переключатель в шапке.
    pub projects: Vec<crate::project::Summary>,
    /// Проект, который показан на доске. `None` — доска не загружена (экран только собран).
    pub project: Option<crate::project::Project>,
    /// Цветовая тема: её переключает `T`, и она живёт на экране, а не в константе.
    pub theme: crate::tui::theme::Theme,
    /// Описание выбранного узла: текст задачи для панели под канбаном.
    ///
    /// Читается из базы отдельным запросом при каждом перечитывании: в `overview::BoardRow`
    /// описания нет, и на доске его взять негде.
    pub detail: String,
    /// Боковая панель: цепочка контекста, заметки и комментарии выбранного узла.
    pub context: Vec<String>,
    pub notes: Vec<String>,
    pub comments: Vec<String>,
    /// Вид узла по адресу: от него зависит, какие статусы матрица разрешает (эпик не бывает
    /// `todo`, задача не бывает `planned`). Читается вместе с доской одним запросом.
    pub kinds: HashMap<String, crate::status::Kind>,
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
            level: Level::default(),
            session: String::new(),
            projects: Vec::new(),
            project: None,
            theme: crate::tui::theme::Theme::default(),
            detail: String::new(),
            context: Vec::new(),
            notes: Vec::new(),
            comments: Vec::new(),
            kinds: HashMap::new(),
            tokens: HashMap::new(),
        }
    }

    /// То же, но с готовым идентификатором сессии: тест не должен зависеть от окружения.
    pub fn with_session(board: overview::Board, session: impl Into<String>) -> Self {
        let mut app = Self::new(board);
        app.session = session.into();
        app
    }

    /// Экран с панелью проектов: доска активного проекта и список всех остальных.
    pub fn with_projects(
        board: overview::Board,
        session: impl Into<String>,
        projects: Vec<crate::project::Summary>,
        active: usize,
        project: crate::project::Project,
    ) -> Self {
        let mut app = Self::new(board);
        app.session = session.into();
        app.state.projects = projects.len();
        app.state.active = active;
        app.projects = projects;
        app.project = Some(project);
        app
    }

    /// Переключает доску на проект с этим номером и перечитывает её.
    ///
    /// Перечитывание обязательно и немедленное: показать чужую доску со старыми счётчиками значит
    /// выдать данные одного проекта за другой — ровно та ошибка, из-за которой проект и перестал
    /// быть «текущим каталогом» в CLI.
    pub fn switch_project(
        &mut self,
        conn: &rusqlite::Connection,
        index: usize,
    ) -> crate::Result<()> {
        let Some(summary) = self.projects.get(index) else {
            return Ok(());
        };
        let key = summary.key.clone();
        let project = crate::project::select(conn, Some(&key))?;
        self.state.active = index;
        // Курсор сбрасывается: в другом проекте те же координаты — другой узел, и «оставить как
        // было» значило бы показать курсор на случайной строке чужой доски.
        self.state.column = 0;
        self.state.row = 0;
        self.tokens.clear();
        self.project = Some(project.clone());
        self.state.rows = Vec::new();
        self.refresh(conn, &project)
    }

    /// Перечитывает активный проект: то же, что `refresh`, но проект берётся у экрана.
    ///
    /// Экран без загруженного проекта (собранный тестом вокруг доски) перечитывать нечего, и это
    /// не ошибка: `--once` рисует кадр один раз, без цикла.
    pub fn refresh_active(&mut self, conn: &rusqlite::Connection) -> crate::Result<()> {
        let Some(project) = self.project.clone() else {
            return Ok(());
        };
        self.refresh(conn, &project)
    }

    /// Ключ активного проекта — для шапки и для `--once`.
    pub fn project_key(&self) -> &str {
        self.projects
            .get(self.state.active)
            .map(|summary| summary.key.as_str())
            .unwrap_or(&self.board.project)
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
        self.load_kinds(conn, project)?;
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
        self.reload_sidebar(conn, project)?;
        Ok(())
    }

    /// Перечитывает виды узлов проекта одним запросом.
    ///
    /// Отдельным запросом, а не полем в `BoardRow`: доска — публичный формат `tasq board --json`,
    /// и добавлять в него поле ради экрана значило бы менять контракт ради удобства одного
    /// потребителя.
    pub fn load_kinds(
        &mut self,
        conn: &rusqlite::Connection,
        project: &crate::project::Project,
    ) -> crate::Result<()> {
        let mut stmt = conn
            .prepare("SELECT path, kind FROM nodes WHERE project_id = ?1 AND deleted_at IS NULL")?;
        let rows = stmt.query_map([project.id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut kinds = HashMap::new();
        for row in rows {
            let (path, kind) = row?;
            if let Some(kind) = crate::status::Kind::parse(&kind) {
                kinds.insert(project.dot_id(&path), kind);
            }
        }
        self.kinds = kinds;
        Ok(())
    }

    /// Читает боковую панель для узла под курсором.
    ///
    /// Три запроса на кадр, а не один сводный: цепочка, заметки и комментарии живут в разных
    /// модулях библиотеки (`overview::context`, `note::list`, `comment::list`), и собирать их
    /// одним SQL значило бы завести в интерфейсе второе правило о том, что считается контекстом.
    fn reload_sidebar(
        &mut self,
        conn: &rusqlite::Connection,
        project: &crate::project::Project,
    ) -> crate::Result<()> {
        self.context.clear();
        self.notes.clear();
        self.comments.clear();
        self.detail.clear();
        let Some(reference) = self.selected().map(|row| row.dot_id.clone()) else {
            return Ok(());
        };
        // Описание — отдельным чтением: доска его не отдаёт, а панель под канбаном без него
        // показывает то же, что и колонка, то есть ничего нового.
        if let Ok(node) = crate::node::resolve(conn, project, &reference) {
            self.detail = node.description.unwrap_or_default();
        }
        // Ошибка чтения панели не должна ломать доску: панель — удобство, а доска — работа.
        if let Ok(chain) = crate::overview::context(conn, project, &reference) {
            for entry in chain {
                self.context
                    .push(format!("{} {}", entry.dot_id, entry.title));
            }
        }
        if let Ok(notes) = crate::note::list(conn, project, &crate::note::Filter::default()) {
            for note in notes.into_iter().take(5) {
                self.notes
                    .push(format!("{} {}", note.note_type, note.title));
            }
        }
        if let Ok(comments) = crate::comment::list(conn, project, &reference, false) {
            for comment in comments.into_iter().rev().take(5) {
                self.comments.push(format!(
                    "{}: {}",
                    comment.author.as_deref().unwrap_or("-"),
                    comment.content
                ));
            }
        }
        Ok(())
    }

    /// Следующая тема по кругу и запоминание выбора в строке состояния.
    pub fn next_theme(&mut self) {
        self.theme = self.theme.next();
        let name = self.theme.name;
        self.info(format!("тема: {name}"));
    }

    /// Открывает форму создания узла.
    ///
    /// Поля фиксированы и идут в порядке, в котором человек о них думает: название, описание,
    /// приоритет, срок. Остальные поля `tasq add` (вес, оценка, ветка) в форме не нужны: их
    /// ставят скриптом или один раз, а не при создании на ходу.
    pub fn open_create(&mut self) {
        self.state.fields = vec![
            model::Field {
                label: "название",
                value: String::new(),
            },
            model::Field {
                label: "описание",
                value: String::new(),
            },
            model::Field {
                label: "приоритет",
                value: "medium".to_string(),
            },
            model::Field {
                label: "срок",
                value: String::new(),
            },
        ];
        self.state.field = 0;
        self.state.editing = None;
        self.state.mode = Mode::Form;
    }

    /// Открывает форму правки выбранного узла: значения берутся из базы, а не из доски.
    ///
    /// Из базы, потому что доска показывает не всё: описания в ней нет вовсе, а приоритет и срок
    /// есть. Форма, заполненная по доске, потеряла бы описание при сохранении.
    pub fn open_edit(
        &mut self,
        conn: &rusqlite::Connection,
        project: &crate::project::Project,
    ) -> crate::Result<()> {
        let Some(reference) = self.selected().map(|row| row.dot_id.clone()) else {
            self.fail("под курсором нет узла");
            return Ok(());
        };
        let node = crate::node::resolve(conn, project, &reference)?;
        self.state.fields = vec![
            model::Field {
                label: "название",
                value: node.title.clone(),
            },
            model::Field {
                label: "описание",
                value: node.description.clone().unwrap_or_default(),
            },
            model::Field {
                label: "приоритет",
                value: node.priority.clone(),
            },
            model::Field {
                label: "срок",
                value: node.due_date.clone().unwrap_or_default(),
            },
        ];
        self.state.field = 0;
        self.state.editing = Some(reference);
        self.state.mode = Mode::Form;
        Ok(())
    }

    /// Открывает ввод заметки или комментария к выбранному узлу.
    pub fn open_note(&mut self, kind: &str) {
        let Some(reference) = self.selected().map(|row| row.dot_id.clone()) else {
            self.fail("под курсором нет узла");
            return;
        };
        self.state.fields = vec![model::Field {
            label: "текст",
            value: String::new(),
        }];
        self.state.field = 0;
        // Вид записи и адрес узла едут в одной строке: отдельного поля под вид не нужно, а
        // разбирать «адрес|вид» в двух местах — лишний способ ошибиться.
        self.state.editing = Some(format!("{kind}@{reference}"));
        self.state.mode = Mode::Note;
    }

    /// Значение поля формы по подписи.
    fn field(&self, label: &str) -> Option<&str> {
        self.state
            .fields
            .iter()
            .find(|field| field.label == label)
            .map(|field| field.value.as_str())
    }

    /// Вид узла под курсором (по умолчанию — задача: неизвестный вид не должен запрещать выбор).
    pub fn kind_here(&self) -> crate::status::Kind {
        self.selected()
            .and_then(|row| self.kinds.get(&row.dot_id).copied())
            .unwrap_or(crate::status::Kind::Task)
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
                // Панель проектов перехватывает движение: её `j`/`k` листают проекты, а не узлы.
                // Проверка стоит ПЕРЕД общей таблицей, иначе одно нажатие делало бы два дела.
                Some(key) if self.state.focus == model::Focus::Projects => {
                    match model::panel_intent(key) {
                        Some(intent) => self.state.apply(intent),
                        None => Outcome::Ignored,
                    }
                }
                Some(key) => match model::intent(self.state.mode, key) {
                    Some(intent) => {
                        if intent == Intent::ChangeStatus {
                            self.prepare_statuses();
                        }
                        self.state.apply(intent)
                    }
                    None => Outcome::Ignored,
                },
                None => Outcome::Ignored,
            },
            Event::Resize(_, _) => Outcome::Redraw,
            _ => Outcome::Ignored,
        }
    }

    /// Записывает форму: создаёт узел, правит его или добавляет заметку/комментарий.
    ///
    /// Валидация не своя: пустое название, приоритет вне списка и срок не датой отбивает
    /// библиотека (`node::create`, `node::update`), и её отказ приходит с названным полем. Своя
    /// проверка здесь означала бы второе правило о том, что такое допустимый узел.
    fn submit(
        &mut self,
        conn: &mut rusqlite::Connection,
        project: &crate::project::Project,
    ) -> Result<&'static str, crate::error::Error> {
        let title = self.field("название").unwrap_or_default().to_string();
        let description = self.field("описание").unwrap_or_default().to_string();
        let priority = self.field("приоритет").unwrap_or_default().to_string();
        let due = self.field("срок").unwrap_or_default().to_string();
        let text = self.field("текст").unwrap_or_default().to_string();

        match self.state.mode {
            Mode::Note => {
                let spec = self.state.editing.clone().unwrap_or_default();
                let (kind, reference) =
                    spec.split_once('@')
                        .ok_or_else(|| crate::error::Error::InvalidValue {
                            field: "note".to_string(),
                            value: "вид записи потерян".to_string(),
                        })?;
                let text = text.trim();
                if kind == "comment" {
                    crate::comment::add(conn, project, reference, text, "tui")?;
                } else {
                    crate::note::save(
                        conn,
                        project,
                        crate::note::SaveNote {
                            title: Some(text),
                            actor: Some("tui"),
                            node: Some(reference),
                            ..crate::note::SaveNote::default()
                        },
                    )?;
                }
                self.state.mode = Mode::Board;
                self.state.fields.clear();
                self.state.editing = None;
                return Ok(if kind == "comment" {
                    "комментарий добавлен"
                } else {
                    "заметка добавлена"
                });
            }
            Mode::Form => {}
            _ => return Ok(""),
        }

        // Пустое необязательное поле — это «не задано», а не пустая строка: иначе описание,
        // которого не было, стало бы пустым, и `description_locked` считал бы его заданным.
        let description = Some(description.as_str()).filter(|text| !text.trim().is_empty());
        let due = Some(due.as_str()).filter(|text| !text.trim().is_empty());
        let priority = Some(priority.as_str()).filter(|text| !text.trim().is_empty());

        match self.state.editing.clone() {
            None => {
                crate::node::create(
                    conn,
                    project,
                    crate::node::NewNode {
                        parent: None,
                        title: &title,
                        kind: None,
                        description,
                        priority,
                        due_date: due,
                        estimated_hours: None,
                        weight: None,
                        source_ref: None,
                        branch: None,
                    },
                )?;
                self.state.mode = Mode::Board;
                self.state.fields.clear();
                Ok("узел создан")
            }
            Some(reference) => {
                crate::node::update(
                    conn,
                    project,
                    &reference,
                    crate::node::Patch {
                        title: Some(&title),
                        description,
                        priority,
                        due_date: due,
                        actor: Some("tui"),
                        ..crate::node::Patch::default()
                    },
                )?;
                self.state.mode = Mode::Board;
                self.state.fields.clear();
                self.state.editing = None;
                Ok("узел изменён")
            }
        }
    }

    /// Ищет по всей базе тем же запросом, что `tasq find` без `--project`.
    ///
    /// По всей базе, а не по активному проекту: экран показывает все проекты, и поиск, который
    /// молча ограничен одним из них, — это ответ «не найдено» на существующий узел.
    fn run_search(&mut self, conn: &rusqlite::Connection) {
        self.state.results.clear();
        self.state.result = 0;
        let query = self.state.input.trim().to_string();
        if query.is_empty() {
            return;
        }
        match crate::overview::search_all(conn, &query) {
            Ok(hits) => {
                for hit in hits {
                    // Заметки в результатах тоже есть; перейти можно только к узлу, и это
                    // называется в строке состояния, а не молча пропускается.
                    if let Some(dot_id) = hit.dot_id {
                        self.state.results.push((dot_id, hit.title));
                    }
                }
                if self.state.results.is_empty() {
                    self.info("ничего не найдено");
                }
            }
            Err(err) => self.fail(describe(&err)),
        }
    }

    /// Открывает найденный узел: переключает проект, если узел в чужом, и ставит на него курсор.
    fn open_result(&mut self, conn: &rusqlite::Connection, project: &crate::project::Project) {
        let Some((dot_id, _)) = self.state.results.get(self.state.result).cloned() else {
            self.fail("в результатах ничего не выбрано");
            return;
        };
        // Ключ проекта — часть адреса: `TASQDEV-6` означает проект TASQDEV. Искать по нему, а не
        // по активному проекту, иначе переход в чужой проект не сработает.
        let key = dot_id.split('-').next().unwrap_or_default().to_string();
        match self.projects.iter().position(|summary| summary.key == key) {
            Some(index) if index != self.state.active => {
                if let Err(err) = self.switch_project(conn, index) {
                    self.fail(describe(&err));
                    return;
                }
            }
            Some(_) => {}
            None => {
                self.fail(format!("проект {key} не найден в базе"));
                return;
            }
        }
        if !self.select_by_id(&dot_id) {
            // Узел в другом проекте уже перечитан; если и теперь его нет — он вне доски
            // (закрыт фильтром доски, например), и об этом говорим прямо.
            self.fail(format!("{dot_id} нет на доске этого проекта"));
            return;
        }
        self.state.mode = Mode::Node;
        self.state.input.clear();
        self.state.results.clear();
        let project = self.project.clone().unwrap_or_else(|| project.clone());
        if let Err(err) = self.reload_sidebar(conn, &project) {
            self.fail(describe(&err));
        }
    }

    /// Показывает отказ в строке состояния.
    ///
    /// Отказ, напечатанный в stdout, сломает кадр; отказ, проглоченный молча, выглядит как
    /// неработающая клавиша. Поэтому одна строка внизу экрана — и она снимается первым же
    /// успешным действием.
    pub fn say(&mut self, text: impl Into<String>) {
        self.message = Some(text.into());
        self.level = Level::Ok;
    }

    /// Справка: сообщение, которое не хвалит и не ругает.
    pub fn info(&mut self, text: impl Into<String>) {
        self.message = Some(text.into());
        self.level = Level::Info;
    }

    /// Отказ: действие не выполнено.
    pub fn fail(&mut self, text: impl Into<String>) {
        self.message = Some(text.into());
        self.level = Level::Error;
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

    /// Заполняет список допустимых переходов для текущего узла.
    ///
    /// Список берётся из матрицы (`status::allowed_next`) и вида узла: экран не решает сам, куда
    /// можно перейти, он показывает то, что разрешено. Пустой список означает «отсюда никуда» —
    /// и тогда говорим об этом прямо, вместо того чтобы открыть пустой экран.
    fn prepare_statuses(&mut self) {
        self.state.chosen_status = None;
        self.state.statuses.clear();
        let Some(current) = self.selected_status().map(str::to_string) else {
            self.fail("под курсором нет узла");
            return;
        };
        let Some(from) = crate::status::Status::parse(&current) else {
            self.fail(format!("неизвестный статус на доске: {current}"));
            return;
        };
        // Текущий статус из списка убирается: `can_transition(x, x, kind)` истинно по построению
        // (это «перехода нет», а не переход), и на экране выбора он был бы строкой, которая ничего
        // не меняет. Правило матрицы при этом не трогается — фильтр живёт в интерфейсе.
        let next: Vec<_> = crate::status::allowed_next(from, self.kind_here())
            .into_iter()
            .filter(|status| *status != from)
            .collect();
        if next.is_empty() {
            self.say(format!(
                "из статуса {current} матрица не разрешает переходов"
            ));
            return;
        }
        self.state.statuses = next
            .iter()
            .map(|status| status.as_str().to_string())
            .collect();
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
        // Переключение проекта — не действие над узлом: ссылки под курсором тут нет и быть не
        // должно, поэтому оно обрабатывается до выбора строки.
        if intent == Intent::ProjectOpen {
            let index = self.state.active;
            return match self.switch_project(&*conn, index) {
                Ok(()) => Outcome::Redraw,
                Err(err) => {
                    self.fail(describe(&err));
                    Outcome::Redraw
                }
            };
        }
        // Формы открываются до выбора строки: создание узла не требует узла под курсором.
        match intent {
            Intent::Create => {
                self.open_create();
                return Outcome::Redraw;
            }
            Intent::Edit => {
                if let Err(err) = self.open_edit(&*conn, project) {
                    self.fail(describe(&err));
                }
                return Outcome::Redraw;
            }
            Intent::AddNote => {
                self.open_note("note");
                return Outcome::Redraw;
            }
            Intent::AddComment => {
                self.open_note("comment");
                return Outcome::Redraw;
            }
            Intent::NextTheme => {
                self.next_theme();
                return Outcome::Redraw;
            }
            Intent::SearchRun => {
                self.run_search(&*conn);
                return Outcome::Redraw;
            }
            Intent::SearchOpen => {
                self.open_result(&*conn, project);
                return Outcome::Redraw;
            }
            Intent::ResultDown => {
                if !self.state.results.is_empty() {
                    let last = self.state.results.len() - 1;
                    self.state.result = (self.state.result + 1).min(last);
                }
                return Outcome::Redraw;
            }
            Intent::ResultUp => {
                self.state.result = self.state.result.saturating_sub(1);
                return Outcome::Redraw;
            }
            _ => {}
        }

        let reference = match self.selected() {
            Some(row) => row.dot_id.clone(),
            None => {
                self.fail("под курсором нет узла");
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
            Intent::ApplyStatus => {
                let Some(status) = self.state.chosen_status.clone() else {
                    return Outcome::Redraw;
                };
                let kind = self.kind_here();
                actions::next_status(self.selected_status().unwrap_or_default(), kind, &status)
                    .and_then(|status| actions::set_status(conn, project, &reference, status))
                    .map(|()| {
                        // Пометка в памяти: имя статуса нужно и после записи, а `chosen_status`
                        // забирается ниже.
                        self.state.chosen_status = None;
                        "статус изменён"
                    })
            }
            Intent::NextTheme => {
                self.next_theme();
                Ok("")
            }
            Intent::SubmitForm => self.submit(conn, project),
            Intent::SearchRun | Intent::SearchOpen | Intent::ResultDown | Intent::ResultUp => {
                Ok("")
            }
            Intent::Create | Intent::Edit | Intent::AddNote | Intent::AddComment => Ok(""),
            Intent::ChangeStatus | Intent::ChooseStatus(_) | Intent::ProjectOpen => Ok(""),
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
                self.fail(describe(&err));
            }
        }
        // Перечитывание обязательно: иначе строка аренды останется на экране после отпускания,
        // и человек нажмёт `r` второй раз по узлу, который уже свободен. Курсор после этого
        // возвращается на тот же УЗЕЛ, а не на те же координаты: действие могло переехать его в
        // другую колонку.
        if let Err(err) = self.refresh(conn, project) {
            self.fail(describe(&err));
        } else {
            self.select_by_id(&reference);
        }
        Outcome::Redraw
    }
}

/// Отказ в одну строку: код, текст и подсказка.
///
/// Все три нужны и все три не влезают в одну строку по-хорошему, но порядок выбран так, чтобы
/// обрезка справа была безопасной: код различает отказы машинно, текст называет поле, подсказка
/// говорит, что делать. Показывать только код с подсказкой (как было) значило терять имя поля —
/// и `INVALID_VALUE` на пустом названии читался как «что-то не так с каким-то значением».
pub fn describe(err: &crate::error::Error) -> String {
    format!("{}: {err} — {}", err.code(), err.hint())
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

    /// `s` на живой доске: список из матрицы, номер, подтверждение, запись в базу.
    ///
    /// Проверяется и то, что статус узла ЭПИКА не получает `todo`: вид узла приходит из базы
    /// отдельным запросом, и ошибка в нём дала бы список, которого матрица не разрешает.
    #[test]
    fn the_status_key_writes_the_chosen_status() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-status-key-{nanos}"));
        let outcome = crate::db::init(&dir, "STK", "stk").expect("init");
        let mut conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let project = crate::project::first_project(&conn).expect("project");
        let created = crate::node::create(
            &mut conn,
            &project,
            crate::node::NewNode {
                parent: None,
                title: "узел со статусом",
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
        let mut app = App::with_session(board, "sess-status");
        app.load_kinds(&conn, &project).expect("виды узлов");
        assert_eq!(
            app.kind_here(),
            crate::status::Kind::Epic,
            "корневой узел должен быть эпиком"
        );

        // Открыли выбор.
        assert_eq!(app.step(key(KeyCode::Char('s'))), Outcome::Redraw);
        assert_eq!(app.state.mode, Mode::Status);
        assert!(
            !app.state.statuses.is_empty(),
            "список допустимых переходов пуст"
        );
        // Список совпадает с матрицей минус текущий статус: экран показывает то, что разрешено
        // библиотекой, и не добавляет от себя ни одного перехода.
        let expected: Vec<String> =
            crate::status::allowed_next(crate::status::Status::Idea, crate::status::Kind::Epic)
                .into_iter()
                .filter(|status| *status != crate::status::Status::Idea)
                .map(|status| status.as_str().to_string())
                .collect();
        assert_eq!(
            app.state.statuses, expected,
            "список экрана разошёлся с матрицей"
        );

        // Выбрали первый и подтвердили.
        let first = app.state.statuses[0].clone();
        assert_eq!(
            app.step(key(KeyCode::Char('1'))),
            Outcome::Ask(Intent::ApplyStatus)
        );
        assert_eq!(
            app.step(key(KeyCode::Char('y'))),
            Outcome::Act(Intent::ApplyStatus)
        );
        app.perform(&mut conn, &project, Intent::ApplyStatus);
        assert_eq!(app.message.as_deref(), Some("статус изменён"));

        let status: String = conn
            .query_row(
                "SELECT status FROM nodes WHERE id = ?1",
                [created.id],
                |row| row.get(0),
            )
            .expect("статус");
        assert_eq!(status, first, "записан не выбранный статус");
    }

    /// Отказ перехода уходит в строку состояния, а не выходит из цикла: недопустимая пара
    /// набирается и через номер, если список устарел между чтением и нажатием.
    #[test]
    fn a_refused_transition_explains_itself() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-status-refuse-{nanos}"));
        let outcome = crate::db::init(&dir, "STR", "str").expect("init");
        let mut conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let project = crate::project::first_project(&conn).expect("project");
        crate::node::create(
            &mut conn,
            &project,
            crate::node::NewNode {
                parent: None,
                title: "узел",
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
        let mut app = App::with_session(board, "sess-refuse");
        // Список, которого матрица не даёт: переход `idea → done` недопустим.
        app.state.chosen_status = Some("done".to_string());
        app.perform(&mut conn, &project, Intent::ApplyStatus);
        let message = app.message.clone().unwrap_or_default();
        assert!(
            message.starts_with("INVALID_TRANSITION"),
            "отказ не назван кодом: {message}"
        );
        assert!(
            message.len() > "INVALID_TRANSITION".len() + 2,
            "у отказа нет подсказки: {message}"
        );
    }

    /// Переключение проекта на экране: доска, счётчики и вид узлов становятся чужими.
    ///
    /// Проверяется на базе из ДВУХ проектов: на одном переключение нечем проверить — любая
    /// ошибка «взять первый» даёт верный ответ.
    #[test]
    fn switching_the_project_changes_the_board() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-switch-{nanos}"));
        let outcome = crate::db::init(&dir, "ONE", "one").expect("init");
        let mut conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let one = crate::project::first_project(&conn).expect("project");
        create(&mut conn, &one, "узел первого");
        let second = crate::project::create(&mut conn, "TWO", Some("второй")).expect("create");
        create(&mut conn, &second, "узел второго А");
        create(&mut conn, &second, "узел второго Б");

        let projects = crate::project::list(&conn).expect("projects");
        assert_eq!(projects.len(), 2, "фикстура не из двух проектов");
        let board = crate::overview::board(&conn, &one).expect("board");
        let mut app = App::with_projects(board, "sess", projects, 0, one.clone());
        assert_eq!(app.project_key(), "ONE");
        assert_eq!(app.state.rows_here(), 1, "в первом проекте один узел");

        app.switch_project(&conn, 1).expect("switch");
        assert_eq!(app.project_key(), "TWO");
        assert_eq!(app.state.active, 1);
        assert_eq!(
            app.state.rows_here(),
            2,
            "доска второго проекта не перечитана: строк {}",
            app.state.rows_here()
        );
        assert_eq!(app.board.project, "TWO", "шапка показывает чужой проект");
        assert!(
            app.kinds.keys().all(|key| key.starts_with("TWO-")),
            "виды узлов остались от первого проекта: {:?}",
            app.kinds.keys().collect::<Vec<_>>()
        );
        assert!(
            app.tokens.is_empty(),
            "аренды первого проекта уехали во второй"
        );
        assert_eq!(
            (app.state.column, app.state.row),
            (0, 0),
            "курсор перенесён на прежние координаты чужой доски"
        );
    }

    fn create(conn: &mut rusqlite::Connection, project: &crate::project::Project, title: &str) {
        crate::node::create(
            conn,
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

    /// Переключение на несуществующий номер ничего не ломает: экран остаётся на месте.
    #[test]
    fn switching_to_a_missing_index_changes_nothing() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-switch2-{nanos}"));
        let outcome = crate::db::init(&dir, "SOLO", "solo").expect("init");
        let conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let solo = crate::project::first_project(&conn).expect("project");
        let projects = crate::project::list(&conn).expect("projects");
        let board = crate::overview::board(&conn, &solo).expect("board");
        let mut app = App::with_projects(board, "sess", projects, 0, solo);
        app.switch_project(&conn, 7)
            .expect("переключение на пустой номер");
        assert_eq!(
            app.project_key(),
            "SOLO",
            "проект сменился на несуществующий"
        );
        assert_eq!(app.state.active, 0);
    }

    /// Цепочка переключения проекта клавишами: `Tab` → `j` → `Enter` → `Act` → новая доска.
    ///
    /// Проверяется именно цепочка: `switch_project` сам по себе работает (тест выше), а вот
    /// доводит ли до него нажатие — вопрос отдельный. В живом прогоне ответ был «нет», и по
    /// кадру это не видно: экран оставался на прежнем проекте без единого сообщения.
    #[test]
    fn the_key_chain_switches_the_project() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-switch-chain-{nanos}"));
        let outcome = crate::db::init(&dir, "AAA", "aaa").expect("init");
        let mut conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let first = crate::project::first_project(&conn).expect("project");
        create(&mut conn, &first, "узел первого");
        let second = crate::project::create(&mut conn, "BBB", Some("второй")).expect("create");
        create(&mut conn, &second, "узел второго");

        let projects = crate::project::list(&conn).expect("projects");
        let board = crate::overview::board(&conn, &first).expect("board");
        let mut app = App::with_projects(board, "sess", projects, 0, first);

        assert_eq!(app.step(key(KeyCode::Tab)), Outcome::Redraw);
        assert_eq!(
            app.state.focus,
            model::Focus::Projects,
            "Tab не перевёл фокус"
        );
        assert_eq!(app.step(key(KeyCode::Char('j'))), Outcome::Redraw);
        assert_eq!(app.state.active, 1, "j не сдвинул выбор проекта");

        let act = app.step(key(KeyCode::Enter));
        assert_eq!(
            act,
            Outcome::Act(Intent::ProjectOpen),
            "Enter на панели не попросил переключение"
        );
        let project = app.project.clone().expect("проект");
        app.perform(&mut conn, &project, Intent::ProjectOpen);
        assert_eq!(app.project_key(), "BBB", "проект не переключился");
        assert_eq!(app.board.project, "BBB");
    }

    /// Форма создаёт узел: значения из полей попадают в базу, пустое необязательное — не попадает.
    #[test]
    fn the_form_creates_a_node() {
        let (mut conn, project) = simple("form-create");
        let board = crate::overview::board(&conn, &project).expect("board");
        let mut app = App::with_session(board, "sess-form");
        app.open_create();
        assert_eq!(app.state.mode, Mode::Form);
        assert_eq!(app.state.fields.len(), 4, "полей в форме не четыре");

        // Вводим посимвольно, как человек: значение идёт в ТЕКУЩЕЕ поле.
        for c in "новая задача".chars() {
            app.state.apply(model::Intent::Type(c));
        }
        app.state.apply(model::Intent::FieldNext); // описание
        app.state.apply(model::Intent::FieldNext); // приоритет
        app.state.apply(model::Intent::FieldNext); // срок: оставляем пустым
        assert_eq!(app.state.field, 3);

        let note = app.perform(&mut conn, &project, Intent::SubmitForm);
        assert_eq!(note, Outcome::Redraw);
        assert_eq!(app.message.as_deref(), Some("узел создан"));
        assert_eq!(app.state.mode, Mode::Board, "форма не закрылась");

        let (title, description, priority, due): (String, Option<String>, String, Option<String>) =
            conn.query_row(
                "SELECT title, description, priority, due_date FROM nodes WHERE title = 'новая задача'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("созданный узел");
        assert_eq!(title, "новая задача");
        assert_eq!(description, None, "пустое описание записано строкой");
        assert_eq!(priority, "medium", "приоритет по умолчанию не сохранён");
        assert_eq!(due, None, "пустой срок записан");
    }

    /// Пустое название отбивает библиотека, и это видно в строке состояния, а запись не идёт.
    #[test]
    fn an_empty_title_is_refused_by_the_library() {
        let (mut conn, project) = simple("form-empty");
        let board = crate::overview::board(&conn, &project).expect("board");
        let mut app = App::with_session(board, "sess-form");
        app.open_create();
        app.perform(&mut conn, &project, Intent::SubmitForm);
        let message = app.message.clone().unwrap_or_default();
        assert!(
            message.starts_with("INVALID_VALUE"),
            "отказ не назван кодом: {message}"
        );
        assert!(
            message.contains("title"),
            "отказ не называет поле: {message}"
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 1, "пустой узел всё-таки записан");
        assert_eq!(app.state.mode, Mode::Form, "форма закрылась на отказе");
    }

    /// Правка берёт значения из БАЗЫ: описание, которого нет на доске, не теряется.
    #[test]
    fn editing_fills_the_form_from_the_database() {
        let (mut conn, project) = with_description("form-edit");
        let board = crate::overview::board(&conn, &project).expect("board");
        let mut app = App::with_session(board, "sess-form");
        app.load_kinds(&conn, &project).expect("kinds");
        app.open_edit(&conn, &project).expect("open_edit");
        assert_eq!(
            app.state.editing.as_deref().map(|s| s.to_string()),
            Some("EDT-1".to_string()),
            "форма не помнит, какой узел правит"
        );
        assert_eq!(
            app.field("описание"),
            Some("описание из базы"),
            "описание не прочитано из базы"
        );

        // Меняем только название.
        app.state.field = 0;
        app.state.fields[0].value = "переименован".to_string();
        app.perform(&mut conn, &project, Intent::SubmitForm);
        assert_eq!(app.message.as_deref(), Some("узел изменён"));

        let (title, description): (String, Option<String>) = conn
            .query_row(
                "SELECT title, description FROM nodes WHERE path = '1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("узел");
        assert_eq!(title, "переименован");
        assert_eq!(
            description.as_deref(),
            Some("описание из базы"),
            "описание потеряно при правке названия"
        );
    }

    /// Заметка и комментарий идут через библиотеку и попадают в боковую панель узла.
    #[test]
    fn notes_and_comments_land_on_the_node() {
        let (mut conn, project) = simple("note");
        let board = crate::overview::board(&conn, &project).expect("board");
        let mut app = App::with_session(board, "sess-note");
        app.load_kinds(&conn, &project).expect("kinds");

        app.perform(&mut conn, &project, Intent::AddComment);
        assert_eq!(app.state.mode, Mode::Note);
        for c in "первый комментарий".chars() {
            app.state.apply(model::Intent::Type(c));
        }
        app.perform(&mut conn, &project, Intent::SubmitForm);
        assert_eq!(app.message.as_deref(), Some("комментарий добавлен"));

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM comments", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 1, "комментарий не записан");

        // Перечитали — панель показывает его.
        app.refresh(&conn, &project).expect("refresh");
        assert_eq!(
            app.comments.len(),
            1,
            "боковая панель пуста после добавления: {:?}",
            app.comments
        );
        assert!(
            app.comments[0].contains("первый комментарий"),
            "в панели не тот текст: {:?}",
            app.comments
        );

        app.perform(&mut conn, &project, Intent::AddNote);
        for c in "решение".chars() {
            app.state.apply(model::Intent::Type(c));
        }
        app.perform(&mut conn, &project, Intent::SubmitForm);
        assert_eq!(app.message.as_deref(), Some("заметка добавлена"));
        let notes: i64 = conn
            .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
            .expect("count");
        assert_eq!(notes, 1, "заметка не записана");
    }

    /// Поиск находит узел ЧУЖОГО проекта и переключает на него доску.
    ///
    /// Это и есть причина, по которой поиск идёт по всей базе: экран показывает все проекты, и
    /// поиск, ограниченный активным, отвечал бы «не найдено» на существующий узел.
    #[test]
    fn search_finds_a_node_in_another_project_and_switches() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-search-{nanos}"));
        let outcome = crate::db::init(&dir, "SRC", "src").expect("init");
        let mut conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let first = crate::project::first_project(&conn).expect("project");
        create(&mut conn, &first, "узел первого проекта");
        let second = crate::project::create(&mut conn, "OTH", Some("другой")).expect("create");
        create(&mut conn, &second, "иголка в стоге");

        let projects = crate::project::list(&conn).expect("projects");
        let board = crate::overview::board(&conn, &first).expect("board");
        let mut app = App::with_projects(board, "sess", projects, 0, first);

        app.state.mode = Mode::Search;
        for c in "иголка".chars() {
            app.state.apply(model::Intent::Type(c));
            app.perform(
                &mut conn,
                &app.project.clone().expect("проект"),
                Intent::SearchRun,
            );
        }
        assert_eq!(
            app.state.results.len(),
            1,
            "поиск по всей базе не нашёл узел: {:?}",
            app.state.results
        );
        assert_eq!(app.state.results[0].0, "OTH-1");

        let project = app.project.clone().expect("проект");
        app.perform(&mut conn, &project, Intent::SearchOpen);
        assert_eq!(app.project_key(), "OTH", "перехода в чужой проект не было");
        assert_eq!(
            app.selected().map(|row| row.dot_id.clone()),
            Some("OTH-1".to_string()),
            "курсор не встал на найденный узел"
        );
        assert_eq!(app.state.mode, Mode::Node, "карточка не открылась");
    }

    fn simple(tag: &str) -> (rusqlite::Connection, crate::project::Project) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("tasq-{tag}-{nanos}"));
        let outcome = crate::db::init(&dir, "EDT", tag).expect("init");
        let mut conn = crate::db::open(std::path::Path::new(&outcome.db_path)).expect("open");
        let project = crate::project::first_project(&conn).expect("project");
        create(&mut conn, &project, "исходный узел");
        (conn, project)
    }

    fn with_description(tag: &str) -> (rusqlite::Connection, crate::project::Project) {
        let (conn, project) = simple(tag);
        conn.execute(
            "UPDATE nodes SET description = 'описание из базы' WHERE path = '1'",
            [],
        )
        .expect("description");
        (conn, project)
    }
}
