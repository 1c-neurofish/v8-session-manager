//! Изолированная проверка wire-контракта manager ↔ masking-service.
//!
//! Тест намеренно ignored: вызывающая сторона должна передать UDS временного
//! экземпляра сервиса с подготовленными database/job fixture.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use v8_session_manager::session_manager::masking::client::{
    ClientError, FinalizeOutcome, FinalizeRequest, MaskingServiceClient, PreflightRequest,
};
use v8_session_manager::session_manager::protocol::ToolContent;

const CONFIGURED_CLUSTER_GUID: &str = "123e4567-e89b-12d3-a456-426614174001";
const CONFIGURED_INFOBASE_GUID: &str = "123e4567-e89b-12d3-a456-426614174000";
const UNKNOWN_INFOBASE_GUID: &str = "123e4567-e89b-12d3-a456-426614174099";

fn contract_client() -> MaskingServiceClient {
    let socket = std::env::var_os("MASKING_CONTRACT_SOCKET")
        .map(PathBuf::from)
        .expect("MASKING_CONTRACT_SOCKET must point to the isolated service UDS");
    MaskingServiceClient::new(socket, Duration::from_secs(3), Duration::from_secs(15))
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
            cluster_server: "onec-infra".to_owned(),
            infobase_name: "contract-ib".to_owned(),
            instance_id: format!("ras:{}:{}", CONFIGURED_CLUSTER_GUID, UNKNOWN_INFOBASE_GUID),
            caller: Some("contract-client/1.0 #0000abcd".to_owned()),
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
            cluster_server: "onec-infra".to_owned(),
            infobase_name: "contract-ib".to_owned(),
            instance_id: format!(
                "ras:{}:{}",
                CONFIGURED_CLUSTER_GUID, CONFIGURED_INFOBASE_GUID
            ),
            caller: Some("contract-client/1.0 #0000abcd".to_owned()),
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
            cluster_server: "onec-infra".to_owned(),
            infobase_name: "contract-ib".to_owned(),
            instance_id: format!(
                "ras:{}:{}",
                CONFIGURED_CLUSTER_GUID, CONFIGURED_INFOBASE_GUID
            ),
            caller: Some("contract-client/1.0 #0000abcd".to_owned()),
            tool_name: "get_metadata".to_owned(),
            outcome: FinalizeOutcome::ToolResult {
                // Контракт Р2: непрозрачный бизнес-результат; `data` должна
                // доехать до сервиса и обратно без потери полей.
                result: json!({
                    "success": true,
                    "data": [["contract-ok"]],
                    "status": "ok"
                }),
            },
            field_sources: Some(json!({
                "schema": {"version": 1},
                "lineage": [],
                "degraded_reasons": []
            })),
        })
        .await
        .expect("opaque tool result must be accepted by finalize");
    assert_eq!(finalized.schema_version, 1);
    assert!(!finalized.public_result.is_error);
    assert_eq!(finalized.public_result.content.len(), 1);
    let ToolContent::Text { text } = &finalized.public_result.content[0] else {
        panic!("public result must wrap the masked value into text content");
    };
    let returned: serde_json::Value =
        serde_json::from_str(text).expect("masked public value must be valid JSON");
    assert_eq!(returned["data"][0][0], "contract-ok");
    assert_eq!(returned["status"], "ok");

    let transport = client
        .finalize(&FinalizeRequest {
            schema_version: 1,
            call_id: "123e4567-e89b-12d3-a456-426614174107".to_owned(),
            correlation_id: "123e4567-e89b-12d3-a456-426614174108".to_owned(),
            cluster_server: "onec-infra".to_owned(),
            infobase_name: "contract-ib".to_owned(),
            instance_id: format!(
                "ras:{}:{}",
                CONFIGURED_CLUSTER_GUID, CONFIGURED_INFOBASE_GUID
            ),
            caller: Some("contract-client/1.0 #0000abcd".to_owned()),
            tool_name: "get_metadata".to_owned(),
            outcome: FinalizeOutcome::TransportError {
                error: json!({"code": "fixture_transport_error"}),
            },
            field_sources: None,
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
        cluster_server: "onec-infra".to_owned(),
        infobase_name: "contract-ib".to_owned(),
        instance_id: format!(
            "ras:{}:{}",
            CONFIGURED_CLUSTER_GUID, CONFIGURED_INFOBASE_GUID
        ),
        caller: Some("contract-client/1.0 #0000abcd".to_owned()),
        tool_name: "execute_query".to_owned(),
        outcome: FinalizeOutcome::ToolResult {
            result: json!({
                "success": true,
                "data": [format!("{over_limit_marker}{}", "x".repeat(8 * 1024 * 1024))]
            }),
        },
        field_sources: None,
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
}
