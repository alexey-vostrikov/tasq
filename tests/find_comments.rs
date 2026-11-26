//! Поиск находит комментарии — то место, где на деле живут обоснования.
//!
//! Зачем это отдельный файл. Индекс узлов и индекс заметок тестами покрыты, а комментариев в
//! поиске не было вовсе: `find` отвечал «ничего не найдено» на вопрос, ответ на который в базе
//! был. На живой базе это 250 комментариев против нуля заметок, то есть самая содержательная
//! часть данных была невидима для поиска.
//!
//! Проверяются все пути записи, потому что пропущенный путь делает индекс тихо неполным: так уже
//! вышло с `nodes_fts` при импорте проекта, пока его не начали перестраивать явно.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use tasq::project::{self, Project};
use tasq::{comment, db, node, overview};

fn fixture(tag: &str) -> (PathBuf, Connection, Project) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-find-comments-{tag}-{nanos}"));
    let outcome = db::init(&dir, "FIND", "поиск").expect("init");
    let conn = db::open(Path::new(&outcome.db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");
    (dir, conn, project)
}

fn add(conn: &mut Connection, project: &Project, title: &str) -> node::Node {
    node::create(
        conn,
        project,
        node::NewNode {
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
            tags: None,
        },
    )
    .expect("узел")
}

fn say(conn: &mut Connection, project: &Project, dot_id: &str, text: &str) -> comment::Comment {
    comment::add(conn, project, dot_id, text, "тест").expect("комментарий")
}

/// Слово, которого нет ни в одном заголовке, находится в комментарии — и ответ называет узел.
#[test]
fn a_word_that_lives_only_in_a_comment_is_found() {
    let (dir, mut conn, project) = fixture("found");
    let epic = add(&mut conn, &project, "Эпик без лишних слов");
    say(
        &mut conn,
        &project,
        &epic.dot_id,
        "зюйдвестка пришлась ко двору: обоснование решения",
    );

    let hits = overview::search(&conn, &project, "зюйдвестка").expect("поиск");
    assert_eq!(hits.len(), 1, "комментарий не найден: {hits:?}");
    assert_eq!(hits[0].entity, "comment");
    assert_eq!(
        hits[0].dot_id.as_deref(),
        Some(epic.dot_id.as_str()),
        "ответ поиска обязан называть узел, иначе найденное не открыть"
    );
    assert!(
        hits[0].title.contains("тест"),
        "в заголовке нет автора: {}",
        hits[0].title
    );
    assert!(
        hits[0].snippet.contains("зюйдвестка"),
        "в сниппете нет совпадения: {}",
        hits[0].snippet
    );

    let _ = std::fs::remove_dir_all(dir);
}

/// Правка текста комментария видна поиску сразу: за это отвечает триггер `au`.
#[test]
fn an_edited_comment_is_searched_by_its_new_text() {
    let (dir, mut conn, project) = fixture("edit");
    let epic = add(&mut conn, &project, "Эпик");
    let said = say(&mut conn, &project, &epic.dot_id, "старое слово флогистон");
    assert_eq!(
        overview::search(&conn, &project, "флогистон")
            .expect("поиск")
            .len(),
        1
    );

    comment::edit(&mut conn, &project, said.id, "новое слово эфир", "тест").expect("правка");

    assert!(
        overview::search(&conn, &project, "флогистон")
            .expect("поиск")
            .is_empty(),
        "старый текст остался в индексе"
    );
    assert_eq!(
        overview::search(&conn, &project, "эфир")
            .expect("поиск")
            .len(),
        1,
        "новый текст в индекс не попал"
    );

    let _ = std::fs::remove_dir_all(dir);
}

/// Мягко удалённый комментарий остаётся находимым: он часть истории, как архивный узел.
///
/// Это решение, а не недосмотр: удалённый комментарий исчезает из ленты (`comment ls`), но
/// вопрос «кто это написал и когда убрал» остаётся законным, и ответ на него должен находиться.
#[test]
fn a_soft_deleted_comment_stays_searchable() {
    let (dir, mut conn, project) = fixture("soft");
    let epic = add(&mut conn, &project, "Эпик");
    let said = say(&mut conn, &project, &epic.dot_id, "спорное слово шапито");
    comment::delete(&mut conn, &project, said.id, "тест", Some("устарело")).expect("удаление");

    assert_eq!(
        overview::search(&conn, &project, "шапито")
            .expect("поиск")
            .len(),
        1,
        "удалённый комментарий пропал из истории поиска"
    );
    // А из ленты он пропал — это другое поведение и другой тест.
    assert!(
        comment::list(&conn, &project, &epic.dot_id, false)
            .expect("лента")
            .is_empty(),
        "удалённый комментарий остался в ленте"
    );

    let _ = std::fs::remove_dir_all(dir);
}

/// Комментарии удалённого узла не находятся: узла нет, и адрес в ответе был бы битым.
#[test]
fn comments_of_a_deleted_node_are_not_found() {
    let (dir, mut conn, project) = fixture("deleted-node");
    let epic = add(&mut conn, &project, "Эпик");
    say(&mut conn, &project, &epic.dot_id, "слово кашалот");
    assert_eq!(
        overview::search(&conn, &project, "кашалот")
            .expect("поиск")
            .len(),
        1
    );

    node::delete(&mut conn, &project, &epic.dot_id).expect("удаление узла");
    assert!(
        overview::search(&conn, &project, "кашалот")
            .expect("поиск")
            .is_empty(),
        "комментарий удалённого узла всё ещё находится"
    );

    let _ = std::fs::remove_dir_all(dir);
}

/// Поиск в проекте не возвращает комментарии соседнего: проект — граница ответа.
#[test]
fn a_comment_from_another_project_is_not_returned() {
    let (dir, mut conn, project) = fixture("scope");
    let other = project::create(&mut conn, "OTHER", Some("соседний")).expect("проект");
    let here = add(&mut conn, &project, "Эпик");
    let there = add(&mut conn, &other, "Чужой эпик");
    say(&mut conn, &project, &here.dot_id, "слово вельбот тут");
    say(&mut conn, &other, &there.dot_id, "слово вельбот там");

    let in_project = overview::search(&conn, &project, "вельбот").expect("поиск");
    assert_eq!(
        in_project.len(),
        1,
        "поиск вернул чужой проект: {in_project:?}"
    );
    assert_eq!(in_project[0].dot_id.as_deref(), Some(here.dot_id.as_str()));

    // А по всей базе — оба: там границы нет и быть не должно.
    let everywhere = overview::search_all(&conn, "вельбот").expect("поиск по базе");
    assert_eq!(everywhere.len(), 2, "{everywhere:?}");

    let _ = std::fs::remove_dir_all(dir);
}

/// Запрос, который FTS не разобрал, всё равно находит комментарий запасным путём.
///
/// Тот же принцип, что у узлов и заметок: пунктуация пользователя — не повод отвечать пустотой.
#[test]
fn the_fallback_path_also_searches_comments() {
    let (dir, mut conn, project) = fixture("fallback");
    let epic = add(&mut conn, &project, "Эпик");
    say(&mut conn, &project, &epic.dot_id, "фрегат и бриг");

    // Двоеточие ломает выражение FTS, и поиск обязан отступить к сравнению подстрок.
    let hits = overview::search(&conn, &project, "фрегат:").expect("поиск");
    assert!(
        hits.iter().any(|hit| hit.entity == "comment"),
        "запасной путь не нашёл комментарий: {hits:?}"
    );

    let _ = std::fs::remove_dir_all(dir);
}
