//! HTTP-транспорт MCP: только loopback, только по явному флагу (spec §13.3).
//!
//! Зачем он есть. Клиенты, которые не умеют stdio (часть веб-IDE и внешних панелей), иначе не
//! подключаются вовсе. Замер TASQDEV-20.8 показал, что очередь записи тут ни при чём: сто
//! вызовов через один сервер укладываются в 51–66 мс, поэтому HTTP — это удобство подключения,
//! а не производительность.
//!
//! Зачем он выключен по умолчанию. Открытый порт — это поверхность, которой у инструмента,
//! работающего с одним файлом, быть не должно без спроса. Поэтому: отдельная подкоманда
//! `tasq serve`, флаг `--http` обязателен, адрес обязан быть loopback, TLS не терминируется
//! вовсе (`https://` — отказ с причиной: поднимать самодельный TLS хуже, чем не поднимать).
//!
//! Проверки запроса — не формальность. `Host` проверяется потому, что браузер на чужой странице
//! может отправить запрос на `127.0.0.1` (DNS rebinding): без проверки `Host` локальный порт
//! становится доступен любому открытому сайту. `Origin` проверяется по той же причине. Тело
//! ограничено 1 МиБ: это канал вызовов инструментов, а не файлопомойка.
//!
//! Соединения обрабатываются по одному. Обработчик не `Sync` (внутри соединение SQLite), и
//! параллельная обработка означала бы мьютекс вокруг всей работы — то есть ту же очередь, но с
//! потоками. Очередь соединений держит ядро в backlog'е слушателя.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::mcp::rpc;

/// Предел тела запроса: больше — отказ, а не обрезание.
pub const MAX_BODY: usize = 1024 * 1024;

/// Предел заголовков: защита от строки без конца.
const MAX_HEADERS: usize = 16 * 1024;

/// Сколько ждать запрос от клиента, прежде чем закрыть соединение.
///
/// Без этого один открытый сокет без данных держит сервер навсегда: соединения обрабатываются
/// по одному, и «молчащий клиент» останавливает всех остальных.
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Поднимает HTTP-сервер и обслуживает соединения до ошибки слушателя.
pub fn serve<H: rpc::Handler>(handler: &mut H, addr: &str) -> Result<()> {
    let listener = bind(addr)?;
    let local = listener.local_addr()?;
    // Адрес печатается в stderr: stdout занят протоколом, и клиент читает адрес оттуда же, откуда
    // остальную диагностику. При `:0` это единственный способ узнать порт.
    eprintln!("tasq mcp: listening on http://{local}");
    loop {
        let (stream, _peer) = listener.accept()?;
        match handle_connection(handler, stream) {
            Ok(()) => {}
            // Один плохой запрос не должен ронять сервер: клиент получил отказ, остальные ждут.
            Err(err) => eprintln!("tasq mcp: http: {err}"),
        }
    }
}

/// Разбирает адрес и привязывается к нему, если он loopback.
fn bind(addr: &str) -> Result<TcpListener> {
    let socket = parse_loopback(addr)?;
    Ok(TcpListener::bind(socket)?)
}

/// Адрес `127.0.0.1:PORT` из аргумента. Всё, что не loopback, — отказ.
///
/// Принимаются три написания loopback и ничего больше: `0.0.0.0` открыл бы порт в сеть, а
/// `https://` обещал бы TLS, которого здесь нет.
pub fn parse_loopback(addr: &str) -> Result<SocketAddr> {
    let text = addr.trim();
    if text.starts_with("https://") || text.starts_with("wss://") {
        return Err(Error::InvalidValue {
            field: "http".to_string(),
            value: format!("{addr} (TLS is not terminated by tasq; enable it in a proxy in front)"),
        });
    }
    let text = text
        .strip_prefix("http://")
        .unwrap_or(text)
        .trim_end_matches('/');
    // `localhost` разворачивается в 127.0.0.1 явно: `ToSocketAddrs` может вернуть и `::1`, и
    // первый попавшийся адрес, а проверку loopback тогда пришлось бы делать после привязки.
    let text = if let Some(rest) = text.strip_prefix("localhost:") {
        format!("127.0.0.1:{rest}")
    } else {
        text.to_string()
    };
    // Имя хоста, кроме `localhost`, не разворачивается: DNS-ответ может указать куда угодно, а
    // обещание «слушаем loopback» должно держаться на разборе строки, а не на резолвере.
    let socket: SocketAddr = text.parse().map_err(|_| Error::InvalidValue {
        field: "http".to_string(),
        value: format!(
            "{addr} (expected 127.0.0.1:PORT, localhost:PORT or [::1]:PORT; loopback only)"
        ),
    })?;
    if !socket.ip().is_loopback() {
        return Err(Error::InvalidValue {
            field: "http".to_string(),
            value: format!("{addr} (loopback address required)"),
        });
    }
    Ok(socket)
}

