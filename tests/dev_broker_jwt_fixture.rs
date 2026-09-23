//! Изолированная DEV-фикстура broker JWT для полного HTTP MCP пути.
//!
//! Тест ignored, потому что запускает отдельный binary и внешний `openssl`.
//! Все ключи и конфиг создаются в `TempDir`, имеют mode 0600 и удаляются
//! после остановки дочернего процесса. Live manager, 1С и masking-service
//! не используются.

#![cfg(unix)]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1;
use hyper::header::{HeaderMap, HeaderValue, ACCEPT, CONTENT_TYPE};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;
use v8_session_manager::config::model::{
    AppConfig, MaskingConfig, MaskingIdentityBinding, McpConfig, McpSessionManagerConfig,
    ToolsCacheConfig,
};
use v8_session_manager::session_manager::protocol::{
    methods, Id, SessionRegisterParams, ToolDescriptor, ToolVisibility, WireMessage,
};

const ASSERTION_HEADER: &str = "x-v8-conversation-assertion";
const BROKER_ISSUER: &str = "trusted-mcp-broker";
const BROKER_AUDIENCE: &str = "v8-session-manager";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Serialize)]
struct ConversationClaims<'a> {
    conversation_id: &'a str,
    jti: String,
    exp: u64,
    iat: u64,
    aud: &'a str,
    iss: &'a str,
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn stop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

#[tokio::test]
#[ignore = "isolated DEV fixture: starts a private manager binary and requires openssl"]
async fn dev_broker_assertion_travels_in_normal_mcp_request_header() {
    let temp = tempfile::tempdir().expect("create isolated fixture directory");
    let private_key_path = temp.path().join("broker-private.pem");
    let public_key_path = temp.path().join("broker-public.pem");
    generate_ephemeral_ed25519_keys(&private_key_path, &public_key_path);

    let (ws_port, http_port) = reserve_loopback_ports();
    let instance_id = Uuid::new_v4();
    let database_id = Uuid::new_v4();
    let config_path = write_isolated_config(
        &temp,
        ws_port,
        http_port,
        instance_id,
        database_id,
        &public_key_path,
    );

    let child = Command::new(env!("CARGO_BIN_EXE_v8-session-manager"))
        .arg("--config")
        .arg(&config_path)
        .arg("--log-level")
        .arg("error")
        .current_dir(temp.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start isolated manager");
    let mut manager = ChildGuard(Some(child));

    wait_for_listener(ws_port).await;
    wait_for_listener(http_port).await;

    let (mut ws, _) = connect_async(format!("ws://127.0.0.1:{ws_port}/sessions"))
        .await
        .expect("connect isolated fake DEV route");
    register_fake_dev_route(&mut ws, instance_id).await;

    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "isolated-dev-jwt-fixture", "version": "1"}
        }
    });
    let initialized = send_mcp_post(http_port, None, None, initialize).await;
    assert_eq!(initialized.status, StatusCode::OK);
    let session_id = initialized
        .headers
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .expect("initialize response must contain MCP session id")
        .to_owned();
    let initialize_body = parse_mcp_response(&initialized.body);
    assert_eq!(initialize_body["id"], 1);

    let notification = send_mcp_post(
        http_port,
        Some(&session_id),
        None,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    assert_eq!(notification.status, StatusCode::ACCEPTED);

    let missing_assertion = call_managed_tool(http_port, &session_id, 2, None).await;
    assert_eq!(
        tool_error_code(&missing_assertion),
        "CHAT_IDENTITY_REQUIRED"
    );

    let private_key = fs::read(&private_key_path).expect("read ephemeral private key");
    let first = sign_unique_assertion(&private_key, "isolated-dev-conversation");
    let second = sign_unique_assertion(&private_key, "isolated-dev-conversation");
    assert!(first != second, "each MCP call must use a unique assertion");

    let first_response = call_managed_tool(http_port, &session_id, 3, Some(&first)).await;
    assert_eq!(tool_error_code(&first_response), "SERVICE_NOT_READY");
    let second_response = call_managed_tool(http_port, &session_id, 4, Some(&second)).await;
    assert_eq!(tool_error_code(&second_response), "SERVICE_NOT_READY");

    drop(first);
    drop(second);
    drop(private_key);
    let _ = ws.close(None).await;
    manager.stop();
    temp.close().expect("remove isolated fixture directory");
}

