//! Шаблоны в `templates/` загружаются, применяются и не обещают лишнего.
//!
//! Зачем проверка файлов, а не «механизм уже протестирован». Каталог — это данные: он стареет
//! молча, и файл с опечаткой в `{переменной}`, с пустым списком узлов или с `kind`, приколотым к
//! глубине, перестанет применяться без единого красного теста. Проверяется ровно то, что человек
//! делает руками: загрузить и применить — в корень и под узел.
//!
//! Сверка с `templates/README.md` держит вторую половину: таблица в документе обязана называть
//! каждый файл каталога. Иначе «какие шаблоны есть» отвечают по-разному репозиторий и его
//! собственное описание.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use tasq::{db, node, project, template};

/// Ожидаемое число узлов на шаблон — третьей копией факта не является: это то, что проверяет
/// тест, и расхождение с файлом обязано быть видно как расхождение, а не подгоняться.
const EXPECTED: &[(&str, usize)] = &[
    ("backend-behind-seam", 4),
    ("compat-client", 4),
    ("dead-code-removal", 3),
    ("docs-truth", 3),
    ("mass-operation", 3),
    ("measure-gate", 4),
    ("perf-regression", 4),
    ("protocol-defect", 5),
    ("release", 4),
    ("schema-migration", 4),
    ("single-source", 3),
    ("tui-feature", 6),
];

fn templates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("templates")
}

