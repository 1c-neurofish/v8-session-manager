//! MCP HTTP transport adapter wired to the session-manager.
//!
//! После урезания (#5/post-extraction) менеджер выполняет ТОЛЬКО роль
//! агрегатора и точки доступа к проксированным tool'ам клиентов. Здесь:
//!
//! - `serve_session_manager(config, overrides)` — entry point: WS endpoint
//!   `:4000/sessions` + MCP HTTP `:4001/mcp`, делящие один
//!   `Arc<SessionRegistry>`.
//! - `McpToolServer` с `tool_router`, содержащим **только** `session.list`.
//!   Остальные `session.*`-tool'ы (`call`/`spawn`/`kill`/`swap`) удалены —
//!   AI вызывает прокси-tool'ы клиентских сессий напрямую через
//!   `<prefix>__<tool>` маршруты.
//! - Прокси к публикуемым tool'ам клиентских сессий через `ProxyRouter`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{
    header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE},
    Method, Request, Response, StatusCode,
};
use rmcp::{
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResult, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
    tool, tool_router,
    transport::{
        streamable_http_server::{
            session::{
                local::{LocalSessionManager, SessionConfig},
                SessionId,
            },
            StreamableHttpServerConfig,
        },
        StreamableHttpService,
    },
    ErrorData, RoleServer, ServerHandler,
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::config::model::AppConfig;
use crate::session_manager::dispatcher::DispatcherError;
use crate::session_manager::management;
use crate::session_manager::notify::{spawn_notifier, ToolsListChangedNotifier, DEBOUNCE_WINDOW};
use crate::session_manager::protocol::{ToolCallParams, ToolCallResult};
use crate::session_manager::registry::SessionRegistry;
use crate::session_manager::router::{
    arguments_to_value, build_proxy_view, proxy_tools, resolve_published, ProxyRouter, ResolveError,
};
use crate::session_manager::tools_cache::{ToolsCacheConfig, ToolsCacheStore};
use crate::session_manager::transport as session_transport;

const HTTP_BODY_LIMIT_BYTES: usize = 1024 * 1024;

/// Bootstrap errors returned by MCP transports.
#[derive(Debug, Error)]
pub enum McpServerError {
    #[error("failed to build tokio runtime for MCP transport: {0}")]
    BuildRuntime(std::io::Error),

    #[error("failed to bind MCP HTTP listener on {address}: {source}")]
    BindHttp {
        address: String,
        source: std::io::Error,
    },

    #[error("MCP transport task failed: {0}")]
    Task(String),
}

/// Запускает менеджер клиентских сессий: WS transport + MCP HTTP server,
/// делящие один и тот же `Arc<SessionRegistry>`.
///
/// CLI overrides уже применены вызывающим кодом (`app::run`).
pub fn serve_session_manager(config: AppConfig) -> Result<(), McpServerError> {
    maybe_install_metrics(&config);
    if config.mcp.http.auth_token.is_none() {
        tracing::warn!("mcp.http.auth_token is not set — MCP HTTP endpoint is open");
    }

    if config.mcp.session_manager.is_none() {
        tracing::info!(
            "mcp.session_manager is not configured — using defaults (ws bind {})",
            crate::config::model::McpSessionManagerConfig::default().bind_address
        );
    }
    let session_cfg = config.mcp.session_manager.clone().unwrap_or_default();

    let shutdown_timeout = shutdown_grace_period(&config);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("v8-session-mgr")
        .build()
        .map_err(McpServerError::BuildRuntime)?;

    let result = runtime.block_on(async move {
        let config = Arc::new(config);
        let shutdown = CancellationToken::new();

        let registry = Arc::new(SessionRegistry::new());

        // ADR-0035: persistent tools cache. Кешовый storage_path резолвится
        // от workPath, если в YAML дан относительный путь или None.
        let cache_config = build_tools_cache_config(&config);
        let cache_notifier = {
            let reg = Arc::clone(&registry);
            Arc::new(move || reg.mark_tools_changed_external()) as Arc<dyn Fn() + Send + Sync>
        };
        let tools_cache = ToolsCacheStore::load_or_empty(&cache_config, cache_notifier);
        registry.attach_tools_cache(Arc::clone(&tools_cache));

        let lifecycle = crate::session_manager::lifecycle::LifecycleManager::new(
            Arc::clone(&registry),
            session_cfg.graceful_kill_grace_ms,
        );

        let server = McpToolServer::new(config.clone())
            .with_session_registry(Arc::clone(&registry))
            .with_tools_cache(Arc::clone(&tools_cache));
        let notifier = server.tools_changed_notifier();
        let notifier_task = spawn_notifier(
            Arc::clone(&registry),
            Arc::clone(&notifier),
            DEBOUNCE_WINDOW,
        );

        let idle_timeout = std::time::Duration::from_secs(session_cfg.idle_timeout_secs);
        let sweeper_task = crate::session_manager::lifecycle::run_idle_sweeper(
            Arc::clone(&lifecycle),
            idle_timeout,
            std::time::Duration::from_secs(1),
        );

        let service = HttpMcpService::new(server, config.clone(), shutdown.child_token());

        let http_listener = tokio::net::TcpListener::bind(config.mcp.http.bind_address.as_str())
            .await
            .map_err(|source| McpServerError::BindHttp {
                address: config.mcp.http.bind_address.clone(),
                source,
            })?;
        let mcp_router = axum::Router::new().route(
            config.mcp.http.path.as_str(),
            axum::routing::any({
                let service = service.clone();
                move |request| {
                    let service = service.clone();
                    async move { service.handle(request).await }
                }
            }),
        );
        let mcp_serve = axum::serve(http_listener, mcp_router).with_graceful_shutdown({
            let shutdown = shutdown.clone();
            async move { shutdown.cancelled().await }
        });

        let server_version = format!("v8-session-manager/{}", env!("CARGO_PKG_VERSION"));
        let ws_bind_address = session_cfg.bind_address.clone();
        let running = session_transport::start(Arc::clone(&registry), session_cfg, server_version)
            .await
            .map_err(|source| McpServerError::BindHttp {
                address: ws_bind_address,
                source,
            })?;

        tracing::info!(
            mcp_http = %config.mcp.http.bind_address,
            ws = %running.local_addr,
            "session-manager: both surfaces are listening"
        );

        let shutdown_signal = {
            let shutdown = shutdown.clone();
            async move {
                wait_for_shutdown_signal().await;
                shutdown.cancel();
            }
        };

        let mcp_result = tokio::select! {
            res = mcp_serve => res.map_err(|e| McpServerError::Task(e.to_string())),
            _ = shutdown_signal => Ok(()),
        };

        running.shutdown();
        notifier_task.abort();
        lifecycle.cancel_token().cancel();
        let _ = sweeper_task.await;
        drop(service);
        shutdown.cancel();
        mcp_result
    });

    runtime.shutdown_timeout(shutdown_timeout);
    result
}

/// rmcp-backed MCP transport adapter exposing `session.list` + a proxy
/// router to per-session published tools.
#[derive(Clone)]
pub struct McpToolServer {
    #[allow(dead_code)]
    config: Arc<AppConfig>,
    session_registry: Arc<SessionRegistry>,
    proxy_router: Arc<ProxyRouter>,
    tools_changed_notifier: Arc<ToolsListChangedNotifier>,
    /// ADR-0035: persistent tools cache. `None` для unit-тестов, которые
    /// конструируют сервер без `serve_session_manager` (поведение — ADR-0034).
    tools_cache: Option<Arc<ToolsCacheStore>>,
    tool_router: ToolRouter<Self>,
}

impl McpToolServer {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            config,
            session_registry: Arc::new(SessionRegistry::new()),
            proxy_router: Arc::new(ProxyRouter::new()),
            tools_changed_notifier: ToolsListChangedNotifier::new(),
            tools_cache: None,
            tool_router: Self::tool_router(),
        }
    }

    pub fn with_session_registry(mut self, registry: Arc<SessionRegistry>) -> Self {
        self.session_registry = registry;
        self
    }

    pub fn with_tools_cache(mut self, cache: Arc<ToolsCacheStore>) -> Self {
        self.tools_cache = Some(cache);
        self
    }

    pub fn tools_changed_notifier(&self) -> Arc<ToolsListChangedNotifier> {
        Arc::clone(&self.tools_changed_notifier)
    }
}

