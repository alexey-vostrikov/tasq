//! Обзор проекта: поиск, цепочка контекста, доска, пакетные правки, учёт времени.
//!
//! Это «одна точка входа» для агента: что происходит, что искать, что делать дальше.

use rusqlite::types::Value;
use rusqlite::{Connection, params, params_from_iter};
use serde::Serialize;

use crate::activity;
use crate::error::{Error, Result};
use crate::node::{self, Filter};
use crate::project::Project;
use crate::status::Status;

/// Найденная сущность.
#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub entity: String,
    pub dot_id: Option<String>,
    pub title: String,
    pub snippet: String,
}

/// Почему поверхность поиска ответила не индексом.
///
/// Два случая различаются принципиально, и смешивать их нельзя. `QueryNotUnderstood` — обычное
/// дело: FTS5 не разбирает `файл:строка` или незакрытую кавычку, и отступить к поиску по словам
/// здесь правильно. `IndexMissing` — дефект состояния базы: индекса нет, и ответ пришёл другим
/// путём. Оба ведут к ответу худшего качества (без ранжирования и словоформ), поэтому оба
/// называются, но чинятся по-разному: первый не чинится никак, второй — `tasq migrate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Fallback {
    /// Запрос не разобрался как выражение FTS5; искали по словам.
    QueryNotUnderstood,
    /// Индекса нет или он повреждён; искали полным перебором.
    IndexMissing,
}

impl Fallback {
    /// Код для документации и для тех, кто сравнивает строки, а не разбирает JSON.
    pub fn code(self) -> &'static str {
        match self {
            Fallback::QueryNotUnderstood => "query_not_understood",
            Fallback::IndexMissing => "index_missing",
        }
    }
}

/// Одна поверхность, ответившая запасным путём.
#[derive(Debug, Clone, Serialize)]
pub struct Degraded {
    /// `nodes`, `notes` или `comments`.
    pub surface: &'static str,
    pub reason: Fallback,
    /// Человеческая строка для stderr и для текстового вывода MCP.
    pub note: String,
}

/// Ответ поиска вместе с честной пометкой о качестве.
///
/// Почему не просто `Vec<Hit>`. Запасной путь (`LIKE`, точнее — сравнение по словам) не
/// ранжирует и не находит словоформы: «миграциях» он не найдёт по запросу «миграция», тогда как
/// индекс найдёт. Раньше это отступление было молчаливым, и человек видел другой ответ, не зная
/// об этом, — а на живой базе `find` вообще отвечал запасным путём, потому что схема оставалась
/// старой и индекса не было (TASQDEV-19). Пометка в ответе — то, чего не хватало: «работает, но
/// иначе» хуже видимой ошибки.
///
/// `Deref` до среза оставлен намеренно: большинству вызывающих нужен только список находок, и
/// требовать от них разыменования поля значило бы менять десятки мест ради ничего. Но кто
/// печатает ответ наружу, обязан посмотреть на `degraded`.
#[derive(Debug, Clone, Default)]
pub struct SearchOutcome {
    pub hits: Vec<Hit>,
    /// Пусто — ответ целиком от индекса.
    pub degraded: Vec<Degraded>,
}

impl SearchOutcome {
    /// Ответ полон: все поверхности ответили индексом.
    ///
    /// В JSON это же значение едет полем `complete` — отдельным, а не выводом потребителя:
    /// вычислять его на стороне клиента значило бы требовать аккуратного разбора `degraded` от
    /// каждого, кто пишется на своём языке. Одно булево поле нельзя случайно не заметить.
    pub fn is_complete(&self) -> bool {
        self.degraded.is_empty()
    }
}

impl Serialize for SearchOutcome {
    /// `{"hits": […], "complete": true, "degraded": []}`.
    ///
    /// Массив находок верхним уровнем был бы удобнее для разбора, но он не оставляет места для
    /// ответа на вопрос «а индекс ли это». Форма с конвертом дороже на одно поле и честнее.
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("SearchOutcome", 3)?;
        state.serialize_field("hits", &self.hits)?;
        state.serialize_field("complete", &self.is_complete())?;
        state.serialize_field("degraded", &self.degraded)?;
        state.end()
    }
}

impl std::ops::Deref for SearchOutcome {
    type Target = [Hit];
    fn deref(&self) -> &[Hit] {
        &self.hits
    }
}

impl IntoIterator for SearchOutcome {
    type Item = Hit;
    type IntoIter = std::vec::IntoIter<Hit>;
    fn into_iter(self) -> Self::IntoIter {
        self.hits.into_iter()
    }
}

