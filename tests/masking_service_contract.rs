//! Изолированная проверка wire-контракта manager ↔ masking-service.
//!
//! Тест намеренно ignored: вызывающая сторона должна передать UDS временного
//! экземпляра сервиса с подготовленными database/job fixture.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use sha2::{Digest, Sha256};
use v8_session_manager::session_manager::masking::client::{
    ClientError, FeedActivateRequest, FeedChunkPayload, FeedChunkRequest, FinalizeOutcome,
    FinalizeRequest, MaskingServiceClient, PreflightRequest,
};
use v8_session_manager::session_manager::protocol::{ToolCallResult, ToolContent};

const CONFIGURED_DATABASE_ID: &str = "123e4567-e89b-12d3-a456-426614174000";
const UNKNOWN_DATABASE_ID: &str = "123e4567-e89b-12d3-a456-426614174099";

fn contract_client() -> MaskingServiceClient {
    let socket = std::env::var_os("MASKING_CONTRACT_SOCKET")
        .map(PathBuf::from)
        .expect("MASKING_CONTRACT_SOCKET must point to the isolated service UDS");
    MaskingServiceClient::new(
        socket,
        Duration::from_secs(3),
        Duration::from_secs(15),
        Duration::from_secs(10),
        Duration::from_secs(60),
    )
}

