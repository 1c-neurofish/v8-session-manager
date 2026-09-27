//! Internal UDS endpoint для вызовов сервиса маскирования → менеджер.
//!
//! Контракт (см. TASK-222, N из TASK-225): единственный метод
//! `POST /internal/v1/tools/call` на `masking.internal_listen_path` в том же
//! shared volume, что и `service.sock`. Доступ ограничен peer UID равным
//! `masking.service_expected_uid` — сокет лежит рядом с сокетом сервиса, а
//! peer creds гарантируют, что caller именно сервис.
//!
//! Тело запроса — `{"instance_id","name","arguments"}`: точный ключ базы
//! (`ras:<c>:<i>`/`gen:<srvr>/<ref>`), тот же что `databases.instance_id`
//! в сервисе. `name` должен быть в `masking.internal_tools`; иначе
//! `404 {"success":false,"error":{"code":"method_not_found"}}`. Целевая
//! сессия резолвится точным равенством `SessionRecord.database_key` —
//! склейка баз невозможна по построению; вызов уходит в 1С через обычный
//! `tool.call` диспетчер.

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

use crate::session_manager::masking::MaskingGate;
use crate::session_manager::protocol::{ToolCallParams, ToolCallResult, ToolVisibility};
use crate::session_manager::registry::{SessionRegistry, SessionState};

const INTERNAL_CALL_PATH: &str = "/internal/v1/tools/call";
const MAX_INTERNAL_REQUEST_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum InternalDispatchError {
    /// Ни одна Active-сессия не несёт этот ключ с нужным internal
    /// tool: сервис читает как «база известна, но 1С сейчас не подключена»
    /// → `DATABASE_NOT_CONNECTED` (retryable на стороне сервиса).
    #[error("no verified internal tool target")]
    NoTarget,
    #[error("ambiguous verified internal tool target")]
    AmbiguousTarget,
    #[error("internal tool dispatch failed")]
    Dispatch,
}

/// Target нельзя получить через agent resolver: требуется одновременно
/// internal visibility и точное совпадение ключа базы.
#[derive(Debug, Clone)]
pub(crate) struct VerifiedInternalTarget {
    record: crate::session_manager::registry::SessionRecord,
}

/// Резолвит единственную Active-сессию с `database_key` равным ключу
/// запроса и tool `tool_name` с visibility `Internal`. Сравнение —
/// только точное равенство строки: никаких координатных совпадений,
/// ras- и generated-ключи одних и тех же координат не пересекаются.
pub(crate) fn resolve_internal_target(
    registry: &SessionRegistry,
    call: &InternalCallRequest,
) -> Result<VerifiedInternalTarget, InternalDispatchError> {
    let mut matches = registry
        .snapshot()
        .into_iter()
        .filter(|record| record.state == SessionState::Active)
        .filter(|record| record.database_key.as_deref() == Some(call.instance_id.as_str()))
        .filter(|record| {
            record
                .tools
                .iter()
                .any(|tool| tool.name == call.name && tool.visibility == ToolVisibility::Internal)
        })
        .map(|record| VerifiedInternalTarget { record });
    let target = matches.next().ok_or(InternalDispatchError::NoTarget)?;
    if matches.next().is_some() {
        return Err(InternalDispatchError::AmbiguousTarget);
    }
    Ok(target)
}

