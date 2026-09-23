//! Manager-only dispatch для hidden feed tools.

use std::time::Duration;

use std::collections::HashSet;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::session_manager::masking::client::{
    DictionarySelector, FeedActivateRequest, FeedChunkPayload, FeedChunkRequest,
    FeedDictionaryValue, FeedFailRequest, FeedJob, FeedMetadataItem,
};
use crate::session_manager::masking::identity::{IdentityResolver, VerifiedDatabaseIdentity};
use crate::session_manager::masking::MaskingGate;
use crate::session_manager::protocol::{ToolCallParams, ToolCallResult, ToolVisibility};
use crate::session_manager::registry::{SessionRecord, SessionRegistry, SessionState};

#[derive(Debug, thiserror::Error)]
pub enum InternalDispatchError {
    #[error("no verified internal tool target")]
    NoTarget,
    #[error("ambiguous verified internal tool target")]
    AmbiguousTarget,
    #[error("internal tool dispatch failed")]
    Dispatch,
}

/// Target нельзя получить через agent resolver: требуется одновременно
/// internal visibility и проверенная deployment identity базы.
#[derive(Debug, Clone)]
pub struct VerifiedInternalTarget {
    record: SessionRecord,
    identity: VerifiedDatabaseIdentity,
}

pub fn resolve_internal_target(
    registry: &SessionRegistry,
    identities: &IdentityResolver,
    database_id: &str,
    tool_name: &str,
) -> Result<VerifiedInternalTarget, InternalDispatchError> {
    let mut matches = registry
        .snapshot()
        .into_iter()
        .filter(|record| record.state == SessionState::Active)
        .filter_map(|record| {
            let identity = identities.verify(&record)?;
            if identity.database_id().to_string() != database_id
                || !record.tools.iter().any(|tool| {
                    tool.name == tool_name && tool.visibility == ToolVisibility::Internal
                })
            {
                return None;
            }
            Some(VerifiedInternalTarget { record, identity })
        });
    let target = matches.next().ok_or(InternalDispatchError::NoTarget)?;
    if matches.next().is_some() {
        return Err(InternalDispatchError::AmbiguousTarget);
    }
    Ok(target)
}

pub async fn dispatch_internal_tool(
    target: VerifiedInternalTarget,
    tool_name: String,
    arguments: Value,
    timeout: Duration,
) -> Result<ToolCallResult, InternalDispatchError> {
    let _verified_database = target.identity;
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

const METADATA_TOOL: &str = "mcp_internal_masking_metadata_feed";
const DICTIONARY_TOOL: &str = "mcp_internal_masking_dictionary_feed";
const MAX_PAGES_PER_SELECTION: u32 = 10_000;
const MAX_FILTER_AST_DEPTH: usize = 16;
const MAX_FILTER_AST_NODES: usize = 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FeedPage {
    success: bool,
    metadata: Vec<FeedMetadataItem>,
    dictionary_values: Vec<FeedDictionaryValue>,
    next_cursor: Option<String>,
    final_chunk: bool,
    #[serde(default, alias = "error")]
    error_code: Option<String>,
}

struct FeedProgress {
    correlation_id: String,
    chunk_index: u32,
    metadata_count: u64,
    dictionary_count: u64,
    dictionary_source_paths: HashSet<String>,
    aggregate: Sha256,
}

/// Фоновый worker использует только hidden tools и internal UDS API.
pub fn spawn_feed_worker(
    gate: Arc<MaskingGate>,
    registry: Arc<SessionRegistry>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(client) = gate.service_client() else {
            return;
        };
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {
                    let Ok(response) = client.feed_jobs(10).await else { continue };
                    if response.schema_version != 1 { continue; }
                    for job in response.jobs {
                        if shutdown.is_cancelled() { return; }
                        if Uuid::parse_str(&job.job_id).is_err() { continue; }
                        if let Err(code) = process_job(&gate, &registry, &job).await {
                            let _ = client.feed_fail(&job.job_id, &FeedFailRequest {
                                schema_version: 1,
                                correlation_id: Uuid::new_v4().to_string(),
                                reason_code: code.to_owned(),
                            }).await;
                        }
                    }
                }
            }
        }
    })
}

