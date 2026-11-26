//! Шаблоны без проекта: список по всей базе, общие шаблоны и переопределение.
//!
//! Зачем отдельный файл. Поведение «без проекта видно всё» — это контракт чтения, и ломается он
//! тихо: список просто вернёт меньше строк, а человек решит, что шаблон не сохранился. Поэтому
//! проверяется не «список не пуст», а состав: чей шаблон, сколько их, что находится по имени.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use tasq::project::{self, Project};
use tasq::template;
use tasq::{db, node};

struct Fixture {
    dir: PathBuf,
    conn: Connection,
    alpha: Project,
    beta: Project,
}

fn fixture(tag: &str) -> Fixture {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-tpl-scope-{tag}-{nanos}"));
    let outcome = db::init(&dir, "ALPHA", "альфа").expect("init");
    let mut conn = db::open(Path::new(&outcome.db_path)).expect("open");
    let alpha = project::first_project(&conn).expect("project");
    let beta = project::create(&mut conn, "BETA", Some("бета")).expect("project");
    Fixture {
        dir,
        conn,
        alpha,
        beta,
    }
}

impl Fixture {
    fn save(&mut self, project: Option<&Project>, name: &str, title: &str) -> template::Template {
        let raw = format!(r#"{{"nodes": [{{"title": "{title} {{x}}"}}]}}"#);
        template::save(&mut self.conn, project, name, None, &raw, "тест")
            .unwrap_or_else(|err| panic!("сохранение {name}: {err:?}"))
    }
}

/// Без проекта список отдаёт шаблоны ВСЕХ проектов и общие, и у каждого назван владелец.
///
/// Порядок не проверяется намеренно: он задан для читаемости, а не как контракт, и тест на
/// порядок краснел бы от переименования проекта.
#[test]
fn without_a_project_every_template_is_listed_with_its_owner() {
    let mut fixture = fixture("all");
    fixture.save(Some(&fixture.alpha.clone()), "only-alpha", "Только альфа");
    fixture.save(Some(&fixture.beta.clone()), "only-beta", "Только бета");
    fixture.save(None, "shared", "Общий");

    let all = template::list_all(&fixture.conn).expect("list_all");
    let owners: BTreeSet<(String, Option<String>)> = all
        .iter()
        .map(|template| (template.name.clone(), template.project.clone()))
        .collect();
    assert_eq!(
        owners,
        BTreeSet::from([
            ("only-alpha".to_string(), Some("ALPHA".to_string())),
            ("only-beta".to_string(), Some("BETA".to_string())),
            ("shared".to_string(), None),
        ]),
        "список без проекта обязан назвать владельца у каждого шаблона"
    );

    let _ = std::fs::remove_dir_all(fixture.dir);
}

/// С проектом видны свои и общие, но не чужие.
#[test]
fn with_a_project_the_list_is_its_own_plus_the_global_ones() {
    let mut fixture = fixture("scoped");
    let alpha = fixture.alpha.clone();
    let beta = fixture.beta.clone();
    fixture.save(Some(&alpha), "only-alpha", "Только альфа");
    fixture.save(Some(&beta), "only-beta", "Только бета");
    fixture.save(None, "shared", "Общий");

    let names: Vec<String> = template::list(&fixture.conn, &alpha)
        .expect("list")
        .into_iter()
        .map(|template| template.name)
        .collect();
    assert_eq!(names, vec!["only-alpha", "shared"]);

    let _ = std::fs::remove_dir_all(fixture.dir);
}

/// Свой шаблон побеждает одноимённый общий — и у каждого проекта по-своему.
///
/// Это и делает общий шаблон полезным: его можно взять из каталога и поправить под проект, не
/// переименовывая и не теряя связь с общим.
#[test]
fn a_project_template_overrides_the_global_one() {
    let mut fixture = fixture("override");
    let alpha = fixture.alpha.clone();
    let beta = fixture.beta.clone();
    fixture.save(None, "common", "Общий");
    fixture.save(Some(&beta), "common", "Только для беты");

    let in_alpha = template::by_name(&fixture.conn, Some(&alpha), "common").expect("alpha");
    assert_eq!(
        in_alpha.project, None,
        "в ALPHA своего нет, значит найден общий"
    );
    assert!(in_alpha.tree.nodes[0].title.starts_with("Общий"));

    let in_beta = template::by_name(&fixture.conn, Some(&beta), "common").expect("beta");
    assert_eq!(in_beta.project.as_deref(), Some("BETA"));
    assert!(in_beta.tree.nodes[0].title.starts_with("Только для беты"));

    // Без проекта находится именно общий: в базе два одноимённых, и «какой-нибудь» здесь не
    // ответ — общий однозначен, проектный нет.
    let without = template::by_name(&fixture.conn, None, "common").expect("global");
    assert_eq!(without.project, None);

    let _ = std::fs::remove_dir_all(fixture.dir);
}

/// Общий шаблон применяется в проекте: иначе он бесполезен.
#[test]
fn a_global_template_applies_inside_a_project() {
    let mut fixture = fixture("apply");
    let alpha = fixture.alpha.clone();
    fixture.save(None, "shared", "Общий");

    let values = vec![("x".to_string(), "раз".to_string())];
    let applied = template::apply(&mut fixture.conn, &alpha, "shared", None, &values)
        .expect("применение общего шаблона");
    assert_eq!(applied.created.len(), 1);
    let created = node::resolve(&fixture.conn, &alpha, &applied.created[0]).expect("узел");
    assert!(
        created.title.contains("раз"),
        "переменная не подставилась: {}",
        created.title
    );
    // И попал он именно в ALPHA, а не в „какой-нибудь“ проект базы.
    assert_eq!(created.project_id, alpha.id);

    let _ = std::fs::remove_dir_all(fixture.dir);
}

/// Общий шаблон из проекта не удаляется — и отказ называет причину, а не «не найдено».
#[test]
fn a_global_template_is_not_deleted_from_inside_a_project() {
    let mut fixture = fixture("delete");
    let alpha = fixture.alpha.clone();
    fixture.save(None, "shared", "Общий");

    let error = template::delete(&mut fixture.conn, Some(&alpha), "shared", "тест")
        .expect_err("удаление общего шаблона из проекта прошло");
    assert_eq!(error.code(), "GLOBAL_TEMPLATE", "{error}");
    // Шаблон на месте: отказ не должен ничего удалять по пути.
    assert!(template::by_name(&fixture.conn, None, "shared").is_ok());

    // Без проекта — удаляется: там он и живёт.
    template::delete(&mut fixture.conn, None, "shared", "тест").expect("удаление общего");
    assert!(template::by_name(&fixture.conn, None, "shared").is_err());

    let _ = std::fs::remove_dir_all(fixture.dir);
}
