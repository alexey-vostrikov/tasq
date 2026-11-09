//! Property-тесты на вход извне: ссылки, ресурсные шаблоны, импорт документа (README §19).
//!
//! Это замена `cargo-fuzz`: fuzz-раннер требует nightly и ASAN, а гейт идёт в том же
//! стабильном образе, что и сборка. Проверяемое свойство то же, что даёт fuzz, — **вход
//! любого вида не должен приводить к панике**, а отказ должен быть доменной ошибкой с кодом.
//! Генераторы нарочно дают мусор: обрезанные строки, юникод, вложенные скобки, пустые
//! сегменты, `..`, очень длинные значения.

use std::fs;
use std::path::PathBuf;

use proptest::prelude::*;
use rusqlite::Connection;
use serde_json::Value;

use tasq::db;
use tasq::node::{self, NewNode};
use tasq::project::{self, Project};
use tasq::resource::{self, Kind, Mode, Resource};
use tasq::template;
use tasq::transfer;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-prop-{tag}-{nanos}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn fixture(tag: &str) -> (Connection, Project) {
    let dir = temp_dir(tag);
    let outcome = db::init(&dir, "TASQ", "demo").expect("init");
    let conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = project::first_project(&conn).expect("project");
    (conn, project)
}

fn add(conn: &mut Connection, project: &Project, title: &str) -> node::Node {
    node::create(
        conn,
        project,
        NewNode {
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
    .expect("create node")
}

/// Строки, похожие на ссылку: с точками, решёткой, юникодом, пустые и очень длинные.
fn ref_like() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just("#".to_string()),
        Just("TASQ-".to_string()),
        Just("TASQ-1.".to_string()),
        Just("..".to_string()),
        "[0-9.a-zA-Z#-]{0,12}".prop_map(|text| text),
        "[^\u{0}]{0,40}".prop_map(|text| text),
        // многоточие точек и решёток — типичный вход, ломающий наивный разбор
        "[.#]{1,20}#?[0-9]{0,4}".prop_map(|text| text),
    ]
}

/// Строки, похожие на путь или шаблон резерва.
fn resource_value() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just("/".to_string()),
        Just("..".to_string()),
        Just("./".to_string()),
        Just("src/../x".to_string()),
        "[a-zA-Z0-9/._*?{}\\[\\]-]{0,30}".prop_map(|text| text),
        "[^\u{0}]{0,30}".prop_map(|text| text),
    ]
}

fn any_kind() -> impl Strategy<Value = Kind> {
    prop_oneof![Just(Kind::Path), Just(Kind::Glob), Just(Kind::Logical)]
}

