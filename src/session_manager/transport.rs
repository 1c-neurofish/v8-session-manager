//! WebSocket-транспорт менеджера сессий.
//!
//! Поднимает axum-роутер с одним WS endpoint'ом (`config.path`).
//! На каждом соединении — задача, читающая JSON-RPC сообщения, и пишущая
//! ответы. Регистрация — обязательное первое действие клиента; до неё
//! принимаются только `ping` и `session.register`.
//!
//! На drop соединения (любая ошибка / WS close без `session.bye`) задача
//! помечает сессию `Disconnected` (если она была зарегистрирована); удаление
//! по grace выполняет фоновый sweeper.
//!
//! Control-plane: `session.register` / `session.bye` / `session.tools_changed`
//! от клиента; `tool.call` / `tool.cancel` / `session.shutdown` от менеджера.
//! Liveness канала держится на WS protocol-level Ping/Pong (RFC 6455) из
//! writer-task — application-level heartbeat не используется.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::model::McpSessionManagerConfig;
use crate::session_manager::connection::ConnectionHandle;
use crate::session_manager::protocol::{
    error_codes, methods, EmptyResult, Id, JsonRpcError, SessionRegisterParams,
    SessionRegisterResult, SessionToolsChangedParams, WireMessage,
};
use crate::session_manager::registry::{RegisterError, RegisterOutcome, SessionRegistry};

/// Запущенный транспорт: JoinHandle всей задачи и фактический адрес.
pub struct RunningTransport {
    pub local_addr: SocketAddr,
    pub server_handle: JoinHandle<()>,
    pub sweeper_handle: JoinHandle<()>,
}

impl RunningTransport {
    /// Останавливает фоновые задачи (abort).
    pub fn shutdown(self) {
        self.server_handle.abort();
        self.sweeper_handle.abort();
    }
}

#[derive(Clone)]
struct AppState {
    registry: Arc<SessionRegistry>,
    config: Arc<McpSessionManagerConfig>,
    server_version: Arc<String>,
}

/// Поднимает WS-сервер на `config.bind_address` + `config.path` и фоновый sweeper.
///
/// Возвращает `RunningTransport` с фактическим `local_addr` (полезно для
/// интеграционных тестов с `bind_address: "127.0.0.1:0"`).
pub async fn start(
    registry: Arc<SessionRegistry>,
    config: McpSessionManagerConfig,
    server_version: impl Into<String>,
) -> std::io::Result<RunningTransport> {
    let listener = TcpListener::bind(&config.bind_address).await?;
    let local_addr = listener.local_addr()?;

    let state = AppState {
        registry: Arc::clone(&registry),
        config: Arc::new(config.clone()),
        server_version: Arc::new(server_version.into()),
    };

    let app = Router::new()
        .route(&config.path, any(ws_handler))
        .with_state(state);

    info!(
        addr = %local_addr,
        path = %config.path,
        "session-manager WS transport listening"
    );

    let server_handle = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            error!(?err, "session-manager WS server stopped with error");
        }
    });

    let sweeper_registry = Arc::clone(&registry);
    let grace = Duration::from_secs(config.reconnection_grace_secs);
    let sweep_interval = Duration::from_secs(grace.as_secs().max(1).min(30));
    let sweeper_handle = tokio::spawn(async move {
        run_grace_sweeper(sweeper_registry, grace, sweep_interval).await;
    });

    Ok(RunningTransport {
        local_addr,
        server_handle,
        sweeper_handle,
    })
}

async fn run_grace_sweeper(registry: Arc<SessionRegistry>, grace: Duration, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let expired = registry.sweep_expired_disconnects(Instant::now(), grace);
        if !expired.is_empty() {
            info!(
                count = expired.len(),
                ?expired,
                "session-manager: removed sessions after grace timeout"
            );
        }
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let peer = ConnectionContext::default();
    debug!("session-manager: WS connection accepted (handshake completed)");
    let result = run_connection(socket, state.clone(), peer.clone()).await;
    debug!(session_id = ?peer.session_id(), ok = result.is_ok(), "session-manager: WS connection loop exited");
    if let Err(err) = result {
        warn!(?err, session_id = ?peer.session_id(), "session connection ended with error");
    }
    if let Some(ident) = peer.identity() {
        // BLOCKER-2: generation-aware mark_disconnected. Если за время WS
        // round-trip случился soft reconnect (новое поколение), наш «зомби»
        // disconnect — no-op, свежая Active-запись остаётся.
        let marked = state.registry.mark_disconnected_if_generation(
            &ident.session_id,
            ident.generation,
            Instant::now(),
        );
        if marked {
            info!(session_id = %ident.session_id, "session-manager: marked session disconnected");
        } else {
            debug!(
                session_id = %ident.session_id,
                generation = ident.generation,
                "session-manager: stale generation on disconnect — ignored"
            );
        }
    }
}

