use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Top-level конфиг менеджера сессий.
///
/// YAML формат `v8project.yaml` сводится к `work_path` + `mcp:`.
/// Никаких base_path / connection / source_sets / build / tools / tests —
/// это были поля v8-runner CLI, удалены при extraction.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    /// Working directory for log files.
    pub work_path: PathBuf,

    /// MCP transport configuration (HTTP server + WS session manager).
    #[serde(default)]
    pub mcp: McpConfig,

    /// Persistent tools-cache (ADR-0035). Кеш переживает рестарт менеджера;
    /// нужен для MCP-харнесов, которые нестабильно реагируют на
    /// `notifications/tools/list_changed` (например Claude Code).
    #[serde(default)]
    pub tools_cache: ToolsCacheConfig,

    /// Fail-closed gate внешнего сервиса маскирования MCP-результатов.
    #[serde(default)]
    pub masking: MaskingConfig,
}

/// Конфигурация внутреннего UDS-клиента сервиса маскирования.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, rename_all = "snake_case")]
pub struct MaskingConfig {
    /// Gate активируется только явно; без полной identity-конфигурации запуск
    /// с `enabled=true` отклоняется валидатором.
    pub enabled: bool,
    pub socket_path: PathBuf,
    pub preflight_timeout_ms: u64,
    pub finalize_timeout_ms: u64,
    pub feed_chunk_timeout_ms: u64,
    pub feed_activate_timeout_ms: u64,
    /// PEM public key доверенного broker-а для проверки JWT/JWS assertion.
    pub broker_public_key_path: PathBuf,
    pub broker_issuer: String,
    pub broker_audience: String,
    pub broker_max_assertion_ttl_secs: u64,
    pub conversation_assertion_header: String,
    /// Набор tools, которые обязаны пройти preflight/finalize. Неизвестное
    /// сервису имя получит `TOOL_PENDING_REVIEW`, а не raw fallback.
    pub managed_tools: Vec<String>,
    pub identity_bindings: Vec<MaskingIdentityBinding>,
}

impl Default for MaskingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            socket_path: PathBuf::from("/run/1c-masking/internal.sock"),
            preflight_timeout_ms: 3_000,
            finalize_timeout_ms: 15_000,
            feed_chunk_timeout_ms: 10_000,
            feed_activate_timeout_ms: 60_000,
            broker_public_key_path: PathBuf::from("/etc/v8-session-manager/broker-ed25519.pub.pem"),
            broker_issuer: "trusted-mcp-broker".to_owned(),
            broker_audience: "v8-session-manager".to_owned(),
            broker_max_assertion_ttl_secs: 300,
            conversation_assertion_header: "x-v8-conversation-assertion".to_owned(),
            managed_tools: vec![
                "execute_query".to_owned(),
                "find_references_to_object".to_owned(),
                "get_object_by_link".to_owned(),
                "get_metadata".to_owned(),
                "get_access_rights".to_owned(),
                "get_link_of_object".to_owned(),
            ],
            identity_bindings: Vec::new(),
        }
    }
}

/// Deployment-owned привязка identity базы к ожидаемому WS route.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct MaskingIdentityBinding {
    pub database_instance_id: String,
    pub database_id: String,
    pub expected_kind: String,
    pub expected_config_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_host_id: Option<String>,
    pub allowed_internal_tools: Vec<String>,
}

/// MCP runtime configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "snake_case")]
pub struct McpConfig {
    /// HTTP transport settings (`/mcp` endpoint :4001 by default).
    pub http: McpHttpConfig,

    /// Shared execution limits for MCP calls.
    pub execution: McpExecutionConfig,

    /// Prometheus metrics exporter configuration.
    pub metrics: MetricsConfig,

    /// Client session manager (WS-tunnel transport for 1C clients).
    /// `None` — менеджер сессий не запускается; для бинарника `v8-session-manager`
    /// при отсутствии будет применён `McpSessionManagerConfig::default()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_manager: Option<McpSessionManagerConfig>,
}

#[allow(clippy::derivable_impls)]
impl Default for McpConfig {
    fn default() -> Self {
        Self {
            http: McpHttpConfig::default(),
            execution: McpExecutionConfig::default(),
            metrics: MetricsConfig::default(),
            session_manager: None,
        }
    }
}

/// HTTP-specific MCP configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "snake_case")]
pub struct McpHttpConfig {
    pub bind_address: String,
    pub path: String,
    pub stateful_sessions: bool,
    pub max_sessions: usize,
    pub idle_ttl_secs: u64,
    pub auth_token: Option<String>,
}

impl Default for McpHttpConfig {
    fn default() -> Self {
        Self {
            bind_address: default_mcp_http_bind_address(),
            path: default_mcp_http_path(),
            stateful_sessions: default_mcp_http_stateful_sessions(),
            max_sessions: default_mcp_http_max_sessions(),
            idle_ttl_secs: default_mcp_http_idle_ttl_secs(),
            auth_token: None,
        }
    }
}

