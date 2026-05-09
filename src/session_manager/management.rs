//! MCP management API менеджера сессий.
//!
//! На этапе 1 — единственный tool `session.list` (см. acceptance #1
//! `spec/IMPLEMENTATION_BACKLOG_SESSION_MANAGER.md` этап 1).
//!
//! Модуль умышленно отделён от `transport.rs` и от `mcp::server`: данные
//! берутся через [`SessionRegistry`], в [`McpToolServer`](crate::mcp::McpToolServer)
//! tool оборачивает [`list`] в `#[tool]`-метод.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::session_manager::protocol::ToolDescriptor;
use crate::session_manager::registry::{SessionRegistry, SessionState};

/// Запрос `session.list` (этап 1: пустой; на этапе 2+ добавятся фильтры по kind/state).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct SessionListRequest {}

/// Сериализуемое состояние записи реестра.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStateView {
    Active,
    Disconnected,
}

impl From<SessionState> for SessionStateView {
    fn from(value: SessionState) -> Self {
        match value {
            SessionState::Active => SessionStateView::Active,
            SessionState::Disconnected => SessionStateView::Disconnected,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListItem {
    /// Сквозной UID сессии (`session_id` ≡ `client_uid`). Стабилен при
    /// soft-reconnect (см. ADR-0022). Используется внешним оркестратором
    /// (например, v8-runner) как идентификатор сессии.
    pub session_id: String,
    pub kind: String,
    pub version: String,
    /// Имя информационной базы 1С (naming v2 от 2026-05-09).
    pub infobase_name: String,
    /// Внутренний номер сеанса 1С (`НомерСеанса()` платформы).
    pub ib_session_number: u32,
    /// Идентификатор хоста, на котором живёт процесс клиента (ADR‑0029).
    /// Fallback `"unknown"` для legacy-клиентов без `host_id`.
    pub host_id: String,
    pub state: SessionStateView,
    pub tools: Vec<ToolDescriptor>,
    pub inflight: u32,
    /// Сколько секунд назад сессия зарегистрирована (моно‑часы; `0` если только что).
    pub registered_secs_ago: u64,
    /// Сколько секунд провела в `Disconnected` к моменту снимка (`null` если `Active`).
    pub disconnected_secs_ago: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListResult {
    pub sessions: Vec<SessionListItem>,
}

/// Возвращает снимок реестра, готовый к выдаче AI‑агенту через MCP.
pub fn list(registry: &Arc<SessionRegistry>) -> SessionListResult {
    let now = Instant::now();
    let mut sessions: Vec<SessionListItem> = registry
        .snapshot()
        .into_iter()
        .map(|rec| SessionListItem {
            registered_secs_ago: secs_since(now, rec.registered_at),
            disconnected_secs_ago: rec.disconnected_at.map(|t| secs_since(now, t)),
            // Источник правды по inflight — per-session dispatcher.
            // Поле SessionRecord::inflight исторически не апдейтилось, поэтому
            // отдано dispatcher'у целиком (см. WARN-5 «stale inflight»).
            inflight: rec.dispatcher.inflight(),
            session_id: rec.session_id,
            kind: rec.kind,
            version: rec.version,
            infobase_name: rec.infobase_name,
            ib_session_number: rec.ib_session_number,
            host_id: rec.host_id,
            state: rec.state.into(),
            tools: rec.tools,
        })
        .collect();
    sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    SessionListResult { sessions }
}

fn secs_since(now: Instant, then: Instant) -> u64 {
    now.checked_duration_since(then)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_manager::protocol::SessionRegisterParams;
    use serde_json::json;

    fn params(uid: &str, kind: &str, tool: &str) -> SessionRegisterParams {
        SessionRegisterParams {
            client_uid: uid.to_owned(),
            kind: kind.to_owned(),
            version: "1.0".to_owned(),
            infobase_name: "test_db".to_owned(),
            ib_session_number: 1,
            tools: vec![ToolDescriptor {
                name: tool.to_owned(),
                description: None,
                input_schema: json!({}),
            }],
            host_id: None,
            pid: None,
            resources: None,
            prompts: None,
            extras: None,
        }
    }

    #[test]
    fn list_returns_sorted_active_sessions() {
        let reg = Arc::new(SessionRegistry::new());
        let now = Instant::now();
        reg.register(params("uid-b", "client", "x"), now, None)
            .unwrap();
        reg.register(params("uid-a", "yaxunit_runner", "y"), now, None)
            .unwrap();

        let result = list(&reg);
        assert_eq!(result.sessions.len(), 2);
        assert_eq!(result.sessions[0].session_id, "uid-a");
        assert_eq!(result.sessions[1].session_id, "uid-b");
        assert!(result
            .sessions
            .iter()
            .all(|s| s.state == SessionStateView::Active));
        assert!(result
            .sessions
            .iter()
            .all(|s| s.disconnected_secs_ago.is_none()));
    }

    #[test]
    fn list_reflects_disconnected_state() {
        let reg = Arc::new(SessionRegistry::new());
        let t0 = Instant::now();
        reg.register(params("uid-d", "client", "x"), t0, None)
            .unwrap();
        reg.mark_disconnected("uid-d", t0);

        let result = list(&reg);
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].state, SessionStateView::Disconnected);
        // `Some(_)` — сам факт наличия таймстампа важнее точного значения.
        assert!(result.sessions[0].disconnected_secs_ago.is_some());
    }

    /// Acceptance #1 backlog'а этапа 1: регистрация через WS → видна в `session.list`.
    #[tokio::test]
    async fn acceptance_register_visible_in_session_list() {
        use crate::config::model::McpSessionManagerConfig;
        use crate::session_manager::transport;
        use futures_util::{SinkExt, StreamExt};
        use serde_json::json;
        use tokio_tungstenite::connect_async;
        use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

        let registry = Arc::new(SessionRegistry::new());
        let config = McpSessionManagerConfig {
            bind_address: "127.0.0.1:0".to_owned(),
            path: "/sessions".to_owned(),
            heartbeat_interval_ms: 15000,
            idle_timeout_secs: 1800,
            reconnection_grace_secs: 30,
            graceful_kill_grace_ms: 5_000,
            ws_ping_interval_ms: 0,
            ws_ping_timeout_ms: 0,
        };
        let running = transport::start(Arc::clone(&registry), config, "v0-test")
            .await
            .unwrap();
        let url = format!("ws://{}/sessions", running.local_addr);

        let (mut ws, _) = connect_async(&url).await.unwrap();
        let req = json!({
            "jsonrpc": "2.0",
            "id": "r1",
            "method": "session.register",
            "params": {
                "client_uid": "A",
                "kind": "client",
                "version": "1.0",
                "infobase_name": "test_db",
                "ib_session_number": 1,
                "tools": [{"name": "echo", "input_schema": {"type": "object"}}]
            }
        });
        ws.send(WsMessage::Text(req.to_string().into()))
            .await
            .unwrap();
        let _ = ws.next().await; // ack

        // session.list видит запись A
        let result = list(&registry);
        assert_eq!(result.sessions.len(), 1);
        let item = &result.sessions[0];
        assert_eq!(item.session_id, "A");
        assert_eq!(item.kind, "client");
        assert_eq!(item.state, SessionStateView::Active);
        assert_eq!(item.tools.len(), 1);
        assert_eq!(item.tools[0].name, "echo");
        assert_eq!(item.inflight, 0);

        ws.close(None).await.ok();
        running.shutdown();
    }

    #[test]
    fn list_serializes_to_json() {
        let reg = Arc::new(SessionRegistry::new());
        reg.register(params("uid-1", "client", "echo"), Instant::now(), None)
            .unwrap();
        let result = list(&reg);
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["sessions"][0]["session_id"], "uid-1");
        assert_eq!(json["sessions"][0]["state"], "active");
        assert_eq!(json["sessions"][0]["tools"][0]["name"], "echo");
    }

    #[test]
    fn session_list_exposes_infobase_and_ib_session_number() {
        // Naming v2: list(...) пробрасывает infobase_name, ib_session_number,
        // host_id из SessionRecord.
        let reg = Arc::new(SessionRegistry::new());
        let mut p = params("uid-1", "client", "echo");
        p.infobase_name = "drive_dev".to_owned();
        p.ib_session_number = 17;
        p.host_id = Some("box-42".to_owned());
        reg.register(p, Instant::now(), None).unwrap();

        let result = list(&reg);
        let item = &result.sessions[0];
        assert_eq!(item.session_id, "uid-1");
        assert_eq!(item.infobase_name, "drive_dev");
        assert_eq!(item.ib_session_number, 17);
        assert_eq!(item.host_id, "box-42");

        // host_id fallback "unknown", когда клиент не передал поле.
        let reg2 = Arc::new(SessionRegistry::new());
        reg2.register(params("uid-2", "client", "echo"), Instant::now(), None)
            .unwrap();
        let result2 = list(&reg2);
        assert_eq!(result2.sessions[0].host_id, "unknown");
    }
}