#[tokio::test]
#[ignore = "requires an isolated masking-service UDS and seeded temporary SQLite fixture"]
async fn isolated_masking_service_wire_contract() {
    let client = contract_client();

    let unknown_correlation = "123e4567-e89b-12d3-a456-426614174101";
    let unknown = client
        .preflight(&PreflightRequest {
            schema_version: 1,
            call_id: "123e4567-e89b-12d3-a456-426614174102".to_owned(),
            correlation_id: unknown_correlation.to_owned(),
            database_id: UNKNOWN_DATABASE_ID.to_owned(),
            chat_id: "contract-chat".to_owned(),
            tool_name: "get_metadata".to_owned(),
            arguments: json!({"probe": "contract"}),
        })
        .await;
    match unknown {
        Err(ClientError::Service { error, .. }) => {
            assert_eq!(error.code, "ACTION_REQUIRED");
            assert_eq!(error.correlation_id, unknown_correlation);
            assert!(!error.retryable);
        }
        other => panic!("unknown database must fail closed, got {other:?}"),
    }

    let arguments = json!({"probe": "contract"});
    let preflight = client
        .preflight(&PreflightRequest {
            schema_version: 1,
            call_id: "123e4567-e89b-12d3-a456-426614174103".to_owned(),
            correlation_id: "123e4567-e89b-12d3-a456-426614174104".to_owned(),
            database_id: CONFIGURED_DATABASE_ID.to_owned(),
            chat_id: "contract-chat".to_owned(),
            tool_name: "get_metadata".to_owned(),
            arguments: arguments.clone(),
        })
        .await
        .expect("configured preflight must be accepted");
    assert_eq!(preflight.schema_version, 1);
    assert_eq!(preflight.decision, "allow");
    assert_eq!(preflight.arguments, arguments);

    let finalized = client
        .finalize(&FinalizeRequest {
            schema_version: 1,
            call_id: "123e4567-e89b-12d3-a456-426614174105".to_owned(),
            correlation_id: "123e4567-e89b-12d3-a456-426614174106".to_owned(),
            database_id: CONFIGURED_DATABASE_ID.to_owned(),
            chat_id: "contract-chat".to_owned(),
            tool_name: "get_metadata".to_owned(),
            outcome: FinalizeOutcome::ToolResult {
                result: ToolCallResult {
                    content: vec![
                        ToolContent::Text {
                            text: "contract-ok".to_owned(),
                        },
                        ToolContent::Json {
                            json: json!({"status": "ok"}),
                        },
                    ],
                    is_error: false,
                    structured_content: Some(json!({"status": "ok"})),
                },
            },
            evidence: Some(json!({
                "schema": {"version": 1},
                "lineage": [],
                "degraded_reasons": []
            })),
        })
        .await
        .expect("full ToolCallResult must be accepted by finalize");
    assert_eq!(finalized.schema_version, 1);
    assert!(!finalized.public_result.is_error);
    assert_eq!(finalized.public_result.content.len(), 2);
    assert_eq!(
        finalized.public_result.structured_content,
        Some(json!({"status": "ok"}))
    );

    let transport = client
        .finalize(&FinalizeRequest {
            schema_version: 1,
            call_id: "123e4567-e89b-12d3-a456-426614174107".to_owned(),
            correlation_id: "123e4567-e89b-12d3-a456-426614174108".to_owned(),
            database_id: CONFIGURED_DATABASE_ID.to_owned(),
            chat_id: "contract-chat".to_owned(),
            tool_name: "get_metadata".to_owned(),
            outcome: FinalizeOutcome::TransportError {
                error: json!({"code": "fixture_transport_error"}),
            },
            evidence: None,
        })
        .await
        .expect("transport error must be finalized into a public result");
    assert!(transport.public_result.is_error);
    assert!(!transport.public_result.content.is_empty());

    let over_limit_marker = "oversized-result-must-not-cross-uds";
    let over_limit_request = FinalizeRequest {
        schema_version: 1,
        call_id: "123e4567-e89b-12d3-a456-426614174111".to_owned(),
        correlation_id: "123e4567-e89b-12d3-a456-426614174112".to_owned(),
        database_id: CONFIGURED_DATABASE_ID.to_owned(),
        chat_id: "contract-chat".to_owned(),
        tool_name: "execute_query".to_owned(),
        outcome: FinalizeOutcome::ToolResult {
            result: ToolCallResult {
                content: vec![ToolContent::Text {
                    text: format!("{over_limit_marker}{}", "x".repeat(8 * 1024 * 1024)),
                }],
                is_error: false,
                structured_content: None,
            },
        },
        evidence: None,
    };
    let over_limit = client
        .finalize(&over_limit_request)
        .await
        .expect("oversized result must finalize through the bounded synthetic outcome");
    assert!(over_limit.public_result.is_error);
    assert!(!serde_json::to_string(&over_limit.public_result)
        .unwrap()
        .contains(over_limit_marker));
    let over_limit_retry = client
        .finalize(&over_limit_request)
        .await
        .expect("duplicate synthetic finalize must be idempotent");
    assert_eq!(over_limit_retry.public_result, over_limit.public_result);

    let jobs = client
        .feed_jobs(10)
        .await
        .expect("feed jobs must deserialize");
    assert_eq!(jobs.schema_version, 1);
    let job = jobs
        .jobs
        .into_iter()
        .find(|job| job.database_id == CONFIGURED_DATABASE_ID)
        .expect("seeded feed job must be returned");
    assert_eq!(job.metadata_selector.mode, "all");
    assert_eq!(job.metadata_selector.page_size, 1000);
    assert!(job.dictionary_selectors.is_empty());

    let payload = FeedChunkPayload {
        selection_id: None,
        page_index: 0,
        metadata: Vec::new(),
        dictionary_values: Vec::new(),
        final_chunk: true,
    };
    let canonical =
        br#"{"dictionary_values":[],"final_chunk":true,"metadata":[],"page_index":0,"selection_id":null}"#;
    let chunk_digest = Sha256::digest(canonical);
    let accepted = client
        .feed_chunk(
            &job.job_id,
            0,
            &FeedChunkRequest {
                schema_version: 1,
                correlation_id: "123e4567-e89b-12d3-a456-426614174109".to_owned(),
                chunk_digest: hex::encode(chunk_digest),
                payload,
            },
        )
        .await
        .expect("canonical feed chunk must be accepted");
    assert_eq!(accepted.schema_version, 1);
    assert_eq!(accepted.accepted_index, 0);

    let aggregate_digest = hex::encode(Sha256::digest(chunk_digest));
    let activated = client
        .feed_activate(
            &job.job_id,
            &FeedActivateRequest {
                schema_version: 1,
                correlation_id: "123e4567-e89b-12d3-a456-426614174110".to_owned(),
                expected_chunks: 1,
                expected_metadata_count: 0,
                expected_dictionary_count: 0,
                aggregate_digest,
            },
        )
        .await
        .expect("feed must activate with manager digest rules");
    assert_eq!(activated.schema_version, 1);
    assert_eq!(activated.cache_version, job.target_version);
    assert_eq!(activated.status, "active");
}