async fn process_job(
    gate: &MaskingGate,
    registry: &SessionRegistry,
    job: &FeedJob,
) -> Result<(), &'static str> {
    validate_job(job)?;
    let mut progress = FeedProgress {
        correlation_id: Uuid::new_v4().to_string(),
        chunk_index: 0,
        metadata_count: 0,
        dictionary_count: 0,
        dictionary_source_paths: HashSet::new(),
        aggregate: Sha256::new(),
    };

    process_selection(
        gate,
        registry,
        job,
        METADATA_TOOL,
        None,
        json!({"selector": job.metadata_selector}),
        job.dictionary_selectors.is_empty(),
        &mut progress,
    )
    .await?;

    for (position, selector) in job.dictionary_selectors.iter().enumerate() {
        process_selection(
            gate,
            registry,
            job,
            DICTIONARY_TOOL,
            Some(selector.selection_id.clone()),
            json!({"selector": selector}),
            position + 1 == job.dictionary_selectors.len(),
            &mut progress,
        )
        .await?;
    }

    let client = gate.service_client().ok_or("SERVICE_NOT_READY")?;
    let response = client
        .feed_activate(
            &job.job_id,
            &FeedActivateRequest {
                schema_version: 1,
                correlation_id: progress.correlation_id,
                expected_chunks: progress.chunk_index,
                expected_metadata_count: progress.metadata_count,
                expected_dictionary_count: progress.dictionary_count,
                aggregate_digest: hex::encode(progress.aggregate.finalize()),
            },
        )
        .await
        .map_err(|_| "FEED_ACTIVATION_FAILED")?;
    if response.schema_version != 1
        || response.status != "active"
        || response.cache_version != job.target_version
    {
        return Err("FEED_ACTIVATION_FAILED");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn process_selection(
    gate: &MaskingGate,
    registry: &SessionRegistry,
    job: &FeedJob,
    tool_name: &str,
    selection_id: Option<String>,
    mut arguments: Value,
    is_last_selection: bool,
    progress: &mut FeedProgress,
) -> Result<(), &'static str> {
    let mut page_index = 0u32;
    loop {
        if page_index >= MAX_PAGES_PER_SELECTION {
            return Err("RESULT_LIMIT_EXCEEDED");
        }
        let target = resolve_internal_target(
            registry,
            gate.identity_resolver(),
            &job.database_id,
            tool_name,
        )
        .map_err(|_| "INTERNAL_TOOL_UNAVAILABLE")?;
        registry.bump_last_call(&target.record.session_id, std::time::Instant::now());
        let result = dispatch_internal_tool(
            target,
            tool_name.to_owned(),
            arguments.clone(),
            Duration::from_secs(10),
        )
        .await
        .map_err(|_| "INTERNAL_TOOL_FAILED")?;
        let page = parse_feed_page(result)?;
        if !page.success {
            return Err(match page.error_code.as_deref() {
                Some("DICTIONARY_FEED_BINDING_REQUIRED") => "DICTIONARY_FEED_BINDING_REQUIRED",
                Some("FILTER_AST_UNSUPPORTED") => "FILTER_AST_UNSUPPORTED",
                _ => "INTERNAL_TOOL_FAILED",
            });
        }
        if page.final_chunk != page.next_cursor.is_none() {
            return Err("RESULT_INVALID");
        }
        track_page_progress(progress, &page, job)?;
        let next_cursor = page.next_cursor;
        let is_last_page = next_cursor.is_none();
        let payload = FeedChunkPayload {
            selection_id: selection_id.clone(),
            page_index,
            metadata: page.metadata,
            dictionary_values: page.dictionary_values,
            final_chunk: is_last_selection && is_last_page,
        };
        let canonical = canonical_json(&payload)?;
        if canonical.len() > job.max_chunk_bytes {
            return Err("RESULT_LIMIT_EXCEEDED");
        }
        let digest = Sha256::digest(&canonical);
        progress.aggregate.update(digest);
        let request = FeedChunkRequest {
            schema_version: 1,
            correlation_id: progress.correlation_id.clone(),
            chunk_digest: hex::encode(digest),
            payload,
        };
        let client = gate.service_client().ok_or("SERVICE_NOT_READY")?;
        let response = client
            .feed_chunk(&job.job_id, progress.chunk_index, &request)
            .await
            .map_err(|_| "FEED_CHUNK_REJECTED")?;
        if response.schema_version != 1 || response.accepted_index != progress.chunk_index {
            return Err("FEED_CHUNK_REJECTED");
        }
        progress.chunk_index = progress.chunk_index.saturating_add(1);
        if is_last_page {
            return Ok(());
        }
        arguments["cursor"] = Value::String(next_cursor.expect("non-final page has cursor"));
        page_index = page_index.saturating_add(1);
    }
}

