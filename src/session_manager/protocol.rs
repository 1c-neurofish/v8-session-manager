//! Wire‑протокол менеджера клиентских сессий.
//!
//! JSON‑RPC 2.0 поверх WebSocket. Соответствует
//! `spec/protocol/messages.schema.json` (этап 1).
//!
//! Каждый WS Text Frame — один JSON‑объект. Поток сообщений двунаправленный:
//! клиент шлёт `session.register`/`session.heartbeat`/`session.bye`/
//! `session.tools_changed`, менеджер шлёт `ping` (опционально, в дополнение
//! к WS ping/pong).
//!
//! Тонкости сериализации:
//!
//! * `Request` имеет `id`, `Notification` не имеет `id`, `Response` имеет
//!   `id` и одно из {`result`, `error`}. Поскольку `serde(untagged)` плохо
//!   различает эти три формы, [`WireMessage`] сериализуется через
//!   `serde_json::Value`‑диспетчер.
//! * Поле `jsonrpc` всегда `"2.0"`. На сериализации проставляется автоматически;
//!   на десериализации проверяется явно.
//!
//! Часть типов (params/result для отдельных методов) определена для
//! использования модулями registry/transport в следующих подзадачах
//! этапа 1; пока они не подключены — `dead_code` глушится локально.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

/// Имена методов JSON‑RPC.
///
/// `session.*` — отправляются клиентом менеджеру (этап 1).
/// `tool.*` — отправляются менеджером клиенту (этап 2, ADR‑0023).
/// `ping` — двусторонний.
#[allow(dead_code)]
pub mod methods {
    pub const SESSION_REGISTER: &str = "session.register";
    pub const SESSION_HEARTBEAT: &str = "session.heartbeat";
    pub const SESSION_BYE: &str = "session.bye";
    pub const SESSION_SHUTDOWN: &str = "session.shutdown";
    pub const SESSION_TOOLS_CHANGED: &str = "session.tools_changed";
    pub const TOOL_CALL: &str = "tool.call";
    pub const TOOL_CANCEL: &str = "tool.cancel";
    pub const PING: &str = "ping";
}

/// Коды ошибок протокола (см. `spec/SESSION_MANAGER.md` §4.6).
#[allow(dead_code)]
pub mod error_codes {
    pub const SESSION_UID_COLLISION: i64 = -32010;
    pub const SESSION_GONE: i64 = -32011;
    pub const CLIENT_TIMEOUT: i64 = -32012;
    pub const SPAWN_TIMEOUT: i64 = -32013;
    pub const KIND_MISMATCH: i64 = -32014;
    pub const TOOL_CANCELLED: i64 = -32800;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    pub const PARSE_ERROR: i64 = -32700;
}

/// JSON‑RPC id. Опаков для получателя.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    String(String),
    Number(i64),
    Null,
}

/// Структурированная ошибка JSON‑RPC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

/// Любое сообщение wire‑протокола.
#[derive(Debug, Clone, PartialEq)]
pub enum WireMessage {
    Request {
        id: Id,
        method: String,
        params: Value,
    },
    Response {
        id: Id,
        result: Result<Value, JsonRpcError>,
    },
    Notification {
        method: String,
        params: Value,
    },
}

/// Ошибка разбора входящего JSON.
#[derive(Debug, Error)]
pub enum WireParseError {
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("missing or invalid 'jsonrpc' field; expected \"2.0\"")]
    JsonRpcVersion,
    #[error("message is neither request, response, nor notification")]
    Shape,
    #[error("response carries both 'result' and 'error'")]
    AmbiguousResponse,
}

impl WireMessage {
    /// Разбирает один WS‑text‑frame.
    pub fn parse(text: &str) -> Result<Self, WireParseError> {
        let value: Value = serde_json::from_str(text)?;
        Self::from_value(value)
    }

    /// Сериализует сообщение в строку (для отправки в WS Text Frame).
    pub fn to_text(&self) -> String {
        serde_json::to_string(&self.to_value()).expect("WireMessage is always serializable")
    }

    fn from_value(value: Value) -> Result<Self, WireParseError> {
        let obj = match value {
            Value::Object(map) => map,
            _ => return Err(WireParseError::Shape),
        };

        match obj.get("jsonrpc").and_then(Value::as_str) {
            Some("2.0") => {}
            _ => return Err(WireParseError::JsonRpcVersion),
        }

        let id = obj.get("id").cloned();
        let method = obj.get("method").and_then(Value::as_str).map(str::to_owned);
        let has_result = obj.contains_key("result");
        let has_error = obj.contains_key("error");

        match (id, method, has_result, has_error) {
            (Some(id_value), Some(method), false, false) => Ok(WireMessage::Request {
                id: id_from_value(id_value)?,
                method,
                params: obj.get("params").cloned().unwrap_or(Value::Null),
            }),
            (None, Some(method), false, false) => Ok(WireMessage::Notification {
                method,
                params: obj.get("params").cloned().unwrap_or(Value::Null),
            }),
            (Some(id_value), None, true, false) => Ok(WireMessage::Response {
                id: id_from_value(id_value)?,
                result: Ok(obj.get("result").cloned().unwrap_or(Value::Null)),
            }),
            (Some(id_value), None, false, true) => {
                let err: JsonRpcError = serde_json::from_value(
                    obj.get("error").cloned().ok_or(WireParseError::Shape)?,
                )?;
                Ok(WireMessage::Response {
                    id: id_from_value(id_value)?,
                    result: Err(err),
                })
            }
            (_, _, true, true) => Err(WireParseError::AmbiguousResponse),
            _ => Err(WireParseError::Shape),
        }
    }

