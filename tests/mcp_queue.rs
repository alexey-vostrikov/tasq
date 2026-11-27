//! Очередь вызовов через MCP-сервер: сто агентов на одну базу.
//!
//! Прежние замеры конкурентности (TASQDEV-19) сделаны на ста CLI-процессах, каждый из которых
//! открывает базу сам. Живой режим другой: у агента живёт ОДИН MCP-процесс, и вызовы приходят в
//! него очередью. Сервер читает stdin построчно и обрабатывает по одному (`rpc::serve`), поэтому
//! здесь нет ни файловых блокировок, ни повторов — есть только очередь внутри процесса, и её
//! длина определяется временем одного вызова.
//!
//! Что проверяется и почему именно это:
//! - ни один из ста вызовов не отказал: любая ошибка в этом сценарии означает, что сериализация
//!   не работает и вызовы столкнулись за файл;
//! - все сто узлов действительно созданы: «успешно» по коду возврата и «создано» — разные вещи,
//!   и проверять надо второе;
//! - сто вызовов укладываются в бюджет: он ловит возврат к посимвольному fsync или к
//!   пятисекундному ожиданию блокировки, а не колебания на конкретной машине;
//! - чтение не ждёт записи: пока сервер создаёт узлы, отдельный процесс читает доску.
//!
//! Замер снаружи (на этой машине): один сервер — 100 вызовов за 51–66 мс, p50 26–33 мс; сто
//! отдельных серверов — 358–364 мс, p95 227 мс, ошибок нет. Отсюда вывод для TASQDEV-20.9:
//! очередь записи в MCP-режиме не является узким местом, и HTTP-транспорт ради неё не нужен.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tasq")
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-mcp-queue-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Server {
    fn start(dir: &std::path::Path, project: &str) -> Self {
        let mut child = Command::new(bin())
            .current_dir(dir)
            .args(["--project", project, "mcp", "--flush", "0"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("запуск tasq mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut server = Server {
            child,
            stdin,
            stdout,
        };
        server.send(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#);
        let _ = server.read_line();
        server
    }

    fn send(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("запись в stdin");
    }

    fn read_line(&mut self) -> String {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("чтение из stdout");
        line
    }

    fn create(&mut self, id: usize, title: &str) {
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"node_create","arguments":{{"title":"{title}"}}}}}}"#
        );
        self.send(&request);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn node_count(dir: &std::path::Path) -> i64 {
    let out = Command::new(bin())
        .current_dir(dir)
        .args(["--project", "MQUE", "ls", "--json"])
        .output()
        .expect("tasq ls");
    let text = String::from_utf8_lossy(&out.stdout);
    text.matches("\"dot_id\"").count() as i64
}

/// Сто вызовов в один сервер: все созданы, ни одного отказа, в бюджет.
#[test]
#[ignore = "замер: запускается целью `make queue`, не в общем прогоне"]
fn a_hundred_calls_queue_through_one_server_without_failures() {
    let dir = temp_dir("one");
    let init = Command::new(bin())
        .current_dir(&dir)
        .args(["init", "--key", "MQUE", "--name", "очередь"])
        .output()
        .expect("init");
    assert!(init.status.success(), "init: {init:?}");

    let mut server = Server::start(&dir, "MQUE");

    // Все запросы уходят в трубу сразу: это и есть очередь. Если бы сервер ждал ответа клиента
    // перед следующим вызовом, замер показывал бы не очередь, а пинг-понг.
    let started = Instant::now();
    for i in 1..=100 {
        server.create(i, &format!("узел {i}"));
    }
    server.stdin.flush().expect("flush");

    let mut errors = 0;
    for _ in 0..100 {
        let line = server.read_line();
        assert!(
            !line.is_empty(),
            "сервер закрыл поток раньше ответа на все вызовы"
        );
        if line.contains("\"error\"") || line.contains("\"isError\":true") {
            errors += 1;
            eprintln!("{line}");
        }
    }
    let wall = started.elapsed();

    assert_eq!(
        errors, 0,
        "часть вызовов отказала: сериализация не работает"
    );
    assert_eq!(node_count(&dir), 100, "создано не сто узлов");
    // Бюджет намеренно грубый: он ловит возврат к fsync на каждом вызове (десятки секунд) или к
    // пятисекундному ожиданию блокировки, а не медленную машину.
    assert!(
        wall < Duration::from_secs(10),
        "сто вызовов заняли {wall:?} — это не очередь, а ожидание блокировки"
    );
    eprintln!(
        "один сервер, 100 вызовов: {:?}, {:.2} мс на вызов",
        wall,
        wall.as_secs_f64() * 1000.0 / 100.0
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Чтение не ждёт записи: пока сервер создаёт узлы, доска отвечает.
///
/// В WAL читатель видит свой снимок и не должен становиться в очередь писателя. Если это
/// нарушится, то симптом будет именно такой: доска начнёт отвечать секундами, пока агент пишет.
#[test]
#[ignore = "замер: запускается целью `make queue`, не в общем прогоне"]
fn reading_does_not_wait_for_a_writing_server() {
    let dir = temp_dir("mixed");
    let init = Command::new(bin())
        .current_dir(&dir)
        .args(["init", "--key", "MQUE", "--name", "очередь"])
        .output()
        .expect("init");
    assert!(init.status.success(), "init: {init:?}");

    let mut server = Server::start(&dir, "MQUE");
    for i in 1..=20 {
        server.create(i, &format!("до {i}"));
        let _ = server.read_line();
    }

    let reads = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Duration>::new()));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let dir = dir.clone();
        let reads = std::sync::Arc::clone(&reads);
        let stop = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let started = Instant::now();
                let out = Command::new(bin())
                    .current_dir(&dir)
                    .args(["--project", "MQUE", "board"])
                    .output()
                    .expect("board");
                assert!(out.status.success(), "board отказал во время записи");
                reads.lock().expect("lock").push(started.elapsed());
            }
        })
    };

    let started = Instant::now();
    for i in 21..=120 {
        server.create(i, &format!("во время чтения {i}"));
    }
    server.stdin.flush().expect("flush");
    for _ in 21..=120 {
        let line = server.read_line();
        assert!(!line.contains("\"error\""), "{line}");
    }
    let write_wall = started.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    reader.join().expect("читатель");

    let reads = reads.lock().expect("lock");
    let mut sorted: Vec<Duration> = reads.clone();
    sorted.sort();
    let slowest = sorted.last().copied().unwrap_or_default();
    assert!(!sorted.is_empty(), "читатель не успел ни одного запроса");
    eprintln!(
        "100 записей при чтении: {:?}; чтений {}, самый долгий {:?}",
        write_wall,
        sorted.len(),
        slowest
    );
    // Чтение на 10 000 узлах — сотни миллисекунд; здесь база на 120 узлов, и потолок нужен
    // только чтобы поймать «читатель встал в очередь писателя» (это секунды).
    assert!(
        slowest < Duration::from_secs(3),
        "самое долгое чтение {slowest:?} — читатель ждал писателя"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