fn track_page_progress(
    progress: &mut FeedProgress,
    page: &FeedPage,
    job: &FeedJob,
) -> Result<(), &'static str> {
    progress.metadata_count = progress
        .metadata_count
        .saturating_add(page.metadata.len() as u64);
    progress.dictionary_count = progress
        .dictionary_count
        .saturating_add(page.dictionary_values.len() as u64);
    progress.dictionary_source_paths.extend(
        page.dictionary_values
            .iter()
            .map(|item| item.source_path.clone()),
    );
    if progress.dictionary_count > job.hard_limits.max_total_values
        || progress.dictionary_source_paths.len() > job.hard_limits.max_source_paths
    {
        return Err("RESULT_LIMIT_EXCEEDED");
    }
    Ok(())
}

fn canonical_json<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, &'static str> {
    let value = serde_json::to_value(value).map_err(|_| "RESULT_INVALID")?;
    serde_json::to_vec(&sort_json(value)).map_err(|_| "RESULT_INVALID")
}

fn sort_json(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries = map.into_iter().collect::<Vec<_>>();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, sort_json(value)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.into_iter().map(sort_json).collect()),
        primitive => primitive,
    }
}

fn parse_feed_page(result: ToolCallResult) -> Result<FeedPage, &'static str> {
    if result.is_error {
        return Err("INTERNAL_TOOL_FAILED");
    }
    if let Some(value) = result.structured_content {
        return serde_json::from_value(value).map_err(|_| "RESULT_INVALID");
    }
    if result.content.len() == 1 {
        match result.content.into_iter().next() {
            Some(crate::session_manager::protocol::ToolContent::Json { json }) => {
                return serde_json::from_value(json).map_err(|_| "RESULT_INVALID");
            }
            Some(crate::session_manager::protocol::ToolContent::Text { text }) => {
                return serde_json::from_str(&text).map_err(|_| "RESULT_INVALID");
            }
            None => {}
        }
    }
    Err("RESULT_INVALID")
}

fn validate_job(job: &FeedJob) -> Result<(), &'static str> {
    Uuid::parse_str(&job.job_id).map_err(|_| "RESULT_INVALID")?;
    Uuid::parse_str(&job.database_id).map_err(|_| "RESULT_INVALID")?;
    if job.max_chunk_bytes == 0
        || job.max_chunk_bytes > 1024 * 1024
        || job.metadata_selector.mode != "all"
        || !(1..=1000).contains(&job.metadata_selector.page_size)
        || job.hard_limits.max_source_paths > 100
        || job.hard_limits.max_total_values > 1_000_000
        || job.dictionary_selectors.len() > job.hard_limits.max_source_paths
    {
        return Err("RESULT_LIMIT_EXCEEDED");
    }
    for selector in &job.dictionary_selectors {
        validate_dictionary_selector(selector)?;
    }
    Ok(())
}

fn validate_dictionary_selector(selector: &DictionarySelector) -> Result<(), &'static str> {
    Uuid::parse_str(&selector.selection_id).map_err(|_| "RESULT_INVALID")?;
    if !is_explicit_catalog_source(&selector.source_path)
        || selector.source_path.len() > 512
        || selector.category.is_empty()
        || selector.category.len() > 32
        || selector
            .filter_ast
            .as_ref()
            .is_some_and(|filter| !filter.is_null() && !valid_filter_ast(filter))
        || !(1..=1000).contains(&selector.page_size)
    {
        return Err("RESULT_INVALID");
    }
    Ok(())
}