/// Поиск по узлам, заметкам и комментариям проекта.
///
/// FTS5 понимает префиксы (`migr*`), фразы в кавычках и `OR`. Если запрос не разобрался
/// как выражение FTS (например, в нём `-` или `:`), он трактуется как обычный текст:
/// падение поиска из-за пунктуации пользователя — не ошибка, а повод искать иначе. Но это
/// отступление теперь названо в `degraded`.
pub fn search(conn: &Connection, project: &Project, query: &str) -> Result<SearchOutcome> {
    search_in(conn, Some(project), query)
}

/// Поиск по всей базе, без выбора проекта.
///
/// Нужен там, где проект не назван: искать «в первом проекте по порядку» значит отвечать
/// «не найдено» на существующую задачу и выглядеть при этом уверенно. Адрес узла начинается
/// с ключа проекта, поэтому найденное остаётся однозначным и открывается через `tasq show`.
pub fn search_all(conn: &Connection, query: &str) -> Result<SearchOutcome> {
    search_in(conn, None, query)
}

fn search_in(conn: &Connection, project: Option<&Project>, query: &str) -> Result<SearchOutcome> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidValue {
            field: "query".to_string(),
            value: "empty".to_string(),
        });
    }

    let fts_query = fts_expression(trimmed);
    let mut outcome = SearchOutcome::default();

    // Три поверхности, один и тот же выбор: попробовать индекс, при отказе — по словам, и
    // записать, почему отступили. Код отступления общий, различается только поверхность.
    let mut surface = |name: &'static str,
                       indexed: Result<Vec<Hit>>,
                       fallback: &mut dyn FnMut() -> Result<Vec<Hit>>|
     -> Result<Vec<Hit>> {
        match indexed {
            Ok(rows) => Ok(rows),
            Err(err) => {
                let reason = classify_fallback(&err);
                let rows = fallback()?;
                outcome.degraded.push(Degraded {
                    surface: name,
                    reason,
                    note: fallback_note(name, reason),
                });
                Ok(rows)
            }
        }
    };

    outcome.hits.extend(surface(
        "nodes",
        fts_nodes(conn, project, &fts_query),
        &mut || like_nodes(conn, project, trimmed),
    )?);
    outcome.hits.extend(surface(
        "notes",
        fts_notes(conn, project, &fts_query),
        &mut || like_notes(conn, project, trimmed),
    )?);
    // Комментарии — третья поверхность поиска, и на живой базе самая содержательная: обоснования
    // решений живут в них, а не в описаниях узлов (250 комментариев против нуля заметок). Без них
    // `find` отвечал «ничего не найдено» на вопрос, ответ на который в базе был.
    outcome.hits.extend(surface(
        "comments",
        fts_comments(conn, project, &fts_query),
        &mut || like_comments(conn, project, trimmed),
    )?);

    Ok(outcome)
}

/// Отличает «индекса нет» от «запрос не разобрался».
///
/// Разница важна действием: первое чинится `tasq migrate`, второе не чинится ничем и означает
/// лишь, что искали по словам. Текст ошибки SQLite — единственный доступный признак: rusqlite
/// отдаёт код `SQLITE_ERROR` и для отсутствующей таблицы, и для синтаксиса выражения MATCH.
fn classify_fallback(err: &Error) -> Fallback {
    let text = err.to_string().to_ascii_lowercase();
    let missing = text.contains("no such table")
        || text.contains("no such module")
        || text.contains("no such column")
        || text.contains("malformed database schema")
        || text.contains("no such fts5");
    if missing {
        Fallback::IndexMissing
    } else {
        Fallback::QueryNotUnderstood
    }
}

/// Строка для человека: что произошло и что с этим делать.
fn fallback_note(surface: &str, reason: Fallback) -> String {
    match reason {
        Fallback::IndexMissing => format!(
            "search: {surface} answered without an index (it is missing); results are unordered \
             and word forms are not matched — run `tasq migrate`"
        ),
        Fallback::QueryNotUnderstood => format!(
            "search: {surface} answered by words, not by the index (the query is not an FTS \
             expression); results are unordered and word forms are not matched"
        ),
    }
}

/// Значение фильтра по проекту для SQL: `-1` — «любой проект».
///
/// Один запрос на два режима вместо двух текстов: два текста расходятся при первой же правке
/// (забыть `deleted_at` во втором — обычное дело), а договорённость «отрицательное значит все»
/// проверяется одним условием в трёх местах.
fn project_param(project: Option<&Project>) -> i64 {
    project.map(|project| project.id).unwrap_or(-1)
}

