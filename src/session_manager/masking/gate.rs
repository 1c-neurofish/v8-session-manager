use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::model::MaskingConfig;
use crate::session_manager::masking::client::{
    ClientError, FinalizeOutcome, FinalizeRequest, MaskingServiceClient, PreflightRequest,
    TerminalRequest, TerminalScope,
};
use crate::session_manager::masking::identity::SessionDatabaseIdentity;
use crate::session_manager::masking::ras::RasResolver;
use crate::session_manager::protocol::ToolCallResult;
use crate::session_manager::protocol::{ToolDescriptor, ToolVisibility};
use crate::session_manager::registry::SessionRecord;
use crate::support::atomic_write::write_json_atomic;

const TERMINAL_OUTBOX_FILE: &str = "masking_terminal_outbox.json";
const MAX_TERMINAL_OUTBOX_EVENTS: usize = 10_000;
const TERMINAL_REPLAY_INTERVAL: Duration = Duration::from_secs(5);

/// Проверенная broker-ом identity диалога; agent payload не является её источником.
#[derive(Debug, Clone)]
pub struct TrustedConversationContext {
    pub conversation_id: String,
}

#[derive(Debug, Deserialize)]
struct ConversationClaims {
    conversation_id: String,
    jti: String,
    exp: usize,
    iat: usize,
    aud: String,
    iss: String,
}

struct ConversationVerifier {
    key: DecodingKey,
    validation: Validation,
    replay_cache: Mutex<HashMap<String, usize>>,
}

const MAX_ASSERTION_REPLAY_ENTRIES: usize = 10_000;

/// Общая idempotency identity preflight/finalize одного вызова.
#[derive(Debug, Clone)]
pub struct MaskingCallContext {
    pub call_id: String,
    pub correlation_id: String,
    //++agent TASK-225 [26.09.2026] N: идентичность — пара GUID-ов,
    // RAS-резолвленная менеджером при session.register.
    pub database: SessionDatabaseIdentity,
    //++agent TASK-225
    pub chat_id: String,
    pub tool_name: String,
}

/// Только безопасные поля, допустимые в ответе агенту.
#[derive(Debug, Clone)]
pub struct MaskingFailure {
    pub code: String,
    pub message: String,
    pub correlation_id: String,
    //++agent TASK-225 [26.09.2026] фаза-2 C: SERVICE_WARMING_UP несёт
    // оценку повтора — уходит в structured_content MCP-ответа.
    pub retry_after_s: Option<u64>,
    //++agent TASK-225
}

impl MaskingFailure {
    pub fn local(code: &str, message: &str) -> Self {
        Self::with_correlation(code, message, Uuid::new_v4().to_string())
    }

