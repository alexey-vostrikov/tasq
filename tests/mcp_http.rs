//! HTTP-транспорт MCP: включается только явно, слушает только loopback, отвечает по протоколу.
//!
//! Проверки здесь не про удобство, а про поверхность. Порт, открытый по умолчанию, доступен
//! любому процессу на машине; порт, слушающий не loopback, — ещё и сети. `Host` проверяется
//! потому, что браузер на чужой странице умеет отправлять запросы на `127.0.0.1` (DNS rebinding),
//! и без этой проверки локальный трекер стал бы доступен любому открытому сайту.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tasq")
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tasq-http-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Сервер, поднятый на свободном порту: адрес читается из stderr, потому что при `:0` его
/// больше негде узнать.
struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn start(dir: &std::path::Path) -> Self {
        let mut child = Command::new(bin())
            .current_dir(dir)
            .args(["serve", "--http", "127.0.0.1:0", "--profile", "agent"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("запуск tasq serve");
        let stderr = child.stderr.take().expect("stderr");
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        let port = loop {
            line.clear();
            if reader.read_line(&mut line).expect("чтение stderr") == 0 {
                panic!("сервер закрыл stderr, не назвав адрес");
            }
            if let Some(rest) = line.split("http://").nth(1) {
                let address = rest.trim();
                break address
                    .rsplit_once(':')
                    .map(|(_, port)| port.parse::<u16>().expect("порт"))
                    .expect("адрес вида host:port");
            }
        };
        Server { child, port }
    }

    fn connect(&self) -> TcpStream {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).expect("подключение");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        stream
    }

    /// Отправляет сырой запрос и возвращает код ответа и тело.
    fn raw(&self, request: &str) -> (u16, String) {
        let mut stream = self.connect();
        stream.write_all(request.as_bytes()).expect("запрос");
        stream.flush().expect("flush");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("ответ");
        let status = response
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default();
        (status, body)
    }

    fn post(&self, body: &str, host: &str, origin: Option<&str>) -> (u16, String) {
        let mut request = format!(
            "POST / HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        );
        if let Some(origin) = origin {
            request.push_str(&format!("Origin: {origin}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(body);
        self.raw(&request)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Без флага сервер не поднимается: порт открывают по просьбе, а не по умолчанию.
#[test]
fn serve_without_the_http_flag_is_refused() {
    let dir = temp_dir("no-flag");
    let out = Command::new(bin())
        .current_dir(&dir)
        .args(["serve"])
        .output()
        .expect("запуск tasq serve");
    assert!(!out.status.success(), "serve без --http не должен работать");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--http"),
        "отказ обязан называть флаг: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Не-loopback адрес и `https://` отвергаются до открытия порта, с причиной.
#[test]
fn only_loopback_without_tls_is_accepted() {
    let dir = temp_dir("bad-addr");
    for (address, expected) in [
        ("0.0.0.0:8787", "loopback"),
        ("https://127.0.0.1:8787", "TLS"),
    ] {
        let out = Command::new(bin())
            .current_dir(&dir)
            .args(["serve", "--http", address])
            .output()
            .expect("запуск tasq serve");
        assert!(!out.status.success(), "{address} не должен запускаться");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(expected),
            "{address}: ожидалось слово «{expected}», получено: {stderr}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Протокол по HTTP: initialize, tools/list, уведомление, и отказы на непрошеное.
#[test]
fn the_http_transport_speaks_json_rpc_and_enforces_the_surface() {
    let dir = temp_dir("rpc");
    let init = Command::new(bin())
        .current_dir(&dir)
        .args(["init", "--key", "HTTP", "--name", "http"])
        .output()
        .expect("init");
    assert!(init.status.success(), "init: {init:?}");

    let server = Server::start(&dir);
    let host = format!("127.0.0.1:{}", server.port);

    let (status, body) = server.post(
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}).to_string(),
        &host,
        None,
    );
    assert_eq!(status, 200, "{body}");
    let response: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(response["jsonrpc"], "2.0");
    assert!(
        response["result"]["protocolVersion"].is_string(),
        "{response}"
    );

    let (status, body) = server.post(
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}).to_string(),
        &host,
        None,
    );
    assert_eq!(status, 200);
    let response: Value = serde_json::from_str(&body).expect("json");
    let tools = response["result"]["tools"]
        .as_array()
        .expect("каталог инструментов");
    assert!(!tools.is_empty(), "профиль agent обязан отдать инструменты");

    // Осмысленный вызов, а не только список: транспорт должен доводить работу до базы.
    let (status, body) = server.post(
        &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "node_create", "arguments": {"title": "узел по HTTP"}}})
        .to_string(),
        &host,
        None,
    );
    assert_eq!(status, 200);
    let response: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(response["result"]["isError"], json!(false), "{response}");

    // Уведомление — без тела, но с ответом: молчащий HTTP-клиент ждал бы вечно.
    let (status, _) = server.post(
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
        &host,
        None,
    );
    assert_eq!(status, 204);

    // Чужой Host — отказ: иначе локальный порт открыт любому сайту (DNS rebinding).
    let (status, _) = server.post(
        &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"}).to_string(),
        "evil.example.com",
        None,
    );
    assert_eq!(status, 403, "чужой Host обязан получить отказ");

    // Чужой Origin — тоже отказ.
    let (status, _) = server.post(
        &json!({"jsonrpc": "2.0", "id": 5, "method": "tools/list"}).to_string(),
        &host,
        Some("http://evil.example.com"),
    );
    assert_eq!(status, 403, "чужой Origin обязан получить отказ");

    // Свой Origin — разрешён.
    let (status, _) = server.post(
        &json!({"jsonrpc": "2.0", "id": 6, "method": "tools/list"}).to_string(),
        &host,
        Some("http://127.0.0.1:3000"),
    );
    assert_eq!(status, 200, "loopback-Origin не должен мешать");

    // GET — не транспорт для вызовов.
    let (status, _) = server.raw(&format!(
        "GET / HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\n\r\n"
    ));
    assert_eq!(status, 405);

    // Тело больше мегабайта не читается вовсе.
    let big = "x".repeat(1024 * 1024 + 1);
    let (status, _) = server.raw(&format!(
        "POST / HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\n\r\n",
        big.len()
    ));
    assert_eq!(status, 413);

    // Без Content-Length — 411: chunked не поддерживается, и притворяться нельзя.
    let (status, _) = server.raw(&format!(
        "POST / HTTP/1.1\r\nHost: {host}\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"
    ));
    assert_eq!(status, 411);

    let _ = std::fs::remove_dir_all(&dir);
}