fn build_tools_cache_config(config: &AppConfig) -> ToolsCacheConfig {
    let storage_path = config
        .tools_cache
        .storage_path
        .as_ref()
        .map(|p| {
            if p.is_absolute() {
                p.clone()
            } else {
                config.work_path.join(p)
            }
        })
        .unwrap_or_else(|| config.work_path.join("tools_cache.json"));
    ToolsCacheConfig {
        enabled: config.tools_cache.enabled,
        cache_life: config.tools_cache.cache_life_period,
        storage_path,
    }
}

#[tool_router(router = tool_router)]
impl McpToolServer {
    #[tool(description = "List active session-manager sessions (1C clients connected via WS)")]
    async fn session_list(
        &self,
        Parameters(_request): Parameters<crate::mcp::request::McpSessionListRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = management::list(&self.session_registry);
        let value = serde_json::to_value(&result)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::structured(value))
    }

    /// ADR-0035: сброс persistent tools cache. Без `config_id` — очистить
    /// весь кеш; с `config_id` — удалить только запись с этим `config_id`.
    /// После сброса менеджер отправит `notifications/tools/list_changed`.
    #[tool(description = "Reset cached proxied tools (full or by config_id).")]
    async fn tools_cache_reset(
        &self,
        Parameters(req): Parameters<crate::mcp::request::McpToolsCacheResetRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        let Some(cache) = self.tools_cache.as_ref() else {
            return Ok(CallToolResult::structured(serde_json::json!({
                "removed": 0,
                "enabled": false,
                "message": "tools_cache is not configured"
            })));
        };
        let (removed, scope) = match req.config_id.as_deref() {
            Some(cid) if !cid.is_empty() => (cache.reset_by_config_id(cid), "by_config_id"),
            _ => (cache.reset_all(), "all"),
        };
        let value = serde_json::json!({
            "removed": removed,
            "scope": scope,
            "enabled": cache.enabled(),
        });
        Ok(CallToolResult::structured(value))
    }
}