    fn to_value(&self) -> Value {
        match self {
            WireMessage::Request { id, method, params } => json!({
                "jsonrpc": "2.0",
                "id": id_to_value(id),
                "method": method,
                "params": params,
            }),
            WireMessage::Notification { method, params } => json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }),
            WireMessage::Response { id, result } => match result {
                Ok(value) => json!({
                    "jsonrpc": "2.0",
                    "id": id_to_value(id),
                    "result": value,
                }),
                Err(err) => json!({
                    "jsonrpc": "2.0",
                    "id": id_to_value(id),
                    "error": err,
                }),
            },
        }
    }
}

fn id_from_value(value: Value) -> Result<Id, WireParseError> {
    match value {
        Value::Null => Ok(Id::Null),
        Value::String(s) => Ok(Id::String(s)),
        Value::Number(n) => n.as_i64().map(Id::Number).ok_or(WireParseError::Shape),
        _ => Err(WireParseError::Shape),
    }
}

fn id_to_value(id: &Id) -> Value {
    match id {
        Id::Null => Value::Null,
        Id::String(s) => Value::String(s.clone()),
        Id::Number(n) => Value::Number((*n).into()),
    }
}

// ---- Per-method params and results -----------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDescriptor {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionRegisterParams {
    pub client_uid: String,
    pub kind: String,
    pub version: String,
    pub tools: Vec<ToolDescriptor>,
    /// Идентификатор хоста клиента (ADR‑0029). Опционален для legacy-клиентов.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// PID процесса клиента (ADR‑0029). Опционален для legacy-клиентов.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extras: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionRegisterResult {
    pub session_id: String,
    pub server_version: String,
    pub heartbeat_interval_ms: u64,
    pub idle_timeout_secs: u64,
    #[serde(default, skip_serializing_if = "is_false_default")]
    pub reconnected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HeartbeatPayload {
    pub ts: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionByeParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EmptyResult {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionToolsChangedParams {
    pub tools: Vec<ToolDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<Value>>,
}

fn is_false_default(value: &bool) -> bool {
    !*value
}

// ---- manager → client: tool.call / tool.cancel (ADR‑0023) ------------------

/// Параметры outbound‑запроса `tool.call` (manager → client).
///
/// `arguments` — произвольный объект, валидация на стороне клиента согласно
/// `input_schema` соответствующего tool из `session.register`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallParams {
    pub name: String,
    pub arguments: Value,
}

/// Один блок content в ответе tool. Совпадает по shape с MCP CallToolResult
/// content — упрощает прокидывание ответа клиента в MCP-ответ AI‑агенту.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    Text { text: String },
    Json { json: Value },
}

/// Результат `tool.call` от клиента менеджеру.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ToolCallResult {
    /// Список content‑блоков (минимум один). Соответствует MCP `content`.
    #[serde(default)]
    pub content: Vec<ToolContent>,
    /// Терминальная ошибка инструмента (бизнес‑семантика). Транспортные
    /// ошибки идут как JSON‑RPC `error` (вне этого типа).
    #[serde(default, skip_serializing_if = "is_false_default")]
    pub is_error: bool,
    /// Машиночитаемое представление результата (для AI‑агента и MCP `structuredContent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
}

/// Параметры `tool.cancel` (manager → client) — отмена ранее посланного
/// `tool.call` по его `id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCancelParams {
    /// `id` из соответствующего `tool.call`-запроса.
    pub id: Id,
    /// Опциональная человекочитаемая причина (для логов/диагностики).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Параметры `session.shutdown` (manager → client) — приглашение клиента
