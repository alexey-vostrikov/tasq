//! Экран в простое не перечитывает доску; чужая запись — перечитывает.
//!
//! Проверка идёт через псевдотерминал, а не через функцию: приёмка эпика сформулирована числом
//! («полных чтений в минуту в простое = 0»), и число обязано приходить из настоящего цикла экрана
//! с настоящим ожиданием нажатия, а не из вызова сторожа в тесте. `script` даёт псевдотерминал,
//! без которого `tasq tui` не запускается вовсе, — и он есть в образе, где идут гейты.
//!
//! Что именно проверяется:
//!
//! * за паузу без нажатий база не опрашивается ВООБЩЕ: экран спит в ожидании письма, а не
//!   спрашивает `data_version` по таймеру;
//! * запись, сделанная ДРУГИМ процессом, доводит до перечитывания — иначе «не читаем в простое»
//!   достигалось бы ценой неработающего экрана, что хуже опроса.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tasq::db;

fn init(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-watch-{tag}-{nanos}"));
    let outcome = db::init(&dir, "WATCH", "сторож").expect("init");
    let mut conn = db::open(std::path::Path::new(&outcome.db_path)).expect("open");
    let project = tasq::project::first_project(&conn).expect("project");
    tasq::node::create(
        &mut conn,
        &project,
        tasq::node::NewNode {
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
            tags: None,
        },
    )
    .expect("node");
    std::path::PathBuf::from(outcome.db_path)
}

/// Числа из строки `watch: N checks, M board reads on change, K board reads by hand, L idle,
/// F full reads, D delta reads, last ...`.
struct Counts {
    checks: u64,
    on_change: u64,
    by_hand: u64,
    idle: u64,
    /// Полных чтений доски: на широкой базе каждое стоит сотни миллисекунд.
    full: u64,
    /// Чтений по изменениям: единицы строк вместо всей доски.
    delta: u64,
}

fn parse_counts(frame: &str) -> Counts {
    // Строка ищется подстрокой, а не с начала строки: экран выходит из чужого буфера
    // escape-последовательностью, и последняя строка кадра приклеивается к ней без перевода
    // строки. Это же свойство означает, что строка сторожа печатается ПОСЛЕ восстановления
    // терминала, а не уезжает вместе с буфером.
    let tail = frame
        .rsplit("watch: ")
        .next()
        .filter(|_| frame.contains("watch: "))
        .unwrap_or_else(|| panic!("в выводе нет строки сторожа: {frame:.400}"));
    let line = format!("watch: {}", tail.lines().next().unwrap_or(""));
    let numbers: Vec<u64> = line
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<u64>().expect("число"))
        .collect();
    // Первые шесть — счётчики; дальше идёт описание последнего обновления, и число в нём
    // («delta of 3 node(s)») к счётчикам не относится.
    assert!(
        numbers.len() >= 6,
        "формат строки сторожа изменился: {line}"
    );
    Counts {
        checks: numbers[0],
        on_change: numbers[1],
        by_hand: numbers[2],
        idle: numbers[3],
        full: numbers[4],
        delta: numbers[5],
    }
}

/// Прогон экрана в псевдотерминале: `before_quit` вызывается, пока экран ещё жив.
///
/// Возвращает то, что напечатал процесс после восстановления терминала (строка сторожа живёт
/// там же), и очищает каталог.
fn run_pty(db_path: &std::path::Path, idle: Duration, before_quit: impl FnOnce()) -> String {
    let binary = env!("CARGO_BIN_EXE_tasq");
    // Размер задаётся явно: у псевдотерминала, созданного без наследования терминала, он нулевой,
    // и кадр выродился бы в пустую рамку — тогда проверка сторожа осталась бы без экрана, который
    // она охраняет.
    let script = format!(
        "stty cols 120 rows 30; {} tui --stats --db {}",
        binary,
        db_path.display()
    );
    let mut child = Command::new("script")
        .args(["-qec", &script, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("script запускается");

    std::thread::sleep(idle);
    before_quit();
    // Пауза после действия короче часового тика (одна секунда) и заметно длиннее реакции на
    // событие файла: так проверка отличает «экран проснулся от события» от «повезло с тиком».
    std::thread::sleep(Duration::from_millis(400));
    // `q` живёт в шкале клавиш основного экрана: панель проектов и формы его не перехватывают,
    // пока фокус не ушёл в поле ввода.
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"q")
        .expect("клавиша уходит в псевдотерминал");
    let output = child.wait_with_output().expect("экран завершился");
    String::from_utf8_lossy(&output.stdout).replace('\0', "")
}

#[test]
fn an_idle_screen_does_not_read_the_board() {
    let path = init("idle");
    let frame = run_pty(&path, Duration::from_millis(1500), || {});
    let counts = parse_counts(&frame);
    // Главное утверждение шага 12.2: в простое база не опрашивается вовсе. Не «реже», а ноль
    // обращений: экран спит в ожидании письма от терминала или от наблюдения за каталогом.
    assert_eq!(
        counts.checks, 0,
        "в простое база обязана не опрашиваться: ноль обращений к data_version"
    );
    assert_eq!(
        counts.on_change, 0,
        "в простое нечего перечитывать по изменению"
    );
    assert_eq!(
        counts.by_hand, 0,
        "нажатий не было — и чтений по нажатию нет"
    );
    assert_eq!(
        counts.idle, 0,
        "полных чтений доски в простое обязано быть ноль, а не «мало»"
    );
    // Начальная сборка идёт мимо счётчиков (экран строится до цикла), поэтому в простое оба
    // числа нулевые: доска не читается ни целиком, ни по изменениям.
    assert_eq!(counts.full, 0, "полное чтение доски в простое");
    assert_eq!(counts.delta, 0, "в простое нечего обновлять по изменениям");
    let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
}

#[test]
fn a_foreign_write_is_noticed_while_idle() {
    let path = init("foreign");
    let started = Instant::now();
    let frame = run_pty(&path, Duration::from_millis(1200), || {
        // Запись делает ДРУГОЙ процесс той же командой, что и человек из соседнего окна: у
        // сторожа нет иного способа узнать о ней, кроме `data_version`.
        let status = Command::new(env!("CARGO_BIN_EXE_tasq"))
            .args([
                "add",
                "узел из соседнего окна",
                "--db",
                path.to_str().expect("путь"),
                "--project",
                "WATCH",
            ])
            .status()
            .expect("соседняя команда");
        assert!(status.success(), "соседняя запись не удалась");
    });
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "прогон не завис"
    );
    let counts = parse_counts(&frame);
    assert!(
        counts.checks >= 1,
        "событие файла обязано дойти до сторожа, а через него до базы"
    );
    assert!(
        counts.on_change >= 1,
        "чужая запись обязана дойти до экрана: {counts_on_change}",
        counts_on_change = counts.on_change
    );
    // И дойти ОБНОВЛЕНИЕМ, а не перечитыванием всей доски: иначе инкрементального пути нет.
    assert!(
        counts.delta >= 1,
        "чужая запись пришла полным чтением, а не обновлением по изменениям"
    );
    // Начальная сборка мимо счётчиков, значит полных чтений в цикле быть не должно: чужая
    // запись обязана прийти обновлением.
    assert_eq!(counts.full, 0, "чужая запись вызвала полное чтение доски");
    assert_eq!(
        counts.idle, 0,
        "перечитывания по изменению — не работа в простое"
    );
    let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
}