/// Execution guardrails for MCP requests.
///
/// Менеджер сейчас сам никаких длительных tool-вызовов не выполняет
/// (только `session.list` + проксирование), поэтому остался единственный
/// параметр `shutdown_grace_period_secs`, влияющий на graceful shutdown
/// tokio-runtime.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "snake_case")]
pub struct McpExecutionConfig {
    pub shutdown_grace_period_secs: u64,
}

impl Default for McpExecutionConfig {
    fn default() -> Self {
        Self {
            shutdown_grace_period_secs: default_mcp_execution_shutdown_grace_period_secs(),
        }
    }
}

/// Metrics (Prometheus) configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "snake_case")]
pub struct MetricsConfig {
    /// Bind address for Prometheus `/metrics` endpoint.
    /// When absent or empty, metrics exporter is disabled.
    pub bind_address: Option<String>,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            bind_address: Some("127.0.0.1:9100".to_owned()),
        }
    }
}

/// Client session manager configuration (см. spec/SESSION_MANAGER.md §8.3).
///
/// После урезания менеджера до агрегатора убраны spawn-template-driven
/// поля (`templates`, `spawn`, `remote_backend`, `register_timeout_ms`):
/// менеджер больше не запускает 1С-процессы, только принимает входящие
/// WS-регистрации.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, rename_all = "snake_case")]
pub struct McpSessionManagerConfig {
    pub bind_address: String,
    pub path: String,
    pub heartbeat_interval_ms: u64,
    pub idle_timeout_secs: u64,
    pub reconnection_grace_secs: u64,
    pub graceful_kill_grace_ms: u64,
    /// Интервал WS protocol-level Ping (RFC 6455 opcode 0x9), мс. Менеджер
    /// шлёт Ping каждому подключённому клиенту в writer-task; tokio-
    /// tungstenite на стороне addin отвечает Pong автоматически без
    /// участия BSL. Поддерживает канал живым (NAT/half-close detection).
    /// `0` — Ping отключён. По умолчанию 20000 мс.
    pub ws_ping_interval_ms: u64,
    /// Таймаут отсутствия Pong, мс. Если за это время от клиента не
    /// пришло ни одного Pong и ни одного входящего фрейма — менеджер
    /// закрывает соединение и через grace timeout удаляет запись.
    /// Должен быть `>= ws_ping_interval_ms` (иначе постоянно false-positive).
    /// По умолчанию 30000 мс.
    pub ws_ping_timeout_ms: u64,
}

impl Default for McpSessionManagerConfig {
    fn default() -> Self {
        Self {
            bind_address: default_mcp_session_manager_bind_address(),
            path: default_mcp_session_manager_path(),
            heartbeat_interval_ms: default_mcp_session_manager_heartbeat_interval_ms(),
            idle_timeout_secs: default_mcp_session_manager_idle_timeout_secs(),
            reconnection_grace_secs: default_mcp_session_manager_reconnection_grace_secs(),
            graceful_kill_grace_ms: default_mcp_session_manager_graceful_kill_grace_ms(),
            ws_ping_interval_ms: default_mcp_session_manager_ws_ping_interval_ms(),
            ws_ping_timeout_ms: default_mcp_session_manager_ws_ping_timeout_ms(),
        }
    }
}

fn default_mcp_http_bind_address() -> String {
    "127.0.0.1:4001".to_owned()
}

fn default_mcp_http_path() -> String {
    "/mcp".to_owned()
}

const fn default_mcp_http_stateful_sessions() -> bool {
    true
}

const fn default_mcp_http_max_sessions() -> usize {
    64
}

const fn default_mcp_http_idle_ttl_secs() -> u64 {
    900
}

const fn default_mcp_execution_shutdown_grace_period_secs() -> u64 {
    30
}

fn default_mcp_session_manager_bind_address() -> String {
    "127.0.0.1:4000".to_owned()
}

fn default_mcp_session_manager_path() -> String {
    "/sessions".to_owned()
}

const fn default_mcp_session_manager_heartbeat_interval_ms() -> u64 {
    15_000
}

const fn default_mcp_session_manager_idle_timeout_secs() -> u64 {
    1_800
}

const fn default_mcp_session_manager_reconnection_grace_secs() -> u64 {
    30
}

const fn default_mcp_session_manager_graceful_kill_grace_ms() -> u64 {
    5_000
}

const fn default_mcp_session_manager_ws_ping_interval_ms() -> u64 {
    20_000
}

const fn default_mcp_session_manager_ws_ping_timeout_ms() -> u64 {
    30_000
}

/// Persistent tools-cache configuration (ADR-0035).
///
/// `enabled: false` ⇒ кеш в no-op режиме (откат к ADR-0034 live-only).
/// `cache_life_period` парсится через humantime (`5d`, `12h`, `30m`).
/// `storage_path: None` ⇒ `${workPath}/tools_cache.json`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "snake_case")]
pub struct ToolsCacheConfig {
    pub enabled: bool,
    #[serde(with = "humantime_serde")]
    pub cache_life_period: Duration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_path: Option<PathBuf>,
}

impl Default for ToolsCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cache_life_period: Duration::from_secs(5 * 24 * 60 * 60),
            storage_path: None,
        }
    }
}