fn valid_filter_ast(value: &Value) -> bool {
    let mut nodes = 0usize;
    valid_filter_ast_node(value, 0, &mut nodes)
}

fn valid_filter_ast_node(value: &Value, depth: usize, nodes: &mut usize) -> bool {
    if depth > MAX_FILTER_AST_DEPTH {
        return false;
    }
    *nodes = nodes.saturating_add(1);
    if *nodes > MAX_FILTER_AST_NODES {
        return false;
    }
    let Some(object) = value.as_object() else {
        return false;
    };
    let Some(operator) = object.get("op").and_then(Value::as_str) else {
        return false;
    };
    match operator {
        "and" | "or" => {
            object.len() == 2
                && object
                    .get("args")
                    .and_then(Value::as_array)
                    .is_some_and(|args| {
                        !args.is_empty()
                            && args.len() <= 32
                            && args
                                .iter()
                                .all(|item| valid_filter_ast_node(item, depth + 1, nodes))
                    })
        }
        "not" => {
            object.len() == 2
                && object
                    .get("arg")
                    .is_some_and(|item| valid_filter_ast_node(item, depth + 1, nodes))
        }
        "eq" | "ne" => {
            object.len() == 3
                && object.get("field").is_some_and(valid_filter_field)
                && object.get("value").is_some_and(valid_filter_scalar)
        }
        "in" => {
            object.len() == 3
                && object.get("field").is_some_and(valid_filter_field)
                && object
                    .get("values")
                    .and_then(Value::as_array)
                    .is_some_and(|values| {
                        values.len() <= 100 && values.iter().all(valid_filter_scalar)
                    })
        }
        _ => false,
    }
}

fn valid_filter_field(value: &Value) -> bool {
    let Some(field) = value.as_str() else {
        return false;
    };
    if field.is_empty() || field.len() > 256 {
        return false;
    }
    let mut characters = field.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_alphabetic())
        && characters.all(|character| character == '_' || character.is_alphanumeric())
}

fn valid_filter_scalar(value: &Value) -> bool {
    value.is_null()
        || value.is_boolean()
        || value.is_number()
        || value.as_str().is_some_and(|text| text.len() <= 1024)
}