impl ServerHandler for McpToolServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.tools_changed_notifier.register_peer(&context.peer);
        Ok(ListToolsResult {
            tools: self.list_all_tools_inner(),
            next_cursor: None,
            meta: None,
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        if let Some(t) = self.tool_router.get(name).cloned() {
            return Some(t);
        }
        let view = build_proxy_view(&self.session_registry);
        view.published
            .iter()
            .find(|s| s.published_name == name)
            .map(|s| {
                let object = s.input_schema.as_object().cloned().unwrap_or_default();
                Tool::new(
                    s.published_name.clone(),
                    s.description.clone().unwrap_or_else(|| {
                        format!("ClientProxy tool from kinds={}", s.kinds.join(","))
                    }),
                    Arc::new(object),
                )
            })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.tools_changed_notifier.register_peer(&context.peer);
        if self.tool_router.has_route(request.name.as_ref()) {
            let ctx = ToolCallContext::new(self, request, context);
            return self.tool_router.call(ctx).await;
        }
        // WARN-2: пробрасываем cancellation token MCP-запроса, чтобы при
        // disconnect/cancel клиента менеджер послал `tool.cancel` в сессию,
        // а не ждал жёсткого 60s дедлайна.
        let cancel = context.ct.clone();
        self.call_proxy_tool_inner(request, cancel).await
    }
}

impl McpToolServer {
    pub fn list_all_tools_inner(&self) -> Vec<Tool> {
        let mut tools = self.tool_router.list_all();
        let view = build_proxy_view(&self.session_registry);
        let live = proxy_tools(&view);

        // ADR-0035: дополняем live-view содержимым persistent cache,
        // дедуплицируя по published_name. Live имеет приоритет — если
        // тул уже опубликован живой сессией, кешевая копия игнорируется.
        let mut seen: HashSet<String> = tools
            .iter()
            .chain(live.iter())
            .map(|t| t.name.to_string())
            .collect();
        tools.extend(live);
        if let Some(cache) = self.tools_cache.as_ref() {
            for entry in cache.list_all() {
                for tool in entry.tools {
                    if seen.insert(tool.name.clone()) {
                        let object = tool.input_schema.as_object().cloned().unwrap_or_default();
                        let description = tool.description.clone().unwrap_or_else(|| {
                            format!(
                                "Cached ClientProxy tool (kind={}, config_id={})",
                                entry.kind, entry.config_id
                            )
                        });
                        tools.push(Tool::new(tool.name, description, Arc::new(object)));
                    }
                }
            }
        }
        tools
    }