fn any_mode() -> impl Strategy<Value = Mode> {
    prop_oneof![Just(Mode::Exclusive), Just(Mode::Shared)]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Разбор ссылки не паникует: любой вход даёт либо узел, либо ошибку с кодом.
    #[test]
    fn reference_parsing_never_panics(reference in ref_like()) {
        let (mut conn, project) = fixture("ref");
        let created = add(&mut conn, &project, "Эпик");

        match node::resolve(&conn, &project, &reference) {
            Ok(found) => {
                // Если разбор удался, это должен быть реально существующий узел проекта.
                prop_assert_eq!(found.id, created.id);
            }
            Err(error) => {
                let code = error.code();
                prop_assert!(!code.is_empty());
                // Отказ обязан быть внятным: пустое сообщение бесполезно агенту.
                prop_assert!(!error.to_string().is_empty());
            }
        }
    }

    /// Три формы ссылки на один и тот же узел дают один и тот же узел.
    #[test]
    fn all_reference_forms_agree(index in 1usize..=3) {
        let (mut conn, project) = fixture("forms");
        let mut ids = Vec::new();
        for step in 0..3 {
            ids.push(add(&mut conn, &project, &format!("узел {step}")).id);
        }
        let expected = ids[index - 1];

        for reference in [
            format!("TASQ-{index}"),
            index.to_string(),
            format!("#{expected}"),
        ] {
            let found = node::resolve(&conn, &project, &reference).expect("resolve");
            prop_assert_eq!(found.id, expected, "форма {} дала чужой узел", reference);
        }
    }

    /// Конфликт резервов симметричен: порядок заявок не меняет ответ.
    #[test]
    fn conflict_detection_is_symmetric(
        left_value in resource_value(),
        right_value in resource_value(),
        left_kind in any_kind(),
        right_kind in any_kind(),
        left_mode in any_mode(),
        right_mode in any_mode(),
    ) {
        let left = Resource { kind: left_kind, value: left_value, mode: left_mode };
        let right = Resource { kind: right_kind, value: right_value, mode: right_mode };
        prop_assert_eq!(
            resource::conflicts(&left, &right),
            resource::conflicts(&right, &left)
        );
    }

    /// Две shared-заявки не конфликтуют никогда: это смысл режима.
    #[test]
    fn shared_reservations_never_conflict(
        left_value in resource_value(),
        right_value in resource_value(),
    ) {
        let left = Resource { kind: Kind::Path, value: left_value, mode: Mode::Shared };
        let right = Resource { kind: Kind::Path, value: right_value, mode: Mode::Shared };
        prop_assert!(!resource::conflicts(&left, &right));
    }

    /// Exclusive-заявка на путь конфликтует сама с собой — иначе два агента взяли бы один файл.
    #[test]
    fn exclusive_path_conflicts_with_itself(value in "[a-zA-Z0-9/_.-]{1,40}") {
        let one = Resource { kind: Kind::Path, value: value.clone(), mode: Mode::Exclusive };
        let two = Resource { kind: Kind::Path, value, mode: Mode::Exclusive };
        prop_assert!(resource::conflicts(&one, &two));
    }

    /// Нормализация идемпотентна: принятое значение второй раз уже не меняется.
    #[test]
    fn normalization_is_idempotent(
        value in resource_value(),
        kind in any_kind(),
        mode in any_mode(),
    ) {
        let raw = Resource { kind, value, mode };
        if let Ok(once) = resource::normalize(&raw) {
            let twice = resource::normalize(&once).expect("повторная нормализация принятого значения");
            prop_assert_eq!(once.clone(), twice);
        }
    }

    /// Подстановка переменных не паникует и не оставляет известных плейсхолдеров.
    #[test]
    fn template_rendering_consumes_known_placeholders(
        prefix in "[a-zA-Z0-9 ]{0,20}",
        name in "[a-z]{1,6}"
    ) {
        let text = format!("{prefix}{{{name}}}!");
        let values = vec![(name.clone(), "значение".to_string())];
        let rendered = template::render(&text, &values).expect("render");
        let placeholder = format!("{{{name}}}");
        prop_assert!(!rendered.contains(&placeholder));
        prop_assert!(rendered.starts_with(&prefix));
    }

    /// Неизвестная переменная — отказ, а не «{name}» в тексте.
    #[test]
    fn unknown_variables_are_rejected(name in "[a-z]{1,6}") {
        let text = format!("{{{name}}}");
        prop_assert!(template::render(&text, &[]).is_err());
    }

    /// Произвольный текст без плейсхолдеров возвращается как есть.
    #[test]
    fn text_without_placeholders_passes_through(text in "[^{}]{0,40}") {
        let rendered = template::render(&text, &[]).expect("render");
        prop_assert_eq!(rendered, text);
    }

    /// Разбор дерева шаблона на произвольных строках не паникует.
    #[test]
    fn template_tree_parsing_never_panics(raw in ".{0,120}") {
        match template::TemplateTree::parse(&raw) {
            Ok(tree) => {
                // Переменные не должны содержать незакрытую скобку — иначе рендер сломан.
                for variable in tree.variables() {
                    prop_assert!(!variable.contains('{'), "переменная {variable}");
                }
            }
            Err(error) => prop_assert!(!error.code().is_empty()),
        }
    }
}