fn generate_ephemeral_ed25519_keys(private_key: &Path, public_key: &Path) {
    let generated = Command::new("openssl")
        .args(["genpkey", "-algorithm", "ED25519", "-out"])
        .arg(private_key)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("openssl must be available for the ignored fixture");
    assert!(
        generated.success(),
        "ephemeral private key generation failed"
    );

    let exported = Command::new("openssl")
        .arg("pkey")
        .arg("-in")
        .arg(private_key)
        .arg("-pubout")
        .arg("-out")
        .arg(public_key)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("openssl must export the ephemeral public key");
    assert!(exported.success(), "ephemeral public key export failed");

    set_mode_0600(private_key);
    set_mode_0600(public_key);
}

fn write_isolated_config(
    temp: &TempDir,
    ws_port: u16,
    http_port: u16,
    instance_id: Uuid,
    database_id: Uuid,
    public_key_path: &Path,
) -> std::path::PathBuf {
    let work_path = temp.path().join("work");
    fs::create_dir_all(&work_path).expect("create isolated work directory");

    let mut mcp = McpConfig::default();
    mcp.http.bind_address = format!("127.0.0.1:{http_port}");
    mcp.http.path = "/mcp".to_owned();
    mcp.http.auth_token = None;
    mcp.metrics.bind_address = None;
    mcp.session_manager = Some(McpSessionManagerConfig {
        bind_address: format!("127.0.0.1:{ws_port}"),
        path: "/sessions".to_owned(),
        heartbeat_interval_ms: 0,
        idle_timeout_secs: 300,
        reconnection_grace_secs: 0,
        graceful_kill_grace_ms: 100,
        ws_ping_interval_ms: 0,
        ws_ping_timeout_ms: 0,
    });

    let tools_cache = ToolsCacheConfig {
        enabled: false,
        ..ToolsCacheConfig::default()
    };
    let masking = MaskingConfig {
        enabled: true,
        socket_path: temp.path().join("masking-service-not-running.sock"),
        preflight_timeout_ms: 500,
        finalize_timeout_ms: 500,
        feed_chunk_timeout_ms: 500,
        feed_activate_timeout_ms: 500,
        broker_public_key_path: public_key_path.to_owned(),
        broker_issuer: BROKER_ISSUER.to_owned(),
        broker_audience: BROKER_AUDIENCE.to_owned(),
        broker_max_assertion_ttl_secs: 60,
        conversation_assertion_header: ASSERTION_HEADER.to_owned(),
        managed_tools: vec![
            "execute_query".to_owned(),
            "find_references_to_object".to_owned(),
            "get_object_by_link".to_owned(),
            "get_metadata".to_owned(),
            "get_access_rights".to_owned(),
            "get_link_of_object".to_owned(),
        ],
        identity_bindings: vec![MaskingIdentityBinding {
            database_instance_id: instance_id.to_string(),
            database_id: database_id.to_string(),
            expected_kind: "server".to_owned(),
            expected_config_id: "server".to_owned(),
            expected_host_id: Some("fixture-host".to_owned()),
            allowed_internal_tools: vec![
                "mcp_internal_masking_metadata_feed".to_owned(),
                "mcp_internal_masking_dictionary_feed".to_owned(),
            ],
        }],
    };
    let config = AppConfig {
        work_path,
        mcp,
        tools_cache,
        masking,
    };
    let config_path = temp.path().join("isolated-v8project.yaml");
    fs::write(
        &config_path,
        serde_yaml::to_string(&config).expect("serialize isolated config"),
    )
    .expect("write isolated config");
    set_mode_0600(&config_path);
    config_path
}