    pub(crate) async fn call_proxy_tool_inner(
        &self,
        request: CallToolRequestParams,
        cancellation: CancellationToken,
    ) -> Result<CallToolResult, ErrorData> {
        let view = build_proxy_view(&self.session_registry);
        let resolved = match resolve_published(request.name.as_ref(), &view, &self.proxy_router) {
            Ok(r) => r,
            Err(ResolveError::SchemaConflict { tool_name }) => {
                return Err(ErrorData::invalid_params(
                    format!(
                        "tool '{tool_name}' is hidden due to schema conflict — sessions registered different schemas under the same name"
                    ),
                    None,
                ));
            }
            Err(ResolveError::NoActiveSessions) => {
                return Ok(no_live_session_response(request.name.as_ref(), None));
            }
            Err(_) => {
                // ADR-0035: имя может быть известно через persistent cache,
                // но live-сессии нет. Возвращаем structured tool error
                // вместо method_not_found, чтобы AI-агент получил _meta с
                // error_code="no_live_session".
                if let Some(cache_hit) = self.lookup_cache_entry(request.name.as_ref()) {
                    return Ok(no_live_session_response(
                        request.name.as_ref(),
                        Some(cache_hit),
                    ));
                }
                return Err(ErrorData::method_not_found::<
                    rmcp::model::CallToolRequestMethod,
                >());
            }
        };
        let rec = self
            .session_registry
            .get(&resolved.session_id)
            .ok_or_else(|| {
                ErrorData::internal_error(
                    format!(
                        "session '{}' disappeared between resolve and call",
                        resolved.session_id
                    ),
                    None,
                )
            })?;
        let connection = rec.connection.clone().ok_or_else(|| {
            ErrorData::internal_error(
                format!("session '{}' has no active connection", resolved.session_id),
                None,
            )
        })?;
        let params = ToolCallParams {
            name: resolved.tool_name,
            arguments: arguments_to_value(request.arguments),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        // BLOCKER-1: фиксируем активность сессии до enqueue, чтобы idle sweeper
        // не реапил busy-сессии, у которых last_call_at не обновлялся со
        // времени register (см. lifecycle::sweep_once).
        self.session_registry
            .bump_last_call(&resolved.session_id, std::time::Instant::now());
        let outcome = rec
            .dispatcher
            .enqueue(connection, params, deadline, cancellation)
            .await;
        dispatcher_outcome_to_call_result(outcome)
    }
}

/// Хит в persistent cache: какому `(kind, config_id)` соответствует tool.
#[derive(Debug, Clone)]
struct CacheHit {
    kind: String,
    config_id: String,
}

impl McpToolServer {
    /// Найти кешевую запись, содержащую tool с указанным `name`.
    fn lookup_cache_entry(&self, name: &str) -> Option<CacheHit> {
        let cache = self.tools_cache.as_ref()?;
        for entry in cache.list_all() {
            if entry.tools.iter().any(|t| t.name == name) {
                return Some(CacheHit {
                    kind: entry.kind,
                    config_id: entry.config_id,
                });
            }
        }
        None
    }
}

/// Структурированный MCP tool-error для отсутствующей live-сессии (ADR-0035).
fn no_live_session_response(tool_name: &str, cache_hit: Option<CacheHit>) -> CallToolResult {
    let (kind, config_id) = match cache_hit {
        Some(hit) => (Some(hit.kind), Some(hit.config_id)),
        None => (None, None),
    };
    let mut meta = serde_json::Map::new();
    meta.insert(
        "error_code".to_owned(),
        serde_json::Value::String("no_live_session".to_owned()),
    );
    meta.insert(
        "tool".to_owned(),
        serde_json::Value::String(tool_name.to_owned()),
    );
    if let Some(k) = kind {
        meta.insert("kind".to_owned(), serde_json::Value::String(k));
    }
    if let Some(cid) = config_id {
        meta.insert("config_id".to_owned(), serde_json::Value::String(cid));
    }
    let payload = serde_json::json!({
        "content": [{
            "type": "text",
            "text": format!(
                "Tool '{tool_name}' is currently unavailable: no live session.",
            )
        }],
        "isError": true,
        "_meta": serde_json::Value::Object(meta),
    });
    CallToolResult::structured_error(payload)
}

fn dispatcher_outcome_to_call_result(
    outcome: Result<ToolCallResult, DispatcherError>,
) -> Result<CallToolResult, ErrorData> {
    match outcome {
        Ok(result) => {
            let value = serde_json::to_value(&result)
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            if result.is_error {
                Ok(CallToolResult::structured_error(value))
            } else {
                Ok(CallToolResult::structured(value))
            }
        }
        Err(DispatcherError::CancelledWhileQueued) => Err(ErrorData::internal_error(
            "tool.call cancelled while queued",
            None,
        )),
        Err(DispatcherError::Cancelled) => Err(ErrorData::internal_error(
            "tool.call cancelled during execution",
            None,
        )),
        Err(DispatcherError::TimedOutWhileQueued) => Err(ErrorData::internal_error(
            "tool.call timed out while queued",
            None,
        )),
        Err(DispatcherError::TimedOutWhileRunning) => Err(ErrorData::internal_error(
            "tool.call timed out during execution",
            None,
        )),
        Err(DispatcherError::SessionGone) => Err(ErrorData::invalid_params(
            "session_gone: target session is disconnected",
            None,
        )),
        Err(DispatcherError::ClientError(e)) => Err(ErrorData::internal_error(
            format!("client error {}: {}", e.code, e.message),
            None,
        )),
        Err(DispatcherError::InvalidResult(msg)) => Err(ErrorData::internal_error(msg, None)),
        Err(DispatcherError::WriterClosed) => Err(ErrorData::internal_error(
            "session connection writer closed",
            None,
        )),
    }
}

// ---------------------------------------------------------------------------
// HTTP MCP wrapper (admission control, auth, initialize handshake)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct HttpSessionAdmissionState {
    reserved: usize,
    active_sessions: HashSet<SessionId>,
}