/// Один запрос — один ответ. Возвращает `Err`, только если ответ отдать не удалось.
fn handle_connection<H: rpc::Handler>(handler: &mut H, mut stream: TcpStream) -> Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);

    let (method, target, headers) = match read_head(&mut reader) {
        Ok(head) => head,
        Err(HeadError::Refused(status, reason)) => {
            return respond(&mut stream, status, reason, "");
        }
        Err(HeadError::Io(err)) => return Err(err.into()),
    };

    // Только POST и только в корень: это JSON-RPC, а не набор ресурсов.
    if method != "POST" {
        return respond(&mut stream, 405, "Method Not Allowed", "");
    }
    if !target.starts_with('/') {
        return respond(&mut stream, 404, "Not Found", "");
    }
    if !headers
        .get("host")
        .is_some_and(|host| host_is_loopback(host))
    {
        return respond(
            &mut stream,
            403,
            "Forbidden",
            "Host must be a loopback address",
        );
    }
    if let Some(origin) = headers.get("origin")
        && !origin_is_loopback(origin)
    {
        return respond(
            &mut stream,
            403,
            "Forbidden",
            "Origin must be a loopback address",
        );
    }
    let length: usize = match headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
    {
        Some(length) => length,
        None => {
            return respond(
                &mut stream,
                411,
                "Length Required",
                "send Content-Length; chunked encoding is not supported",
            );
        }
    };
    if length > MAX_BODY {
        return respond(&mut stream, 413, "Payload Too Large", "");
    }

    let mut body = vec![0u8; length];
    if let Err(err) = reader.read_exact(&mut body) {
        return respond(
            &mut stream,
            400,
            "Bad Request",
            &format!("body is shorter than Content-Length: {err}"),
        );
    }
    let text = match String::from_utf8(body) {
        Ok(text) => text,
        Err(_) => return respond(&mut stream, 400, "Bad Request", "body must be UTF-8"),
    };

    match rpc::dispatch(handler, text.trim()) {
        Some(response) => {
            let json = serde_json::to_string(&response).expect("сериализуемо");
            respond(&mut stream, 200, "OK", &json)
        }
        // Уведомление: по протоколу ответа нет, но HTTP обязан ответить хоть что-то.
        None => respond(&mut stream, 204, "No Content", ""),
    }
}

enum HeadError {
    /// Разбор не удался: отвечаем кодом и закрываем — это не ошибка сервера.
    Refused(u16, &'static str),
    Io(std::io::Error),
}

impl From<std::io::Error> for HeadError {
    fn from(err: std::io::Error) -> Self {
        HeadError::Io(err)
    }
}

/// Читает стартовую строку и заголовки. Тело читается отдельно: его длина уже известна.
fn read_head(
    reader: &mut BufReader<TcpStream>,
) -> std::result::Result<(String, String, std::collections::BTreeMap<String, String>), HeadError> {
    let mut head = String::new();
    let mut total = 0usize;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Err(HeadError::Refused(400, "Bad Request"));
        }
        total += read;
        if total > MAX_HEADERS {
            return Err(HeadError::Refused(431, "Request Header Fields Too Large"));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        head.push_str(&line);
    }