    pub fn with_correlation(code: &str, message: &str, correlation_id: String) -> Self {
        Self {
            code: code.to_owned(),
            message: message.to_owned(),
            correlation_id,
            retry_after_s: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalOutboxSnapshot {
    schema_version: u8,
    events: Vec<TerminalRequest>,
}

struct TerminalOutbox {
    path: PathBuf,
    events: Vec<TerminalRequest>,
}

impl TerminalOutbox {
    fn load(path: PathBuf) -> Result<Self, String> {
        let events = match std::fs::read(&path) {
            Ok(bytes) => {
                let snapshot: TerminalOutboxSnapshot = serde_json::from_slice(&bytes)
                    .map_err(|_| "invalid masking terminal outbox".to_owned())?;
                if snapshot.schema_version != 1
                    || snapshot.events.len() > MAX_TERMINAL_OUTBOX_EVENTS
                    || snapshot.events.iter().any(|event| !valid_terminal(event))
                {
                    return Err("invalid masking terminal outbox".to_owned());
                }
                ensure_unique_terminal_calls(&snapshot.events)?;
                snapshot.events
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(_) => return Err("cannot read masking terminal outbox".to_owned()),
        };
        Ok(Self { path, events })
    }

    fn enqueue(&mut self, event: TerminalRequest) -> Result<(), String> {
        if !valid_terminal(&event) {
            return Err("invalid masking terminal event".to_owned());
        }
        if let Some(existing) = self
            .events
            .iter()
            .find(|existing| existing.call_id == event.call_id)
        {
            return if existing == &event {
                Ok(())
            } else {
                Err("masking terminal call identity collision".to_owned())
            };
        }
        if self.events.len() >= MAX_TERMINAL_OUTBOX_EVENTS {
            return Err("masking terminal outbox is full".to_owned());
        }
        self.events.push(event);
        if let Err(error) = self.persist() {
            self.events.pop();
            return Err(error);
        }
        Ok(())
    }

    fn acknowledge_first(&mut self) -> Result<(), String> {
        let event = self.events.remove(0);
        if let Err(error) = self.persist() {
            self.events.insert(0, event);
            return Err(error);
        }
        Ok(())
    }

    fn persist(&self) -> Result<(), String> {
        write_json_atomic(
            &self.path,
            &TerminalOutboxSnapshot {
                schema_version: 1,
                events: self.events.clone(),
            },
        )
        .map_err(|_| "cannot persist masking terminal outbox".to_owned())
    }
}

fn ensure_unique_terminal_calls(events: &[TerminalRequest]) -> Result<(), String> {
    let mut calls = HashSet::with_capacity(events.len());
    if events.iter().all(|event| calls.insert(&event.call_id)) {
        Ok(())
    } else {
        Err("duplicate call identity in masking terminal outbox".to_owned())
    }
}

fn valid_terminal(event: &TerminalRequest) -> bool {
    if event.schema_version != 1
        || Uuid::parse_str(&event.call_id).is_err()
        || Uuid::parse_str(&event.correlation_id).is_err()
        || event.tool_name.is_empty()
        || event.tool_name.len() > 128
    {
        return false;
    }
    let allowed = match &event.scope {
        //++agent TASK-225 [26.09.2026] N: verified scope — координаты
        // базы (Srvr/Ref обязательны, GUID-ы опциональны и типизированы).
        TerminalScope::Verified {
            cluster_server,
            infobase_name,
            chat_id,
            ..
        } => {
            !cluster_server.is_empty()
                && cluster_server.len() <= 512
                && !infobase_name.is_empty()
                && infobase_name.len() <= 512
                && !chat_id.is_empty()
                && chat_id.len() <= 512
                && matches!(
                    event.error_code.as_str(),
                    "ACTION_REQUIRED"
                        | "TOOL_PENDING_REVIEW"
                        | "MASK_TOKEN_INVALID"
                        | "SERVICE_NOT_READY"
                        //++agent TASK-225 [26.09.2026] фаза-2 C
                        | "SERVICE_WARMING_UP"
                        //++agent TASK-225
                        | "POLICY_INVALID"
                        | "RESULT_LIMIT_EXCEEDED"
                        | "MASKING_TIMEOUT"
                        | "MASKING_FAILED"
                        | "HISTORY_UNAVAILABLE"
                )
        }
        TerminalScope::Unverified => matches!(
            event.error_code.as_str(),
            "CHAT_IDENTITY_REQUIRED" | "DATABASE_IDENTITY_UNVERIFIED" | "SERVICE_NOT_READY"
        ),
    };
    allowed
}

/// Manager-side fail-closed gate внешнего сервиса маскирования.
#[derive(Clone)]
pub struct MaskingGate {
    enabled: bool,
    /// Имена internal tools (`masking.internal_tools`): не публикуются
    /// агенту и вызываются только через internal UDS endpoint.
    internal_tools: Arc<HashSet<String>>,
    //++agent TASK-225 [26.09.2026] N: identity_bindings упразднены; вместо
    // deployment-резолвера — RAS-резолвер (srvr, ref) → GUID-ы кластера.
    ras: Option<Arc<RasResolver>>,
    //++agent TASK-225
    client: Option<MaskingServiceClient>,
    verifier: Option<Arc<ConversationVerifier>>,
    assertion_header: String,
    max_assertion_ttl_secs: u64,
    terminal_outbox: Option<Arc<AsyncMutex<TerminalOutbox>>>,
    /// UDS-listener вызовов сервис → менеджер (`POST /internal/v1/tools/call`).
    internal_listen_path: PathBuf,
    /// Ожидаемый UID сервиса на `internal_listen_path` (peer-cred gate).
    service_expected_uid: Option<u32>,
    /// Дедлайн одного internal tool.call.
    internal_call_timeout: Duration,
}

impl MaskingGate {
    pub fn from_config(config: &MaskingConfig, work_path: &Path) -> Result<Self, String> {
        if !config.enabled {
            return Ok(Self {
                enabled: false,
                internal_tools: Arc::new(config.internal_tools.iter().cloned().collect()),
                ras: None,
                client: None,
                verifier: None,
                assertion_header: config.conversation_assertion_header.clone(),
                max_assertion_ttl_secs: config.broker_max_assertion_ttl_secs,
                terminal_outbox: None,
                internal_listen_path: config.internal_listen_path.clone(),
                service_expected_uid: config.service_expected_uid,
                internal_call_timeout: Duration::from_millis(config.internal_call_timeout_ms),
            });
        }
        let pem = std::fs::read(&config.broker_public_key_path)
            .map_err(|_| "cannot read masking broker public key".to_owned())?;
        let key = DecodingKey::from_ed_pem(&pem)
            .map_err(|_| "invalid masking broker Ed25519 public key".to_owned())?;
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_audience(&[config.broker_audience.as_str()]);
        validation.set_issuer(&[config.broker_issuer.as_str()]);
        validation.validate_exp = true;
        validation.leeway = 0;
        //++agent TASK-225 [25.09.2026]
        // managed_tools устарел и на маршрут не влияет: все публичные
        // proxy-вызовы идут через gate; непустое значение — deprecation
        // warning, не ошибка (совместимость со старыми конфигами).
        //++agent TASK-225
        if !config.managed_tools.is_empty() {
            tracing::warn!(
                "masking.managed_tools is deprecated and ignored: all public proxy calls go through the masking gate"
            );
        }
        Ok(Self {
            enabled: true,
            internal_tools: Arc::new(config.internal_tools.iter().cloned().collect()),
            ras: Some(Arc::new(RasResolver::from_env())),
            client: Some(MaskingServiceClient::new(
                config.socket_path.clone(),
                Duration::from_millis(config.preflight_timeout_ms),
                Duration::from_millis(config.finalize_timeout_ms),
            )),
            verifier: Some(Arc::new(ConversationVerifier {
                key,
                validation,
                replay_cache: Mutex::new(HashMap::new()),
            })),
            assertion_header: config.conversation_assertion_header.clone(),
            max_assertion_ttl_secs: config.broker_max_assertion_ttl_secs,
            terminal_outbox: Some(Arc::new(AsyncMutex::new(TerminalOutbox::load(
                work_path.join(TERMINAL_OUTBOX_FILE),
            )?))),
            internal_listen_path: config.internal_listen_path.clone(),
            service_expected_uid: config.service_expected_uid,
            internal_call_timeout: Duration::from_millis(config.internal_call_timeout_ms),
        })
    }

    /// Помечает visibility каждого tool по имени из конфига — единственная
    /// точка, где descriptor получает `Internal`. Adapter-provided
    /// `Internal` сохраняется как fail-safe (ограничение никогда не
    /// расширяется до публичного); adapter-provided `Public` для имени из
    /// `masking.internal_tools` не доверяется.
    pub fn normalize_tools(&self, tools: &mut [ToolDescriptor]) {
        for tool in tools.iter_mut() {
            tool.visibility = if self.internal_tools.contains(&tool.name)
                || matches!(tool.visibility, ToolVisibility::Internal)
            {
                ToolVisibility::Internal
            } else {
                ToolVisibility::Public
            };
        }
    }

    pub(crate) fn internal_tools(&self) -> &HashSet<String> {
        &self.internal_tools
    }

    pub(crate) fn internal_listen_path(&self) -> &Path {
        &self.internal_listen_path
    }

    pub(crate) fn service_expected_uid(&self) -> Option<u32> {
        self.service_expected_uid
    }

    pub(crate) fn internal_call_timeout(&self) -> Duration {
        self.internal_call_timeout
    }

    //++agent TASK-225 [25.09.2026]
    /// При `masking.enabled=true` КАЖДЫЙ публичный proxy `tools/call` идёт
    /// через gate (единая точка контроля; исключения — классификацией в
    /// сервисе). Идентичность базы сессии (координаты ИБ + RAS GUID-ы)
    /// проверяется внутри ветки вызова: сессия без координат получает
    /// DATABASE_IDENTITY_UNVERIFIED, а не raw-bypass вокруг маскировщика.
    //++agent TASK-225
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn assertion_header(&self) -> &str {
        &self.assertion_header
    }

    /// Проверяет short-lived EdDSA JWT, включая `iss`, `aud`, `exp`.
    pub fn verify_conversation_assertion(
        &self,
        assertion: &str,
    ) -> Option<TrustedConversationContext> {
        let verifier = self.verifier.as_ref()?;
        let claims = decode::<ConversationClaims>(assertion, &verifier.key, &verifier.validation)
            .ok()?
            .claims;
        if claims.conversation_id.is_empty()
            || claims.conversation_id.len() > 512
            || claims.jti.is_empty()
            || claims.jti.len() > 256
        {
            return None;
        }
        let now = chrono::Utc::now().timestamp().max(0) as usize;
        if claims.iat > now
            || claims.exp <= claims.iat
            || claims.exp.saturating_sub(claims.iat) as u64 > self.max_assertion_ttl_secs
        {
            return None;
        }
        let mut replay_cache = verifier.replay_cache.lock().ok()?;
        replay_cache.retain(|_, exp| *exp > now);
        if replay_cache.contains_key(&claims.jti)
            || replay_cache.len() >= MAX_ASSERTION_REPLAY_ENTRIES
        {
            return None;
        }
        replay_cache.insert(claims.jti, claims.exp);
        let _validated_registered_claims = (claims.aud, claims.iss);
        Some(TrustedConversationContext {
            conversation_id: claims.conversation_id,
        })
    }

    //++agent TASK-225 [26.09.2026]
    /// ОВ-2/Б12: клиент сервиса для read-only вызовов вне контура
    /// маскирования (`masking_export_setup`). `None` — masking.enabled=false.
    pub(crate) fn service_client(&self) -> Option<&MaskingServiceClient> {
        self.client.as_ref()
    }
    //--agent TASK-225

    /// Identity сессии — из `SessionRecord` (присланный `cluster_server`
    /// и GUID-ы RAS-резолюции). `None` только для сессий без
    /// `cluster_server` (файловая база) — вызов получает
    /// DATABASE_IDENTITY_UNVERIFIED; неудачная RAS-резолюция identity не
    /// снимает — сервис тогда работает со сгенерированной парой GUID-ов.
    pub fn verify_database(&self, record: &SessionRecord) -> Option<SessionDatabaseIdentity> {
        SessionDatabaseIdentity::from_record(record)
    }

    //++agent TASK-225 [26.09.2026] N: доступ к RAS-резолверу из transport
    /// при регистрации сессии.
    pub(crate) fn ras_resolver(&self) -> Option<&Arc<RasResolver>> {
        self.ras.as_ref()
    }
    //++agent TASK-225

    pub fn call_context(
        &self,
        identity: SessionDatabaseIdentity,
        conversation: &TrustedConversationContext,
        tool_name: &str,
        call_id: String,
        correlation_id: String,
    ) -> MaskingCallContext {
        MaskingCallContext {
            call_id,
            correlation_id,
            database: identity,
            chat_id: conversation.conversation_id.clone(),
            tool_name: tool_name.to_owned(),
        }
    }

    pub fn unverified_terminal(
        &self,
        call_id: String,
        correlation_id: String,
        tool_name: &str,
        error_code: &str,
    ) -> TerminalRequest {
        TerminalRequest {
            schema_version: 1,
            call_id,
            correlation_id,
            tool_name: tool_name.to_owned(),
            error_code: error_code.to_owned(),
            scope: TerminalScope::Unverified,
        }
    }

    fn verified_terminal(context: &MaskingCallContext, error_code: &str) -> TerminalRequest {
        TerminalRequest {
            schema_version: 1,
            call_id: context.call_id.clone(),
            correlation_id: context.correlation_id.clone(),
            tool_name: context.tool_name.clone(),
            error_code: error_code.to_owned(),
            scope: TerminalScope::Verified {
                instance_id: context.database.instance_id.clone(),
                cluster_server: context.database.cluster_server.clone(),
                infobase_name: context.database.infobase_name.clone(),
                chat_id: context.chat_id.clone(),
            },
        }
    }

    pub async fn record_terminal(&self, event: TerminalRequest) -> Result<(), String> {
        let Some(outbox) = &self.terminal_outbox else {
            return Err("masking terminal outbox is disabled".to_owned());
        };
        let mut outbox = outbox.lock().await;
        outbox.enqueue(event)?;
        self.flush_terminal_locked(&mut outbox).await;
        Ok(())
    }

    async fn flush_terminal_locked(&self, outbox: &mut TerminalOutbox) {
        let Some(client) = &self.client else {
            return;
        };
        while let Some(event) = outbox.events.first() {
            let acknowledged = terminal_delivery_ack(client.terminal(event).await);
            if !acknowledged || outbox.acknowledge_first().is_err() {
                break;
            }
        }
    }

    pub fn spawn_terminal_replay(
        self: Arc<Self>,
        shutdown: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if self.terminal_outbox.is_none() {
                return;
            }
            loop {
                if let Some(outbox) = &self.terminal_outbox {
                    let mut outbox = outbox.lock().await;
                    self.flush_terminal_locked(&mut outbox).await;
                }
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(TERMINAL_REPLAY_INTERVAL) => {}
                }
            }
        })
    }

    pub async fn preflight(
        &self,
        context: MaskingCallContext,
        arguments: Value,
    ) -> Result<(MaskingCallContext, Value), MaskingFailure> {
        let request = PreflightRequest {
            schema_version: 1,
            call_id: context.call_id.clone(),
            correlation_id: context.correlation_id.clone(),
            instance_id: context.database.instance_id.clone(),
            cluster_server: context.database.cluster_server.clone(),
            infobase_name: context.database.infobase_name.clone(),
            chat_id: context.chat_id.clone(),
            tool_name: context.tool_name.clone(),
            arguments,
        };
        let response = match self
            .client
            .as_ref()
            .expect("enabled gate has client")
            .preflight(&request)
            .await
        {
            Ok(response) => response,
            Err(error) => {
                let failure = map_client_error(error, &context.correlation_id);
                if self
                    .record_terminal(Self::verified_terminal(
                        &context,
                        terminal_fallback_code(&failure.code),
                    ))
                    .await
                    .is_err()
                {
                    return Err(MaskingFailure::with_correlation(
                        "HISTORY_UNAVAILABLE",
                        "Операция временно недоступна",
                        context.correlation_id,
                    ));
                }
                return Err(failure);
            }
        };
        if response.schema_version != 1 || response.decision != "allow" {
            let failure = MaskingFailure::with_correlation(
                "MASKING_FAILED",
                "Операция временно недоступна",
                context.correlation_id.clone(),
            );
            if self
                .record_terminal(Self::verified_terminal(&context, &failure.code))
                .await
                .is_err()
            {
                return Err(MaskingFailure::with_correlation(
                    "HISTORY_UNAVAILABLE",
                    "Операция временно недоступна",
                    context.correlation_id,
                ));
            }
            return Err(failure);
        }
        Ok((context, response.arguments))
    }

    pub async fn finalize(
        &self,
        context: &MaskingCallContext,
        outcome: FinalizeOutcome,
        field_sources: Option<Value>,
    ) -> Result<ToolCallResult, MaskingFailure> {
        let request = FinalizeRequest {
            schema_version: 1,
            call_id: context.call_id.clone(),
            correlation_id: context.correlation_id.clone(),
            instance_id: context.database.instance_id.clone(),
            cluster_server: context.database.cluster_server.clone(),
            infobase_name: context.database.infobase_name.clone(),
            chat_id: context.chat_id.clone(),
            tool_name: context.tool_name.clone(),
            outcome,
            field_sources,
        };
        let response = match self
            .client
            .as_ref()
            .expect("enabled gate has client")
            .finalize(&request)
            .await
        {
            Ok(response) => response,
            Err(error) => {
                let failure = map_client_error(error, &context.correlation_id);
                if self
                    .record_terminal(Self::verified_terminal(
                        context,
                        terminal_fallback_code(&failure.code),
                    ))
                    .await
                    .is_err()
                {
                    return Err(MaskingFailure::with_correlation(
                        "HISTORY_UNAVAILABLE",
                        "Операция временно недоступна",
                        context.correlation_id.clone(),
                    ));
                }
                return Err(failure);
            }
        };
        if response.schema_version != 1 {
            let failure = MaskingFailure::with_correlation(
                "MASKING_FAILED",
                "Операция временно недоступна",
                context.correlation_id.clone(),
            );
            if self
                .record_terminal(Self::verified_terminal(context, &failure.code))
                .await
                .is_err()
            {
                return Err(MaskingFailure::with_correlation(
                    "HISTORY_UNAVAILABLE",
                    "Операция временно недоступна",
                    context.correlation_id.clone(),
                ));
            }
            return Err(failure);
        }
        Ok(response.public_result)
    }
}

fn terminal_delivery_ack(
    result: Result<crate::session_manager::masking::client::TerminalResponse, ClientError>,
) -> bool {
    match result {
        Ok(response) => response.schema_version == 1 && response.status == "recorded",
        Err(ClientError::Service { status, error }) => {
            status == hyper::StatusCode::CONFLICT && error.code == "TERMINAL_ALREADY_RECORDED"
        }
        Err(_) => false,
    }
}

fn terminal_fallback_code(code: &str) -> &str {
    match code {
        "ACTION_REQUIRED"
        | "TOOL_PENDING_REVIEW"
        | "MASK_TOKEN_INVALID"
        | "SERVICE_NOT_READY"
        //++agent TASK-225 [26.09.2026] фаза-2 C: прогрев словаря —
        // честный код в терминальной записи, а не MASKING_FAILED.
        | "SERVICE_WARMING_UP"
        //++agent TASK-225
        | "POLICY_INVALID"
        | "RESULT_LIMIT_EXCEEDED"
        | "MASKING_TIMEOUT"
        | "MASKING_FAILED"
        | "HISTORY_UNAVAILABLE" => code,
        _ => "MASKING_FAILED",
    }
}

fn map_client_error(error: ClientError, correlation_id: &str) -> MaskingFailure {
    match error {
        ClientError::Service { error, .. } => MaskingFailure {
            code: error.code,
            message: error.message,
            correlation_id: error.correlation_id,
            //++agent TASK-225 [26.09.2026] фаза-2 C
            retry_after_s: error.retry_after_s,
            //++agent TASK-225
        },
        ClientError::Timeout => MaskingFailure::with_correlation(
            "MASKING_TIMEOUT",
            "Операция временно недоступна",
            correlation_id.to_owned(),
        ),
        ClientError::Transport => MaskingFailure::with_correlation(
            "SERVICE_NOT_READY",
            "Операция временно недоступна",
            correlation_id.to_owned(),
        ),
        ClientError::InvalidResponse => MaskingFailure::with_correlation(
            "MASKING_FAILED",
            "Операция временно недоступна",
            correlation_id.to_owned(),
        ),
    }
}

/// Raw dispatcher diagnostics намеренно не входят в service/log payload.
pub fn transport_error_outcome(code: &str) -> FinalizeOutcome {
    FinalizeOutcome::TransportError {
        error: json!({"code": code}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_manager::protocol::{SessionRegisterParams, ToolDescriptor, ToolVisibility};
    use crate::session_manager::registry::SessionRegistry;
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;
    use std::convert::Infallible;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;
    use tokio::net::UnixListener;

    const PRIVATE_KEY: &[u8] = br#"-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIJsIjpj3OJxQ8E1k1uzM1KHxX0H7u+5kzkJboB7MTklh
-----END PRIVATE KEY-----
"#;
    const PUBLIC_KEY: &[u8] = br#"-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAqb56A5d3wWE6xz7XETMTTzhooYvBDkfqBuwYtYdMmvY=
-----END PUBLIC KEY-----
"#;

    const TEST_CLUSTER: &str = "0de031da-e8d9-43de-bb39-7c8bd4d9855c";
    const TEST_INFOBASE: &str = "320f6387-89b5-43fc-b344-67b11f957472";

    /// Identity, которую в бою даёт регистрация с `cluster_server` +
    /// RAS-резолвленным ключом `ras:<cluster>:<infobase>`.
    fn test_identity(infobase_name: &str) -> SessionDatabaseIdentity {
        SessionDatabaseIdentity {
            instance_id: format!("ras:{TEST_CLUSTER}:{TEST_INFOBASE}"),
            cluster_server: "onec-infra".to_owned(),
            infobase_name: infobase_name.to_owned(),
        }
    }

    fn gate() -> MaskingGate {
        let dir = tempfile::tempdir().unwrap();
        let public_key_path = dir.path().join("broker.pub.pem");
        std::fs::write(&public_key_path, PUBLIC_KEY).unwrap();
        let config = MaskingConfig {
            enabled: true,
            broker_public_key_path: public_key_path,
            ..MaskingConfig::default()
        };
        MaskingGate::from_config(&config, dir.path()).unwrap()
    }

    fn registration(client_uid: &str, tools: Vec<ToolDescriptor>) -> SessionRegisterParams {
        registration_with_identity(client_uid, tools, None, None)
    }

    /// `cluster_server`/`database_key` моделируют состояние после
    /// регистрации: `cluster_server` = присланный адаптером `Srvr`,
    /// `database_key` — вычисленный менеджером ключ (`ras:`/`gen:`).
    fn registration_with_identity(
        client_uid: &str,
        tools: Vec<ToolDescriptor>,
        cluster_server: Option<&str>,
        database_key: Option<String>,
    ) -> SessionRegisterParams {
        SessionRegisterParams {
            client_uid: client_uid.to_owned(),
            kind: "server".to_owned(),
            version: "1".to_owned(),
            infobase_name: client_uid.to_owned(),
            ib_session_number: 1,
            tools,
            config_id: Some("server".to_owned()),
            host_id: Some("dev-host".to_owned()),
            pid: None,
            resources: None,
            prompts: None,
            extras: None,
            cluster_server: cluster_server.map(str::to_owned),
            database_key,
        }
    }

    fn tool(name: &str, visibility: ToolVisibility) -> ToolDescriptor {
        ToolDescriptor {
            name: name.to_owned(),
            description: None,
            input_schema: json!({"type":"object"}),
            visibility,
        }
    }

    fn assertion(jti: &str, lifetime_secs: usize) -> String {
        let now = chrono::Utc::now().timestamp() as usize;
        encode(
            &Header::new(Algorithm::EdDSA),
            &json!({
                "conversation_id": "conversation-opaque",
                "jti": jti,
                "iat": now,
                "exp": now + lifetime_secs,
                "aud": "v8-session-manager",
                "iss": "trusted-mcp-broker"
            }),
            &EncodingKey::from_ed_pem(PRIVATE_KEY).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn conversation_assertion_is_short_lived_and_replay_protected() {
        let gate = gate();
        let token = assertion("unique-jti", 60);
        assert_eq!(
            gate.verify_conversation_assertion(&token)
                .unwrap()
                .conversation_id,
            "conversation-opaque"
        );
        assert!(gate.verify_conversation_assertion(&token).is_none());
        assert!(gate
            .verify_conversation_assertion(&assertion("too-long", 301))
            .is_none());
    }

    #[test]
    fn masking_scope_covers_all_tools_and_visibility_is_normalized_by_config() {
        let gate = gate();
        let registry = SessionRegistry::new();

        //++agent TASK-225 [25.09.2026]
        // Все публичные proxy-вызовы идут через gate при enabled — имя
        // инструмента и наличие в бывшем managed_tools роли не играют.
        // Идентичность базы проверяется отдельно: verify_database для
        // сессии без координат → None → DATABASE_IDENTITY_UNVERIFIED.
        //++agent TASK-225
        assert!(gate.is_enabled());
        let legacy = registration(
            "prod-legacy",
            vec![tool("execute_query", ToolVisibility::Public)],
        );
        registry.register(legacy, Instant::now(), None).unwrap();
        let legacy = registry.get("prod-legacy").unwrap();
        assert!(gate.verify_database(&legacy).is_none());

        // adapter-provided `Public` не доверен для имён из конфига;
        // adapter-declared `Internal` сохраняется как fail-safe
        // (ограничение видимости никогда не расширяется).
        let dev_identity = test_identity("dev-trusted");
        let mut dev = registration_with_identity(
            "dev-trusted",
            vec![
                tool("execute_query", ToolVisibility::Internal),
                tool("mcp_internal_masking_metadata_feed", ToolVisibility::Public),
            ],
            Some("onec-infra"),
            Some(dev_identity.instance_id.clone()),
        );
        gate.normalize_tools(&mut dev.tools);
        registry.register(dev, Instant::now(), None).unwrap();
        let dev = registry.get("dev-trusted").unwrap();
        assert_eq!(
            gate.verify_database(&dev),
            Some(test_identity("dev-trusted"))
        );
        assert_eq!(dev.tools[0].visibility, ToolVisibility::Internal);
        assert_eq!(dev.tools[1].visibility, ToolVisibility::Internal);

        // Сессия с неизвестным конфигу именем не получает database identity.
        let other = registration(
            "unknown-session",
            vec![tool("execute_query", ToolVisibility::Public)],
        );
        registry.register(other, Instant::now(), None).unwrap();
        let other = registry.get("unknown-session").unwrap();
        assert!(gate.verify_database(&other).is_none());
    }

    #[test]
    fn terminal_outbox_is_durable_private_bounded_and_rejects_identity_collision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TERMINAL_OUTBOX_FILE);
        let event = TerminalRequest {
            schema_version: 1,
            call_id: Uuid::new_v4().to_string(),
            correlation_id: Uuid::new_v4().to_string(),
            tool_name: "execute_query".to_owned(),
            error_code: "CHAT_IDENTITY_REQUIRED".to_owned(),
            scope: TerminalScope::Unverified,
        };
        let mut outbox = TerminalOutbox::load(path.clone()).unwrap();
        outbox.enqueue(event.clone()).unwrap();
        outbox.enqueue(event.clone()).unwrap();
        assert_eq!(outbox.events.len(), 1);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let restored = TerminalOutbox::load(path.clone()).unwrap();
        assert_eq!(restored.events, vec![event.clone()]);

        let mut collision = event;
        collision.error_code = "SERVICE_NOT_READY".to_owned();
        assert!(outbox.enqueue(collision).is_err());
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "events": [{
                    "schema_version": 1,
                    "call_id": Uuid::new_v4().to_string(),
                    "correlation_id": Uuid::new_v4().to_string(),
                    "tool_name": "execute_query",
                    "error_code": "CHAT_IDENTITY_REQUIRED",
                    "scope": {"kind": "unverified"},
                    "arguments": {"must": "be rejected"}
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(TerminalOutbox::load(path.clone()).is_err());
        std::fs::write(&path, b"not-json").unwrap();
        assert!(TerminalOutbox::load(path).is_err());
    }

    #[test]
    fn terminal_delivery_ack_is_exactly_recorded_or_safe_conflict() {
        use crate::session_manager::masking::client::{ServiceError, TerminalResponse};

        assert!(terminal_delivery_ack(Ok(TerminalResponse {
            schema_version: 1,
            status: "recorded".to_owned(),
        })));
        assert!(terminal_delivery_ack(Err(ClientError::Service {
            status: hyper::StatusCode::CONFLICT,
            error: ServiceError {
                code: "TERMINAL_ALREADY_RECORDED".to_owned(),
                message: "terminal event already exists".to_owned(),
                correlation_id: Uuid::new_v4().to_string(),
                retryable: false,
                retry_after_s: None,
            },
        })));
        assert!(!terminal_delivery_ack(Err(ClientError::Service {
            status: hyper::StatusCode::BAD_REQUEST,
            error: ServiceError {
                code: "TERMINAL_ALREADY_RECORDED".to_owned(),
                message: "wrong status".to_owned(),
                correlation_id: Uuid::new_v4().to_string(),
                retryable: false,
                retry_after_s: None,
            },
        })));
        assert!(!terminal_delivery_ack(Ok(TerminalResponse {
            schema_version: 1,
            status: "already_recorded".to_owned(),
        })));
        assert!(!terminal_delivery_ack(Err(ClientError::Service {
            status: hyper::StatusCode::UNPROCESSABLE_ENTITY,
            error: ServiceError {
                code: "POLICY_INVALID".to_owned(),
                message: "invalid".to_owned(),
                correlation_id: Uuid::new_v4().to_string(),
                retryable: false,
                retry_after_s: None,
            },
        })));
        assert_eq!(
            terminal_fallback_code("MAPPING_UNAVAILABLE"),
            "MASKING_FAILED"
        );
        assert_eq!(
            terminal_fallback_code("TOOL_PENDING_REVIEW"),
            "TOOL_PENDING_REVIEW"
        );
    }

    #[tokio::test]
    async fn terminal_outbox_survives_restart_and_replays_only_safe_wire() {
        let dir = tempfile::tempdir().unwrap();
        let public_key_path = dir.path().join("broker.pub.pem");
        let socket_path = dir.path().join("masking.sock");
        std::fs::write(&public_key_path, PUBLIC_KEY).unwrap();
        let config = MaskingConfig {
            enabled: true,
            socket_path: socket_path.clone(),
            preflight_timeout_ms: 100,
            broker_public_key_path: public_key_path,
            ..MaskingConfig::default()
        };
        let event = TerminalRequest {
            schema_version: 1,
            call_id: Uuid::new_v4().to_string(),
            correlation_id: Uuid::new_v4().to_string(),
            tool_name: "execute_query".to_owned(),
            error_code: "CHAT_IDENTITY_REQUIRED".to_owned(),
            scope: TerminalScope::Unverified,
        };

        let gate = MaskingGate::from_config(&config, dir.path()).unwrap();
        gate.record_terminal(event.clone()).await.unwrap();
        drop(gate);

        let listener = UnixListener::bind(&socket_path).unwrap();
        let (wire_tx, wire_rx) = tokio::sync::oneshot::channel();
        let wire_tx = Arc::new(Mutex::new(Some(wire_tx)));
        let server = tokio::spawn({
            let wire_tx = Arc::clone(&wire_tx);
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |request: Request<hyper::body::Incoming>| {
                            let wire_tx = Arc::clone(&wire_tx);
                            async move {
                                assert_eq!(request.uri().path(), "/internal/v1/calls/terminal");
                                let body = request.into_body().collect().await.unwrap().to_bytes();
                                if let Some(tx) = wire_tx.lock().unwrap().take() {
                                    tx.send(body.to_vec()).unwrap();
                                }
                                Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                                    br#"{"schema_version":1,"status":"recorded"}"#,
                                ))))
                            }
                        }),
                    )
                    .await
                    .unwrap();
            }
        });

        let restored = Arc::new(MaskingGate::from_config(&config, dir.path()).unwrap());
        let shutdown = CancellationToken::new();
        let replay = Arc::clone(&restored).spawn_terminal_replay(shutdown.clone());
        let wire = tokio::time::timeout(Duration::from_secs(2), wire_rx)
            .await
            .unwrap()
            .unwrap();
        let delivered: TerminalRequest = serde_json::from_slice(&wire).unwrap();
        assert_eq!(delivered, event);
        assert_eq!(delivered.scope, TerminalScope::Unverified);
        assert!(!String::from_utf8(wire).unwrap().contains("arguments"));

        server.await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let persisted: TerminalOutboxSnapshot =
            serde_json::from_slice(&std::fs::read(dir.path().join(TERMINAL_OUTBOX_FILE)).unwrap())
                .unwrap();
        assert!(persisted.events.is_empty());
        shutdown.cancel();
        replay.await.unwrap();
    }

    #[tokio::test]
    async fn finalize_transport_failure_persists_only_safe_terminal_event() {
        let dir = tempfile::tempdir().unwrap();
        let public_key_path = dir.path().join("broker.pub.pem");
        std::fs::write(&public_key_path, PUBLIC_KEY).unwrap();
        let config = MaskingConfig {
            enabled: true,
            socket_path: dir.path().join("service-not-running.sock"),
            preflight_timeout_ms: 100,
            finalize_timeout_ms: 100,
            broker_public_key_path: public_key_path,
            ..MaskingConfig::default()
        };
        let gate = MaskingGate::from_config(&config, dir.path()).unwrap();
        let context = MaskingCallContext {
            call_id: Uuid::new_v4().to_string(),
            correlation_id: Uuid::new_v4().to_string(),
            database: test_identity("test-ib"),
            chat_id: "opaque-chat".to_owned(),
            tool_name: "execute_query".to_owned(),
        };
        let failure = gate
            .finalize(
                &context,
                FinalizeOutcome::ToolResult {
                    result: json!({
                        "success": true,
                        "data": ["raw-marker-must-never-enter-terminal-ledger"],
                        "raw": "private"
                    }),
                },
                Some(json!({"lineage": ["private"]})),
            )
            .await
            .unwrap_err();
        assert_eq!(failure.code, "SERVICE_NOT_READY");

        let wire = std::fs::read_to_string(dir.path().join(TERMINAL_OUTBOX_FILE)).unwrap();
        assert!(!wire.contains("raw-marker-must-never-enter-terminal-ledger"));
        assert!(!wire.contains("lineage"));
        let outbox: TerminalOutboxSnapshot = serde_json::from_str(&wire).unwrap();
        assert_eq!(outbox.events.len(), 1);
        assert_eq!(outbox.events[0].error_code, "SERVICE_NOT_READY");
        assert_eq!(
            outbox.events[0].scope,
            TerminalScope::Verified {
                instance_id: context.database.instance_id.clone(),
                cluster_server: context.database.cluster_server.clone(),
                infobase_name: context.database.infobase_name.clone(),
                chat_id: context.chat_id.clone(),
            }
        );
    }

    #[tokio::test]
    async fn preflight_service_not_ready_is_recorded_through_terminal_route() {
        let dir = tempfile::tempdir().unwrap();
        let public_key_path = dir.path().join("broker.pub.pem");
        let socket_path = dir.path().join("masking.sock");
        std::fs::write(&public_key_path, PUBLIC_KEY).unwrap();
        let config = MaskingConfig {
            enabled: true,
            socket_path: socket_path.clone(),
            preflight_timeout_ms: 500,
            broker_public_key_path: public_key_path,
            ..MaskingConfig::default()
        };
        let context = MaskingCallContext {
            call_id: Uuid::new_v4().to_string(),
            correlation_id: Uuid::new_v4().to_string(),
            database: test_identity("test-ib"),
            chat_id: "opaque-chat".to_owned(),
            tool_name: "execute_query".to_owned(),
        };

        let listener = UnixListener::bind(&socket_path).unwrap();
        let (terminal_tx, terminal_rx) = tokio::sync::oneshot::channel();
        let terminal_tx = Arc::new(Mutex::new(Some(terminal_tx)));
        let correlation_id = context.correlation_id.clone();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let Ok(Ok((stream, _))) =
                    tokio::time::timeout(Duration::from_secs(1), listener.accept()).await
                else {
                    break;
                };
                let terminal_tx = Arc::clone(&terminal_tx);
                let correlation_id = correlation_id.clone();
                http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |request: Request<hyper::body::Incoming>| {
                            let terminal_tx = Arc::clone(&terminal_tx);
                            let correlation_id = correlation_id.clone();
                            async move {
                                let path = request.uri().path().to_owned();
                                let body = request.into_body().collect().await.unwrap().to_bytes();
                                if path == "/internal/v1/calls/terminal" {
                                    if let Some(tx) = terminal_tx.lock().unwrap().take() {
                                        tx.send(body.to_vec()).unwrap();
                                    }
                                    return Ok::<_, Infallible>(Response::new(Full::new(
                                        Bytes::from_static(
                                            br#"{"schema_version":1,"status":"recorded"}"#,
                                        ),
                                    )));
                                }
                                assert_eq!(path, "/internal/v1/calls/preflight");
                                Ok::<_, Infallible>(
                                    Response::builder()
                                        .status(hyper::StatusCode::SERVICE_UNAVAILABLE)
                                        .body(Full::new(Bytes::from(
                                            serde_json::to_vec(&json!({
                                                "error": {
                                                    "code": "SERVICE_NOT_READY",
                                                    "message": "Операция временно недоступна",
                                                    "correlation_id": correlation_id,
                                                    "retryable": true
                                                }
                                            }))
                                            .unwrap(),
                                        )))
                                        .unwrap(),
                                )
                            }
                        }),
                    )
                    .await
                    .unwrap();
            }
        });

        let gate = MaskingGate::from_config(&config, dir.path()).unwrap();
        let failure = gate
            .preflight(context.clone(), json!({"query":"private"}))
            .await
            .unwrap_err();
        assert_eq!(failure.code, "SERVICE_NOT_READY");
        let terminal_wire = tokio::time::timeout(Duration::from_millis(700), terminal_rx)
            .await
            .expect("SERVICE_NOT_READY must be recorded through terminal route")
            .unwrap();
        let terminal: TerminalRequest = serde_json::from_slice(&terminal_wire).unwrap();
        assert_eq!(terminal.call_id, context.call_id);
        assert_eq!(terminal.error_code, "SERVICE_NOT_READY");
        assert!(!String::from_utf8(terminal_wire)
            .unwrap()
            .contains("private"));
        server.await.unwrap();
    }

    //++agent TASK-225 [26.09.2026] фаза-2 L R4-1: SERVICE_WARMING_UP —
    // честный код терминала (whitelist valid_terminal + fallback), а не
    // MASKING_FAILED; retry_after_s живёт в MaskingFailure. Сквозная
    // доставка терминала и дренаж outbox покрыты
    // `preflight_service_not_ready_is_recorded_through_terminal_route` и
    // replay-тестом выше — механика для этого кода та же; живой прогон
    // менеджер→сервис сделан на DEV (impl-t227.md, раздел L).
    #[test]
    fn warming_up_code_passes_terminal_whitelist_and_keeps_retry_after() {
        let context = MaskingCallContext {
            call_id: Uuid::new_v4().to_string(),
            correlation_id: Uuid::new_v4().to_string(),
            database: test_identity("test-ib"),
            chat_id: "opaque-chat".to_owned(),
            tool_name: "execute_query".to_owned(),
        };
        let event = MaskingGate::verified_terminal(&context, "SERVICE_WARMING_UP");
        assert!(valid_terminal(&event));
        assert_eq!(
            terminal_fallback_code("SERVICE_WARMING_UP"),
            "SERVICE_WARMING_UP"
        );
        let failure = map_client_error(
            ClientError::Service {
                status: hyper::StatusCode::SERVICE_UNAVAILABLE,
                error: crate::session_manager::masking::client::ServiceError {
                    code: "SERVICE_WARMING_UP".to_owned(),
                    message: "прогрев".to_owned(),
                    correlation_id: context.correlation_id.clone(),
                    retryable: true,
                    retry_after_s: Some(5),
                },
            },
            &context.correlation_id,
        );
        assert_eq!(failure.code, "SERVICE_WARMING_UP");
        assert_eq!(failure.retry_after_s, Some(5));
    }
    //++agent TASK-225
}