#[derive(Clone)]
struct HttpSessionAdmission {
    max_sessions: usize,
    session_manager: Arc<LocalSessionManager>,
    state: Arc<Mutex<HttpSessionAdmissionState>>,
}

impl HttpSessionAdmission {
    fn new(max_sessions: usize, session_manager: Arc<LocalSessionManager>) -> Self {
        Self {
            max_sessions: max_sessions.max(1),
            session_manager,
            state: Arc::new(Mutex::new(HttpSessionAdmissionState::default())),
        }
    }

    async fn reserve_initialize(&self) -> Result<HttpSessionReservation, HttpOverloadError> {
        let live_sessions = self.session_manager.sessions.read().await;
        let mut state = self
            .state
            .lock()
            .expect("http session admission mutex poisoned");
        state
            .active_sessions
            .retain(|session_id| live_sessions.contains_key(session_id));
        if state.active_sessions.len() + state.reserved >= self.max_sessions {
            return Err(HttpOverloadError);
        }
        state.reserved += 1;
        drop(state);
        drop(live_sessions);

        Ok(HttpSessionReservation {
            admission: self.clone(),
            completed: false,
        })
    }

    fn confirm(&self, session_id: SessionId) {
        let mut state = self
            .state
            .lock()
            .expect("http session admission mutex poisoned");
        state.reserved = state.reserved.saturating_sub(1);
        state.active_sessions.insert(session_id);
    }

    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .expect("http session admission mutex poisoned");
        state.reserved = state.reserved.saturating_sub(1);
    }

    fn remove(&self, session_id: &SessionId) {
        let mut state = self
            .state
            .lock()
            .expect("http session admission mutex poisoned");
        state.active_sessions.remove(session_id);
    }
}

struct HttpSessionReservation {
    admission: HttpSessionAdmission,
    completed: bool,
}

impl HttpSessionReservation {
    fn confirm(mut self, session_id: SessionId) {
        self.admission.confirm(session_id);
        self.completed = true;
    }

    fn release(mut self) {
        self.admission.release();
        self.completed = true;
    }
}

impl Drop for HttpSessionReservation {
    fn drop(&mut self) {
        if !self.completed {
            self.admission.release();
            self.completed = true;
        }
    }
}

#[derive(Debug)]
struct HttpOverloadError;

#[derive(Clone)]
struct HttpMcpService {
    inner: StreamableHttpService<McpToolServer, LocalSessionManager>,
    admission: HttpSessionAdmission,
    stateful_sessions: bool,
    auth_token: Option<String>,
}

impl HttpMcpService {
    fn new(server: McpToolServer, config: Arc<AppConfig>, shutdown: CancellationToken) -> Self {
        let auth_token = config.mcp.http.auth_token.clone();
        let session_manager = Arc::new(LocalSessionManager {
            session_config: SessionConfig {
                keep_alive: config
                    .mcp
                    .http
                    .stateful_sessions
                    .then_some(Duration::from_secs(config.mcp.http.idle_ttl_secs.max(1))),
                ..Default::default()
            },
            ..Default::default()
        });
        let admission =
            HttpSessionAdmission::new(config.mcp.http.max_sessions, session_manager.clone());
        let inner = StreamableHttpService::new(
            move || Ok(server.clone()),
            session_manager,
            StreamableHttpServerConfig {
                stateful_mode: config.mcp.http.stateful_sessions,
                cancellation_token: shutdown,
                ..Default::default()
            },
        );

        Self {
            inner,
            admission,
            stateful_sessions: config.mcp.http.stateful_sessions,
            auth_token,
        }
    }