fn set_mode_0600(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .expect("set fixture file mode 0600");
    assert_eq!(
        fs::metadata(path)
            .expect("read fixture mode")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

fn reserve_loopback_ports() -> (u16, u16) {
    let ws = TcpListener::bind(("127.0.0.1", 0)).expect("reserve WS fixture port");
    let http = TcpListener::bind(("127.0.0.1", 0)).expect("reserve HTTP fixture port");
    let ports = (
        ws.local_addr().expect("WS fixture address").port(),
        http.local_addr().expect("HTTP fixture address").port(),
    );
    drop(ws);
    drop(http);
    ports
}

async fn wait_for_listener(port: u16) {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    loop {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "isolated manager did not start on its reserved loopback port"
        );
        sleep(Duration::from_millis(25)).await;
    }
}

async fn register_fake_dev_route<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>, instance: Uuid)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let params = SessionRegisterParams {
        client_uid: "isolated-dev-jwt-fixture".to_owned(),
        kind: "server".to_owned(),
        version: "1".to_owned(),
        infobase_name: "isolated-dev".to_owned(),
        ib_session_number: 1,
        database_instance_id: Some(instance.to_string()),
        tools: vec![ToolDescriptor {
            name: "execute_query".to_owned(),
            description: None,
            input_schema: json!({"type":"object"}),
            visibility: ToolVisibility::Public,
        }],
        config_id: Some("server".to_owned()),
        host_id: Some("fixture-host".to_owned()),
        pid: None,
        resources: None,
        prompts: None,
        extras: None,
    };
    let request = WireMessage::Request {
        id: Id::String("fixture-register".to_owned()),
        method: methods::SESSION_REGISTER.to_owned(),
        params: serde_json::to_value(params).expect("serialize fixture registration"),
    };
    ws.send(Message::Text(request.to_text()))
        .await
        .expect("send fake DEV registration");
    let response = timeout(REQUEST_TIMEOUT, ws.next())
        .await
        .expect("registration response timeout")
        .expect("registration socket closed")
        .expect("registration websocket error");
    let Message::Text(response) = response else {
        panic!("expected registration text response");
    };
    let response = WireMessage::parse(&response).expect("parse registration response");
    match response {
        WireMessage::Response { result: Ok(_), .. } => {}
        _ => panic!("fake DEV route registration failed"),
    }
}

struct HttpResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

async fn send_mcp_post(
    port: u16,
    session_id: Option<&str>,
    assertion: Option<&str>,
    payload: Value,
) -> HttpResponse {
    let stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect isolated MCP HTTP endpoint");
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .expect("perform isolated MCP HTTP handshake");
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let mut builder = Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream");
    if let Some(session_id) = session_id {
        builder = builder.header("mcp-session-id", session_id);
    }
    if let Some(assertion) = assertion {
        builder = builder.header(
            ASSERTION_HEADER,
            HeaderValue::from_str(assertion).expect("JWT is a valid HTTP header value"),
        );
    }
    let request = builder
        .body(Full::new(Bytes::from(
            serde_json::to_vec(&payload).expect("serialize MCP request"),
        )))
        .expect("build MCP request");
    let response = timeout(REQUEST_TIMEOUT, sender.send_request(request))
        .await
        .expect("MCP HTTP response timeout")
        .expect("send isolated MCP HTTP request");
    let status = response.status();
    let headers = response.headers().clone();
    let body = timeout(REQUEST_TIMEOUT, response.into_body().collect())
        .await
        .expect("MCP HTTP body timeout")
        .expect("collect MCP HTTP body")
        .to_bytes();
    HttpResponse {
        status,
        headers,
        body,
    }
}

async fn call_managed_tool(
    port: u16,
    session_id: &str,
    request_id: u64,
    assertion: Option<&str>,
) -> Value {
    let response = send_mcp_post(
        port,
        Some(session_id),
        assertion,
        json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {"name":"execute_query","arguments":{}}
        }),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    parse_mcp_response(&response.body)
}

fn parse_mcp_response(body: &[u8]) -> Value {
    if let Ok(value) = serde_json::from_slice(body) {
        return value;
    }
    let text = std::str::from_utf8(body).expect("MCP response must be UTF-8");
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            if !data.is_empty() && data != "[DONE]" {
                return serde_json::from_str(data)
                    .expect("SSE data must contain JSON-RPC response");
            }
        }
    }
    panic!("MCP response does not contain a JSON-RPC payload");
}

fn tool_error_code(response: &Value) -> &str {
    response["result"]["structuredContent"]["error"]["code"]
        .as_str()
        .expect("tool response must contain a structured safe error code")
}

fn sign_unique_assertion(private_key: &[u8], conversation_id: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before UNIX epoch")
        .as_secs();
    let claims = ConversationClaims {
        conversation_id,
        jti: Uuid::new_v4().to_string(),
        iat: now,
        exp: now + 60,
        aud: BROKER_AUDIENCE,
        iss: BROKER_ISSUER,
    };
    encode(
        &Header::new(Algorithm::EdDSA),
        &claims,
        &EncodingKey::from_ed_pem(private_key).expect("parse ephemeral Ed25519 private key"),
    )
    .expect("sign ephemeral broker assertion")
}