fn like_notes(conn: &Connection, project: Option<&Project>, query: &str) -> Result<Vec<Hit>> {
    let words = query_words(query);
    if words.is_empty() {
        return Ok(Vec::new());
    }
    let mut sql = String::from(
        "SELECT nt.title, nt.content, x.path, p.key FROM notes nt
         JOIN projects p ON p.id = nt.project_id
         LEFT JOIN nodes x ON x.id = nt.node_id
         WHERE (? < 0 OR nt.project_id = ?)",
    );
    let param = Value::Integer(project_param(project));
    let mut values: Vec<Value> = vec![param.clone(), param];
    for word in &words {
        sql.push_str(
            " AND (instr(uni_lower(nt.title), ?) > 0 OR instr(uni_lower(nt.content), ?) > 0)",
        );
        values.push(Value::Text(word.clone()));
        values.push(Value::Text(word.clone()));
    }
    sql.push_str(" ORDER BY nt.created_at DESC LIMIT 50");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values.iter()), |row| {
        let node_path: Option<String> = row.get(2)?;
        let content: String = row.get(1)?;
        let key: String = row.get(3)?;
        Ok(Hit {
            entity: "note".to_string(),
            dot_id: node_path.map(|path| format!("{key}-{path}")),
            title: row.get(0)?,
            snippet: first_line(&content),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

/// Слова запроса: пунктуация отбрасывается, регистр не важен.
pub(crate) fn query_words(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|word| {
            word.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect()
}

/// Готовит выражение FTS: несколько слов склеиваются по И, спецсимволы отбрасываются.
fn fts_expression(query: &str) -> String {
    if query.contains('"') || query.contains(" OR ") || query.contains(" AND ") {
        return query.to_string();
    }
    query_words(query)
        .into_iter()
        .map(|word| format!("\"{word}\"*"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn fts_nodes(conn: &Connection, project: Option<&Project>, query: &str) -> Result<Vec<Hit>> {
    // Ключ проекта берётся из базы, а не из выбранного проекта: в режиме «все проекты»
    // выбранного нет, а адрес узла всё равно должен начинаться с ключа его проекта.
    let mut stmt = conn.prepare(
        "SELECT n.path, n.title, snippet(nodes_fts, -1, '', '', '…', 12), p.key
         FROM nodes_fts f
         JOIN nodes n ON n.id = f.rowid
         JOIN projects p ON p.id = n.project_id
         WHERE nodes_fts MATCH ?1 AND (?2 < 0 OR n.project_id = ?2) AND n.deleted_at IS NULL
         ORDER BY rank
         LIMIT 50",
    )?;
    let rows = stmt.query_map(params![query, project_param(project)], |row| {
        let path: String = row.get(0)?;
        let key: String = row.get(3)?;
        Ok(Hit {
            entity: "node".to_string(),
            dot_id: Some(format!("{key}-{path}")),
            title: row.get(1)?,
            snippet: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn fts_notes(conn: &Connection, project: Option<&Project>, query: &str) -> Result<Vec<Hit>> {
    let mut stmt = conn.prepare(
        "SELECT nt.title, snippet(notes_fts, -1, '', '', '…', 12), x.path, p.key
         FROM notes_fts f
         JOIN notes nt ON nt.id = f.rowid
         JOIN projects p ON p.id = nt.project_id
         LEFT JOIN nodes x ON x.id = nt.node_id
         WHERE notes_fts MATCH ?1 AND (?2 < 0 OR nt.project_id = ?2)
         ORDER BY rank
         LIMIT 50",
    )?;
    let rows = stmt.query_map(params![query, project_param(project)], |row| {
        let node_path: Option<String> = row.get(2)?;
        let key: String = row.get(3)?;
        Ok(Hit {
            entity: "note".to_string(),
            dot_id: node_path.map(|path| format!("{key}-{path}")),
            title: row.get(0)?,
            snippet: row.get(1)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Заголовок найденного комментария: автор и первая строка, обрезанная по границе слова.
///
/// Сниппет FTS несёт контекст вокруг совпадения и бывает в несколько строк; в таблице он идёт
/// отдельной строкой ниже. Заголовок нужен, чтобы отличить один комментарий от другого, поэтому
/// он — одна строка: иначе вывод на экране разъезжается и два найденных комментария сливаются.
fn comment_title(author: Option<&str>, snippet: &str) -> String {
    let line = snippet.lines().next().unwrap_or("").trim();
    let short: String = if line.chars().count() > 80 {
        let cut: String = line.chars().take(80).collect();
        format!("{}…", cut.trim_end())
    } else {
        line.to_string()
    };
    match author {
        Some(author) => format!("{author}: {short}"),
        None => short,
    }
}

/// Поиск по комментариям через FTS: ответ — сам комментарий и узел, к которому он написан.
///
/// Заголовок ответа — начало комментария, а не заголовок узла: два комментария к одному узлу
/// иначе выглядели бы одинаково, и человек не понял бы, какой из них нашёлся. Адрес узла
/// остаётся в `dot_id`, поэтому найденное открывается через `show`.
fn fts_comments(conn: &Connection, project: Option<&Project>, query: &str) -> Result<Vec<Hit>> {
    let mut stmt = conn.prepare(
        "SELECT snippet(comments_fts, 0, '', '', '…', 12), x.path, p.key, c.author
         FROM comments_fts f
         JOIN comments c ON c.id = f.rowid
         JOIN nodes n ON n.id = c.node_id
         JOIN projects p ON p.id = n.project_id
         LEFT JOIN nodes x ON x.id = c.node_id
         WHERE comments_fts MATCH ?1 AND (?2 < 0 OR n.project_id = ?2) AND n.deleted_at IS NULL
         ORDER BY rank
         LIMIT 50",
    )?;
    let rows = stmt.query_map(params![query, project_param(project)], |row| {
        let snippet: String = row.get(0)?;
        let node_path: Option<String> = row.get(1)?;
        let key: String = row.get(2)?;
        let author: Option<String> = row.get(3)?;
        Ok(Hit {
            entity: "comment".to_string(),
            dot_id: node_path.map(|path| format!("{key}-{path}")),
            title: comment_title(author.as_deref(), &snippet),
            snippet,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Запасной путь по комментариям — для запросов, которые FTS не разобрал.
fn like_comments(conn: &Connection, project: Option<&Project>, query: &str) -> Result<Vec<Hit>> {
    let words = query_words(query);
    if words.is_empty() {
        return Ok(Vec::new());
    }
    let mut sql = String::from(
        "SELECT c.content, x.path, p.key, c.author FROM comments c
         JOIN nodes n ON n.id = c.node_id
         JOIN projects p ON p.id = n.project_id
         LEFT JOIN nodes x ON x.id = c.node_id
         WHERE (? < 0 OR n.project_id = ?) AND n.deleted_at IS NULL",
    );
    let param = Value::Integer(project_param(project));
    let mut values: Vec<Value> = vec![param.clone(), param];
    for word in &words {
        sql.push_str(" AND instr(uni_lower(c.content), ?) > 0");
        values.push(Value::Text(word.clone()));
    }
    sql.push_str(" ORDER BY c.created_at DESC LIMIT 50");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values.iter()), |row| {
        let content: String = row.get(0)?;
        let node_path: Option<String> = row.get(1)?;
        let key: String = row.get(2)?;
        let author: Option<String> = row.get(3)?;
        let snippet = content.chars().take(160).collect::<String>();
        Ok(Hit {
            entity: "comment".to_string(),
            dot_id: node_path.map(|path| format!("{key}-{path}")),
            title: comment_title(author.as_deref(), &snippet),
            snippet,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn like_nodes(conn: &Connection, project: Option<&Project>, query: &str) -> Result<Vec<Hit>> {
    // Запасной путь для запросов, которые FTS не разобрал (пунктуация, двоеточия):
    // каждое слово ищется отдельно и все слова должны встретиться.
    let words = query_words(query);
    if words.is_empty() {
        return Ok(Vec::new());
    }

    let mut sql = String::from(
        "SELECT n.path, n.title, COALESCE(n.description, ''), p.key FROM nodes n
         JOIN projects p ON p.id = n.project_id
         WHERE (? < 0 OR n.project_id = ?) AND n.deleted_at IS NULL",
    );
    let param = Value::Integer(project_param(project));
    let mut values: Vec<Value> = vec![param.clone(), param];
    for word in &words {
        // Колонки названы с псевдонимом: у `projects` тоже есть `description`, и после JOIN
        // голое имя становится неоднозначным — запрос падает вместо того, чтобы искать.
        sql.push_str(
            " AND (instr(uni_lower(n.title), ?) > 0 OR instr(uni_lower(COALESCE(n.description, '')), ?) > 0)",
        );
        values.push(Value::Text(word.clone()));
        values.push(Value::Text(word.clone()));
    }
    sql.push_str(" ORDER BY n.path LIMIT 50");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values.iter()), |row| {
        let path: String = row.get(0)?;
        let description: String = row.get(2)?;
        let key: String = row.get(3)?;
        Ok(Hit {
            entity: "node".to_string(),
            dot_id: Some(format!("{key}-{path}")),
            title: row.get(1)?,
            snippet: first_line(&description),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Узел в цепочке контекста: от корня вниз.
#[derive(Debug, Clone, Serialize)]
pub struct ContextEntry {
    pub dot_id: String,
    pub kind: String,
    pub status: String,
    pub title: String,
    pub description: Option<String>,
    pub due_date: Option<String>,
    pub lease_holder: Option<String>,
}

/// Цепочка контекста: родители сверху вниз, затем сам узел (README §6).
///
/// Агент получает «зачем» от эпика и «что именно» от своего уровня в одном ответе —
/// ровно то, ради чего заведён dot-notation.
pub fn context(conn: &Connection, project: &Project, reference: &str) -> Result<Vec<ContextEntry>> {
    let (_, node) = node::resolve_shown(conn, project, reference)?;
    let mut chain = Vec::new();
    let mut current = Some(node);
    while let Some(item) = current {
        chain.push(ContextEntry {
            dot_id: item.dot_id.clone(),
            kind: item.kind.clone(),
            status: item.effective_status.clone(),
            title: item.title.clone(),
            description: item.description.clone(),
            due_date: item.due_date.clone(),
            lease_holder: item.lease_holder.clone(),
        });
        current = if item.depth > 1 {
            parent_of(conn, project, &item.path)?
        } else {
            None
        };
    }
    chain.reverse();
    Ok(chain)
}

fn parent_of(conn: &Connection, project: &Project, path: &str) -> Result<Option<node::Node>> {
    let parent_path = match path.rsplit_once('.') {
        Some((head, _)) => head,
        None => return Ok(None),
    };
    Ok(node::resolve(conn, project, parent_path).ok())
}

/// Строка доски: узел в разрезе статуса.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BoardRow {
    pub dot_id: String,
    pub title: String,
    pub priority: String,
    pub due_date: Option<String>,
    pub days_overdue: i64,
    pub lease_holder: Option<String>,
    pub progress: String,
}

/// Колонка доски.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BoardColumn {
    pub status: String,
    pub count: usize,
    pub rows: Vec<BoardRow>,
}

/// Доска: узлы по эффективному статусу, с просрочкой и живыми арендами.
///
/// `PartialEq` — не для удобства, а для проверки: инкрементальное обновление обязано давать
/// ровно ту же доску, что и полное чтение, и утверждать это словами бессмысленно — только
/// сравнением.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Board {
    pub project: String,
    pub columns: Vec<BoardColumn>,
    pub overdue: usize,
    pub leased: usize,
    pub open: usize,
    pub done: usize,
    /// Закрыто, но не выполнено: эпик, прекращённый с открытыми задачами.
    ///
    /// Отдельным числом, а не внутри `done`: `done` в сводке означает «работа сделана», а
    /// `incomplete` — «работу прекратили». Сложить их значило бы снова завысить результат.
    #[serde(default)]
    pub incomplete: usize,
}

/// Что доска показывает: те же два переключателя, что у `ls -n` и `ls -a`.
///
/// Один тип на экран и на CLI, чтобы «скрыть выполненное» означало одно и то же в обоих местах:
/// правило «`incomplete` виден там, где `done` скрыт» нарушилось бы ровно на расхождении фильтров.
// `Copy` снят вместе с появлением ветки: `String` копировать бесплатно нельзя, а
// `BoardFilter` передаётся в `board_with` по значению — `Clone` для этого достаточно, и
// вызывающий, которому нужен свой экземпляр, клонирует явно.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoardFilter {
    /// Скрыть выполненное (`ls -n`). Закрытое с хвостами (`incomplete`) при этом остаётся:
    /// это единственный сигнал, что под узлом есть незакрытые задачи.
    pub hide_done: bool,
    /// Показать архив (`ls -a`).
    pub show_archived: bool,
    /// Только узлы этой ветки (`ls --branch`); `None` — фильтра нет.
    ///
    /// Владелец — `String`, а не `&str`, потому что фильтр живёт на экране дольше любого
    /// вызова: имя приходит из git один раз при включении и потом просто сравнивается.
    pub branch: Option<String>,
}

pub fn board(conn: &Connection, project: &Project) -> Result<Board> {
    board_with(conn, project, BoardFilter::default())
}

pub fn board_with(conn: &Connection, project: &Project, filter: BoardFilter) -> Result<Board> {
    // Отбор идёт и в SQL (там же, где остальные списки, и так дешевле на широкой базе), и в
    // `build_board` — потому что экран держит узлы без отбора и применяет фильтр при сборке.
    // Повторный отбор безвреден, а расходиться двум реализациям не даёт тест: он сверяет доску
    // из состояния с доской из CLI для каждой комбинации переключателей.
    let nodes = node::list(
        conn,
        project,
        &Filter {
            include_archived: filter.show_archived,
            exclude_done: filter.hide_done,
            branch: filter.branch.as_deref(),
            ..Filter::default()
        },
    )?;
    let today = today_days(conn)?;
    Ok(build_board(&project.key, &nodes, &filter, today))
}

/// Собирает доску из уже прочитанных узлов.
///
/// Отделено от чтения, чтобы одна и та же сборка годилась и полному чтению, и инкрементальному
/// обновлению: у них разный источник узлов и один и тот же ответ. Второй экземпляр этих правил
/// (подсчёт просрочки, прогресса, `incomplete`) разошёлся бы с первым на первой же правке — и
/// разошёлся бы молча, потому что оба ответа выглядят правдоподобно.
///
/// Отбор здесь повторяет SQL-отбор доски: он нужен, когда на руках узлы без отбора (экран).
pub fn build_board(
    project_key: &str,
    nodes: &[node::Node],
    filter: &BoardFilter,
    today: i64,
) -> Board {
    // Порядок колонок берётся из единственного списка статусов: своя копия здесь
    // расходится с `Status::all()` на первом же добавленном статусе.
    let order = crate::status::Status::all();

    let mut columns: Vec<BoardColumn> = order
        .iter()
        .map(|status| BoardColumn {
            status: status.as_str().to_string(),
            count: 0,
            rows: Vec::new(),
        })
        .collect();
    // `incomplete` — вычисляемое состояние, в справочнике его нет, поэтому и в `Status::all()`.
    // Колонка добавляется здесь, а не в списке статусов: иначе его пришлось бы держать в
    // справочнике базы, то есть снова хранить вывод.
    columns.push(BoardColumn {
        status: "incomplete".to_string(),
        count: 0,
        rows: Vec::new(),
    });

    // Указатель «статус -> колонка» строится один раз: поиск колонки по имени на каждом узле —
    // это четырнадцать сравнений строк на строку доски, то есть бо́льшая часть стоимости сборки
    // на десяти тысячах узлов. Замер: 35 мс -> 9 мс на фикстуре (make perf).
    let by_status: std::collections::HashMap<String, usize> = columns
        .iter()
        .enumerate()
        .map(|(index, column)| (column.status.clone(), index))
        .collect();

    let mut overdue = 0;
    let mut leased = 0;
    let mut open = 0;
    let mut done = 0;
    let mut incomplete = 0;

    for node in nodes {
        if !filter.show_archived && node.archived_at.is_some() {
            continue;
        }
        if let Some(branch) = &filter.branch
            && node.branch.as_deref() != Some(branch.as_str())
        {
            continue;
        }
        if filter.hide_done && node.effective_status == "done" {
            continue;
        }
        let status = crate::status::Status::parse(&node.effective_status);
        // Закрытость и выполненность — разные вопросы. `incomplete` закрыт (работа прекращена),
        // поэтому в «открыто» не идёт, но и в `done` не попадает: работа не сделана.
        if status.is_some_and(crate::status::Status::is_terminal) {
            if status.is_some_and(crate::status::Status::counts_as_done) {
                done += 1;
            } else if node.effective_status == "incomplete" {
                incomplete += 1;
            }
        } else {
            open += 1;
        }
        if node.lease_holder.is_some() {
            leased += 1;
        }
        let days_overdue = match (&node.due_date, status) {
            (Some(due), Some(status)) if !status.is_terminal() => {
                let due_days = date_to_days(due).unwrap_or(today);
                let delta = today - due_days;
                if delta > 0 { delta } else { 0 }
            }
            _ => 0,
        };
        if days_overdue > 0 {
            overdue += 1;
        }

        let progress = if node.progress_total > 0.0 {
            format!("{:.0}%", node.progress_done / node.progress_total * 100.0)
        } else {
            "-".to_string()
        };

        let row = BoardRow {
            dot_id: node.dot_id.clone(),
            title: node.title.clone(),
            priority: node.priority.clone(),
            due_date: node.due_date.clone(),
            days_overdue,
            lease_holder: node.lease_holder.clone(),
            progress,
        };
        if let Some(index) = by_status.get(&node.effective_status) {
            let column = &mut columns[*index];
            column.count += 1;
            column.rows.push(row);
        }
    }
    columns.retain(|column| column.count > 0);

    Board {
        project: project_key.to_string(),
        columns,
        overdue,
        leased,
        open,
        done,
        incomplete,
    }
}

/// Что дало инкрементальное обновление: сколько узлов перечитано и почему.
///
/// Число возвращается наружу, а не живёт в счётчике внутри: без него «стало легче» снова
/// осталось бы на слово, а с ним видно, что правка одного узла стоит трёх строк, а не десяти
/// тысяч.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// Ничего не изменилось: доска осталась прежней.
    Nothing,
    /// Перечитаны только затронутые узлы и их предки.
    Delta { nodes: usize },
    /// Перечитана вся доска: сменился проект или фильтр.
    Full,
}

/// Доска вместе с тем, из чего она собрана, — чтобы обновлять её по изменениям.
///
/// Зачем нужен второй тип рядом с [`Board`]. Доска — ответ для человека: строки по колонкам и
/// четыре счётчика. Инкрементальное обновление требует держать исходные узлы и точку отсчёта по
/// журналу; если положить это в `Board`, оно поедет в `tasq board --json` и станет частью
/// публичного формата, где никому не нужно.
#[derive(Debug, Clone)]
pub struct BoardState {
    project: String,
    filter: BoardFilter,
    /// Все живые и архивные узлы проекта — без отбора: фильтр применяется при сборке, иначе
    /// узел, ставший `done`, нечем было бы убрать из доски, не перечитывая всё.
    nodes: Vec<node::Node>,
    /// `MAX(id)` журнала на момент чтения — «до какого места база уже учтена».
    watermark: i64,
    board: Board,
}

impl BoardState {
    /// Полное чтение: стартовая точка для дальнейших обновлений.
    pub fn full(conn: &Connection, project: &Project, filter: BoardFilter) -> Result<Self> {
        // Архив читается всегда, даже когда скрыт: это цена одного условия на строку, зато
        // включение `a` на экране не требует перечитать базу.
        let nodes = node::list(
            conn,
            project,
            &Filter {
                include_archived: true,
                ..Filter::default()
            },
        )?;
        let watermark = crate::activity::watermark(conn)?;
        let today = today_days(conn)?;
        let board = build_board(&project.key, &nodes, &filter, today);
        Ok(Self {
            project: project.key.clone(),
            filter,
            nodes,
            watermark,
            board,
        })
    }

    pub fn board(&self) -> &Board {
        &self.board
    }

    /// Перечитывает то, чего коснулись изменения в базе, и пересобирает доску.
    ///
    /// Что именно перечитывается: узлы из журнала после точки отсчёта и их ПРЕДКИ. Предки —
    /// потому что в строке доски стоит прогресс поддерева: закрытая подзадача меняет проценты у
    /// родителя, не породив записи журнала про родителя.
    ///
    /// Переезд узла меняет адреса у всего поддерева, поэтому если у затронутого узла сменился
    /// путь, его потомки (по прежнему пути) тоже попадают в перечитывание: иначе на доске
    /// остались бы строки со старыми адресами — их не видно, но они ломают курсор и ссылки.
    pub fn refresh(
        &mut self,
        conn: &Connection,
        project: &Project,
        filter: BoardFilter,
    ) -> Result<Refresh> {
        if self.project != project.key || self.filter != filter {
            *self = Self::full(conn, project, filter)?;
            return Ok(Refresh::Full);
        }
        let watermark = crate::activity::watermark(conn)?;
        let touched = crate::activity::touched_since(conn, project.id, self.watermark)?;
        if touched.is_empty() {
            // Журнал мог вырасти записями других проектов и комментариями к отсутствующим узлам:
            // доска от них не меняется, но точку отсчёта сдвинуть надо, иначе это же множество
            // будет перечитываться на каждом круге.
            self.watermark = watermark;
            return Ok(Refresh::Nothing);
        }

        let by_path: std::collections::HashMap<String, i64> = self
            .nodes
            .iter()
            .map(|node| (node.path.clone(), node.id))
            .collect();
        let mut targets: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
        for id in &touched {
            targets.insert(*id);
            // Предки по пути: `2.1.3` -> `2.1`, `2`. Путь — единственное место, где связь
            // «родитель» уже посчитана, и второй обход дерева ради этого не нужен.
            if let Some(path) = self
                .nodes
                .iter()
                .find(|node| node.id == *id)
                .map(|node| node.path.clone())
            {
                let mut prefix = path.as_str();
                while let Some(cut) = prefix.rfind('.') {
                    prefix = &prefix[..cut];
                    if let Some(ancestor) = by_path.get(prefix) {
                        targets.insert(*ancestor);
                    }
                }
                // Потомки переехавшего узла: путь был `p`, все, кто начинался с `p.`.
                let marker = format!("{path}.");
                for node in &self.nodes {
                    if node.path.starts_with(&marker) {
                        targets.insert(node.id);
                    }
                }
            }
        }

        let ids: Vec<i64> = targets.into_iter().collect();
        let fresh = node::by_ids(conn, project, &ids)?;
        let present: std::collections::HashSet<i64> = fresh.iter().map(|node| node.id).collect();

        // Удалённые: их больше нет в базе (удаление узла ставит `deleted_at`, а журнал называет
        // его id), поэтому из набора они уходят.
        let dropped: std::collections::HashSet<i64> = ids
            .iter()
            .copied()
            .filter(|id| !present.contains(id))
            .collect();
        self.nodes.retain(|node| !dropped.contains(&node.id));

        for node in fresh {
            match self.nodes.iter_mut().find(|old| old.id == node.id) {
                Some(old) => *old = node,
                None => self.nodes.push(node),
            }
        }
        self.nodes
            .sort_by_key(|node| node::path_sort_key(&node.path));

        let today = today_days(conn)?;
        self.board = build_board(&project.key, &self.nodes, &self.filter, today);
        self.watermark = watermark;
        Ok(Refresh::Delta { nodes: ids.len() })
    }
}

/// Доски всех проектов базы — то, что нужно обзору без выбора проекта.
///
/// Считается по проекту отдельно, а не одной сводкой по всем узлам: у каждого свои колонки,
/// свои просрочки и свои аренды, и сложенные вместе числа не отвечают ни на один вопрос.
/// Проекты без единого узла в ответ не попадают: пустая секция — не сведения.
pub fn board_all(conn: &Connection) -> Result<Vec<Board>> {
    let mut boards = Vec::new();
    for summary in crate::project::list(conn)? {
        let Some(project) = crate::project::by_key(conn, &summary.key)? else {
            continue;
        };
        let board = board(conn, &project)?;
        if board.columns.is_empty() {
            continue;
        }
        boards.push(board);
    }
    Ok(boards)
}

fn today_days(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT CAST(julianday(date('now')) AS INTEGER)",
        [],
        |row| row.get(0),
    )?)
}

fn date_to_days(value: &str) -> Option<i64> {
    use time::Date;
    use time::format_description::well_known::Iso8601;
    Date::parse(value, &Iso8601::DATE)
        .ok()
        .map(|date| i64::from(date.to_julian_day()))
}

/// Результат пакетной правки.
#[derive(Debug, Clone, Serialize)]
pub struct BulkResult {
    pub updated: Vec<String>,
    pub failed: Vec<BulkFailure>,
}

/// Неудача по одному узлу пакета.
#[derive(Debug, Clone, Serialize)]
pub struct BulkFailure {
    pub dot_id: String,
    pub code: String,
    pub message: String,
}

/// Пакетная смена статуса: одна ошибка не отменяет остальные, но и не прячется.
pub fn bulk_status(
    conn: &mut Connection,
    project: &Project,
    references: &[String],
    target: Status,
    force: bool,
    actor: &str,
) -> Result<BulkResult> {
    let mut updated = Vec::new();
    let mut failed = Vec::new();
    for reference in references {
        let result = node::update(
            conn,
            project,
            reference,
            node::Patch {
                status: Some(target),
                force,
                // Массовая правка аренды не держит: `in_progress` через неё запрещён.
                lease: None,
                actor: Some(actor),
                ..node::Patch::default()
            },
        );
        match result {
            Ok(node) => updated.push(node.dot_id),
            Err(err) => failed.push(BulkFailure {
                dot_id: reference.clone(),
                code: err.code().to_string(),
                message: err.to_string(),
            }),
        }
    }
    Ok(BulkResult { updated, failed })
}

/// Часы по узлам: оценка против факта из попыток.
#[derive(Debug, Clone, Serialize)]
pub struct TimeRow {
    pub dot_id: String,
    pub title: String,
    pub estimated_hours: Option<f64>,
    pub actual_hours: f64,
    pub attempts: i64,
}

/// Учёт времени: суммарная длительность попыток по каждому узлу.
pub fn time_report(
    conn: &Connection,
    project: &Project,
    reference: Option<&str>,
) -> Result<Vec<TimeRow>> {
    // Отбор поддерева — по id: обход структуры вместо скана по `path GLOB` (см. `crate::tree`).
    let subtree_id = match reference {
        Some(reference) => {
            let (_, node) = node::resolve_shown(conn, project, reference)?;
            Some(node.id)
        }
        None => None,
    };

    let mut sql = String::from(
        "SELECT n.path, n.title, n.estimated_hours, COALESCE(h.hours, 0.0),
                (SELECT COUNT(*) FROM attempts a WHERE a.node_id = n.id)
         FROM nodes n
         LEFT JOIN v_attempt_hours h ON h.node_id = n.id
         WHERE n.project_id = ?1 AND n.deleted_at IS NULL",
    );
    let mut values: Vec<Value> = vec![Value::Integer(project.id)];
    if let Some(id) = subtree_id {
        sql.push_str(" AND n.id IN (");
        sql.push_str(&crate::tree::subtree_sql());
        sql.push(')');
        values.push(Value::Integer(id));
    }
    sql.push_str(" ORDER BY n.path");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values.iter()), |row| {
        let path: String = row.get(0)?;
        Ok(TimeRow {
            dot_id: project.dot_id(&path),
            title: row.get(1)?,
            estimated_hours: row.get(2)?,
            actual_hours: row.get(3)?,
            attempts: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Запись в лог о пакетной операции: одна строка на всю пачку.
pub fn log_bulk(
    conn: &Connection,
    project: &Project,
    action: &str,
    summary: &str,
    actor: &str,
) -> Result<()> {
    activity::log(
        conn,
        &activity::Entry {
            project_id: project.id,
            node_id: None,
            entity_type: "batch",
            entity_id: 0,
            action,
            field_name: None,
            old_value: None,
            new_value: None,
            summary: Some(summary),
            actor,
        },
    )?;
    Ok(())
}