/// Обычный `tool.call` через dispatcher целевой сессии с менеджерским
/// дедлайном `internal_call_timeout`. Результат 1С передаётся сервису как
/// есть — проверки страниц/cursor делает сам сервис.
pub(crate) async fn dispatch_internal_tool(
    target: VerifiedInternalTarget,
    tool_name: String,
    arguments: Value,
    timeout: Duration,
) -> Result<ToolCallResult, InternalDispatchError> {
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

/// Тело запроса `POST /internal/v1/tools/call`. `instance_id` —
/// обязательный точный ключ базы (тот же `databases.instance_id`).
#[derive(Debug, Deserialize)]
pub(crate) struct InternalCallRequest {
    instance_id: String,
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Clone)]
struct InternalEndpointContext {
    authorized: bool,
    internal_tools: Arc<std::collections::HashSet<String>>,
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
    if !context.internal_tools.contains(&call.name) {
        return Ok(failure(StatusCode::NOT_FOUND, "method_not_found"));
    }
    let target = match resolve_internal_target(&context.registry, &call) {
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
    use crate::config::model::MaskingConfig;
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
    const CLUSTER: &str = "0de031da-e8d9-43de-bb39-7c8bd4d9855c";
    const INFOBASE: &str = "320f6387-89b5-43fc-b344-67b11f957472";

    fn own_uid() -> u32 {
        let (a, _b) = UnixStream::pair().unwrap();
        a.peer_cred().unwrap().uid()
    }

    fn ras_key() -> String {
        format!("ras:{CLUSTER}:{INFOBASE}")
    }

    /// Enabled gate с tempdir-сокетами; маршрут — только по точному ключу
    /// из регистрации.
    fn gate_fixture(dir: &tempfile::TempDir, expected_uid: u32) -> Arc<MaskingGate> {
        let public_key_path = dir.path().join("broker.pub.pem");
        std::fs::write(&public_key_path, PUBLIC_KEY).unwrap();
        let config = MaskingConfig {
            enabled: true,
            socket_path: dir.path().join("service.sock"),
            internal_listen_path: dir.path().join("manager.sock"),
            internal_call_timeout_ms: 2_000,
            service_expected_uid: Some(expected_uid),
            broker_public_key_path: public_key_path,
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

    /// Регистрирует сессию с ключом базы: `database_key` — как после
    /// вычисления менеджером (ras:/gen:), `None` — файл-база без ключа.
    fn register_session(
        registry: &SessionRegistry,
        session: &str,
        cluster_server: Option<&str>,
        infobase_name: &str,
        database_key: Option<String>,
        tools: Vec<ToolDescriptor>,
        connection: Option<Arc<ConnectionHandle>>,
    ) {
        registry
            .register(
                SessionRegisterParams {
                    client_uid: session.to_owned(),
                    kind: "server".to_owned(),
                    version: "1".to_owned(),
                    infobase_name: infobase_name.to_owned(),
                    ib_session_number: 1,
                    tools,
                    config_id: None,
                    host_id: None,
                    pid: None,
                    resources: None,
                    prompts: None,
                    extras: None,
                    cluster_server: cluster_server.map(str::to_owned),
                    database_key,
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

    fn call_body(instance_id: &str, name: &str) -> String {
        format!(r#"{{"instance_id":"{instance_id}","name":"{name}","arguments":{{}}}}"#)
    }

    fn call_req(instance_id: &str, name: &str) -> InternalCallRequest {
        InternalCallRequest {
            instance_id: instance_id.to_owned(),
            name: name.to_owned(),
            arguments: Value::Null,
        }
    }

    /// O2: маршрут — только точное равенство ключа; координаты в нём не
    /// участвуют, склейка невозможна.
    #[tokio::test]
    async fn internal_target_resolves_by_exact_key_only() {
        let registry = SessionRegistry::new();
        register_session(
            &registry,
            "server-gbig_pam_ai",
            Some("onec-infra:1541"),
            "gbig_pam_ai",
            Some(ras_key()),
            vec![internal_tool()],
            None,
        );
        // Сессия той же ИБ, но без internal-инструмента.
        register_session(
            &registry,
            "same-key-no-tools",
            Some("onec-infra:1541"),
            "gbig_pam_ai",
            Some(ras_key()),
            vec![],
            None,
        );
        // generated-ключ тех же координат — отдельная «база».
        register_session(
            &registry,
            "same-coords-gen",
            Some("onec-infra:1541"),
            "gbig_pam_ai",
            Some("gen:onec-infra:1541/gbig_pam_ai".to_owned()),
            vec![internal_tool()],
            None,
        );
        // Сессия без ключа (файловая база).
        register_session(
            &registry,
            "file-ib",
            None,
            "file_ib",
            None,
            vec![internal_tool()],
            None,
        );

        // Точный ras-ключ → сессия с тем же ключом.
        assert!(resolve_internal_target(&registry, &call_req(&ras_key(), METADATA_TOOL)).is_ok());
        // Точный gen-ключ → generated-сессия, а не ras-сессия тех же координат.
        let target = resolve_internal_target(
            &registry,
            &call_req("gen:onec-infra:1541/gbig_pam_ai", METADATA_TOOL),
        )
        .unwrap();
        assert_eq!(
            target.record.session_id, "same-coords-gen",
            "ген-ключ маршрутится только в свою сессию"
        );
        // Другой ras-ключ (та же ИБ, другой кластер) → no_target.
        assert!(matches!(
            resolve_internal_target(
                &registry,
                &call_req(
                    &format!("ras:{}:{}", uuid::Uuid::new_v4(), INFOBASE),
                    METADATA_TOOL
                ),
            ),
            Err(InternalDispatchError::NoTarget)
        ));
        // Ключ без активной сессии/инструмента → no_target.
        assert!(matches!(
            resolve_internal_target(&registry, &call_req("gen:x/y", METADATA_TOOL)),
            Err(InternalDispatchError::NoTarget)
        ));
        assert!(matches!(
            resolve_internal_target(&registry, &call_req(&ras_key(), "other_tool")),
            Err(InternalDispatchError::NoTarget)
        ));
        // Два активных кандидата с одним ключом → ambiguous_target.
        register_session(
            &registry,
            "duplicate",
            Some("onec-infra:1541"),
            "gbig_pam_ai",
            Some(ras_key()),
            vec![internal_tool()],
            None,
        );
        assert!(matches!(
            resolve_internal_target(&registry, &call_req(&ras_key(), METADATA_TOOL)),
            Err(InternalDispatchError::AmbiguousTarget)
        ));
    }

    #[tokio::test]
    async fn dispatch_internal_tool_round_trips_tool_call() {
        let registry = SessionRegistry::new();
        let (tx, mut outbound) = mpsc::unbounded_channel();
        let connection = Arc::new(ConnectionHandle::new(tx));
        register_session(
            &registry,
            "server-gbig_pam_ai",
            Some("onec-infra:1541"),
            "gbig_pam_ai",
            Some(ras_key()),
            vec![internal_tool()],
            Some(Arc::clone(&connection)),
        );
        let target =
            resolve_internal_target(&registry, &call_req(&ras_key(), METADATA_TOOL)).unwrap();

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
        let registry = Arc::new(SessionRegistry::new());
        register_session(
            &registry,
            "server-gbig_pam_ai",
            Some("onec-infra:1541"),
            "gbig_pam_ai",
            Some(ras_key()),
            vec![internal_tool()],
            None,
        );

        // Peer UID чужой — каждый запрос отклоняется forbidden до проверки пути.
        let gate = gate_fixture(&dir, own_uid() + 1_000);
        let shutdown = CancellationToken::new();
        let task = spawn_internal_endpoint(gate, Arc::clone(&registry), shutdown.clone()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (status, body) = http_post(
            &dir.path().join("manager.sock"),
            call_body(&ras_key(), METADATA_TOOL).as_bytes(),
        )
        .await;
        assert_eq!(status, 403);
        assert_eq!(body["error"]["code"], "forbidden");

        shutdown.cancel();
        task.await.unwrap();

        // Свой UID, но имя вне masking.internal_tools → method_not_found.
        let dir2 = tempfile::tempdir().unwrap();
        let gate = gate_fixture(&dir2, own_uid());
        let shutdown = CancellationToken::new();
        let task = spawn_internal_endpoint(gate, Arc::clone(&registry), shutdown.clone()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (status, body) = http_post(
            &dir2.path().join("manager.sock"),
            call_body(&ras_key(), "execute_query").as_bytes(),
        )
        .await;
        assert_eq!(status, 404);
        assert_eq!(body["error"]["code"], "method_not_found");

        // Невалидный JSON → invalid_request (serde до маршрутизации).
        let (status, body) = http_post(
            &dir2.path().join("manager.sock"),
            br#"{"instance_id":123,"name":"x"}"#,
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(body["error"]["code"], "invalid_request");

        // Ключ без активной сессии (база существует в сервисе, но 1С не
        // подключена) → no_target.
        let (status, body) = http_post(
            &dir2.path().join("manager.sock"),
            call_body(
                &format!("ras:{CLUSTER}:{}", uuid::Uuid::new_v4()),
                METADATA_TOOL,
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
    async fn endpoint_dispatches_internal_tool_by_exact_key() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(SessionRegistry::new());
        let (tx, mut outbound) = mpsc::unbounded_channel();
        let connection = Arc::new(ConnectionHandle::new(tx));
        register_session(
            &registry,
            "server-gbig_pam_ai",
            Some("onec-infra:1541"),
            "gbig_pam_ai",
            Some(ras_key()),
            vec![internal_tool()],
            Some(Arc::clone(&connection)),
        );

        let gate = gate_fixture(&dir, own_uid());
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
                r#"{{"instance_id":"{}","name":"{METADATA_TOOL}","arguments":{{"selector":{{}},"cursor":null}}}}"#,
                ras_key()
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