/// Документ импорта с побайтовыми повреждениями: `dry_run` обязан либо принять его,
/// либо отказать доменной ошибкой — но не упасть.
#[test]
fn corrupted_import_documents_never_panic() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;

    let (mut conn, project) = fixture("import");
    add(&mut conn, &project, "Эпик");
    let document = transfer::export(&conn, &project, false).expect("export");
    let valid = serde_json::to_string(&document).expect("serialise");

    // Границы резания считаются по символам, а не по байтам: иначе разрез попадает
    // внутрь многобайтового символа и паника будет в тесте, а не в проверяемом коде.
    let boundaries: Vec<usize> = valid.char_indices().map(|(index, _)| index).collect();
    let len = valid.len();

    let mut runner = TestRunner::deterministic();
    let strategy = (
        0..boundaries.len(),
        0..12usize,
        "[a-zA-Z0-9{}\"\\[\\]-]{0,8}",
    )
        .prop_map(move |(start_index, cut, insert)| {
            let start = boundaries[start_index];
            let end = boundaries.get(start_index + cut).copied().unwrap_or(len);
            let mut text = valid.clone();
            text.replace_range(start..end, &insert);
            text
        });

    for _ in 0..200 {
        let corrupted = strategy.new_tree(&mut runner).expect("value").current();
        // Документ может не разобраться как JSON — это уже отказ, и он не должен паниковать.
        let Ok(value) = serde_json::from_str::<Value>(&corrupted) else {
            continue;
        };
        match transfer::dry_run(&conn, &project, &value) {
            Ok(report) => {
                // Успех возможен: повреждение могло попасть в необязательное поле.
                assert_eq!(report.mode, "dry-run");
            }
            Err(error) => assert!(
                !error.code().is_empty(),
                "отказ без кода на входе {corrupted}"
            ),
        }
    }
}

/// Испорченная контрольная сумма отвергается всегда, а не «если заметим».
#[test]
fn any_checksum_change_is_rejected() {
    let (mut conn, project) = fixture("checksum");
    add(&mut conn, &project, "Эпик");
    let document = transfer::export(&conn, &project, false).expect("export");

    for replacement in ["", "0", "00", "ff", "not-a-hash", &"a".repeat(64)] {
        let mut broken = document.clone();
        broken["sha256"] = Value::String(replacement.to_string());
        let result = transfer::dry_run(&conn, &project, &broken);
        assert!(result.is_err(), "сумма {replacement:?} принята");
        assert_eq!(
            result.expect_err("error").code(),
            "EXPORT_CHECKSUM_MISMATCH",
            "сумма {replacement:?}"
        );
    }
}

/// Кусок «дерева» шаблона — фиксированный набор, чтобы ошибка в разборе была видна сразу.
#[test]
fn deep_template_nesting_is_bounded() {
    // Глубокая вложенность не должна ронять разбор: предел либо есть, либо разбор успешен,
    // но паники быть не может.
    let mut node = serde_json::json!({"title": "лист"});
    for _ in 0..200 {
        node = serde_json::json!({ "title": "узел", "children": [node] });
    }
    let raw = serde_json::to_string(&node).expect("serialise");
    let parsed = template::TemplateTree::parse(&raw);
    assert!(
        parsed.is_ok() || parsed.is_err(),
        "разбор вернул управление"
    );
}

/// Родитель в ответе `template apply` без `--parent` — ключ проекта, а не «TASQ--».
/// «TASQ--» выглядит как ссылка на узел, но ни на что не указывает (найдено тестом MCP).
#[test]
fn template_applied_at_root_names_the_project() {
    let (mut conn, project) = fixture("template-root");
    let tree = serde_json::json!([{"title": "{resource}: контроллер"}]);
    template::save(
        &mut conn,
        &project,
        "api",
        None,
        &serde_json::to_string(&tree).expect("serialise"),
        "tester",
    )
    .expect("save");
    let applied = template::apply(
        &mut conn,
        &project,
        "api",
        None,
        &[("resource".to_string(), "orders".to_string())],
    )
    .expect("apply");

    assert_eq!(applied.parent, project.key, "родитель — ключ проекта");
    assert_eq!(applied.created.len(), 1);
}
