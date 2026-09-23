use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::UnixStream;

use crate::session_manager::protocol::ToolCallResult;

const MAX_INTERNAL_BODY_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct PreflightRequest {
    pub schema_version: u8,
    pub call_id: String,
    pub correlation_id: String,
    pub database_id: String,
    pub chat_id: String,
    pub tool_name: String,
    pub arguments: Value,
}

#[derive(Debug, Deserialize)]
pub struct PreflightResponse {
    pub schema_version: u8,
    pub decision: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct FinalizeRequest {
    pub schema_version: u8,
    pub call_id: String,
    pub correlation_id: String,
    pub database_id: String,
    pub chat_id: String,
    pub tool_name: String,
    pub outcome: FinalizeOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FinalizeOutcome {
    ToolResult {
        #[serde(serialize_with = "serialize_finalize_tool_result")]
        result: ToolCallResult,
    },
    TransportError {
        error: Value,
    },
}

fn serialize_finalize_tool_result<S>(
    result: &ToolCallResult,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let field_count = if result.structured_content.is_some() {
        3
    } else {
        2
    };
    let mut wire = serializer.serialize_struct("ToolCallResult", field_count)?;
    wire.serialize_field("content", &result.content)?;
    wire.serialize_field("is_error", &result.is_error)?;
    if let Some(structured_content) = &result.structured_content {
        wire.serialize_field("structured_content", structured_content)?;
    }
    wire.end()
}

#[derive(Debug, Deserialize)]
pub struct FinalizeResponse {
    pub schema_version: u8,
    pub public_result: ToolCallResult,
}

/// Safe terminal event: intentionally cannot carry arguments, response bodies
/// or an unverified candidate database/chat identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TerminalRequest {
    pub schema_version: u8,
    pub call_id: String,
    pub correlation_id: String,
    pub tool_name: String,
    pub error_code: String,
    pub scope: TerminalScope,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TerminalScope {
    Verified {
        database_id: String,
        chat_id: String,
    },
    Unverified,
}

#[derive(Debug, Deserialize)]
pub struct TerminalResponse {
    pub schema_version: u8,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceErrorEnvelope {
    pub error: ServiceError,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceError {
    pub code: String,
    pub message: String,
    pub correlation_id: String,
    pub retryable: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("masking service transport unavailable")]
    Transport,
    #[error("masking service deadline exceeded")]
    Timeout,
    #[error("masking service returned an invalid response")]
    InvalidResponse,
    #[error("masking service denied the call")]
    Service {
        status: StatusCode,
        error: ServiceError,
    },
}

/// Typed HTTP/1.1 client over a Unix domain socket.
#[derive(Debug, Clone)]
pub struct MaskingServiceClient {
    socket_path: PathBuf,
    preflight_timeout: Duration,
    finalize_timeout: Duration,
    feed_chunk_timeout: Duration,
    feed_activate_timeout: Duration,
}

impl MaskingServiceClient {
    pub fn new(
        socket_path: PathBuf,
        preflight_timeout: Duration,
        finalize_timeout: Duration,
        feed_chunk_timeout: Duration,
        feed_activate_timeout: Duration,
    ) -> Self {
        Self {
            socket_path,
            preflight_timeout,
            finalize_timeout,
            feed_chunk_timeout,
            feed_activate_timeout,
        }
    }

    pub async fn preflight(
        &self,
        request: &PreflightRequest,
    ) -> Result<PreflightResponse, ClientError> {
        self.post(
            "/internal/v1/calls/preflight",
            request,
            self.preflight_timeout,
        )
        .await
    }

    pub async fn finalize(
        &self,
        request: &FinalizeRequest,
    ) -> Result<FinalizeResponse, ClientError> {
        let body = serialize_finalize_request(request)?;
        let attempt = async {
            match request_json(
                &self.socket_path,
                hyper::Method::POST,
                "/internal/v1/calls/finalize",
                body.clone(),
            )
            .await
            {
                Err(ClientError::Transport) => {
                    request_json(
                        &self.socket_path,
                        hyper::Method::POST,
                        "/internal/v1/calls/finalize",
                        body,
                    )
                    .await
                }
                result => result,
            }
        };
        tokio::time::timeout(self.finalize_timeout, attempt)
            .await
            .map_err(|_| ClientError::Timeout)?
    }

    pub async fn terminal(
        &self,
        request: &TerminalRequest,
    ) -> Result<TerminalResponse, ClientError> {
        let body = serde_json::to_vec(request).map_err(|_| ClientError::InvalidResponse)?;
        let attempt = async {
            match request_json(
                &self.socket_path,
                hyper::Method::POST,
                "/internal/v1/calls/terminal",
                body.clone(),
            )
            .await
            {
                Err(ClientError::Transport) => {
                    request_json(
                        &self.socket_path,
                        hyper::Method::POST,
                        "/internal/v1/calls/terminal",
                        body,
                    )
                    .await
                }
                result => result,
            }
        };
        tokio::time::timeout(self.preflight_timeout, attempt)
            .await
            .map_err(|_| ClientError::Timeout)?
    }

    async fn post<T, R>(&self, path: &str, payload: &T, timeout: Duration) -> Result<R, ClientError>
    where
        T: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let body = serde_json::to_vec(payload).map_err(|_| ClientError::InvalidResponse)?;
        let future = request_json(&self.socket_path, hyper::Method::POST, path, body);
        match tokio::time::timeout(timeout, future).await {
            Ok(result) => result,
            Err(_) => Err(ClientError::Timeout),
        }
    }

    pub async fn feed_jobs(&self, limit: u8) -> Result<FeedJobsResponse, ClientError> {
        let path = format!("/internal/v1/feed/jobs?limit={}", limit.clamp(1, 10));
        let future = request_json(&self.socket_path, hyper::Method::GET, &path, Vec::new());
        tokio::time::timeout(self.feed_chunk_timeout, future)
            .await
            .map_err(|_| ClientError::Timeout)?
    }

    pub async fn feed_chunk(
        &self,
        job_id: &str,
        index: u32,
        request: &FeedChunkRequest,
    ) -> Result<FeedChunkResponse, ClientError> {
        self.post(
            &format!("/internal/v1/feed/jobs/{job_id}/chunks/{index}"),
            request,
            self.feed_chunk_timeout,
        )
        .await
    }

    pub async fn feed_activate(
        &self,
        job_id: &str,
        request: &FeedActivateRequest,
    ) -> Result<FeedActivateResponse, ClientError> {
        self.post(
            &format!("/internal/v1/feed/jobs/{job_id}/activate"),
            request,
            self.feed_activate_timeout,
        )
        .await
    }

    pub async fn feed_fail(
        &self,
        job_id: &str,
        request: &FeedFailRequest,
    ) -> Result<FeedFailResponse, ClientError> {
        self.post(
            &format!("/internal/v1/feed/jobs/{job_id}/fail"),
            request,
            self.feed_chunk_timeout,
        )
        .await
    }
}

/// Если полный результат не помещается в bounded internal API, сохраняем ту же
/// idempotency identity и передаём только фиксированный terminal outcome. Raw
/// результат и evidence при этом не пересекают UDS и не попадают в history.
fn serialize_finalize_request(request: &FinalizeRequest) -> Result<Vec<u8>, ClientError> {
    let body = serde_json::to_vec(request).map_err(|_| ClientError::InvalidResponse)?;
    if body.len() <= MAX_INTERNAL_BODY_BYTES {
        return Ok(body);
    }

    let fallback = FinalizeRequest {
        schema_version: request.schema_version,
        call_id: request.call_id.clone(),
        correlation_id: request.correlation_id.clone(),
        database_id: request.database_id.clone(),
        chat_id: request.chat_id.clone(),
        tool_name: request.tool_name.clone(),
        outcome: FinalizeOutcome::TransportError {
            error: serde_json::json!({"code": "RESULT_LIMIT_EXCEEDED"}),
        },
        evidence: Some(serde_json::json!({
            "degraded_reasons": ["manager:result_limit_exceeded"]
        })),
    };
    let body = serde_json::to_vec(&fallback).map_err(|_| ClientError::InvalidResponse)?;
    if body.len() > MAX_INTERNAL_BODY_BYTES {
        return Err(ClientError::InvalidResponse);
    }
    Ok(body)
}

async fn request_json<R: DeserializeOwned>(
    socket_path: &Path,
    method: hyper::Method,
    path: &str,
    body: Vec<u8>,
) -> Result<R, ClientError> {
    if body.len() > MAX_INTERNAL_BODY_BYTES {
        return Err(ClientError::InvalidResponse);
    }
    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(|_| ClientError::Transport)?;
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|_| ClientError::Transport)?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, "1c-masking")
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .map_err(|_| ClientError::InvalidResponse)?;
    let mut response = sender
        .send_request(request)
        .await
        .map_err(|_| ClientError::Transport)?;
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(frame) = response.body_mut().frame().await {
        let frame = frame.map_err(|_| ClientError::Transport)?;
        if let Ok(data) = frame.into_data() {
            if bytes.len().saturating_add(data.len()) > MAX_INTERNAL_BODY_BYTES {
                return Err(ClientError::InvalidResponse);
            }
            bytes.extend_from_slice(&data);
        }
    }
    if status == StatusCode::OK {
        return serde_json::from_slice(&bytes).map_err(|_| ClientError::InvalidResponse);
    }
    if let Ok(error) = serde_json::from_slice::<ServiceErrorEnvelope>(&bytes) {
        return Err(ClientError::Service {
            status,
            error: error.error,
        });
    }
    Err(ClientError::InvalidResponse)
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeedJobsResponse {
    pub schema_version: u8,
    pub jobs: Vec<FeedJob>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeedJob {
    pub job_id: String,
    pub database_id: String,
    pub target_version: u64,
    pub max_chunk_bytes: usize,
    pub metadata_selector: MetadataSelector,
    pub dictionary_selectors: Vec<DictionarySelector>,
    pub hard_limits: FeedHardLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataSelector {
    pub mode: String,
    pub page_size: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictionarySelector {
    pub selection_id: String,
    pub source_path: String,
    pub category: String,
    pub filter_ast: Option<Value>,
    pub page_size: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeedHardLimits {
    pub max_source_paths: usize,
    pub max_total_values: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedMetadataItem {
    pub source_path: String,
    pub field_name: String,
    pub field_type: String,
    pub password_mode: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedDictionaryValue {
    pub source_path: String,
    pub category: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeedChunkPayload {
    pub selection_id: Option<String>,
    pub page_index: u32,
    pub metadata: Vec<FeedMetadataItem>,
    pub dictionary_values: Vec<FeedDictionaryValue>,
    pub final_chunk: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeedChunkRequest {
    pub schema_version: u8,
    pub correlation_id: String,
    pub chunk_digest: String,
    pub payload: FeedChunkPayload,
}

#[derive(Debug, Deserialize)]
pub struct FeedChunkResponse {
    pub schema_version: u8,
    pub accepted_index: u32,
}

#[derive(Debug, Serialize)]
pub struct FeedActivateRequest {
    pub schema_version: u8,
    pub correlation_id: String,
    pub expected_chunks: u32,
    pub expected_metadata_count: u64,
    pub expected_dictionary_count: u64,
    pub aggregate_digest: String,
}

#[derive(Debug, Deserialize)]
pub struct FeedActivateResponse {
    pub schema_version: u8,
    pub cache_version: u64,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct FeedFailRequest {
    pub schema_version: u8,
    pub correlation_id: String,
    pub reason_code: String,
}

#[derive(Debug, Deserialize)]
pub struct FeedFailResponse {
    pub schema_version: u8,
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn internal_wire_shapes_match_frozen_contract() {
        let preflight = PreflightRequest {
            schema_version: 1,
            call_id: "call".to_owned(),
            correlation_id: "corr".to_owned(),
            database_id: "db".to_owned(),
            chat_id: "chat".to_owned(),
            tool_name: "execute_query".to_owned(),
            arguments: json!({"query": "select"}),
        };
        assert_eq!(
            serde_json::to_value(preflight).unwrap()["schema_version"],
            1
        );

        let finalize = FinalizeRequest {
            schema_version: 1,
            call_id: "call".to_owned(),
            correlation_id: "corr".to_owned(),
            database_id: "db".to_owned(),
            chat_id: "chat".to_owned(),
            tool_name: "execute_query".to_owned(),
            outcome: FinalizeOutcome::ToolResult {
                result: ToolCallResult {
                    content: vec![crate::session_manager::protocol::ToolContent::Text {
                        text: "masked".to_owned(),
                    }],
                    is_error: false,
                    structured_content: Some(json!({"masked": true})),
                },
            },
            evidence: Some(json!({"lineage": []})),
        };
        let value = serde_json::to_value(finalize).unwrap();
        assert_eq!(value["outcome"]["kind"], "tool_result");
        assert_eq!(value["outcome"]["result"]["content"][0]["type"], "text");
        assert_eq!(value["outcome"]["result"]["is_error"], false);
        assert!(value.get("history_id").is_none());

        let metadata_chunk = FeedChunkPayload {
            selection_id: None,
            page_index: 0,
            metadata: Vec::new(),
            dictionary_values: Vec::new(),
            final_chunk: true,
        };
        assert_eq!(
            serde_json::to_value(metadata_chunk).unwrap()["selection_id"],
            Value::Null
        );

        let verified_terminal = TerminalRequest {
            schema_version: 1,
            call_id: "123e4567-e89b-12d3-a456-426614174001".to_owned(),
            correlation_id: "123e4567-e89b-12d3-a456-426614174002".to_owned(),
            tool_name: "execute_query".to_owned(),
            error_code: "SERVICE_NOT_READY".to_owned(),
            scope: TerminalScope::Verified {
                database_id: "123e4567-e89b-12d3-a456-426614174003".to_owned(),
                chat_id: "opaque-chat".to_owned(),
            },
        };
        assert_eq!(
            serde_json::to_value(verified_terminal).unwrap(),
            json!({
                "schema_version": 1,
                "call_id": "123e4567-e89b-12d3-a456-426614174001",
                "correlation_id": "123e4567-e89b-12d3-a456-426614174002",
                "tool_name": "execute_query",
                "error_code": "SERVICE_NOT_READY",
                "scope": {
                    "kind": "verified",
                    "database_id": "123e4567-e89b-12d3-a456-426614174003",
                    "chat_id": "opaque-chat"
                }
            })
        );

        let unverified_terminal = TerminalRequest {
            schema_version: 1,
            call_id: "123e4567-e89b-12d3-a456-426614174011".to_owned(),
            correlation_id: "123e4567-e89b-12d3-a456-426614174012".to_owned(),
            tool_name: "execute_query".to_owned(),
            error_code: "CHAT_IDENTITY_REQUIRED".to_owned(),
            scope: TerminalScope::Unverified,
        };
        let wire = serde_json::to_value(unverified_terminal).unwrap();
        assert_eq!(wire["scope"], json!({"kind": "unverified"}));
        assert!(wire["scope"].get("database_id").is_none());
        assert!(wire["scope"].get("chat_id").is_none());
    }

    #[test]
    fn oversized_finalize_becomes_bounded_idempotent_safe_outcome() {
        let raw_marker = "must-not-cross-uds";
        let request = FinalizeRequest {
            schema_version: 1,
            call_id: "123e4567-e89b-12d3-a456-426614174001".to_owned(),
            correlation_id: "123e4567-e89b-12d3-a456-426614174002".to_owned(),
            database_id: "123e4567-e89b-12d3-a456-426614174003".to_owned(),
            chat_id: "opaque-chat".to_owned(),
            tool_name: "execute_query".to_owned(),
            outcome: FinalizeOutcome::ToolResult {
                result: ToolCallResult {
                    content: vec![crate::session_manager::protocol::ToolContent::Text {
                        text: format!("{raw_marker}{}", "x".repeat(MAX_INTERNAL_BODY_BYTES)),
                    }],
                    is_error: false,
                    structured_content: None,
                },
            },
            evidence: Some(json!({"lineage": [raw_marker]})),
        };

        let body = serialize_finalize_request(&request).unwrap();
        assert!(body.len() <= MAX_INTERNAL_BODY_BYTES);
        let wire: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(wire["call_id"], request.call_id);
        assert_eq!(wire["correlation_id"], request.correlation_id);
        assert_eq!(wire["database_id"], request.database_id);
        assert_eq!(wire["chat_id"], request.chat_id);
        assert_eq!(wire["tool_name"], request.tool_name);
        assert_eq!(wire["outcome"]["kind"], "transport_error");
        assert_eq!(
            wire["outcome"]["error"],
            json!({"code":"RESULT_LIMIT_EXCEEDED"})
        );
        assert_eq!(
            wire["evidence"],
            json!({"degraded_reasons":["manager:result_limit_exceeded"]})
        );
        assert!(!String::from_utf8(body).unwrap().contains(raw_marker));
    }
}