fn is_explicit_catalog_source(source_path: &str) -> bool {
    let parts = source_path.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && matches!(parts[0], "Catalog" | "Справочник")
        && parts[1..]
            .iter()
            .all(|part| !part.is_empty() && *part != "*")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::MaskingIdentityBinding;
    use crate::session_manager::protocol::{SessionRegisterParams, ToolDescriptor, ToolVisibility};
    use std::time::Instant;

    fn feed_job(dictionary_selectors: Vec<DictionarySelector>) -> FeedJob {
        serde_json::from_value(json!({
            "job_id": Uuid::new_v4().to_string(),
            "database_id": Uuid::new_v4().to_string(),
            "target_version": 1,
            "max_chunk_bytes": 1024 * 1024,
            "metadata_selector": {"mode": "all", "page_size": 1000},
            "dictionary_selectors": dictionary_selectors,
            "hard_limits": {"max_source_paths": 100, "max_total_values": 1_000_000}
        }))
        .unwrap()
    }

    fn feed_progress() -> FeedProgress {
        FeedProgress {
            correlation_id: Uuid::new_v4().to_string(),
            chunk_index: 0,
            metadata_count: 0,
            dictionary_count: 0,
            dictionary_source_paths: HashSet::new(),
            aggregate: Sha256::new(),
        }
    }

    fn dictionary_selector(filter_ast: Option<Value>) -> DictionarySelector {
        DictionarySelector {
            selection_id: Uuid::new_v4().to_string(),
            source_path: "Catalog.Account.Description".to_owned(),
            category: "person".to_owned(),
            filter_ast,
            page_size: 1000,
        }
    }

    #[test]
    fn canonical_json_sorts_nested_object_keys() {
        let value = json!({"b":1,"a":{"z":2,"y":[{"b":2,"a":1}]}});
        let canonical = canonical_json(&value).unwrap();
        assert_eq!(
            std::str::from_utf8(&canonical).unwrap(),
            r#"{"a":{"y":[{"a":1,"b":2}],"z":2},"b":1}"#
        );
        assert_eq!(
            hex::encode(Sha256::digest(&canonical)),
            "1bc9d75d3768190183ddcf8ff16e2bf4d037ae6130767c63c802b1513b70e175"
        );
    }

    #[test]
    fn metadata_chunk_digest_includes_null_selection_id() {
        let payload = FeedChunkPayload {
            selection_id: None,
            page_index: 0,
            metadata: Vec::new(),
            dictionary_values: Vec::new(),
            final_chunk: true,
        };
        let canonical = canonical_json(&payload).unwrap();
        assert_eq!(
            std::str::from_utf8(&canonical).unwrap(),
            r#"{"dictionary_values":[],"final_chunk":true,"metadata":[],"page_index":0,"selection_id":null}"#
        );
        assert_eq!(
            hex::encode(Sha256::digest(&canonical)),
            "38d1cbab19f796bdeb1448eb2bf61a858163ac1ef3c4dcd0cd3177ad17d2101b"
        );
    }

    #[test]
    fn feed_page_accepts_safe_1c_error_field() {
        let page = parse_feed_page(ToolCallResult {
            content: Vec::new(),
            is_error: false,
            structured_content: Some(json!({
                "success": false,
                "error": "FILTER_AST_UNSUPPORTED",
                "metadata": [],
                "dictionary_values": [],
                "next_cursor": null,
                "final_chunk": true
            })),
        })
        .unwrap();
        assert_eq!(page.error_code.as_deref(), Some("FILTER_AST_UNSUPPORTED"));
    }

    #[test]
    fn full_metadata_inventory_is_not_limited_by_dictionary_source_cap() {
        let job = feed_job(Vec::new());
        let page = FeedPage {
            success: true,
            metadata: (0..101)
                .map(|index| FeedMetadataItem {
                    source_path: format!("Catalog.Account.Field{index}"),
                    field_name: format!("Field{index}"),
                    field_type: "String".to_owned(),
                    password_mode: false,
                })
                .collect(),
            dictionary_values: Vec::new(),
            next_cursor: None,
            final_chunk: true,
            error_code: None,
        };
        let mut progress = feed_progress();

        assert_eq!(track_page_progress(&mut progress, &page, &job), Ok(()));
        assert_eq!(progress.metadata_count, 101);
        assert!(progress.dictionary_source_paths.is_empty());
    }

    #[test]
    fn dictionary_selector_count_cannot_exceed_source_cap() {
        let selectors = (0..101)
            .map(|index| DictionarySelector {
                selection_id: Uuid::new_v4().to_string(),
                source_path: format!("Catalog.Account.Field{index}"),
                category: "person".to_owned(),
                filter_ast: None,
                page_size: 1000,
            })
            .collect();

        assert_eq!(
            validate_job(&feed_job(selectors)),
            Err("RESULT_LIMIT_EXCEEDED")
        );
    }

    #[test]
    fn valid_dictionary_filter_ast_is_forwarded_unchanged() {
        let ast = json!({
            "op": "and",
            "args": [
                {"op":"eq", "field":"ПометкаУдаления", "value":false},
                {"op":"or", "args":[
                    {"op":"in", "field":"_ДемоКод2", "values":[]},
                    {"op":"not", "arg":{
                        "op":"ne", "field":"Description", "value":"я".repeat(512)
                    }}
                ]}
            ]
        });
        let selector = dictionary_selector(Some(ast.clone()));

        assert_eq!(validate_dictionary_selector(&selector), Ok(()));
        assert_eq!(json!({"selector": selector})["selector"]["filter_ast"], ast);

        let mut depth_boundary = json!({"op":"eq", "field":"Name", "value":true});
        for _ in 0..16 {
            depth_boundary = json!({"op":"not", "arg":depth_boundary});
        }
        assert_eq!(
            validate_dictionary_selector(&dictionary_selector(Some(depth_boundary))),
            Ok(())
        );
        assert_eq!(
            validate_dictionary_selector(&dictionary_selector(Some(Value::Null))),
            Ok(())
        );
    }

    #[test]
    fn dictionary_filter_ast_rejects_unsupported_or_overbound_shapes() {
        let mut too_deep = json!({"op":"eq", "field":"Name", "value":true});
        for _ in 0..17 {
            too_deep = json!({"op":"not", "arg":too_deep});
        }
        let branch = json!({
            "op":"and",
            "args":(0..32)
                .map(|_| json!({"op":"eq", "field":"Name", "value":true}))
                .collect::<Vec<_>>()
        });
        let too_many_nodes = json!({"op":"and", "args":vec![branch; 32]});
        let invalid = vec![
            json!({"op":"contains", "field":"Name", "value":"x"}),
            json!({"op":"eq", "field":"Name", "value":true, "extra":false}),
            json!({"op":"and", "args":[]}),
            json!({"op":"or", "args":vec![json!({"op":"eq", "field":"Name", "value":1}); 33]}),
            json!({"op":"in", "field":"Name", "values":vec![Value::Null; 101]}),
            json!({"op":"eq", "field":"Account.Name", "value":"x"}),
            json!({"op":"eq", "field":"я".repeat(129), "value":"x"}),
            json!({"op":"eq", "field":"Name", "value":"я".repeat(513)}),
            json!({"op":"eq", "field":"Name", "value":{}}),
            too_deep,
            too_many_nodes,
        ];

        for ast in invalid {
            assert_eq!(
                validate_dictionary_selector(&dictionary_selector(Some(ast))),
                Err("RESULT_INVALID")
            );
        }
    }

    #[test]
    fn dictionary_result_paths_remain_bounded() {
        let job = feed_job(Vec::new());
        let page = FeedPage {
            success: true,
            metadata: Vec::new(),
            dictionary_values: (0..101)
                .map(|index| FeedDictionaryValue {
                    source_path: format!("Catalog.Account.Field{index}"),
                    category: "person".to_owned(),
                    value: format!("Value{index}"),
                })
                .collect(),
            next_cursor: None,
            final_chunk: true,
            error_code: None,
        };

        assert_eq!(
            track_page_progress(&mut feed_progress(), &page, &job),
            Err("RESULT_LIMIT_EXCEEDED")
        );
    }

    #[test]
    fn internal_resolver_requires_hidden_visibility_and_verified_database() {
        let instance = Uuid::new_v4();
        let database = Uuid::new_v4();
        let registry = SessionRegistry::new();
        registry
            .register_trusted(
                SessionRegisterParams {
                    client_uid: "feed-session".to_owned(),
                    kind: "server".to_owned(),
                    version: "1".to_owned(),
                    infobase_name: "db".to_owned(),
                    ib_session_number: 1,
                    database_instance_id: Some(instance.to_string()),
                    tools: vec![ToolDescriptor {
                        name: METADATA_TOOL.to_owned(),
                        description: None,
                        input_schema: json!({"type":"object"}),
                        visibility: ToolVisibility::Internal,
                    }],
                    config_id: Some("server".to_owned()),
                    host_id: Some("host".to_owned()),
                    pid: None,
                    resources: None,
                    prompts: None,
                    extras: None,
                },
                Instant::now(),
                None,
                Some(crate::session_manager::registry::TrustedRouteContext {
                    database_instance_id: instance,
                    database_id: database,
                }),
            )
            .unwrap();
        let identities = IdentityResolver::new(&[MaskingIdentityBinding {
            database_instance_id: instance.to_string(),
            database_id: database.to_string(),
            expected_kind: "server".to_owned(),
            expected_config_id: "server".to_owned(),
            expected_host_id: Some("host".to_owned()),
            allowed_internal_tools: vec![METADATA_TOOL.to_owned(), DICTIONARY_TOOL.to_owned()],
        }]);
        assert!(resolve_internal_target(
            &registry,
            &identities,
            &database.to_string(),
            METADATA_TOOL
        )
        .is_ok());
        assert!(resolve_internal_target(
            &registry,
            &identities,
            &Uuid::new_v4().to_string(),
            METADATA_TOOL
        )
        .is_err());
    }
}