    async fn handle(&self, request: Request<Body>) -> Response<Body> {
        let session_id = session_id_from_headers(request.headers());
        let method = request.method().clone();
        let is_initialize_candidate =
            self.stateful_sessions && method == Method::POST && session_id.is_none();

        if let Some(token) = &self.auth_token {
            if let Some(reject) = check_bearer_auth(&request, token) {
                return reject;
            }
        }

        if !is_initialize_candidate {
            let response = self.inner.handle(request).await.map(Body::new);
            if method == Method::DELETE && response.status() == StatusCode::ACCEPTED {
                if let Some(session_id) = session_id {
                    self.admission.remove(&session_id);
                }
            }
            return response;
        }

        if !valid_streamable_post_headers(request.headers()) {
            return self.inner.handle(request).await.map(Body::new);
        }

        let (request, rpc_method) = match extract_http_rpc_method(request).await {
            Ok(result) => result,
            Err(response) => return response,
        };
        if rpc_method.as_deref() != Some("initialize") {
            let _ = request;
            return missing_initialize_response();
        }

        let reservation = match self.admission.reserve_initialize().await {
            Ok(reservation) => reservation,
            Err(_) => return overload_response(),
        };

        let response = self.inner.handle(request).await.map(Body::new);
        if response.status() == StatusCode::OK {
            if let Some(session_id) = session_id_from_headers(response.headers()) {
                reservation.confirm(session_id);
            } else {
                reservation.release();
            }
        } else {
            reservation.release();
        }

        response
    }
}

async fn extract_http_rpc_method(
    request: Request<Body>,
) -> Result<(Request<Body>, Option<String>), Response<Body>> {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, HTTP_BODY_LIMIT_BYTES)
        .await
        .map_err(|error| {
            Response::builder()
                .status(StatusCode::PAYLOAD_TOO_LARGE)
                .header(
                    CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; charset=utf-8"),
                )
                .body(Body::from(format!("Payload Too Large: {error}")))
                .expect("valid overload response")
        })?;
    let method = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("method")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        });
    Ok((Request::from_parts(parts, Body::from(body)), method))
}

fn overload_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(
            CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )
        .body(Body::from(
            "Service Unavailable: MCP session capacity exhausted",
        ))
        .expect("valid overload response")
}

fn missing_initialize_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header(
            CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )
        .body(Body::from(
            "Bad Request: initialize request is required before session creation",
        ))
        .expect("valid missing initialize response")
}

fn session_id_from_headers(headers: &axum::http::HeaderMap) -> Option<SessionId> {
    headers
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(Into::into)
}

fn valid_streamable_post_headers(headers: &axum::http::HeaderMap) -> bool {
    let accepts_both = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.contains("application/json") && value.contains("text/event-stream")
        });
    let content_type_is_json = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));

    accepts_both && content_type_is_json
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn shutdown_grace_period(config: &AppConfig) -> Duration {
    Duration::from_secs(config.mcp.execution.shutdown_grace_period_secs.max(1))
}

fn maybe_install_metrics(config: &AppConfig) {
    if let Some(addr) = &config.mcp.metrics.bind_address {
        if addr.is_empty() {
            return;
        }
        match crate::session_manager::metrics::install_prometheus_exporter(addr) {
            Ok(()) => tracing::info!(bind_address = %addr, "metrics: Prometheus exporter started"),
            Err(e) => tracing::warn!("metrics: failed to start Prometheus exporter: {}", e),
        }
    }
}

/// Validate the `Authorization: Bearer <token>` header.
fn check_bearer_auth(request: &Request<Body>, expected_token: &str) -> Option<Response<Body>> {
    let auth_header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let provided = match auth_header {
        Some(h) if h.starts_with("Bearer ") => &h[7..],
        _ => {
            return Some(
                Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .header(
                        "WWW-Authenticate",
                        HeaderValue::from_static("Bearer realm=\"mcp\""),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
        }
    };

    if provided == expected_token {
        None
    } else {
        Some(
            Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::empty())
                .unwrap(),
        )
    }
}
