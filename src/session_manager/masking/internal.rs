//! Internal UDS endpoint для вызовов сервиса маскирования → менеджер.
//!
//! Контракт (см. TASK-222): единственный метод
//! `POST /internal/v1/tools/call` на `masking.internal_listen_path` в том же
//! shared volume, что и `service.sock`. Доступ ограничен peer UID равным
//! `masking.service_expected_uid` — сокет лежит рядом с сокетом сервиса, а
//! peer creds гарантируют, что caller именно сервис.
//!
//! Тело запроса — `{"database_id","name","arguments"}`. `name` должен быть в
//! `masking.internal_tools`; иначе `404 {"success":false,"error":{"code":
//! "method_not_found"}}`. Целевая сессия резолвится по имени из конфигурации
//! (`identity_bindings`: session ↔ database_id), вызов уходит в 1С через
//! обычный `tool.call` диспетчер.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;
use tracing::error;
use uuid::Uuid;

use crate::session_manager::masking::identity::{IdentityResolver, VerifiedDatabaseIdentity};
use crate::session_manager::masking::MaskingGate;
use crate::session_manager::protocol::{ToolCallParams, ToolCallResult, ToolVisibility};
use crate::session_manager::registry::{SessionRecord, SessionRegistry, SessionState};

const INTERNAL_CALL_PATH: &str = "/internal/v1/tools/call";
const MAX_INTERNAL_REQUEST_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum InternalDispatchError {
    #[error("no verified internal tool target")]
    NoTarget,
    #[error("ambiguous verified internal tool target")]
    AmbiguousTarget,
    #[error("internal tool dispatch failed")]
    Dispatch,
}

/// Target нельзя получить через agent resolver: требуется одновременно
/// internal visibility и проверенная deployment identity базы.
#[derive(Debug, Clone)]
pub struct VerifiedInternalTarget {
    record: SessionRecord,
    identity: VerifiedDatabaseIdentity,
}

/// Резолвит единственную Active-сессию, которой конфиг привязал
/// `database_id` и которая зарегистрировала tool `tool_name` с visibility
/// `Internal`. База = сервер+имя: сопоставление идёт по `client_uid` через
/// `IdentityResolver`, а не по client-provided идентификаторам.
pub fn resolve_internal_target(
    registry: &SessionRegistry,
    identities: &IdentityResolver,
    database_id: &str,
    tool_name: &str,
) -> Result<VerifiedInternalTarget, InternalDispatchError> {
    let mut matches = registry
        .snapshot()
        .into_iter()
        .filter(|record| record.state == SessionState::Active)
        .filter_map(|record| {
            let identity = identities.verify(&record)?;
            if identity.database_id.to_string() != database_id
                || !record.tools.iter().any(|tool| {
                    tool.name == tool_name && tool.visibility == ToolVisibility::Internal
                })
            {
                return None;
            }
            Some(VerifiedInternalTarget { record, identity })
        });
    let target = matches.next().ok_or(InternalDispatchError::NoTarget)?;
    if matches.next().is_some() {
        return Err(InternalDispatchError::AmbiguousTarget);
    }
    Ok(target)
}

/// Обычный `tool.call` через dispatcher целевой сессии с менеджерским
/// дедлайном `internal_call_timeout`. Результат 1С передаётся сервису как
/// есть — проверки страниц/cursor делает сам сервис.
pub async fn dispatch_internal_tool(
    target: VerifiedInternalTarget,
    tool_name: String,
    arguments: Value,
    timeout: Duration,
) -> Result<ToolCallResult, InternalDispatchError> {
    let _verified_database = target.identity;
    let connection = target
        .record
        .connection
        .ok_or(InternalDispatchError::NoTarget)?;
    target
        .record
        .dispatcher
        .enqueue(
            connection,
            ToolCallParams {
                name: tool_name,
                arguments,
            },
            Some(tokio::time::Instant::now() + timeout),
            CancellationToken::new(),
        )
        .await
        .map_err(|_| InternalDispatchError::Dispatch)
}

/// Тело запроса `POST /internal/v1/tools/call`.
#[derive(Debug, Deserialize)]
struct InternalCallRequest {
    database_id: String,
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Clone)]
struct InternalEndpointContext {
    authorized: bool,
    internal_tools: Arc<std::collections::HashSet<String>>,
    identities: Arc<IdentityResolver>,
    registry: Arc<SessionRegistry>,
    call_timeout: Duration,
}