    let mut lines = head.lines();
    let start = lines.next().unwrap_or_default();
    let mut parts = start.split_whitespace();
    let (Some(method), Some(target), Some(_version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(HeadError::Refused(400, "Bad Request"));
    };

    let mut headers = std::collections::BTreeMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(HeadError::Refused(400, "Bad Request"));
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    Ok((method.to_string(), target.to_string(), headers))
}

/// `Host` указывает на loopback — с портом или без.
///
/// Имя разбирается как IP-адрес, а не сравнивается префиксом. Префиксное сравнение пропускало бы
/// `127.0.0.1.evil.com`: строка начинается с `127.`, а хост — чужой, и подделанный `Host` открыл
/// бы порт любому сайту. Ровно эту дыру и держит проверка.
fn host_is_loopback(host: &str) -> bool {
    let host = host.trim();
    let name = if let Some(rest) = host.strip_prefix('[') {
        // IPv6 в скобках: `[::1]:8787`.
        match rest.split_once(']') {
            Some((inside, _)) => inside,
            None => return false,
        }
    } else if host.matches(':').count() == 1 {
        // `host:port` — порт отрезается только когда двоеточие одно, иначе `::1` потеряло бы хвост.
        match host.rsplit_once(':') {
            Some((name, port)) if port.chars().all(|ch| ch.is_ascii_digit()) => name,
            _ => host,
        }
    } else {
        host
    };
    if name.eq_ignore_ascii_case("localhost") {
        return true;
    }
    name.parse::<std::net::IpAddr>()
        .map(|address| address.is_loopback())
        .unwrap_or(false)
}

/// `Origin` — URL со схемой; loopback-хост в нём обязателен.
fn origin_is_loopback(origin: &str) -> bool {
    let origin = origin.trim();
    // `null` — песочница браузера; такой источник ничего не подтверждает, и доверять ему нельзя.
    match origin.split_once("://") {
        Some((scheme, rest)) => matches!(scheme, "http" | "https") && host_is_loopback(rest),
        None => false,
    }
}

fn respond(stream: &mut TcpStream, status: u16, reason: &str, body: &str) -> Result<()> {
    let headers = if body.is_empty() {
        format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    } else {
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    stream.write_all(headers.as_bytes())?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loopback принимается в трёх написаниях, потому что все три пишут люди и клиенты.
    #[test]
    fn loopback_addresses_are_accepted() {
        assert!(parse_loopback("127.0.0.1:8787").is_ok());
        assert!(parse_loopback("localhost:8787").is_ok());
        assert!(parse_loopback("[::1]:8787").is_ok());
        assert!(parse_loopback("http://127.0.0.1:0").is_ok());
    }

    /// Всё остальное — отказ с причиной, а не «попробуем и посмотрим».
    #[test]
    fn non_loopback_and_tls_are_refused() {
        for address in ["0.0.0.0:8787", "192.168.1.10:8787", "example.com:80"] {
            let err = parse_loopback(address).expect_err(address);
            assert!(
                err.to_string().contains("loopback"),
                "{address}: ожидалась причина про loopback, получено {err}"
            );
        }
        for address in ["https://127.0.0.1:8787", "wss://127.0.0.1:8787"] {
            let err = parse_loopback(address).expect_err(address);
            assert!(
                err.to_string().contains("TLS"),
                "{address}: ожидалась причина про TLS, получено {err}"
            );
        }
        assert!(parse_loopback("просто строка").is_err());
    }

    /// `Host` и `Origin` проверяются одинаково строго: это защита от DNS rebinding, а не вкус.
    #[test]
    fn host_and_origin_must_be_loopback() {
        for host in ["127.0.0.1:8787", "localhost:80", "[::1]:8787", "127.0.0.1"] {
            assert!(host_is_loopback(host), "{host}");
        }
        for host in ["evil.example.com", "192.168.0.1:80", "127.0.0.1.evil.com"] {
            assert!(
                !host_is_loopback(host),
                "{host} не должен считаться loopback"
            );
        }
        assert!(origin_is_loopback("http://127.0.0.1:3000"));
        assert!(origin_is_loopback("https://localhost"));
        assert!(!origin_is_loopback("null"));
        assert!(!origin_is_loopback("http://evil.example.com"));
        assert!(!origin_is_loopback("file://localhost"));
    }
}
