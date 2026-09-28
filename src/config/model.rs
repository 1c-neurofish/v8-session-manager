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
    /// UDS сервиса маскирования (manager → service: preflight/finalize/terminal).
    pub socket_path: PathBuf,
    /// UDS-listener менеджера для внутренних вызовов сервиса
    /// (service → manager: `POST /internal/v1/tools/call`). Тот же
    /// shared-volume, что и `socket_path`; доступ ограничен peer UID
    /// `service_expected_uid`.
    pub internal_listen_path: PathBuf,
    pub preflight_timeout_ms: u64,
    pub finalize_timeout_ms: u64,
    /// Дедлайн одного internal tool.call (например, страница словаря)
    /// на стороне менеджера.
    pub internal_call_timeout_ms: u64,
    // Проверка conversation-assertion брокера упразднена: у вызова нет
    // понятия «разговора», токены маскирования общие на базу. Прежние
    // ключи `broker_*` / `conversation_assertion_header` в YAML
    // игнорируются (структура не deny_unknown_fields) — их следует убрать.
    //++agent TASK-225 [25.09.2026]
    /// УСТАРЕВШЕЕ: при `masking.enabled=true` ВСЕ публичные proxy-вызовы
    /// проходят через сервис маскирования — список больше не влияет на
    /// маршрут (решение пользователя: единая точка контроля, исключения
    /// настраиваются классификацией в админке сервиса, а не конфигом MCP).
    /// Поле оставлено для совместимости десериализации старых конфигов;
    /// непустое значение — предупреждение в логе при старте gate.
    //++agent TASK-225
    pub managed_tools: Vec<String>,
    /// Имена внутренних tools mcp_tools. Не публикуются агенту в tools/list,
    /// вызов агентом отклоняется; вызываются только сервисом маскирования
    /// через `internal_listen_path`. Менеджер помечает их `Internal` по имени
    /// из этого списка (источник — конфиг, а не payload регистрации 1С).
    pub internal_tools: Vec<String>,
    /// Ожидаемый UID сервиса маскирования на `internal_listen_path`;
    /// обязателен при `enabled=true`.
    pub service_expected_uid: Option<u32>,
    //++agent TASK-225 [26.09.2026] N: `identity_bindings` (имя сессии →
    // database_id сервиса) удалено из модели — идентичность базы
    // детерминированно определяется парой (GUID кластера, GUID ИБ),
    // которую менеджер резолвит через RAS по `cluster_server`+
    // `infobase_name` из `session.register`. Настройки RAS — только в env
    // (`V8SM_RAC_PATH`/`V8SM_RAS_ADDRESS`/`V8SM_RAS_CLUSTER_USER`/
    // `V8SM_RAS_CLUSTER_PASSWORD`), см. masking::ras.
    //++agent TASK-225
}

impl Default for MaskingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            socket_path: PathBuf::from("/run/1c-masking/internal.sock"),
            internal_listen_path: PathBuf::from("/run/1c-masking/manager.sock"),
            preflight_timeout_ms: 3_000,
            finalize_timeout_ms: 15_000,
            internal_call_timeout_ms: 10_000,
            //++agent TASK-225 [25.09.2026]
            // Устаревшее поле: пустой default, чтобы отсутствие ключа в
            // конфиге не порождало ложное deprecation-предупреждение.
            //++agent TASK-225
            managed_tools: Vec::new(),
            internal_tools: vec![
                "mcp_internal_masking_metadata_feed".to_owned(),
                "mcp_internal_masking_dictionary_feed".to_owned(),
            ],
            service_expected_uid: None,
        }
    }
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