/// корректно завершиться. Клиент должен ответить `session.bye` и закрыть WS.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct SessionShutdownParams {
    /// Опциональная причина: `idle_timeout`, `force_kill`, `manager_shutdown`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Сколько менеджер готов ждать `session.bye` перед `child.kill()` (мс).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grace_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_round_trip() {
        let text = r#"{"jsonrpc":"2.0","id":"1","method":"session.heartbeat","params":{"ts":42}}"#;
        let parsed = WireMessage::parse(text).expect("parse");
        match &parsed {
            WireMessage::Request { id, method, params } => {
                assert_eq!(*id, Id::String("1".to_owned()));
                assert_eq!(method, methods::SESSION_HEARTBEAT);
                let payload: HeartbeatPayload =
                    serde_json::from_value(params.clone()).expect("heartbeat params");
                assert_eq!(payload.ts, 42);
            }
            _ => panic!("expected request, got {parsed:?}"),
        }
        let re = WireMessage::parse(&parsed.to_text()).expect("round trip");
        assert_eq!(re, parsed);
    }

    #[test]
    fn parse_notification_without_id() {
        let text = r#"{"jsonrpc":"2.0","method":"session.tools_changed","params":{"tools":[]}}"#;
        let parsed = WireMessage::parse(text).expect("parse");
        match &parsed {
            WireMessage::Notification { method, .. } => {
                assert_eq!(method, methods::SESSION_TOOLS_CHANGED);
            }
            _ => panic!("expected notification"),
        }
        let re = WireMessage::parse(&parsed.to_text()).expect("round trip");
        assert_eq!(re, parsed);
    }

    #[test]
    fn parse_success_response() {
        let text = r#"{"jsonrpc":"2.0","id":7,"result":{"ts":99}}"#;
        let parsed = WireMessage::parse(text).expect("parse");
        match &parsed {
            WireMessage::Response { id, result } => {
                assert_eq!(*id, Id::Number(7));
                let value = result.as_ref().expect("ok response");
                assert_eq!(value["ts"], 99);
            }
            _ => panic!("expected response"),
        }
    }

    #[test]
    fn parse_error_response() {
        let text = r#"{"jsonrpc":"2.0","id":"x","error":{"code":-32010,"message":"session_uid_collision"}}"#;
        let parsed = WireMessage::parse(text).expect("parse");
        match &parsed {
            WireMessage::Response { result, .. } => {
                let err = result.as_ref().expect_err("error response");
                assert_eq!(err.code, error_codes::SESSION_UID_COLLISION);
            }
            _ => panic!("expected response"),
        }
    }

    #[test]
    fn rejects_missing_jsonrpc_version() {
        let text = r#"{"id":1,"method":"ping"}"#;
        let err = WireMessage::parse(text).expect_err("must reject");
        assert!(matches!(err, WireParseError::JsonRpcVersion));
    }

    #[test]
    fn rejects_response_with_both_result_and_error() {
        let text = r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-1,"message":"x"}}"#;
        let err = WireMessage::parse(text).expect_err("must reject");
        assert!(matches!(err, WireParseError::AmbiguousResponse));
    }

    #[test]
    fn round_trip_tool_call_params() {
        let p = ToolCallParams {
            name: "echo".to_owned(),
            arguments: json!({"x": 1, "msg": "hi"}),
        };
        let v = serde_json::to_value(&p).unwrap();
        let back: ToolCallParams = serde_json::from_value(v).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn round_trip_tool_call_result_with_text_content() {
        let r = ToolCallResult {
            content: vec![ToolContent::Text {
                text: "ok".to_owned(),
            }],
            is_error: false,
            structured_content: Some(json!({"value": 42})),
        };
        let v = serde_json::to_value(&r).unwrap();
        // is_error is false → должно быть пропущено в сериализации.
        assert!(v.get("is_error").is_none());
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "ok");
        let back: ToolCallResult = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn round_trip_tool_cancel_params() {
        let p = ToolCancelParams {
            id: Id::String("mgr-7".to_owned()),
            reason: Some("user aborted".to_owned()),
        };
        let v = serde_json::to_value(&p).unwrap();
        let back: ToolCancelParams = serde_json::from_value(v).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn round_trip_session_shutdown_params() {
        let p = SessionShutdownParams {
            reason: Some("idle_timeout".to_owned()),
            grace_ms: Some(5_000),
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["reason"], "idle_timeout");
        assert_eq!(v["grace_ms"], 5_000);
        let back: SessionShutdownParams = serde_json::from_value(v).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn shutdown_params_skip_serializing_when_empty() {
        let p = SessionShutdownParams::default();
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v.as_object().expect("object").len(), 0);
    }

    #[test]
    fn round_trip_register_request() {
        let params = SessionRegisterParams {
            client_uid: "uid-1".to_owned(),
            kind: "client".to_owned(),
            version: "1.0".to_owned(),
            tools: vec![ToolDescriptor {
                name: "echo".to_owned(),
                description: None,
                input_schema: json!({ "type": "object" }),
            }],
            host_id: None,
            pid: None,
            resources: None,
            prompts: None,
            extras: None,
        };
        let request = WireMessage::Request {
            id: Id::String("r1".to_owned()),
            method: methods::SESSION_REGISTER.to_owned(),
            params: serde_json::to_value(&params).unwrap(),
        };
        let text = request.to_text();
        let re = WireMessage::parse(&text).unwrap();
        assert_eq!(re, request);
    }
}
