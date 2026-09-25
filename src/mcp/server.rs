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
//!   AI вызывает прокси-tool'ы клиентских сессий напрямую по голому имени,
//!   при необходимости указывая `session_id`.
//! - Прокси к публикуемым tool'ам клиентских сессий с точной маршрутизацией
//!   по зарезервированному аргументу `session_id`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{
    header::{HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE},
    Method, Request, Response, StatusCode,
};
use rmcp::{
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
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
use crate::session_manager::masking::client::FinalizeOutcome;
use crate::session_manager::masking::gate::transport_error_outcome;
use crate::session_manager::masking::internal::spawn_internal_endpoint;
use crate::session_manager::masking::{MaskingFailure, MaskingGate, TrustedConversationContext};
use crate::session_manager::notify::{spawn_notifier, ToolsListChangedNotifier, DEBOUNCE_WINDOW};
use crate::session_manager::protocol::{ToolCallParams, ToolCallResult};
use crate::session_manager::registry::SessionRegistry;
use crate::session_manager::router::{
    build_proxy_view, input_schema_with_session_selector, proxy_tools, resolve_published,
    ResolveError, SESSION_ID_ARGUMENT,
};
use crate::session_manager::tools_cache::{ToolsCacheConfig, ToolsCacheStore};
use crate::session_manager::transport as session_transport;

const HTTP_BODY_LIMIT_BYTES: usize = 1024 * 1024;
const CLIENT_PROXY_CALL_TIMEOUT_SECS: u64 = 60;
const SERVER_PROXY_CALL_TIMEOUT_SECS: u64 = 3_600;
const MANAGER_CALL_OPTIONS_KEY: &str = "_v8_call_options";
const MANAGER_NO_DEADLINE_KEY: &str = "no_deadline";

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

    #[error("invalid masking integration configuration: {0}")]
    MaskingConfig(String),
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

        let server = McpToolServer::try_new(config.clone())?
            .with_session_registry(Arc::clone(&registry))
            .with_tools_cache(Arc::clone(&tools_cache));
        let notifier = server.tools_changed_notifier();
        // Internal UDS endpoint для вызовов сервиса маскирования → менеджер
        // (`POST /internal/v1/tools/call`); None при masking.enabled=false.
        let internal_endpoint_task = spawn_internal_endpoint(
            Arc::clone(&server.masking_gate),
            Arc::clone(&registry),
            shutdown.child_token(),
        );
        let terminal_replay_task =
            Arc::clone(&server.masking_gate).spawn_terminal_replay(shutdown.child_token());
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

        let session_masking_gate = Arc::clone(&server.masking_gate);
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
        let running = session_transport::start_with_masking(
            Arc::clone(&registry),
            session_cfg,
            server_version,
            session_masking_gate,
        )
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
        if let Some(task) = internal_endpoint_task {
            task.abort();
        }
        terminal_replay_task.abort();
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
    tools_changed_notifier: Arc<ToolsListChangedNotifier>,
    /// ADR-0035: persistent tools cache. `None` для unit-тестов, которые
    /// конструируют сервер без `serve_session_manager` (поведение — ADR-0034).
    tools_cache: Option<Arc<ToolsCacheStore>>,
    masking_gate: Arc<MaskingGate>,
    tool_router: ToolRouter<Self>,
}