/// Поднимает internal UDS endpoint при `masking.enabled=true`.
/// `None` — gate выключен либо `service_expected_uid` не задан (последнее
/// невозможно после валидации конфига; защитный fail-closed).
pub fn spawn_internal_endpoint(
    gate: Arc<MaskingGate>,
    registry: Arc<SessionRegistry>,
    shutdown: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    if !gate.is_enabled() {
        return None;
    }
    let expected_uid = gate.service_expected_uid()?;
    let path = gate.internal_listen_path().to_path_buf();
    let identities = Arc::new(gate.identity_resolver().clone());
    let internal_tools = Arc::new(gate.internal_tools().clone());
    let call_timeout = gate.internal_call_timeout();
    Some(tokio::spawn(async move {
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(err) => {
                error!(?err, path = %path.display(), "masking internal endpoint bind failed");
                return;
            }
        };
        let context = InternalEndpointContext {
            authorized: false,
            internal_tools,
            identities,
            registry,
            call_timeout,
        };
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else {
                        continue;
                    };
                    let authorized = stream
                        .peer_cred()
                        .map(|cred| cred.uid() == expected_uid)
                        .unwrap_or(false);
                    let mut conn_ctx = context.clone();
                    conn_ctx.authorized = authorized;
                    tokio::spawn(serve_connection(stream, conn_ctx));
                }
            }
        }
        let _ = std::fs::remove_file(&path);
    }))
}

async fn serve_connection(stream: UnixStream, context: InternalEndpointContext) {
    let service = service_fn(move |request: Request<hyper::body::Incoming>| {
        let context = context.clone();
        async move { handle_request(request, context).await }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await;
}

fn respond(status: StatusCode, body: Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(
            serde_json::to_vec(&body).expect("internal response"),
        )))
        .expect("internal response")
}

fn failure(status: StatusCode, code: &str) -> Response<Full<Bytes>> {
    respond(status, json!({"success": false, "error": {"code": code}}))
}