#[derive(Clone, Default)]
struct ConnectionContext {
    inner: Arc<std::sync::Mutex<Option<ConnectionIdentity>>>,
}

#[derive(Clone, Debug)]
struct ConnectionIdentity {
    session_id: String,
    /// `connection_generation` записи на момент register/reconnect этого peer'а.
    /// Используется в transport для generation-aware mark_disconnected/remove
    /// (BLOCKER-2): «зомби» WS-task старого коннекта не должен убивать
    /// свежезареконнектившуюся запись.
    generation: u64,
}

impl ConnectionContext {
    fn set(&self, id: String, generation: u64) {
        *self.inner.lock().expect("ctx poisoned") = Some(ConnectionIdentity {
            session_id: id,
            generation,
        });
    }
    fn session_id(&self) -> Option<String> {
        self.inner
            .lock()
            .expect("ctx poisoned")
            .as_ref()
            .map(|ident| ident.session_id.clone())
    }
    fn identity(&self) -> Option<ConnectionIdentity> {
        self.inner.lock().expect("ctx poisoned").clone()
    }
}

async fn run_connection(
    socket: WebSocket,
    state: AppState,
    peer: ConnectionContext,
) -> Result<(), ConnectionError> {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<WireMessage>();
    let connection = Arc::new(ConnectionHandle::new(tx.clone()));

    // Liveness канала: WS protocol-level Ping (RFC 6455 opcode 0x9).
    // Менеджер шлёт Ping каждые `ws_ping_interval_ms`; tokio-tungstenite
    // на стороне клиента отвечает Pong автоматически без участия BSL —
    // это намеренно, чтобы открытый модальный диалог или длинный BSL-
    // обработчик не выглядели для менеджера как «зависание».
    //
    // Если за `ws_ping_timeout_ms` от клиента не пришло ни одного фрейма
    // (Pong или Text), writer-task закрывает sink. CancellationToken
    // сигналит reader-loop'у не ждать следующий фрейм, чтобы не зависнуть
    // на полузакрытом сокете (peer не прислал FIN).
    let last_inbound_at = Arc::new(std::sync::Mutex::new(Instant::now()));
    let cancel = CancellationToken::new();

    let ping_interval = Duration::from_millis(state.config.ws_ping_interval_ms);
    let ping_timeout = Duration::from_millis(state.config.ws_ping_timeout_ms);
    let last_inbound_for_writer = Arc::clone(&last_inbound_at);
    let cancel_for_writer = cancel.clone();

    let writer = tokio::spawn(async move {
        let mut ticker = if ping_interval.is_zero() {
            None
        } else {
            let mut t = tokio::time::interval(ping_interval);
            // Skip первый immediate tick — иначе шлём Ping ещё до того, как
            // клиент успел зарегистрироваться.
            t.tick().await;
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            Some(t)
        };

        loop {
            if let Some(ref mut t) = ticker {
                tokio::select! {
                    biased;
                    msg = rx.recv() => {
                        match msg {
                            Some(m) => {
                                if let Err(err) = sink
                                    .send(Message::Text(m.to_text().into()))
                                    .await
                                {
                                    debug!(?err, "ws sink send failed; closing writer");
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    _ = t.tick() => {
                        if !ping_timeout.is_zero() {
                            let elapsed = last_inbound_for_writer
                                .lock()
                                .expect("last_inbound poisoned")
                                .elapsed();
                            if elapsed > ping_timeout {
                                info!(
                                    elapsed_ms = elapsed.as_millis() as u64,
                                    timeout_ms = ping_timeout.as_millis() as u64,
                                    "ws keep-alive timeout — no inbound frames; closing connection",
                                );
                                cancel_for_writer.cancel();
                                break;
                            }
                        }
                        if let Err(err) = sink.send(Message::Ping(Vec::new().into())).await {
                            debug!(?err, "ws ping send failed; closing writer");
                            break;
                        }
                    }
                    _ = cancel_for_writer.cancelled() => break,
                }
            } else {
                tokio::select! {
                    biased;
                    msg = rx.recv() => {
                        match msg {
                            Some(m) => {
                                if let Err(err) = sink
                                    .send(Message::Text(m.to_text().into()))
                                    .await
                                {
                                    debug!(?err, "ws sink send failed; closing writer");
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    _ = cancel_for_writer.cancelled() => break,
                }
            }
        }
        let _ = sink.close().await;
    });

    let result = loop {
        let frame = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                debug!("ws reader cancelled by writer (keep-alive timeout)");
                break Err(ConnectionError::Ws("ws keep-alive timeout".to_owned()));
            }
            f = stream.next() => f,
        };

        let msg = match frame {
            Some(Ok(m)) => {
                debug!(kind = ?std::mem::discriminant(&m), "ws frame received");
                *last_inbound_at.lock().expect("last_inbound poisoned") = Instant::now();
                m
            }
            Some(Err(err)) => {
                debug!(?err, "ws stream returned error; breaking loop");
                break Err(ConnectionError::Ws(err.to_string()));
            }
            None => {
                debug!("ws stream returned None (peer closed); breaking loop");
                break Ok(());
            }
        };

        match msg {
            Message::Text(text) => {
                debug!(text_len = text.len(), preview = %text.chars().take(160).collect::<String>(), "ws text frame");
                match WireMessage::parse(&text) {
                    Ok(wire) => {
                        if let Err(err) = dispatch(&state, &peer, &connection, wire, &tx).await {
                            break Err(err);
                        }
                    }
                    Err(parse_err) => {
                        let err = JsonRpcError::new(
                            error_codes::PARSE_ERROR,
                            format!("parse error: {parse_err}"),
                        );
                        let _ = tx.send(WireMessage::Response {
                            id: Id::Null,
                            result: Err(err),
                        });
                    }
                }
            }
            Message::Binary(_) => {
                // Этап 1 не использует бинарные фреймы.
                debug!("session-manager: ignoring binary frame");
            }
            Message::Ping(payload) => {
                // axum ответит pong сам.
                debug!(payload_len = payload.len(), "ws ping received");
            }
            Message::Pong(_) => {
                debug!("ws pong received");
            }
            Message::Close(_) => break Ok(()),
        }
    };
    cancel.cancel();

    // Закрываем outbound‑канал: writer выйдет, когда последний sender дропнут.
    // drain_pending обнуляет sender внутри ConnectionHandle (важно — он живёт
    // в registry, локального drop(tx) недостаточно). Идемпотентно с
    // последующим mark_disconnected в handle_socket.
    connection.drain_pending();
    drop(tx);
    let _ = writer.await;
    result
}

#[derive(Debug, thiserror::Error)]
enum ConnectionError {
    #[error("ws error: {0}")]
    Ws(String),
    #[error("session bye received")]
    Bye,
}

async fn dispatch(
    state: &AppState,
    peer: &ConnectionContext,
    connection: &Arc<ConnectionHandle>,
    msg: WireMessage,
    tx: &mpsc::UnboundedSender<WireMessage>,
) -> Result<(), ConnectionError> {
    match msg {
        WireMessage::Request { id, method, params } => {
            handle_request(state, peer, connection, id, method, params, tx).await
        }
        WireMessage::Notification { method, params } => {
            handle_notification(state, peer, method, params).await;
            Ok(())
        }
        WireMessage::Response { id, result } => {
            // ADR-0023: ответы от клиента маршрутизируются в pending‑таблицу outbound‑вызовов.
            if !connection.complete_response(id.clone(), result) {
                warn!(?id, "session-manager: response with unknown id; dropped");
            }
            Ok(())
        }
    }
}

async fn handle_request(
    state: &AppState,
    peer: &ConnectionContext,
    connection: &Arc<ConnectionHandle>,
    id: Id,
    method: String,
    params: serde_json::Value,
    tx: &mpsc::UnboundedSender<WireMessage>,
) -> Result<(), ConnectionError> {
    match method.as_str() {
        methods::SESSION_REGISTER => {
            // Один WS-коннект = одна сессия. Повторный session.register
            // на том же соединении — нарушение протокола: первый register
            // успел заспаунить ping-task и проставить identity, повторный
            // overwrite перевёл бы первую сессию в orphan-состояние
            // (ping-task на чужом ConnectionHandle, mark_disconnected при
            // обрыве WS затронет только последнюю identity).
            if peer.identity().is_some() {
                send_error(
                    tx,
                    id,
                    error_codes::INVALID_REQUEST,
                    "session already registered on this connection".to_owned(),
                );
                return Ok(());
            }
            let parsed: SessionRegisterParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(err) => {
                    send_error(tx, id, error_codes::INVALID_PARAMS, format!("{err}"));
                    return Ok(());
                }
            };
            let now = Instant::now();
            let client_uid = parsed.client_uid.clone();
            match state
                .registry
                .register(parsed, now, Some(Arc::clone(connection)))
            {
                Ok(outcome) => {
                    // BLOCKER-2: фиксируем поколение коннекта для этого peer'а,
                    // чтобы последующий disconnect/bye мог использовать
                    // generation-aware варианты mark_disconnected/remove.
                    let generation = state
                        .registry
                        .get(&client_uid)
                        .map(|r| r.connection_generation)
                        .unwrap_or(0);
                    peer.set(client_uid.clone(), generation);
                    let result = SessionRegisterResult {
                        session_id: client_uid.clone(),
                        server_version: state.server_version.as_ref().clone(),
                        heartbeat_interval_ms: state.config.heartbeat_interval_ms,
                        idle_timeout_secs: state.config.idle_timeout_secs,
                        reconnected: matches!(outcome, RegisterOutcome::Reconnected),
                    };
                    let value = serde_json::to_value(&result).expect("register result");
                    let _ = tx.send(WireMessage::Response {
                        id,
                        result: Ok(value),
                    });
                    // Liveness канала держит WS Ping/Pong в writer-task
                    // (см. `run_connection`). Application-level ping не
                    // используется: 1С может легитимно «не отвечать»
                    // на BSL-уровне (открытая модалка, длинный запрос).
                    let _ = (client_uid, generation);
                }
                Err(RegisterError::UidCollision(uid)) => {
                    send_error(
                        tx,
                        id,
                        error_codes::SESSION_UID_COLLISION,
                        format!("client_uid '{uid}' is already active"),
                    );
                }
            }
            Ok(())
        }
        methods::SESSION_BYE => {
            let identity = peer.identity();
            if let Some(ref ident) = identity {
                // BLOCKER-2: generation-aware remove. Если за время WS
                // round-trip случился soft reconnect (новое поколение),
                // bye старого peer'а не должно удалять свежую запись.
                let removed = state
                    .registry
                    .remove_if_generation(&ident.session_id, ident.generation);
                if removed.is_some() {
                    info!(session_id = %ident.session_id, "session-manager: session.bye received");
                } else {
                    debug!(
                        session_id = %ident.session_id,
                        generation = ident.generation,
                        "session-manager: session.bye on stale generation — ignored"
                    );
                }
            }
            let _ = tx.send(WireMessage::Response {
                id,
                result: Ok(serde_json::to_value(EmptyResult {}).expect("empty")),
            });
            // Очищаем context: на disconnect mark_disconnected по уже отсутствующей записи — no-op.
            *peer.inner.lock().expect("ctx poisoned") = None;
            Err(ConnectionError::Bye)
        }
        // Application-level `ping` handler удалён: liveness держится на
        // WS protocol-level Ping/Pong (см. `run_connection`). Если кто-то
        // всё-таки шлёт `ping` JSON-RPC — вернётся METHOD_NOT_FOUND.
        other => {
            send_error(
                tx,
                id,
                error_codes::METHOD_NOT_FOUND,
                format!("method '{other}' is not supported"),
            );
            Ok(())
        }
    }
}

async fn handle_notification(
    state: &AppState,
    peer: &ConnectionContext,
    method: String,
    params: serde_json::Value,
) {
    match method.as_str() {
        methods::SESSION_TOOLS_CHANGED => {
            let Some(session_id) = peer.session_id() else {
                warn!("session.tools_changed before session.register; ignored");
                return;
            };
            let parsed: SessionToolsChangedParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(err) => {
                    warn!(?err, "session.tools_changed: invalid params");
                    return;
                }
            };
            state.registry.update_tools(&session_id, parsed.tools);
        }
        other => {
            debug!(method = %other, "session-manager: unknown notification");
        }
    }
}

fn send_error(
    tx: &mpsc::UnboundedSender<WireMessage>,
    id: Id,
    code: i64,
    message: impl Into<String>,
) {
    let _ = tx.send(WireMessage::Response {
        id,
        result: Err(JsonRpcError::new(code, message)),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_manager::protocol::ToolDescriptor;
    use serde_json::{json, Value};
    use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

    fn test_config() -> McpSessionManagerConfig {
        McpSessionManagerConfig {
            bind_address: "127.0.0.1:0".to_owned(),
            path: "/sessions".to_owned(),
            heartbeat_interval_ms: 15000,
            idle_timeout_secs: 1800,
            reconnection_grace_secs: 1,
            graceful_kill_grace_ms: 5_000,
            // Тесты транспорта по-умолчанию не зависят от WS keep-alive.
            // Отдельные тесты переопределяют интервалы.
            ws_ping_interval_ms: 0,
            ws_ping_timeout_ms: 0,
        }
    }

    async fn boot() -> (Arc<SessionRegistry>, RunningTransport, String) {
        boot_with(test_config()).await
    }

    async fn boot_with(
        config: McpSessionManagerConfig,
    ) -> (Arc<SessionRegistry>, RunningTransport, String) {
        let registry = Arc::new(SessionRegistry::new());
        let running = start(Arc::clone(&registry), config, "test-1.0")
            .await
            .expect("start");
        let url = format!("ws://{}/sessions", running.local_addr);
        (registry, running, url)
    }

    async fn connect(
        url: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let (ws, _resp) = tokio_tungstenite::connect_async(url)
            .await
            .expect("connect");
        ws
    }

    fn register_text(uid: &str, kind: &str, tool: &str) -> String {
        let req = WireMessage::Request {
            id: Id::String("r1".to_owned()),
            method: methods::SESSION_REGISTER.to_owned(),
            params: serde_json::to_value(SessionRegisterParams {
                client_uid: uid.to_owned(),
                kind: kind.to_owned(),
                version: "1.0".to_owned(),
                infobase_name: "test_db".to_owned(),
                ib_session_number: 1,
                tools: vec![ToolDescriptor {
                    name: tool.to_owned(),
                    description: None,
                    input_schema: json!({"type": "object"}),
                }],
                config_id: None,
                host_id: None,
                pid: None,
                resources: None,
                prompts: None,
                extras: None,
            })
            .unwrap(),
        };
        req.to_text()
    }

    async fn next_text(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> String {
        loop {
            match ws.next().await {
                Some(Ok(WsMessage::Text(t))) => return t.to_string(),
                Some(Ok(WsMessage::Ping(_))) | Some(Ok(WsMessage::Pong(_))) => continue,
                Some(Ok(WsMessage::Binary(_))) => continue,
                Some(Ok(WsMessage::Close(_))) | None => panic!("connection closed"),
                Some(Ok(WsMessage::Frame(_))) => continue,
                Some(Err(err)) => panic!("ws error: {err}"),
            }
        }
    }

    #[tokio::test]
    async fn register_flow_creates_session_and_responds() {
        let (registry, running, url) = boot().await;
        let mut ws = connect(&url).await;
        ws.send(WsMessage::Text(
            register_text("uid-1", "client", "echo").into(),
        ))
        .await
        .unwrap();

        let resp = next_text(&mut ws).await;
        let parsed = WireMessage::parse(&resp).unwrap();
        match parsed {
            WireMessage::Response { result, .. } => {
                let value = result.expect("ok");
                let res: SessionRegisterResult = serde_json::from_value(value).unwrap();
                assert_eq!(res.session_id, "uid-1");
                assert!(!res.reconnected);
                assert_eq!(res.server_version, "test-1.0");
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(registry.len(), 1);

        ws.close(None).await.ok();
        running.shutdown();
    }

    #[tokio::test]
    async fn duplicate_uid_returns_collision_error() {
        let (_registry, running, url) = boot().await;
        let mut ws1 = connect(&url).await;
        ws1.send(WsMessage::Text(
            register_text("uid-x", "client", "a").into(),
        ))
        .await
        .unwrap();
        let _ = next_text(&mut ws1).await;

        let mut ws2 = connect(&url).await;
        ws2.send(WsMessage::Text(
            register_text("uid-x", "client", "b").into(),
        ))
        .await
        .unwrap();
        let resp = next_text(&mut ws2).await;
        let parsed = WireMessage::parse(&resp).unwrap();
        match parsed {
            WireMessage::Response { result, .. } => {
                let err = result.expect_err("collision");
                assert_eq!(err.code, error_codes::SESSION_UID_COLLISION);
            }
            other => panic!("unexpected: {other:?}"),
        }
        ws1.close(None).await.ok();
        ws2.close(None).await.ok();
        running.shutdown();
    }

    #[tokio::test]
    async fn second_register_on_same_ws_is_rejected() {
        // Защита от orphan-сессий: повторный session.register на том же
        // WS-коннекте должен быть отклонён, иначе первый register оставит
        // в реестре сессию с ping-task на чужом ConnectionHandle —
        // mark_disconnected при разрыве заденет только последнюю identity.
        let (registry, running, url) = boot().await;
        let mut ws = connect(&url).await;
        ws.send(WsMessage::Text(
            register_text("uid-first", "client", "a").into(),
        ))
        .await
        .unwrap();
        let _ = next_text(&mut ws).await;
        assert_eq!(registry.len(), 1);

        // Второй register на ТОМ же WS под другим uid.
        ws.send(WsMessage::Text(
            register_text("uid-second", "client", "b").into(),
        ))
        .await
        .unwrap();
        let resp = next_text(&mut ws).await;
        let parsed = WireMessage::parse(&resp).unwrap();
        match parsed {
            WireMessage::Response { result, .. } => {
                let err = result.expect_err("invalid_request");
                assert_eq!(err.code, error_codes::INVALID_REQUEST);
            }
            other => panic!("unexpected: {other:?}"),
        }
        // В реестре остаётся только первая сессия.
        assert_eq!(registry.len(), 1);
        assert!(registry.get("uid-first").is_some());
        assert!(registry.get("uid-second").is_none());

        ws.close(None).await.ok();
        running.shutdown();
    }

    #[tokio::test]
    async fn disconnect_marks_session_then_grace_removes() {
        let (registry, running, url) = boot().await;
        let mut ws = connect(&url).await;
        ws.send(WsMessage::Text(
            register_text("uid-d", "client", "x").into(),
        ))
        .await
        .unwrap();
        let _ = next_text(&mut ws).await;
        assert_eq!(registry.len(), 1);

        // drop connection
        ws.close(None).await.ok();
        drop(ws);

        // ждём, чтобы сервер успел заметить разрыв и пометить Disconnected
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Some(rec) = registry.get("uid-d") {
                if rec.state == crate::session_manager::registry::SessionState::Disconnected {
                    break;
                }
            }
        }
        let rec = registry.get("uid-d").expect("still here");
        assert_eq!(
            rec.state,
            crate::session_manager::registry::SessionState::Disconnected
        );

        // grace=1s; sweeper тикает раз в 1s; ждём до ~3s
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if registry.get("uid-d").is_none() {
                break;
            }
        }
        assert!(
            registry.get("uid-d").is_none(),
            "sweeper must remove after grace"
        );

        running.shutdown();
    }

    #[tokio::test]
    async fn reconnect_within_grace_restores_session() {
        let (registry, running, url) = boot().await;
        let mut ws = connect(&url).await;
        ws.send(WsMessage::Text(
            register_text("uid-r", "client", "v1").into(),
        ))
        .await
        .unwrap();
        let _ = next_text(&mut ws).await;
        ws.close(None).await.ok();
        drop(ws);

        // дождаться Disconnected
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Some(rec) = registry.get("uid-r") {
                if rec.state == crate::session_manager::registry::SessionState::Disconnected {
                    break;
                }
            }
        }

        // reconnect быстро (до grace 1s)
        let mut ws2 = connect(&url).await;
        ws2.send(WsMessage::Text(
            register_text("uid-r", "client", "v2").into(),
        ))
        .await
        .unwrap();
        let resp = next_text(&mut ws2).await;
        let parsed = WireMessage::parse(&resp).unwrap();
        match parsed {
            WireMessage::Response { result, .. } => {
                let value = result.expect("ok");
                let res: SessionRegisterResult = serde_json::from_value(value).unwrap();
                assert!(res.reconnected, "reconnected flag must be true");
                assert_eq!(res.session_id, "uid-r");
            }
            other => panic!("unexpected: {other:?}"),
        }
        let rec = registry.get("uid-r").unwrap();
        assert_eq!(rec.tools[0].name, "v2");
        ws2.close(None).await.ok();
        running.shutdown();
    }

    #[tokio::test]
    async fn application_ping_method_returns_method_not_found() {
        // App-level `ping` удалён в пользу WS protocol-level Ping/Pong.
        // Если клиент по-старому шлёт JSON-RPC ping — должен прийти
        // стандартный METHOD_NOT_FOUND.
        let (_registry, running, url) = boot().await;
        let mut ws = connect(&url).await;
        let req = WireMessage::Request {
            id: Id::Number(7),
            method: methods::PING.to_owned(),
            params: Value::Null,
        };
        ws.send(WsMessage::Text(req.to_text().into()))
            .await
            .unwrap();
        let resp = next_text(&mut ws).await;
        let parsed = WireMessage::parse(&resp).unwrap();
        match parsed {
            WireMessage::Response { id, result } => {
                assert_eq!(id, Id::Number(7));
                let err = result.expect_err("ping must be METHOD_NOT_FOUND");
                assert_eq!(err.code, error_codes::METHOD_NOT_FOUND);
            }
            other => panic!("unexpected: {other:?}"),
        }
        ws.close(None).await.ok();
        running.shutdown();
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let (_registry, running, url) = boot().await;
        let mut ws = connect(&url).await;
        let req = WireMessage::Request {
            id: Id::Number(1),
            method: "unknown.method".to_owned(),
            params: Value::Null,
        };
        ws.send(WsMessage::Text(req.to_text().into()))
            .await
            .unwrap();
        let resp = next_text(&mut ws).await;
        let parsed = WireMessage::parse(&resp).unwrap();
        match parsed {
            WireMessage::Response { result, .. } => {
                let err = result.expect_err("error");
                assert_eq!(err.code, error_codes::METHOD_NOT_FOUND);
            }
            other => panic!("unexpected: {other:?}"),
        }
        ws.close(None).await.ok();
        running.shutdown();
    }

    #[tokio::test]
    async fn invalid_json_returns_parse_error() {
        let (_registry, running, url) = boot().await;
        let mut ws = connect(&url).await;
        ws.send(WsMessage::Text("not a json".to_owned().into()))
            .await
            .unwrap();
        let resp = next_text(&mut ws).await;
        let parsed = WireMessage::parse(&resp).unwrap();
        match parsed {
            WireMessage::Response { id, result } => {
                assert_eq!(id, Id::Null);
                let err = result.expect_err("error");
                assert_eq!(err.code, error_codes::PARSE_ERROR);
            }
            other => panic!("unexpected: {other:?}"),
        }
        ws.close(None).await.ok();
        running.shutdown();
    }

    #[tokio::test]
    async fn session_bye_removes_record() {
        let (registry, running, url) = boot().await;
        let mut ws = connect(&url).await;
        ws.send(WsMessage::Text(
            register_text("uid-b", "client", "x").into(),
        ))
        .await
        .unwrap();
        let _ = next_text(&mut ws).await;
        assert_eq!(registry.len(), 1);

        let bye = WireMessage::Request {
            id: Id::Number(2),
            method: methods::SESSION_BYE.to_owned(),
            params: json!({}),
        };
        ws.send(WsMessage::Text(bye.to_text().into()))
            .await
            .unwrap();
        let _ = next_text(&mut ws).await;

        // дать серверу обработать close
        for _ in 0..50 {
            if registry.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            registry.is_empty(),
            "after session.bye registry must be empty"
        );
        running.shutdown();
    }

    #[tokio::test]
    async fn tools_changed_updates_registry() {
        let (registry, running, url) = boot().await;
        let mut ws = connect(&url).await;
        ws.send(WsMessage::Text(
            register_text("uid-t", "client", "old").into(),
        ))
        .await
        .unwrap();
        let _ = next_text(&mut ws).await;

        let notif = WireMessage::Notification {
            method: methods::SESSION_TOOLS_CHANGED.to_owned(),
            params: json!({
                "tools": [
                    {"name": "new1", "input_schema": {}},
                    {"name": "new2", "input_schema": {}}
                ]
            }),
        };
        ws.send(WsMessage::Text(notif.to_text().into()))
            .await
            .unwrap();

        // дать серверу обработать
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Some(rec) = registry.get("uid-t") {
                if rec.tools.len() == 2 {
                    break;
                }
            }
        }
        let rec = registry.get("uid-t").unwrap();
        let names: Vec<&str> = rec.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["new1", "new2"]);
        ws.close(None).await.ok();
        running.shutdown();
    }

    /// ADR‑0023: bidirectional outbound — менеджер шлёт `tool.call` клиенту,
    /// клиент отвечает, ConnectionHandle::call возвращает результат.
    #[tokio::test]
    async fn outbound_tool_call_round_trips_through_websocket() {
        let (registry, running, url) = boot().await;
        let mut ws = connect(&url).await;
        ws.send(WsMessage::Text(
            register_text("uid-out", "client", "echo").into(),
        ))
        .await
        .unwrap();
        let _ = next_text(&mut ws).await; // register ack

        // Достаём ConnectionHandle из реестра.
        let conn = registry
            .get("uid-out")
            .expect("record")
            .connection
            .expect("connection handle present after register");

        // Параллельно: менеджер шлёт outbound, клиент читает запрос и отвечает.
        let conn_clone = Arc::clone(&conn);
        let outbound_task = tokio::spawn(async move {
            conn_clone
                .call(
                    "tool.call",
                    json!({"name": "echo", "arguments": {"x": 1}}),
                    tokio::time::Instant::now() + Duration::from_secs(2),
                )
                .await
        });

        // Клиент видит outbound‑request, отвечает Response с тем же id.
        let outbound_text = next_text(&mut ws).await;
        let outbound_msg = WireMessage::parse(&outbound_text).unwrap();
        let id = match outbound_msg {
            WireMessage::Request { id, method, .. } => {
                assert_eq!(method, "tool.call");
                id
            }
            other => panic!("expected outbound tool.call request, got {other:?}"),
        };
        let response = WireMessage::Response {
            id,
            result: Ok(json!({"ok": true})),
        };
        ws.send(WsMessage::Text(response.to_text().into()))
            .await
            .unwrap();

        let result = outbound_task.await.unwrap().expect("call ok");
        assert_eq!(result, json!({"ok": true}));

        ws.close(None).await.ok();
        running.shutdown();
    }

    /// ADR‑0023: на disconnect — pending outbound‑вызовы завершаются Disconnected.
    #[tokio::test]
    async fn outbound_pending_drains_on_disconnect() {
        let (registry, running, url) = boot().await;
        let mut ws = connect(&url).await;
        ws.send(WsMessage::Text(
            register_text("uid-drop", "client", "x").into(),
        ))
        .await
        .unwrap();
        let _ = next_text(&mut ws).await;

        let conn = registry
            .get("uid-drop")
            .expect("record")
            .connection
            .expect("conn");

        // Клиент игнорирует запрос — менеджер ждёт.
        let conn_clone = Arc::clone(&conn);
        let task = tokio::spawn(async move {
            conn_clone
                .call(
                    "tool.call",
                    json!({}),
                    tokio::time::Instant::now() + Duration::from_secs(10),
                )
                .await
        });

        // Дать call успеть положиться в pending.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Клиент рвёт WS.
        ws.close(None).await.ok();
        drop(ws);

        // Ждём, пока registry заметит и mark_disconnected → drain_pending.
        let err = task.await.unwrap().unwrap_err();
        assert!(matches!(
            err,
            crate::session_manager::connection::ConnectionCallError::Disconnected
        ));

        running.shutdown();
    }

    fn keepalive_config(interval_ms: u64, timeout_ms: u64) -> McpSessionManagerConfig {
        let mut c = test_config();
        c.ws_ping_interval_ms = interval_ms;
        c.ws_ping_timeout_ms = timeout_ms;
        // grace=1s в test_config — для keep-alive тестов хватит.
        c
    }

    #[tokio::test]
    async fn ws_keepalive_drops_session_when_no_pong_arrives() {
        // Boot с агрессивными интервалами: ping каждые 100мс, timeout 250мс.
        // Клиент tokio_tungstenite по дефолту авто-Pong'ает; чтобы ИМИТИРОВАТЬ
        // зависший peer, перехватываем все входящие фреймы вручную и не
        // отвечаем на Ping. Tungstenite-клиент шлёт Pong автоматически
        // только когда `next()` обрабатывает Ping; если мы ни разу не дёрнем
        // `next()`, Pong не отправится.
        let config = keepalive_config(100, 250);
        let (registry, running, url) = boot_with(config).await;

        let mut ws = connect(&url).await;
        ws.send(WsMessage::Text(
            register_text("uid-keepalive", "client", "x").into(),
        ))
        .await
        .unwrap();
        let _ = next_text(&mut ws).await;
        assert_eq!(registry.len(), 1);

        // НЕ читаем из ws — auto-Pong не сработает. Ждём, пока менеджер
        // решит, что peer мёртв (timeout=250мс + sweep до Disconnected).
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Some(rec) = registry.get("uid-keepalive") {
                if rec.state == crate::session_manager::registry::SessionState::Disconnected {
                    break;
                }
            }
        }
        let rec = registry
            .get("uid-keepalive")
            .expect("session still in registry (Disconnected)");
        assert_eq!(
            rec.state,
            crate::session_manager::registry::SessionState::Disconnected,
            "keep-alive timeout should mark session Disconnected"
        );

        drop(ws);
        running.shutdown();
    }

    #[tokio::test]
    async fn ws_keepalive_keeps_session_alive_with_auto_pong() {
        // Те же агрессивные интервалы, но клиент крутит `next()` в фоне —
        // tungstenite авто-Pong'ает. Сессия должна остаться Active.
        let config = keepalive_config(100, 300);
        let (registry, running, url) = boot_with(config).await;

        let ws = connect(&url).await;
        let (mut sink, mut stream) = ws.split();
        sink.send(WsMessage::Text(
            register_text("uid-alive", "client", "x").into(),
        ))
        .await
        .unwrap();

        // Pump фоновой task'ой: читаем фреймы (auto-Pong на Ping), забываем результат.
        let pump = tokio::spawn(async move {
            while let Some(Ok(_msg)) = stream.next().await {
                // авто-Pong отрабатывает в момент чтения Ping
            }
        });

        // Ждём заметно дольше timeout (300мс) — сессия должна остаться Active.
        tokio::time::sleep(Duration::from_millis(800)).await;

        let rec = registry.get("uid-alive").expect("still here");
        assert_eq!(
            rec.state,
            crate::session_manager::registry::SessionState::Active,
            "auto-Pong from tungstenite must keep session Active",
        );

        pump.abort();
        sink.close().await.ok();
        running.shutdown();
    }
}