impl McpToolServer {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self::try_new(config).expect("default/test masking configuration must be valid")
    }

    pub fn try_new(config: Arc<AppConfig>) -> Result<Self, McpServerError> {
        let masking_gate = Arc::new(
            MaskingGate::from_config(&config.masking, &config.work_path)
                .map_err(McpServerError::MaskingConfig)?,
        );
        Ok(Self {
            config,
            session_registry: Arc::new(SessionRegistry::new()),
            tools_changed_notifier: ToolsListChangedNotifier::new(),
            tools_cache: None,
            masking_gate,
            tool_router: Self::tool_router(),
        })
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
        proxy_tools(&view)
            .into_iter()
            .find(|tool| tool.name.as_ref() == name)
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
        let conversation = trusted_conversation_context(&context);
        self.call_proxy_tool_inner(request, cancel, conversation)
            .await
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
                    if !tool.visibility.is_public() {
                        continue;
                    }
                    if seen.insert(tool.name.clone()) {
                        // У кешевой записи нет live session_id, но схема должна
                        // честно показывать зарезервированный аргумент менеджера.
                        let schema =
                            input_schema_with_session_selector(&tool.input_schema, &[], false);
                        let object = schema.as_object().cloned().unwrap_or_default();
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
        trusted_conversation: Option<TrustedConversationContext>,
    ) -> Result<CallToolResult, ErrorData> {
        let (arguments, requested_session_id, no_deadline_requested) =
            extract_manager_arguments(request.arguments)?;
        if let Some(session_id) = requested_session_id.as_deref() {
            validate_explicit_target(&self.session_registry, session_id, request.name.as_ref())?;
        }
        let view = build_proxy_view(&self.session_registry);
        let resolved = match resolve_published(
            request.name.as_ref(),
            &view,
            requested_session_id.as_deref(),
        ) {
            Ok(r) => r,
            Err(ResolveError::SessionRequired {
                tool_name,
                session_ids,
            }) => {
                return Err(ErrorData::invalid_params(
                    format!(
                        "tool '{tool_name}' has multiple active targets; session_id is required (candidates: {})",
                        session_ids.join(", ")
                    ),
                    None,
                ));
            }
            Err(ResolveError::SessionToolMismatch {
                tool_name,
                session_id,
            }) => {
                return Err(ErrorData::invalid_params(
                    format!("session '{session_id}' does not publish tool '{tool_name}'"),
                    None,
                ));
            }
            Err(ResolveError::NotProxyTool) => {
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
                //++agent TASK-225 [25.09.2026]
                // Неизвестное имя — structured tool error вместо JSON-RPC
                // method_not_found: протокольная ошибка уходит агенту без
                // content-блока (клиенты показывают "no text content"),
                // а несуществующий инструмент — это ошибка вызова, а не
                // протокола. Поведение симметрично no_live_session (ADR-0035).
                //++agent TASK-225
                return Ok(unknown_tool_response(request.name.as_ref()));
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
        if rec.state != crate::session_manager::registry::SessionState::Active {
            return Err(ErrorData::invalid_params(
                format!("session '{}' is not active", resolved.session_id),
                None,
            ));
        }
        //++agent TASK-225 [25.09.2026]
        // При masking.enabled каждый публичный proxy tools/call идёт через
        // gate — managed_tools больше не участвует в маршруте. Проверка
        // привязки к database_id идёт ПЕРВОЙ: непривязанная сессия получает
        // DATABASE_IDENTITY_UNVERIFIED даже при отсутствующем assertion.
        //++agent TASK-225
        let (masking_context, arguments) = if self.masking_gate.is_enabled() {
            let call_id = uuid::Uuid::new_v4().to_string();
            let correlation_id = uuid::Uuid::new_v4().to_string();
            let Some(identity) = self.masking_gate.verify_database(&rec) else {
                let failure = MaskingFailure::with_correlation(
                    "DATABASE_IDENTITY_UNVERIFIED",
                    "Идентичность базы не подтверждена",
                    correlation_id.clone(),
                );
                let terminal = self.masking_gate.unverified_terminal(
                    call_id,
                    correlation_id.clone(),
                    request.name.as_ref(),
                    &failure.code,
                );
                if self.masking_gate.record_terminal(terminal).await.is_err() {
                    return Ok(masking_failure_response(MaskingFailure::with_correlation(
                        "HISTORY_UNAVAILABLE",
                        "Операция временно недоступна",
                        correlation_id,
                    )));
                }
                return Ok(masking_failure_response(failure));
            };
            let Some(conversation) = trusted_conversation.as_ref() else {
                let failure = MaskingFailure::with_correlation(
                    "CHAT_IDENTITY_REQUIRED",
                    "Для операции требуется подтверждённый контекст диалога",
                    correlation_id.clone(),
                );
                let terminal = self.masking_gate.unverified_terminal(
                    call_id,
                    correlation_id.clone(),
                    request.name.as_ref(),
                    &failure.code,
                );
                if self.masking_gate.record_terminal(terminal).await.is_err() {
                    return Ok(masking_failure_response(MaskingFailure::with_correlation(
                        "HISTORY_UNAVAILABLE",
                        "Операция временно недоступна",
                        correlation_id,
                    )));
                }
                return Ok(masking_failure_response(failure));
            };
            let context = self.masking_gate.call_context(
                identity,
                conversation,
                request.name.as_ref(),
                call_id,
                correlation_id,
            );
            match self.masking_gate.preflight(context, arguments).await {
                Ok((context, arguments)) => (Some(context), arguments),
                Err(failure) => return Ok(masking_failure_response(failure)),
            }
        } else {
            (None, arguments)
        };
        let connection = rec.connection.clone().ok_or_else(|| {
            ErrorData::internal_error(
                format!("session '{}' has no active connection", resolved.session_id),
                None,
            )
        })?;
        let deadline = proxy_call_deadline(&rec.kind, no_deadline_requested)?;
        let params = ToolCallParams {
            name: resolved.tool_name,
            arguments,
        };
        // BLOCKER-1: фиксируем активность сессии до enqueue, чтобы idle sweeper
        // не реапил busy-сессии, у которых last_call_at не обновлялся со
        // времени register (см. lifecycle::sweep_once).
        self.session_registry
            .bump_last_call(&resolved.session_id, std::time::Instant::now());
        let outcome = rec
            .dispatcher
            .enqueue(connection, params, deadline, cancellation.clone())
            .await;
        if let Some(masking_context) = masking_context.as_ref() {
            //++agent TASK-225 [25.09.2026]
            // Результат без конверта границы данных — непрозрачный JSON
            // (весь ToolCallResult) с пустыми field_sources: конверт
            // требуется только для data-mask класса; сломанный конверт
            // (есть schema_version, но не валиден) — fail-closed отказ,
            // а не opaque-проход.
            //++agent TASK-225
            let (finalize_outcome, field_sources) = match outcome {
                Ok(result) => match extract_tool_payload(result) {
                    ToolPayload::Envelope {
                        result,
                        field_sources,
                    } => (FinalizeOutcome::ToolResult { result }, Some(field_sources)),
                    ToolPayload::Opaque { result } => {
                        (FinalizeOutcome::ToolResult { result }, None)
                    }
                    ToolPayload::Malformed { code } => (transport_error_outcome(code), None),
                },
                Err(ref error) => (transport_error_outcome(dispatcher_error_code(error)), None),
            };
            return match self
                .masking_gate
                .finalize(masking_context, finalize_outcome, field_sources)
                .await
            {
                Ok(result) => dispatcher_outcome_to_call_result(Ok(result)),
                Err(failure) => Ok(masking_failure_response(failure)),
            };
        }
        dispatcher_outcome_to_call_result(outcome)
    }
}

fn trusted_conversation_context(
    context: &RequestContext<RoleServer>,
) -> Option<TrustedConversationContext> {
    context
        .extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<TrustedConversationContext>())
        .cloned()
}

/// Конверт результата managed tool из первого text-блока `content`
/// (boundary формат mcp_tools, контракт TASK-222 П1). Менеджер валидирует
/// только конверт и не интерпретирует `result`/`field_sources`: `result`
/// непрозрачен (контракт Р2) и передаётся сервису как JSON-значение.
#[derive(serde::Deserialize)]
struct MaskingResultEnvelope {
    schema_version: u8,
    result: serde_json::Value,
    /// Field-origin информация для сервиса маскирования — идёт в finalize
    /// как field_sources без manager-side интерпретации.
    field_sources: serde_json::Value,
}

//++agent TASK-225 [25.09.2026]
/// Разбор результата proxy-вызова для finalize: конверт границы данных
/// ИЛИ непрозрачный результат. Третьего пути нет — сырой ответ агенту
/// минуя сервис не возвращается.
enum ToolPayload {
    /// Валидный конверт {schema_version:1, result, field_sources}.
    Envelope {
        result: serde_json::Value,
        field_sources: serde_json::Value,
    },
    /// Результат без конверта: весь ToolCallResult как JSON
    /// ({content, is_error, structured_content}), field_sources пустые.
    Opaque { result: serde_json::Value },
    /// Текст объявляет конверт (top-level `schema_version`/`field_sources`),
    /// но не соответствует контракту — fail-closed, opaque не спасает.
    Malformed { code: &'static str },
}

fn extract_tool_payload(outer: ToolCallResult) -> ToolPayload {
    // Претензия на конверт — top-level ключи-маркеры конверта в первом
    // text-блоке. `field_sources` — специфичное для конверта имя;
    // одиночный `result` — обычное поле бизнес-JSON и признаком не служит.
    let claims_envelope = outer
        .content
        .first()
        .and_then(|content| match content {
            crate::session_manager::protocol::ToolContent::Text { text } => {
                serde_json::from_str::<serde_json::Value>(text).ok()
            }
            crate::session_manager::protocol::ToolContent::Json { .. } => None,
        })
        .and_then(|value| {
            let object = value.as_object()?;
            (object.contains_key("schema_version") || object.contains_key("field_sources"))
                .then_some(())
        })
        .is_some();
    if claims_envelope {
        let Some(crate::session_manager::protocol::ToolContent::Text { text }) =
            outer.content.into_iter().next()
        else {
            return ToolPayload::Malformed {
                code: "RESULT_INVALID",
            };
        };
        let envelope: MaskingResultEnvelope = match serde_json::from_str(&text) {
            Ok(envelope) => envelope,
            Err(_) => {
                return ToolPayload::Malformed {
                    code: "RESULT_INVALID",
                }
            }
        };
        if envelope.schema_version != 1 {
            return ToolPayload::Malformed {
                code: "RESULT_INVALID",
            };
        }
        return ToolPayload::Envelope {
            result: envelope.result,
            field_sources: envelope.field_sources,
        };
    }
    // Opaque: ToolCallResult сериализуется детерминированно (serde),
    // падение невозможно — но неизменяемость лучше panic.
    match serde_json::to_value(&outer) {
        Ok(result) => ToolPayload::Opaque { result },
        Err(_) => ToolPayload::Malformed {
            code: "RESULT_INVALID",
        },
    }
}
//++agent TASK-225

fn dispatcher_error_code(error: &DispatcherError) -> &'static str {
    match error {
        DispatcherError::CancelledWhileQueued | DispatcherError::Cancelled => "TOOL_CANCELLED",
        DispatcherError::TimedOutWhileQueued | DispatcherError::TimedOutWhileRunning => {
            "TOOL_TIMEOUT"
        }
        DispatcherError::SessionGone | DispatcherError::WriterClosed => "SESSION_GONE",
        DispatcherError::ClientError(_) => "CLIENT_ERROR",
        DispatcherError::InvalidResult(_) => "RESULT_INVALID",
    }
}

fn masking_failure_response(failure: MaskingFailure) -> CallToolResult {
    let mut result = CallToolResult::error(vec![Content::text(failure.message.clone())]);
    result.structured_content = Some(serde_json::json!({
        "error": {
            "code": failure.code,
            "message": failure.message,
            "correlation_id": failure.correlation_id
        }
    }));
    result
}

fn extract_manager_arguments(
    arguments: Option<rmcp::model::JsonObject>,
) -> Result<(serde_json::Value, Option<String>, bool), ErrorData> {
    let mut arguments = arguments.unwrap_or_default();
    let requested_session_id = match arguments.remove(SESSION_ID_ARGUMENT) {
        Some(serde_json::Value::String(value)) if !value.is_empty() => Some(value),
        None => None,
        Some(_) => {
            return Err(ErrorData::invalid_params(
                format!("{SESSION_ID_ARGUMENT} must be a non-empty string"),
                None,
            ));
        }
    };
    let Some(options) = arguments.remove(MANAGER_CALL_OPTIONS_KEY) else {
        return Ok((
            serde_json::Value::Object(arguments),
            requested_session_id,
            false,
        ));
    };
    let serde_json::Value::Object(mut options) = options else {
        return Err(ErrorData::invalid_params(
            format!("{MANAGER_CALL_OPTIONS_KEY} must be an object"),
            None,
        ));
    };
    let no_deadline = match options.remove(MANAGER_NO_DEADLINE_KEY) {
        Some(serde_json::Value::Bool(value)) => value,
        None => false,
        Some(_) => {
            return Err(ErrorData::invalid_params(
                format!("{MANAGER_CALL_OPTIONS_KEY}.{MANAGER_NO_DEADLINE_KEY} must be boolean"),
                None,
            ));
        }
    };
    if !options.is_empty() {
        let mut keys = options.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        return Err(ErrorData::invalid_params(
            format!(
                "unknown {MANAGER_CALL_OPTIONS_KEY} keys: {}",
                keys.join(", ")
            ),
            None,
        ));
    }
    Ok((
        serde_json::Value::Object(arguments),
        requested_session_id,
        no_deadline,
    ))
}

fn validate_explicit_target(
    registry: &SessionRegistry,
    session_id: &str,
    tool_name: &str,
) -> Result<(), ErrorData> {
    let rec = registry.get(session_id).ok_or_else(|| {
        ErrorData::invalid_params(format!("unknown session_id '{session_id}'"), None)
    })?;
    if rec.state != crate::session_manager::registry::SessionState::Active {
        return Err(ErrorData::invalid_params(
            format!("session '{session_id}' is not active"),
            None,
        ));
    }
    if !rec
        .tools
        .iter()
        .any(|tool| tool.name == tool_name && tool.visibility.is_public())
    {
        return Err(ErrorData::invalid_params(
            format!("session '{session_id}' does not publish tool '{tool_name}'"),
            None,
        ));
    }
    Ok(())
}

fn proxy_call_deadline(
    kind: &str,
    no_deadline_requested: bool,
) -> Result<Option<tokio::time::Instant>, ErrorData> {
    if no_deadline_requested {
        if kind == "server" {
            return Ok(None);
        }
        return Err(ErrorData::invalid_params(
            format!(
                "{MANAGER_CALL_OPTIONS_KEY}.{MANAGER_NO_DEADLINE_KEY} is allowed only for server sessions"
            ),
            None,
        ));
    }
    let timeout_secs = if kind == "server" {
        SERVER_PROXY_CALL_TIMEOUT_SECS
    } else {
        CLIENT_PROXY_CALL_TIMEOUT_SECS
    };
    Ok(Some(
        tokio::time::Instant::now() + Duration::from_secs(timeout_secs),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::{MaskingIdentityBinding, McpConfig, ToolsCacheConfig};
    use crate::session_manager::protocol::{SessionRegisterParams, ToolDescriptor};
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::Instant;

    const TEST_BROKER_PUBLIC_KEY: &[u8] = br#"-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAqb56A5d3wWE6xz7XETMTTzhooYvBDkfqBuwYtYdMmvY=
-----END PUBLIC KEY-----
"#;

    fn register_fake_session(registry: &SessionRegistry, session_id: &str, tools: &[&str]) {
        registry
            .register(
                SessionRegisterParams {
                    client_uid: session_id.to_owned(),
                    kind: "client".to_owned(),
                    version: "1.0".to_owned(),
                    infobase_name: "test_db".to_owned(),
                    ib_session_number: 1,
                    tools: tools
                        .iter()
                        .map(|name| ToolDescriptor {
                            name: (*name).to_owned(),
                            description: None,
                            input_schema: json!({"type": "object"}),
                            visibility: Default::default(),
                        })
                        .collect(),
                    config_id: None,
                    host_id: None,
                    pid: None,
                    resources: None,
                    prompts: None,
                    extras: None,
                },
                Instant::now(),
                None,
            )
            .unwrap();
    }

    #[test]
    fn manager_routing_and_call_options_are_removed_from_tool_arguments() {
        let mut args = rmcp::model::JsonObject::new();
        args.insert("payload".to_owned(), json!("value"));
        args.insert(SESSION_ID_ARGUMENT.to_owned(), json!("dev"));
        args.insert(
            MANAGER_CALL_OPTIONS_KEY.to_owned(),
            json!({ MANAGER_NO_DEADLINE_KEY: true }),
        );

        let (cleaned, session_id, no_deadline) = extract_manager_arguments(Some(args)).unwrap();

        assert!(no_deadline);
        assert_eq!(session_id.as_deref(), Some("dev"));
        assert_eq!(cleaned, json!({"payload": "value"}));
    }

    #[test]
    fn invalid_inactive_and_mismatched_explicit_targets_are_rejected() {
        let registry = SessionRegistry::new();
        register_fake_session(&registry, "prod", &["echo"]);
        register_fake_session(&registry, "dev", &["other"]);

        assert!(validate_explicit_target(&registry, "missing", "echo").is_err());
        assert!(validate_explicit_target(&registry, "dev", "echo").is_err());
        registry.mark_disconnected("prod", Instant::now());
        assert!(validate_explicit_target(&registry, "prod", "echo").is_err());
    }

    #[test]
    fn no_deadline_is_allowed_only_for_server_sessions() {
        assert!(proxy_call_deadline("server", true).unwrap().is_none());
        assert!(proxy_call_deadline("client", true).is_err());
    }

    #[test]
    fn default_deadline_remains_for_client_and_server_sessions() {
        assert!(proxy_call_deadline("client", false).unwrap().is_some());
        assert!(proxy_call_deadline("server", false).unwrap().is_some());
    }

    #[test]
    fn masking_envelope_parses_json_from_first_text_content() {
        // Контракт Р2: `result` — непрозрачный JSON; данные бизнес-результата
        // доезжают до сервиса без потери полей (data не теряется).
        let inner = json!({
            "success": true,
            "data": [["ФИО-значение", 42]],
            "truncated": false,
            "custom_shape": {"nested": [1, 2, 3]}
        });
        let outer = ToolCallResult {
            content: vec![crate::session_manager::protocol::ToolContent::Text {
                text: serde_json::to_string(&json!({
                    "schema_version": 1,
                    "result": inner,
                    "field_sources": {"schema": {"columns": []}, "lineage": []}
                }))
                .unwrap(),
            }],
            is_error: false,
            structured_content: None,
        };

        //++agent TASK-225 [25.09.2026]: extract_tool_payload — Envelope.
        let ToolPayload::Envelope {
            result: unwrapped,
            field_sources,
        } = extract_tool_payload(outer)
        else {
            panic!("expected envelope payload");
        };
        //++agent TASK-225
        assert_eq!(unwrapped, inner);
        assert_eq!(unwrapped["data"][0][1], json!(42));
        assert_eq!(unwrapped["custom_shape"]["nested"], json!([1, 2, 3]));
        assert_eq!(field_sources["schema"]["columns"], json!([]));
    }

    //++agent TASK-225 [25.09.2026]
    // Результат без конверта — opaque: сервису уходит весь ToolCallResult
    // (включая is_error) как непрозрачный JSON.
    #[test]
    fn opaque_result_without_envelope_passes_whole_result() {
        let outer = ToolCallResult {
            content: vec![crate::session_manager::protocol::ToolContent::Text {
                text: "plain text answer".to_owned(),
            }],
            is_error: true,
            structured_content: None,
        };
        let ToolPayload::Opaque { result } = extract_tool_payload(outer) else {
            panic!("expected opaque payload");
        };
        assert_eq!(result["is_error"], json!(true));
        assert_eq!(result["content"][0]["text"], json!("plain text answer"));

        // JSON без маркеров конверта — тоже opaque (одиночный `result` —
        // обычное имя поля бизнес-JSON, конверт не объявляет).
        let outer = ToolCallResult {
            content: vec![crate::session_manager::protocol::ToolContent::Text {
                text: serde_json::to_string(&json!({"result": {"rows": 5}})).unwrap(),
            }],
            is_error: false,
            structured_content: None,
        };
        let ToolPayload::Opaque { result } = extract_tool_payload(outer) else {
            panic!("expected opaque payload");
        };
        assert!(result.get("is_error").is_none());
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("rows"));

        // Первый блок не text / content пуст — opaque, а не отказ.
        let outer = ToolCallResult {
            content: vec![],
            is_error: false,
            structured_content: Some(json!({"a": 1})),
        };
        assert!(matches!(
            extract_tool_payload(outer),
            ToolPayload::Opaque { .. }
        ));

        // text — JSON-строковый литерал ("{...}": бизнес-result-строка от
        // границы до исправления двойной сериализации) — opaque; менеджер
        // не трогает содержимое, text идёт в сервис дословно.
        let business = "{\"valid\": true}";
        let quoted = serde_json::to_string(&json!(business)).unwrap();
        let outer = ToolCallResult {
            content: vec![crate::session_manager::protocol::ToolContent::Text {
                text: quoted.clone(),
            }],
            is_error: false,
            structured_content: None,
        };
        let ToolPayload::Opaque { result } = extract_tool_payload(outer) else {
            panic!("expected opaque payload");
        };
        assert_eq!(result["content"][0]["text"], json!(quoted));

        // text — сериализованный JSON-объект (исправленная граница):
        // тоже opaque, без пересборки.
        let raw_json = "{\"valid\": true}";
        let outer = ToolCallResult {
            content: vec![crate::session_manager::protocol::ToolContent::Text {
                text: raw_json.to_owned(),
            }],
            is_error: false,
            structured_content: None,
        };
        let ToolPayload::Opaque { result } = extract_tool_payload(outer) else {
            panic!("expected opaque payload");
        };
        assert_eq!(result["content"][0]["text"], json!(raw_json));
    }

    #[test]
    fn claimed_but_malformed_envelope_is_fail_closed_not_opaque() {
        for text in [
            // schema_version есть — конверт заявлен; невалиден → отказ.
            serde_json::to_string(&json!({"schema_version": 2, "result": {}, "field_sources": {}}))
                .unwrap(),
            serde_json::to_string(
                &json!({"schema_version": 1, "result": {"content": [], "is_error": false}}),
            )
            .unwrap(),
            serde_json::to_string(&json!({"schema_version": 1})).unwrap(),
            // field_sources — специфичный ключ конверта; без остального — сломан.
            serde_json::to_string(&json!({"field_sources": {"schema": {}}})).unwrap(),
        ] {
            let outer = ToolCallResult {
                content: vec![crate::session_manager::protocol::ToolContent::Text { text }],
                is_error: false,
                structured_content: None,
            };
            assert!(matches!(
                extract_tool_payload(outer),
                ToolPayload::Malformed {
                    code: "RESULT_INVALID"
                }
            ));
        }
    }
    //++agent TASK-225

    #[test]
    fn reserved_v8_meta_is_removed_without_touching_tool_arguments() {
        let mut request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "_meta": {"v8.conversation_id": "spoofed", "trace": "kept"},
                "arguments": {"_meta": {"v8.business": "untouched"}}
            }
        });
        assert!(strip_reserved_v8_meta(&mut request));
        assert_eq!(request["params"]["_meta"], json!({"trace": "kept"}));
        assert_eq!(
            request["params"]["arguments"]["_meta"]["v8.business"],
            "untouched"
        );
    }

    #[test]
    fn malformed_upstream_result_uses_bounded_history_code() {
        assert_eq!(
            dispatcher_error_code(&DispatcherError::InvalidResult("invalid shape".to_owned())),
            "RESULT_INVALID"
        );
    }

    #[tokio::test]
    async fn configured_route_fails_closed_without_conversation_or_service() {
        let temp = tempfile::tempdir().unwrap();
        let public_key_path = temp.path().join("broker.pub.pem");
        std::fs::write(&public_key_path, TEST_BROKER_PUBLIC_KEY).unwrap();
        let database_id = uuid::Uuid::new_v4();
        let masking = crate::config::model::MaskingConfig {
            enabled: true,
            socket_path: temp.path().join("service-not-running.sock"),
            internal_listen_path: temp.path().join("manager.sock"),
            service_expected_uid: Some(994),
            broker_public_key_path: public_key_path,
            identity_bindings: vec![MaskingIdentityBinding {
                session: "dev-trusted".to_owned(),
                database_id: database_id.to_string(),
            }],
            ..Default::default()
        };
        let config = Arc::new(AppConfig {
            work_path: PathBuf::from(temp.path()),
            mcp: McpConfig::default(),
            tools_cache: ToolsCacheConfig::default(),
            masking,
        });
        let registry = Arc::new(SessionRegistry::new());
        let server = McpToolServer::try_new(config)
            .unwrap()
            .with_session_registry(Arc::clone(&registry));
        let registration = SessionRegisterParams {
            client_uid: "dev-trusted".to_owned(),
            kind: "server".to_owned(),
            version: "1".to_owned(),
            infobase_name: "dev".to_owned(),
            ib_session_number: 1,
            tools: vec![ToolDescriptor {
                name: "execute_query".to_owned(),
                description: None,
                input_schema: json!({"type":"object"}),
                visibility: Default::default(),
            }],
            config_id: Some("server".to_owned()),
            host_id: Some("dev-host".to_owned()),
            pid: None,
            resources: None,
            prompts: None,
            extras: None,
        };
        registry
            .register(registration, Instant::now(), None)
            .unwrap();

        let request = CallToolRequestParams::new("execute_query").with_arguments(
            serde_json::Map::from_iter([(
                "query".to_owned(),
                json!("raw-marker-must-never-enter-terminal-ledger"),
            )]),
        );
        let missing_conversation = server
            .call_proxy_tool_inner(request.clone(), CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(missing_conversation.is_error, Some(true));
        let missing_conversation_body = missing_conversation.structured_content.unwrap();
        assert_eq!(
            missing_conversation_body["error"]["code"],
            "CHAT_IDENTITY_REQUIRED"
        );

        let outbox_path = temp.path().join("masking_terminal_outbox.json");
        let outbox: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&outbox_path).unwrap()).unwrap();
        assert_eq!(outbox["events"].as_array().unwrap().len(), 1);
        assert_eq!(outbox["events"][0]["scope"], json!({"kind":"unverified"}));
        assert_eq!(
            outbox["events"][0]["correlation_id"],
            missing_conversation_body["error"]["correlation_id"]
        );
        assert!(!serde_json::to_string(&outbox)
            .unwrap()
            .contains("raw-marker-must-never-enter-terminal-ledger"));

        let service_unavailable = server
            .call_proxy_tool_inner(
                request,
                CancellationToken::new(),
                Some(TrustedConversationContext {
                    conversation_id: "opaque-dev-conversation".to_owned(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(service_unavailable.is_error, Some(true));
        let service_unavailable_body = service_unavailable.structured_content.unwrap();
        assert_eq!(
            service_unavailable_body["error"]["code"],
            "SERVICE_NOT_READY"
        );
        let outbox: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&outbox_path).unwrap()).unwrap();
        assert_eq!(outbox["events"].as_array().unwrap().len(), 2);
        assert_eq!(outbox["events"][1]["scope"]["kind"], "verified");
        assert_eq!(
            outbox["events"][1]["scope"]["database_id"],
            database_id.to_string()
        );
        assert_eq!(
            outbox["events"][1]["scope"]["chat_id"],
            "opaque-dev-conversation"
        );
        assert_eq!(
            outbox["events"][1]["correlation_id"],
            service_unavailable_body["error"]["correlation_id"]
        );
        assert!(!serde_json::to_string(&outbox)
            .unwrap()
            .contains("raw-marker-must-never-enter-terminal-ledger"));
    }

    //++agent TASK-225 [25.09.2026]
    // Gate при enabled покрывает КАЖДЫЙ публичный proxy-вызов: имя
    // инструмента из бывшего managed_tools списка роли не играет, а
    // непривязанная к database_id сессия отклоняется до проверки
    // conversation assertion. Internal tools до gate не доходят —
    // resolve_published их не публикует.
    #[tokio::test]
    async fn all_public_calls_route_through_gate_and_unbound_is_denied() {
        let temp = tempfile::tempdir().unwrap();
        let public_key_path = temp.path().join("broker.pub.pem");
        std::fs::write(&public_key_path, TEST_BROKER_PUBLIC_KEY).unwrap();
        let database_id = uuid::Uuid::new_v4();
        let masking = crate::config::model::MaskingConfig {
            enabled: true,
            socket_path: temp.path().join("service-not-running.sock"),
            internal_listen_path: temp.path().join("manager.sock"),
            service_expected_uid: Some(994),
            broker_public_key_path: public_key_path,
            identity_bindings: vec![MaskingIdentityBinding {
                session: "dev-trusted".to_owned(),
                database_id: database_id.to_string(),
            }],
            ..Default::default()
        };
        let config = Arc::new(AppConfig {
            work_path: PathBuf::from(temp.path()),
            mcp: McpConfig::default(),
            tools_cache: ToolsCacheConfig::default(),
            masking,
        });
        let registry = Arc::new(SessionRegistry::new());
        let server = McpToolServer::try_new(config)
            .unwrap()
            .with_session_registry(Arc::clone(&registry));

        // Привязанная сессия: публичный и internal инструменты.
        registry
            .register(
                SessionRegisterParams {
                    client_uid: "dev-trusted".to_owned(),
                    kind: "server".to_owned(),
                    version: "1".to_owned(),
                    infobase_name: "dev".to_owned(),
                    ib_session_number: 1,
                    tools: vec![
                        ToolDescriptor {
                            name: "brand_new_tool".to_owned(),
                            description: None,
                            input_schema: json!({"type":"object"}),
                            visibility: Default::default(),
                        },
                        ToolDescriptor {
                            name: "mcp_internal_masking_metadata_feed".to_owned(),
                            description: None,
                            input_schema: json!({"type":"object"}),
                            visibility: crate::session_manager::protocol::ToolVisibility::Internal,
                        },
                    ],
                    config_id: Some("server".to_owned()),
                    host_id: Some("dev-host".to_owned()),
                    pid: None,
                    resources: None,
                    prompts: None,
                    extras: None,
                },
                Instant::now(),
                None,
            )
            .unwrap();
        // Непривязанная сессия (identity_bindings её не знает).
        register_fake_session(&registry, "unbound-session", &["any_tool"]);

        // Инструмент вне бывшего managed_tools — доходит до gate:
        // отказ по отсутствию conversation assertion, а не raw-вызов в 1С.
        let request = CallToolRequestParams::new("brand_new_tool");
        let denied = server
            .call_proxy_tool_inner(request, CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(denied.is_error, Some(true));
        assert_eq!(
            denied.structured_content.unwrap()["error"]["code"],
            "CHAT_IDENTITY_REQUIRED"
        );

        // Сессия без привязки — DATABASE_IDENTITY_UNVERIFIED даже без
        // assertion (проверка привязки идёт первой).
        let request = CallToolRequestParams::new("any_tool");
        let denied = server
            .call_proxy_tool_inner(request, CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(denied.is_error, Some(true));
        assert_eq!(
            denied.structured_content.unwrap()["error"]["code"],
            "DATABASE_IDENTITY_UNVERIFIED"
        );

        // Internal tool агенту не доступен: resolve не находит его в
        // публичной витрине — вызов завершается до gate structured
        // ошибкой unknown_tool (не protocol error и не вызов в 1С).
        let request = CallToolRequestParams::new("mcp_internal_masking_metadata_feed");
        let denied = server
            .call_proxy_tool_inner(request, CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(denied.is_error, Some(true));
        assert_eq!(
            denied.structured_content.unwrap()["_meta"]["error_code"],
            "unknown_tool"
        );

        // Совсем неизвестное имя (нет ни в live, ни в tools cache) —
        // тот же structured unknown_tool вместо method_not_found.
        let request = CallToolRequestParams::new("get_1c_version_probe");
        let denied = server
            .call_proxy_tool_inner(request, CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(denied.is_error, Some(true));
        assert_eq!(
            denied.structured_content.unwrap()["_meta"]["error_code"],
            "unknown_tool"
        );
    }
    //++agent TASK-225
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
            if entry
                .tools
                .iter()
                .any(|t| t.name == name && t.visibility.is_public())
            {
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

//++agent TASK-225 [25.09.2026]
/// Structured tool error для имени, которого нет ни в live-сессиях, ни в
/// persistent tools cache: агент получает читаемый text-блок и
/// `_meta.error_code="unknown_tool"` вместо протокольной ошибки.
fn unknown_tool_response(tool_name: &str) -> CallToolResult {
    let payload = serde_json::json!({
        "content": [{
            "type": "text",
            "text": format!(
                "Tool '{tool_name}' is unavailable: unknown tool name.",
            )
        }],
        "isError": true,
        "_meta": {
            "error_code": "unknown_tool",
            "tool": tool_name,
        },
    });
    CallToolResult::structured_error(payload)
}
//++agent TASK-225

fn dispatcher_outcome_to_call_result(
    outcome: Result<ToolCallResult, DispatcherError>,
) -> Result<CallToolResult, ErrorData> {
    match outcome {
        Ok(result) => {
            let mut content = Vec::with_capacity(result.content.len());
            for block in result.content {
                match block {
                    crate::session_manager::protocol::ToolContent::Text { text } => {
                        content.push(Content::text(text));
                    }
                    crate::session_manager::protocol::ToolContent::Json { json } => {
                        let text = serde_json::to_string(&json).map_err(|_| {
                            ErrorData::internal_error("failed to serialize JSON content", None)
                        })?;
                        content.push(Content::text(text));
                    }
                }
            }
            let mut public = if result.is_error {
                CallToolResult::error(content)
            } else {
                CallToolResult::success(content)
            };
            public.structured_content = result.structured_content;
            Ok(public)
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
    masking_gate: Arc<MaskingGate>,
    conversation_assertion_header: HeaderName,
}

impl HttpMcpService {
    fn new(server: McpToolServer, config: Arc<AppConfig>, shutdown: CancellationToken) -> Self {
        let auth_token = config.mcp.http.auth_token.clone();
        let masking_gate = Arc::clone(&server.masking_gate);
        let conversation_assertion_header = masking_gate
            .assertion_header()
            .parse()
            .expect("validated conversation assertion header");
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
            masking_gate,
            conversation_assertion_header,
        }
    }

    async fn handle(&self, mut request: Request<Body>) -> Response<Body> {
        let session_id = session_id_from_headers(request.headers());
        let method = request.method().clone();
        let is_initialize_candidate =
            self.stateful_sessions && method == Method::POST && session_id.is_none();

        if let Some(token) = &self.auth_token {
            if let Some(reject) = check_bearer_auth(&request, token) {
                return reject;
            }
        }

        // Assertion удаляется до передачи в rmcp. В RequestContext попадает
        // только проверенная server-side identity, а не JWT или spoofable _meta.
        // Bearer gate выполняется раньше, чтобы unauthenticated request не мог
        // поглотить одноразовый `jti` валидного broker assertion.
        if let Some(assertion) = request
            .headers_mut()
            .remove(&self.conversation_assertion_header)
            .and_then(|value| value.to_str().ok().map(ToOwned::to_owned))
        {
            if let Some(context) = self
                .masking_gate
                .verify_conversation_assertion(assertion.as_str())
            {
                request.extensions_mut().insert(context);
            }
        }

        let rpc_method = if method == Method::POST {
            match sanitize_http_rpc_request(request).await {
                Ok((sanitized, rpc_method)) => {
                    request = sanitized;
                    rpc_method
                }
                Err(response) => return response,
            }
        } else {
            None
        };

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

async fn sanitize_http_rpc_request(
    request: Request<Body>,
) -> Result<(Request<Body>, Option<String>), Response<Body>> {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, HTTP_BODY_LIMIT_BYTES).await.map_err(|_| {
        Response::builder()
            .status(StatusCode::PAYLOAD_TOO_LARGE)
            .header(
                CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )
            .body(Body::from("Payload Too Large"))
            .expect("valid overload response")
    })?;
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return Ok((Request::from_parts(parts, Body::from(body)), None));
    };
    let method = value
        .get("method")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    let sanitized = if strip_reserved_v8_meta(&mut value) {
        serde_json::to_vec(&value).map_err(|_| {
            Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Bad Request"))
                .expect("valid bad request response")
        })?
    } else {
        body.to_vec()
    };
    Ok((Request::from_parts(parts, Body::from(sanitized)), method))
}

fn strip_reserved_v8_meta(value: &mut serde_json::Value) -> bool {
    match value {
        serde_json::Value::Array(requests) => {
            let mut changed = false;
            for request in requests {
                changed |= strip_reserved_v8_meta(request);
            }
            changed
        }
        serde_json::Value::Object(request) => request
            .get_mut("params")
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.get_mut("_meta"))
            .and_then(serde_json::Value::as_object_mut)
            .is_some_and(|meta| {
                let before = meta.len();
                meta.retain(|key, _| !key.starts_with("v8."));
                meta.len() != before
            }),
        _ => false,
    }
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
