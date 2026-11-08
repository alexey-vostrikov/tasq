//! JSON-RPC 2.0 поверх stdio (README §13.1).
//!
//! Одно сообщение — одна строка JSON. Ответ — тоже строка. Диагностика идёт только в stderr:
//! stdout занят протоколом, и любой посторонний вывод ломает клиента.
//!
//! Реализация без внешних крейтов: протокол MCP в объёме `initialize`/`ping`/`tools/list`/
//! `tools/call` — это несколько десятков строк, а лишняя зависимость в единственном бинаре
//! дороже, чем эти строки.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

/// Коды ошибок JSON-RPC.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// Версия протокола, которую объявляет сервер.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Ответ на одно сообщение: либо результат, либо ошибка.
pub enum Response {
    /// Успех: `result`.
    Ok(Value),
    /// Ошибка протокола: `error{code,message}`.
    Err(i64, String),
    /// Отвечать не нужно (уведомление без `id`).
    None,
}

impl Response {
    pub fn into_json(self, id: Option<Value>) -> Option<Value> {
        match self {
            Response::None => None,
            Response::Ok(result) => Some(json!({
                "jsonrpc": "2.0",
                "id": id.unwrap_or(Value::Null),
                "result": result,
            })),
            Response::Err(code, message) => Some(json!({
                "jsonrpc": "2.0",
                "id": id.unwrap_or(Value::Null),
                "error": {"code": code, "message": message},
            })),
        }
    }
}

/// Обработчик одного вызова: получает имя метода и параметры.
pub trait Handler {
    /// `initialize`
    fn initialize(&mut self) -> Value;
    /// `tools/list`
    fn list_tools(&mut self) -> Value;
    /// `tools/call`
    fn call_tool(&mut self, name: &str, arguments: &Value) -> Value;
    /// Всё остальное, что сервер умеет помимо протокола (`prompts/list` и т.п. — заглушки).
    fn ping(&mut self) -> Value {
        json!({})
    }
}

/// Разбирает одно входящее сообщение и возвращает ответ (или `None`, если это уведомление).
pub fn dispatch<H: Handler>(handler: &mut H, line: &str) -> Option<Value> {
    let message: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(err) => {
            return Response::Err(PARSE_ERROR, format!("invalid JSON: {err}"))
                .into_json(Some(Value::Null));
        }
    };

    let id = message.get("id").cloned();
    let is_notification = id.is_none();

    let method = match message.get("method").and_then(Value::as_str) {
        Some(method) => method,
        None => {
            // Ответ клиента на наш запрос: сервер запросов не шлёт, поэтому игнорируем.
            if message.get("result").is_some() || message.get("error").is_some() {
                return None;
            }
            return Response::Err(INVALID_REQUEST, "method is missing".to_string()).into_json(id);
        }
    };

    let params = message.get("params").cloned().unwrap_or(Value::Null);

    // Уведомления не получают ответа, даже если метод неизвестен.
    let response = match method {
        "initialize" => Response::Ok(handler.initialize()),
        "ping" => Response::Ok(handler.ping()),
        "tools/list" => Response::Ok(handler.list_tools()),
        "notifications/initialized" | "notifications/cancelled" | "initialized" => Response::None,
        "tools/call" => {
            let name = match params.get("name").and_then(Value::as_str) {
                Some(name) => name.to_string(),
                None => {
                    return Response::Err(INVALID_PARAMS, "params.name is missing".to_string())
                        .into_json(id);
                }
            };
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            Response::Ok(handler.call_tool(&name, &arguments))
        }
        other => {
            if is_notification {
                Response::None
            } else {
                Response::Err(METHOD_NOT_FOUND, format!("unknown method {other}"))
            }
        }
    };

    if is_notification {
        return None;
    }
    response.into_json(id)
}

/// Читает stdin построчно и пишет ответы в stdout. Возвращает число обработанных сообщений.
pub fn serve<H: Handler>(handler: &mut H) -> std::io::Result<usize> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut handled = 0usize;
    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        handled += 1;
        if let Some(response) = dispatch(handler, trimmed) {
            let text = serde_json::to_string(&response)?;
            writeln!(stdout, "{text}")?;
            stdout.flush()?;
        }
    }
    Ok(handled)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    impl Handler for Echo {
        fn initialize(&mut self) -> Value {
            json!({"protocolVersion": PROTOCOL_VERSION, "capabilities": {"tools": {}}})
        }
        fn list_tools(&mut self) -> Value {
            json!({"tools": []})
        }
        fn call_tool(&mut self, name: &str, arguments: &Value) -> Value {
            json!({"content": [{"type": "text", "text": format!("{name}:{arguments}")}]})
        }
    }

    fn run(line: &str) -> Option<Value> {
        dispatch(&mut Echo, line)
    }

    #[test]
    fn initialize_answers_with_the_protocol_version() {
        let response =
            run(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).expect("response");
        assert_eq!(response["id"], json!(1));
        assert_eq!(
            response["result"]["protocolVersion"],
            json!(PROTOCOL_VERSION)
        );
    }

    #[test]
    fn notification_without_id_gets_no_response() {
        let response = run(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert!(response.is_none());
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let response = run(r#"{"jsonrpc":"2.0","id":7,"method":"nope/method"}"#).expect("response");
        assert_eq!(response["error"]["code"], json!(METHOD_NOT_FOUND));
        assert_eq!(response["id"], json!(7));
    }

    #[test]
    fn broken_json_is_a_parse_error() {
        let response = run("{not json").expect("response");
        assert_eq!(response["error"]["code"], json!(PARSE_ERROR));
        assert!(response["id"].is_null());
    }

    #[test]
    fn tools_call_without_name_is_invalid_params() {
        let response =
            run(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{}}"#).expect("response");
        assert_eq!(response["error"]["code"], json!(INVALID_PARAMS));
    }

    #[test]
    fn tools_call_reaches_the_handler() {
        let response = run(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"x","arguments":{"a":1}}}"#,
        )
        .expect("response");
        assert_eq!(
            response["result"]["content"][0]["text"],
            json!("x:{\"a\":1}")
        );
    }
}
