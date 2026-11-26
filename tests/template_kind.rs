//! Вид в шаблоне — подсказка о замысле, а не требование к месту.
//!
//! Зачем это проверять. Правило «вид следует глубине» уже действует при переезде узла; шаблон
//! ему противоречил: объявив `kind: task`, автор запирал шаблон на одной глубине, и применение в
//! корень проекта отказывало словами про глубину. Отказ называл не ту причину, и человек правил
//! место вместо объявления.
//!
//! Проверяются обе половины: вид выводится из места (и расхождение называется числом), а
//! объявления внутри шаблона по-прежнему обязаны быть согласованы между собой — иначе автор
//! заявил две несовместимые вещи, и какая из них верна, знает только он.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use tasq::project::{self, Project};
use tasq::status::Kind;
use tasq::{db, node, template};

struct Fixture {
    dir: PathBuf,
    conn: Connection,
    project: Project,
}

fn fixture(tag: &str) -> Fixture {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-tpl-kind-{tag}-{nanos}"));
    let outcome = db::init(&dir, "KIND", "виды").expect("init");
    let conn = db::open(Path::new(&outcome.db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");
    Fixture { dir, conn, project }
}

impl Fixture {
    fn save(&mut self, name: &str, raw: &str) -> tasq::Result<template::Template> {
        template::save(&mut self.conn, Some(&self.project), name, None, raw, "тест")
    }

    fn apply(&mut self, name: &str, parent: Option<&str>) -> template::Applied {
        template::apply(&mut self.conn, &self.project, name, parent, &[])
            .unwrap_or_else(|err| panic!("применение {name}: {err:?}"))
    }

    fn kinds(&self, applied: &template::Applied) -> Vec<String> {
        applied
            .created
            .iter()
            .map(|dot| {
                node::resolve(&self.conn, &self.project, dot)
                    .expect("узел")
                    .kind
            })
            .collect()
    }
}

/// Шаблон «задача с подзадачей» применяется в корень и становится «эпик с задачей».
#[test]
fn a_task_template_applied_at_the_root_becomes_an_epic() {
    let mut fixture = fixture("root");
    fixture
        .save(
            "handler",
            r#"{"nodes": [{"title": "Ручка", "kind": "task", "children": [
                 {"title": "Тест", "kind": "subtask"}]}]}"#,
        )
        .expect("сохранение: объявления согласованы между собой");

    let applied = fixture.apply("handler", None);
    assert_eq!(
        fixture.kinds(&applied),
        vec![Kind::Epic.as_str(), Kind::Task.as_str()],
        "вид обязан следовать глубине, а не объявлению"
    );
    assert_eq!(
        applied.rekinded, 2,
        "число подменённых видов — это то, что видит человек, и оно обязано быть точным"
    );

    let _ = std::fs::remove_dir_all(fixture.dir);
}

/// Тот же шаблон под эпиком — уже «задача с подзадачей», и подмены нет.
#[test]
fn the_same_template_under_a_node_keeps_its_declared_kinds() {
    let mut fixture = fixture("under");
    fixture
        .save(
            "handler",
            r#"{"nodes": [{"title": "Ручка", "kind": "task", "children": [
                 {"title": "Тест", "kind": "subtask"}]}]}"#,
        )
        .expect("сохранение");

    let epic = node::create(
        &mut fixture.conn,
        &fixture.project,
        node::NewNode {
            parent: None,
            title: "Эпик",
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
    .expect("эпик");

    let applied = fixture.apply("handler", Some(&epic.dot_id));
    assert_eq!(
        fixture.kinds(&applied),
        vec![Kind::Task.as_str(), Kind::Subtask.as_str()]
    );
    assert_eq!(
        applied.rekinded, 0,
        "объявление совпало с местом — подмены нет"
    );

    let _ = std::fs::remove_dir_all(fixture.dir);
}

/// Эпик-шаблон, опущенный под узел, остаётся работоспособным: он становится задачей.
#[test]
fn an_epic_template_under_a_node_becomes_a_task() {
    let mut fixture = fixture("epic-down");
    fixture
        .save(
            "domain",
            r#"{"nodes": [{"title": "Домен", "kind": "epic", "children": [
                 {"title": "Схема", "kind": "task"}]}]}"#,
        )
        .expect("сохранение");
    let epic = node::create(
        &mut fixture.conn,
        &fixture.project,
        node::NewNode {
            parent: None,
            title: "Верх",
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
    .expect("узел");

    let applied = fixture.apply("domain", Some(&epic.dot_id));
    assert_eq!(
        fixture.kinds(&applied),
        vec![Kind::Task.as_str(), Kind::Subtask.as_str()],
        "эпик, опущенный на уровень глубже, — задача"
    );
    assert_eq!(applied.rekinded, 2);

    let _ = std::fs::remove_dir_all(fixture.dir);
}

/// Шаблон без объявленных видов применяется где угодно и ничего не подменяет.
#[test]
fn a_template_without_kinds_fits_any_level() {
    let mut fixture = fixture("no-kind");
    fixture
        .save(
            "plain",
            r#"{"nodes": [{"title": "Шаг", "children": [{"title": "Дитя"}]}]}"#,
        )
        .expect("сохранение");

    let at_root = fixture.apply("plain", None);
    assert_eq!(
        fixture.kinds(&at_root),
        vec![Kind::Epic.as_str(), Kind::Task.as_str()]
    );
    assert_eq!(at_root.rekinded, 0);

    let under = fixture.apply("plain", Some(&at_root.created[0].clone()));
    assert_eq!(
        fixture.kinds(&under),
        vec![Kind::Task.as_str(), Kind::Subtask.as_str()]
    );
    assert_eq!(under.rekinded, 0);

    let _ = std::fs::remove_dir_all(fixture.dir);
}

/// Объявления, противоречащие друг другу, ловятся при СОХРАНЕНИИ — и отказ называет узел.
///
/// Ловить при применении поздно: шаблон пишется один раз, а применяется много, и отказ пришёл бы
/// против уже выбранного родителя, то есть там, где ошибки нет.
#[test]
fn contradicting_declarations_are_refused_when_the_template_is_saved() {
    let mut fixture = fixture("contradiction");
    let error = fixture
        .save(
            "broken",
            r#"{"nodes": [{"title": "Корень", "kind": "task", "children": [
                 {"title": "Ребёнок", "kind": "epic"}]}]}"#,
        )
        .expect_err("противоречие прошло проверку");
    let text = error.to_string();
    assert!(
        text.contains("Корень") || text.contains("Ребёнок"),
        "отказ обязан называть узел, иначе искать противоречие в большом шаблоне невозможно: {text}"
    );
    assert_eq!(
        template::list(&fixture.conn, &fixture.project)
            .expect("list")
            .len(),
        0,
        "негодный шаблон не должен сохраниться"
    );

    let _ = std::fs::remove_dir_all(fixture.dir);
}