async fn handle_request(
    request: Request<hyper::body::Incoming>,
    context: InternalEndpointContext,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    // UID gate фиксирован кодом до любой проверки пути — чужому процессу
    // endpoint не раскрывает даже топологию методов.
    if !context.authorized {
        return Ok(failure(StatusCode::FORBIDDEN, "forbidden"));
    }
    if request.method() != Method::POST || request.uri().path() != INTERNAL_CALL_PATH {
        return Ok(failure(StatusCode::NOT_FOUND, "not_found"));
    }
    let body = match request.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return Ok(failure(StatusCode::BAD_REQUEST, "invalid_request")),
    };
    if body.len() > MAX_INTERNAL_REQUEST_BYTES {
        return Ok(failure(StatusCode::BAD_REQUEST, "invalid_request"));
    }
    let call: InternalCallRequest = match serde_json::from_slice(&body) {
        Ok(call) => call,
        Err(_) => return Ok(failure(StatusCode::BAD_REQUEST, "invalid_request")),
    };
    if Uuid::parse_str(&call.database_id).is_err() {
        return Ok(failure(StatusCode::BAD_REQUEST, "invalid_request"));
    }
    if !context.internal_tools.contains(&call.name) {
        return Ok(failure(StatusCode::NOT_FOUND, "method_not_found"));
    }
    let target = match resolve_internal_target(
        &context.registry,
        &context.identities,
        &call.database_id,
        &call.name,
    ) {
        Ok(target) => target,
        Err(InternalDispatchError::NoTarget) => {
            return Ok(failure(StatusCode::SERVICE_UNAVAILABLE, "no_target"));
        }
        Err(InternalDispatchError::AmbiguousTarget) => {
            return Ok(failure(StatusCode::SERVICE_UNAVAILABLE, "ambiguous_target"));
        }
        Err(InternalDispatchError::Dispatch) => unreachable!("resolve does not dispatch"),
    };
    match dispatch_internal_tool(target, call.name, call.arguments, context.call_timeout).await {
        Ok(result) => Ok(respond(
            StatusCode::OK,
            json!({"success": true, "result": serde_json::to_value(result).unwrap_or(Value::Null)}),
        )),
        Err(_) => Ok(failure(StatusCode::BAD_GATEWAY, "dispatch_failed")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::{MaskingConfig, MaskingIdentityBinding};
    use crate::session_manager::connection::ConnectionHandle;
    use crate::session_manager::protocol::{SessionRegisterParams, ToolDescriptor, WireMessage};
    use std::path::PathBuf;
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    const PUBLIC_KEY: &[u8] = br#"-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAqb56A5d3wWE6xz7XETMTTzhooYvBDkfqBuwYtYdMmvY=
-----END PUBLIC KEY-----
"#;

    const METADATA_TOOL: &str = "mcp_internal_masking_metadata_feed";

    fn own_uid() -> u32 {
        let (a, _b) = UnixStream::pair().unwrap();
        a.peer_cred().unwrap().uid()
    }

    /// Enabled gate с tempdir-сокетами; identity binding — session → database.
    fn gate_fixture(
        dir: &tempfile::TempDir,
        session: &str,
        database: Uuid,
        expected_uid: u32,
    ) -> Arc<MaskingGate> {
        let public_key_path = dir.path().join("broker.pub.pem");
        std::fs::write(&public_key_path, PUBLIC_KEY).unwrap();
        let config = MaskingConfig {
            enabled: true,
            socket_path: dir.path().join("service.sock"),
            internal_listen_path: dir.path().join("manager.sock"),
            internal_call_timeout_ms: 2_000,
            service_expected_uid: Some(expected_uid),
            broker_public_key_path: public_key_path,
            identity_bindings: vec![MaskingIdentityBinding {
                session: session.to_owned(),
                database_id: database.to_string(),
            }],
            ..MaskingConfig::default()
        };
        Arc::new(MaskingGate::from_config(&config, dir.path()).unwrap())
    }

    fn internal_tool() -> ToolDescriptor {
        ToolDescriptor {
            name: METADATA_TOOL.to_owned(),
            description: None,
            input_schema: json!({"type":"object"}),
            visibility: ToolVisibility::Internal,
        }
    }

    fn register_session(
        registry: &SessionRegistry,
        session: &str,
        tools: Vec<ToolDescriptor>,
        connection: Option<Arc<ConnectionHandle>>,
    ) {
        registry
            .register(
                SessionRegisterParams {
                    client_uid: session.to_owned(),
                    kind: "server".to_owned(),
                    version: "1".to_owned(),
                    infobase_name: "db".to_owned(),
                    ib_session_number: 1,
                    tools,
                    config_id: None,
                    host_id: None,
                    pid: None,
                    resources: None,
                    prompts: None,
                    extras: None,
                },
                Instant::now(),
                connection,
            )
            .unwrap();
    }

    /// Сырой HTTP POST по UDS: `Connection: close` даёт EOF после ответа.
    async fn http_post(socket: &PathBuf, body: &[u8]) -> (u16, Value) {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        let request = format!(
            "POST /internal/v1/tools/call HTTP/1.1\r\nHost: mgr\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.unwrap();
        let raw = String::from_utf8(raw).unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap();
        let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, serde_json::from_str(body).unwrap())
    }

    #[tokio::test]
    async fn internal_target_resolves_by_configured_session_name() {
        let database = Uuid::new_v4();
        let registry = SessionRegistry::new();
        register_session(&registry, "server-gbig_pam_ai", vec![internal_tool()], None);
        register_session(&registry, "other-session", vec![internal_tool()], None);

        let identities = IdentityResolver::new(&[MaskingIdentityBinding {
            session: "server-gbig_pam_ai".to_owned(),
            database_id: database.to_string(),
        }]);

        assert!(resolve_internal_target(
            &registry,
            &identities,
            &database.to_string(),
            METADATA_TOOL
        )
        .is_ok());
        // Чужой database_id и сессия без internal-дескриптора не резолвятся.
        assert!(matches!(
            resolve_internal_target(
                &registry,
                &identities,
                &Uuid::new_v4().to_string(),
                METADATA_TOOL
            ),
            Err(InternalDispatchError::NoTarget)
        ));
        assert!(matches!(
            resolve_internal_target(&registry, &identities, &database.to_string(), "other_tool"),
            Err(InternalDispatchError::NoTarget)
        ));
    }

    #[tokio::test]
    async fn dispatch_internal_tool_round_trips_tool_call() {
        let database = Uuid::new_v4();
        let registry = SessionRegistry::new();
        let (tx, mut outbound) = mpsc::unbounded_channel();
        let connection = Arc::new(ConnectionHandle::new(tx));
        register_session(
            &registry,
            "server-gbig_pam_ai",
            vec![internal_tool()],
            Some(Arc::clone(&connection)),
        );
        let identities = IdentityResolver::new(&[MaskingIdentityBinding {
            session: "server-gbig_pam_ai".to_owned(),
            database_id: database.to_string(),
        }]);
        let target =
            resolve_internal_target(&registry, &identities, &database.to_string(), METADATA_TOOL)
                .unwrap();

        let responder = tokio::spawn(async move {
            let msg = outbound.recv().await.unwrap();
            match msg {
                WireMessage::Request { id, method, params } => {
                    assert_eq!(method, "tool.call");
                    assert_eq!(params["name"], METADATA_TOOL);
                    connection.complete_response(
                        id,
                        Ok(json!({
                            "content": [{"type":"text","text":"{}"}],
                            "is_error": false
                        })),
                    );
                }
                _ => panic!("expected request"),
            }
        });
        let result = dispatch_internal_tool(
            target,
            METADATA_TOOL.to_owned(),
            json!({"selector": {}, "cursor": null}),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert!(!result.is_error);
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn endpoint_rejects_foreign_uid_and_unknown_tool_name() {
        let dir = tempfile::tempdir().unwrap();
        let database = Uuid::new_v4();
        let registry = Arc::new(SessionRegistry::new());
        register_session(&registry, "server-gbig_pam_ai", vec![internal_tool()], None);

        // Peer UID чужой — каждый запрос отклоняется forbidden до проверки пути.
        let gate = gate_fixture(&dir, "server-gbig_pam_ai", database, own_uid() + 1_000);
        let shutdown = CancellationToken::new();
        let task = spawn_internal_endpoint(gate, Arc::clone(&registry), shutdown.clone()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (status, body) = http_post(
            &dir.path().join("manager.sock"),
            br#"{"database_id":"x","name":"mcp_internal_masking_metadata_feed","arguments":{}}"#,
        )
        .await;
        assert_eq!(status, 403);
        assert_eq!(body["error"]["code"], "forbidden");

        shutdown.cancel();
        task.await.unwrap();

        // Свой UID, но имя вне masking.internal_tools → method_not_found.
        let dir2 = tempfile::tempdir().unwrap();
        let gate = gate_fixture(&dir2, "server-gbig_pam_ai", database, own_uid());
        let shutdown = CancellationToken::new();
        let task = spawn_internal_endpoint(gate, Arc::clone(&registry), shutdown.clone()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (status, body) = http_post(
            &dir2.path().join("manager.sock"),
            br#"{"database_id":"cc370548-d259-4093-ac21-97e066cf0f62","name":"execute_query","arguments":{}}"#,
        )
        .await;
        assert_eq!(status, 404);
        assert_eq!(body["error"]["code"], "method_not_found");

        // Невалидный database_id → invalid_request.
        let (status, body) = http_post(
            &dir2.path().join("manager.sock"),
            br#"{"database_id":"not-a-uuid","name":"mcp_internal_masking_metadata_feed","arguments":{}}"#,
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(body["error"]["code"], "invalid_request");

        // Чужой database_id → no_target.
        let (status, body) = http_post(
            &dir2.path().join("manager.sock"),
            format!(
                r#"{{"database_id":"{}","name":"{}","arguments":{{}}}}"#,
                Uuid::new_v4(),
                METADATA_TOOL
            )
            .as_bytes(),
        )
        .await;
        assert_eq!(status, 503);
        assert_eq!(body["error"]["code"], "no_target");

        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn endpoint_dispatches_internal_tool_to_bound_session() {
        let dir = tempfile::tempdir().unwrap();
        let database = Uuid::new_v4();
        let registry = Arc::new(SessionRegistry::new());
        let (tx, mut outbound) = mpsc::unbounded_channel();
        let connection = Arc::new(ConnectionHandle::new(tx));
        register_session(
            &registry,
            "server-gbig_pam_ai",
            vec![internal_tool()],
            Some(Arc::clone(&connection)),
        );

        let gate = gate_fixture(&dir, "server-gbig_pam_ai", database, own_uid());
        let shutdown = CancellationToken::new();
        let task = spawn_internal_endpoint(gate, Arc::clone(&registry), shutdown.clone()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let responder = tokio::spawn(async move {
            let msg = outbound.recv().await.unwrap();
            match msg {
                WireMessage::Request { id, params, .. } => {
                    assert_eq!(params["name"], METADATA_TOOL);
                    assert_eq!(params["arguments"]["cursor"], Value::Null);
                    connection.complete_response(
                        id,
                        Ok(json!({
                            "content": [{"type":"text","text":"{\"ok\":true}"}],
                            "is_error": false
                        })),
                    );
                }
                _ => panic!("expected request"),
            }
        });

        let (status, body) = http_post(
            &dir.path().join("manager.sock"),
            format!(
                r#"{{"database_id":"{database}","name":"{METADATA_TOOL}","arguments":{{"selector":{{}},"cursor":null}}}}"#
            )
            .as_bytes(),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["success"], true);
        // `is_error` сериализуется только при true (skip_serializing_if).
        assert!(body["result"].get("is_error").is_none());
        assert_eq!(body["result"]["content"][0]["text"], "{\"ok\":true}");

        responder.await.unwrap();
        shutdown.cancel();
        task.await.unwrap();
    }
}