/// База в отдельном каталоге: у каждого теста своя, чтобы прогоны не делили состояние.
fn fresh(tag: &str) -> (PathBuf, Connection, project::Project) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-templates-{tag}-{nanos}"));
    let outcome = db::init(&dir, "TPLS", "шаблоны").expect("init");
    let conn = db::open(Path::new(&outcome.db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");
    (dir, conn, project)
}

fn count_nodes(conn: &Connection, project_id: i64) -> usize {
    conn.query_row(
        "SELECT COUNT(*) FROM nodes WHERE project_id = ?1 AND deleted_at IS NULL",
        [project_id],
        |row| row.get::<_, i64>(0),
    )
    .expect("count") as usize
}

/// Каждый файл каталога сохраняется и применяется — сначала в корень проекта, потом под узел.
///
/// Второе применение и есть проверка «вид выводится из глубины»: шаблон, у которого `kind`
/// приколот к уровню, прошёл бы первое и упал на втором, и это ровно тот дефект, ради которого
/// `kind` в файлах не указан.
#[test]
fn every_template_applies_at_the_root_and_under_a_node() {
    let (dir, mut conn, project) = fresh("apply");
    let mut catalogue = Vec::new();
    for entry in std::fs::read_dir(templates_dir()).expect("каталог шаблонов") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("имя файла — UTF-8")
            .to_string();
        catalogue.push((name, path));
    }
    catalogue.sort();
    assert!(!catalogue.is_empty(), "каталог шаблонов пуст");

    for (name, path) in catalogue {
        let raw = std::fs::read_to_string(&path).expect("файл читается");
        let tree = template::TemplateTree::parse(&raw).expect("дерево шаблона");
        template::save(&mut conn, Some(&project), &name, None, &raw, "тест").expect("сохранение");

        let expected = EXPECTED
            .iter()
            .find(|(expected_name, _)| *expected_name == name)
            .map(|(_, count)| *count)
            .unwrap_or_else(|| panic!("шаблон {name} не назван в EXPECTED этого теста"));
        assert_eq!(
            tree.count(),
            expected,
            "число узлов в {name}.json не совпало с ожидаемым"
        );

        // Переменные обязательны: применение без них обязано отказать, иначе {placeholder}
        // окажется в заголовке задачи, которую человек прочитает как готовую.
        let before = count_nodes(&conn, project.id);
        let bare = template::apply(&mut conn, &project, &name, None, &[]);
        assert!(
            bare.is_err(),
            "шаблон {name} применился без значений переменных"
        );
        assert_eq!(
            count_nodes(&conn, project.id),
            before,
            "{name}: база изменилась при отказе"
        );

        let variables: Vec<(String, String)> = tree
            .variables()
            .into_iter()
            .map(|variable| (variable, "проверка".to_string()))
            .collect();

        let at_root = template::apply(&mut conn, &project, &name, None, &variables)
            .unwrap_or_else(|err| panic!("{name} не применился в корень: {err:?}"));
        assert_eq!(
            at_root.created.len(),
            expected,
            "{name}: узлов в корне не столько"
        );

        // Второй экземпляр — под корнем, созданным первым: глубина на единицу больше.
        let root = at_root.created.first().expect("хотя бы один узел").clone();
        let under = template::apply(&mut conn, &project, &name, Some(&root), &variables)
            .unwrap_or_else(|err| panic!("{name} не применился под узлом: {err:?}"));
        assert_eq!(
            under.created.len(),
            expected,
            "{name}: узлов под узлом не столько"
        );

        // Узлы не остались незаполненными — ни в заголовке, ни в описании. Проверяется обе
        // строки: незакрытая скобка в описании (`{symptom` без `}`) не попадает в список
        // переменных, применение её не заметит, и подстановка пройдёт мимо, оставив мусор в
        // тексте задачи, которую человек прочитает как готовую.
        for dot_id in at_root.created.iter().chain(under.created.iter()) {
            let node = node::resolve(&conn, &project, dot_id).expect("узел");
            for (where_, text) in [
                ("заголовке", Some(node.title.as_str())),
                ("описании", node.description.as_deref()),
            ] {
                let Some(text) = text else { continue };
                assert!(
                    !text.contains('{') && !text.contains('}'),
                    "{dot_id}: в {where_} осталась фигурная скобка «{text}»"
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Таблица в `templates/README.md` и файлы каталога говорят одно и то же — втроём.
///
/// Проверка нашлась негативным контролем: первая версия искала имя шаблона где угодно в тексте, и
/// удаление строки таблицы проходило молча, потому что имя упомянуто ещё и в примере командой.
/// Поэтому строки разбираются как строки таблицы, а сверяются три вещи сразу — имя, переменные и
/// число узлов, — и каждая из трёх портится отдельно:
///
/// * файл без строки в таблице — шаблон, о котором никто не узнает;
/// * строка без файла — обещание, которого нет;
/// * переменная в таблице не та, что в файле — человек передаст `--var` не с тем именем и получит
///   отказ на верном с виду шаблоне;
/// * число узлов не то — планировщик посчитает объём по документу, а не по факту.
#[test]
fn the_catalogue_and_its_readme_name_the_same_templates() {
    let readme = std::fs::read_to_string(templates_dir().join("README.md")).expect("README");

    let mut rows: Vec<(String, Vec<String>, usize)> = Vec::new();
    for line in readme.lines() {
        // Строка таблицы шаблонов: имя в обрамлении, переменные в обрамлении, число, источник.
        let Some(rest) = line.strip_prefix("| `") else {
            continue;
        };
        let Some((name, tail)) = rest.split_once('`') else {
            continue;
        };
        let tail = tail.trim_start().trim_start_matches('|').trim();
        let Some(tail) = tail.strip_prefix('`') else {
            continue;
        };
        let Some((variables_text, tail)) = tail.split_once('`') else {
            continue;
        };
        // Поле числа обрамлено пробелами и вертикальными чертами: `| 5 |`. Ведущая черта
        // снимается отдельно: `split_once` на строке, начинающейся с неё, вернул бы пустое поле,
        // и таблица молча оказалась бы пустой (негативный контроль это и поймал).
        let Some((count_text, _)) = tail.trim_start().trim_start_matches('|').split_once('|')
        else {
            continue;
        };
        let Ok(count) = count_text.trim().parse::<usize>() else {
            continue;
        };
        let variables = variables_text
            .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
            .filter(|word| !word.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        rows.push((name.to_string(), variables, count));
    }
    assert!(!rows.is_empty(), "в README нет таблицы шаблонов");

    let mut files = Vec::new();
    for entry in std::fs::read_dir(templates_dir()).expect("каталог") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        files.push((
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .expect("имя файла — UTF-8")
                .to_string(),
            path,
        ));
    }
    files.sort();

    for (name, path) in &files {
        let row = rows
            .iter()
            .find(|(row_name, _, _)| row_name == name)
            .unwrap_or_else(|| panic!("шаблон {name} не назван в templates/README.md"));
        let tree = template::TemplateTree::parse(&std::fs::read_to_string(path).expect("файл"))
            .expect("дерево шаблона");
        let mut declared = row.1.clone();
        declared.sort();
        let mut actual = tree.variables();
        actual.sort();
        assert_eq!(
            declared, actual,
            "{name}: README называет переменные {declared:?}, а файл объявляет {actual:?}"
        );
        assert_eq!(
            row.2,
            tree.count(),
            "{name}: README обещает {} узлов, в файле {}",
            row.2,
            tree.count()
        );
        let expected = EXPECTED
            .iter()
            .find(|(expected_name, _)| expected_name == name)
            .map(|(_, count)| *count)
            .unwrap_or_else(|| panic!("шаблон {name} не назван в EXPECTED"));
        assert_eq!(row.2, expected, "{name}: README разошёлся с EXPECTED");
    }

    for (name, _, _) in &rows {
        assert!(
            files.iter().any(|(file, _)| file == name),
            "README называет шаблон {name}, а файла templates/{name}.json нет"
        );
    }
}
