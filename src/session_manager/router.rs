//! ClientProxy router (ADR‑0025, naming v2 от 2026-05-09).
//!
//! Динамически вычисляет публикуемые tool'ы из `SessionRegistry`. Публикуется
//! голое имя `tool_name`; конкретная сессия выбирается зарезервированным
//! аргументом [`SESSION_ID_ARGUMENT`]. Если активный кандидат один, аргумент
//! остаётся опциональным для обратной совместимости.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use rmcp::model::Tool;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::session_manager::registry::{SessionRegistry, SessionState};

/// Зарезервированный менеджером аргумент маршрутизации. Он публикуется в
/// `inputSchema`, но удаляется до отправки аргументов выбранной WS-сессии.
pub const SESSION_ID_ARGUMENT: &str = "session_id";

/// SHA‑256 от канонически отсортированного `input_schema`. Возвращает hex‑строку.
pub fn schema_hash(schema: &Value) -> String {
    let canonical = canonicalize(schema);
    let bytes = serde_json::to_vec(&canonical).expect("canonical json");
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    hex::encode(digest.as_slice())
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<&String, Value> =
                map.iter().map(|(k, v)| (k, canonicalize(v))).collect();
            let mut out = serde_json::Map::with_capacity(sorted.len());
            for (k, v) in sorted {
                out.insert(k.clone(), v);
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

/// Описание одного слота публикации. `published_name` ≡ `tool_name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxySlot {
    /// Информационный список kinds, чьи сессии участвуют в группе. Используется
    /// в description fallback и для диагностики; в имя публикации не входит.
    pub kinds: Vec<String>,
    pub tool_name: String,
    pub published_name: String,
    pub schema_hash: String,
    pub description: Option<String>,
    pub input_schema: Value,
    /// Список активных `session_id`, публикующих `tool_name`, в стабильной
    /// (отсортированной по uid) последовательности.
    pub session_ids: Vec<String>,
}

/// Результат группировки реестра по `tool_name`.
#[derive(Debug, Default)]
pub struct ProxyView {
    /// По одному опубликованному slot на каждое уникальное имя tool.
    pub published: Vec<ProxySlot>,
}

/// Группирует Active‑записи реестра в slots ClientProxy.
pub fn build_proxy_view(registry: &SessionRegistry) -> ProxyView {
    // Группировка теперь только по `tool_name`. Внутри одной группы накапливаем
    // buckets по `schema_hash`; собираем kinds для информационного описания.
    let mut groups: HashMap<String, HashMap<String, GroupAccumulator>> = HashMap::new();

    for rec in registry.snapshot() {
        if rec.state != SessionState::Active {
            continue;
        }
        for tool in &rec.tools {
            if !tool.visibility.is_public() {
                continue;
            }
            let h = schema_hash(&tool.input_schema);
            let group = groups.entry(tool.name.clone()).or_default();
            let acc = group.entry(h.clone()).or_insert_with(|| GroupAccumulator {
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
                session_ids: Vec::new(),
                kinds: Vec::new(),
            });
            acc.session_ids.push(rec.session_id.clone());
            if !acc.kinds.contains(&rec.kind) {
                acc.kinds.push(rec.kind.clone());
            }
        }
    }

    let mut view = ProxyView::default();
    for (tool_name, buckets) in groups {
        let mut variants: Vec<(String, GroupAccumulator)> = buckets.into_iter().collect();
        variants.sort_by(|a, b| a.0.cmp(&b.0));
        let mut session_ids = Vec::new();
        let mut kinds = Vec::new();
        for (_, acc) in &variants {
            session_ids.extend(acc.session_ids.iter().cloned());
            for kind in &acc.kinds {
                if !kinds.contains(kind) {
                    kinds.push(kind.clone());
                }
            }
        }
        session_ids.sort();
        kinds.sort();
        let require_session_id = session_ids.len() > 1;
        let input_schema = if variants.len() == 1 {
            let (_, acc) = &variants[0];
            input_schema_with_session_selector(&acc.input_schema, &session_ids, require_session_id)
        } else {
            let one_of = variants
                .iter()
                .map(|(_, acc)| {
                    let mut variant_ids = acc.session_ids.clone();
                    variant_ids.sort();
                    input_schema_with_session_selector(&acc.input_schema, &variant_ids, true)
                })
                .collect::<Vec<_>>();
            serde_json::json!({"type": "object", "oneOf": one_of})
        };
        let (schema_hash, description) = if variants.len() == 1 {
            (variants[0].0.clone(), variants[0].1.description.clone())
        } else {
            (
                schema_hash(&input_schema),
                Some(format!(
                    "ClientProxy tool with session-specific schemas; select one of: {}",
                    session_ids.join(", ")
                )),
            )
        };
        view.published.push(ProxySlot {
            published_name: tool_name.clone(),
            kinds,
            tool_name,
            schema_hash,
            description,
            input_schema,
            session_ids,
        });
    }
    view.published
        .sort_by(|a, b| a.published_name.cmp(&b.published_name));
    view
}

/// Добавляет в клиентскую схему фактически принимаемый менеджером селектор.
/// Для нескольких кандидатов селектор обязателен; для единственного — только
/// документирует допустимый идентификатор, сохраняя старые вызовы без него.
pub fn input_schema_with_session_selector(
    schema: &Value,
    session_ids: &[String],
    required: bool,
) -> Value {
    let mut object = schema.as_object().cloned().unwrap_or_default();
    object
        .entry("type".to_owned())
        .or_insert_with(|| Value::String("object".to_owned()));

    let properties = object
        .entry("properties".to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !properties.is_object() {
        *properties = Value::Object(serde_json::Map::new());
    }
    let mut selector = serde_json::Map::new();
    selector.insert("type".to_owned(), Value::String("string".to_owned()));
    selector.insert(
        "description".to_owned(),
        Value::String(
            "Session-manager routing selector; removed before forwarding to the target tool"
                .to_owned(),
        ),
    );
    if !session_ids.is_empty() {
        selector.insert(
            "enum".to_owned(),
            Value::Array(session_ids.iter().cloned().map(Value::String).collect()),
        );
    }
    properties
        .as_object_mut()
        .expect("properties normalized to object")
        .insert(SESSION_ID_ARGUMENT.to_owned(), Value::Object(selector));

    if required {
        let required_values = object
            .entry("required".to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        if !required_values.is_array() {
            *required_values = Value::Array(Vec::new());
        }
        let required_array = required_values
            .as_array_mut()
            .expect("required normalized to array");
        if !required_array
            .iter()
            .any(|value| value.as_str() == Some(SESSION_ID_ARGUMENT))
        {
            required_array.push(Value::String(SESSION_ID_ARGUMENT.to_owned()));
        }
    }
    Value::Object(object)
}

#[derive(Debug)]
struct GroupAccumulator {
    description: Option<String>,
    input_schema: Value,
    session_ids: Vec<String>,
    kinds: Vec<String>,
}

/// Собирает `Vec<rmcp::Tool>` для `tools/list` из view'а.
pub fn proxy_tools(view: &ProxyView) -> Vec<Tool> {
    view.published
        .iter()
        .map(|slot| {
            // input_schema должен быть JSON object; иначе — empty.
            let object = match slot.input_schema.as_object() {
                Some(map) => map.clone(),
                None => serde_json::Map::new(),
            };
            Tool::new(
                slot.published_name.clone(),
                slot.description.clone().unwrap_or_else(|| {
                    format!("ClientProxy tool from kinds={}", slot.kinds.join(","))
                }),
                Arc::new(object),
            )
        })
        .collect()
}

/// Резолвит `(name, args)` от MCP в выбор сессии + данные для `tool.call`.
///
/// * Явный `session_id` обязан быть кандидатом именно для этого tool.
/// * Без `session_id` единственный кандидат выбирается автоматически.
/// * Без `session_id` несколько кандидатов дают ошибку неоднозначности.
/// * Иначе — `Err(ResolveError::NotProxyTool)` (caller делегирует server‑router).
pub fn resolve_published(
    name: &str,
    view: &ProxyView,
    requested_session_id: Option<&str>,
) -> Result<ResolvedCall, ResolveError> {
    if let Some(slot) = view.published.iter().find(|s| s.published_name == name) {
        let session = match requested_session_id {
            Some(session_id) if slot.session_ids.iter().any(|id| id == session_id) => {
                session_id.to_owned()
            }
            Some(session_id) => {
                return Err(ResolveError::SessionToolMismatch {
                    tool_name: slot.tool_name.clone(),
                    session_id: session_id.to_owned(),
                });
            }
            None if slot.session_ids.len() == 1 => slot.session_ids[0].clone(),
            None => {
                return Err(ResolveError::SessionRequired {
                    tool_name: slot.tool_name.clone(),
                    session_ids: slot.session_ids.clone(),
                });
            }
        };
        return Ok(ResolvedCall {
            session_id: session,
            tool_name: slot.tool_name.clone(),
        });
    }
    Err(ResolveError::NotProxyTool)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCall {
    pub session_id: String,
    pub tool_name: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("not a client-proxy tool name")]
    NotProxyTool,
    #[error("session_id is required for tool {tool_name:?}; candidates: {session_ids:?}")]
    SessionRequired {
        tool_name: String,
        session_ids: Vec<String>,
    },
    #[error("session {session_id:?} does not publish tool {tool_name:?}")]
    SessionToolMismatch {
        tool_name: String,
        session_id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_manager::protocol::{SessionRegisterParams, ToolDescriptor};
    use serde_json::json;
    use std::time::Instant;

    fn params(uid: &str, kind: &str, tools: Vec<(&str, Value)>) -> SessionRegisterParams {
        SessionRegisterParams {
            client_uid: uid.to_owned(),
            kind: kind.to_owned(),
            version: "1.0".to_owned(),
            infobase_name: "test_db".to_owned(),
            ib_session_number: 1,
            tools: tools
                .into_iter()
                .map(|(n, schema)| ToolDescriptor {
                    name: n.to_owned(),
                    description: None,
                    input_schema: schema,
                    visibility: Default::default(),
                })
                .collect(),
            config_id: None,
            host_id: None,
            pid: None,
            resources: None,
            prompts: None,
            extras: None,
        }
    }

    #[test]
    fn schema_hash_is_canonical() {
        let a = json!({"type": "object", "properties": {"a": 1, "b": 2}});
        let b = json!({"properties": {"b": 2, "a": 1}, "type": "object"});
        assert_eq!(schema_hash(&a), schema_hash(&b));
        let c = json!({"type": "object", "properties": {"a": 2, "b": 1}});
        assert_ne!(schema_hash(&a), schema_hash(&c));
    }

    #[test]
    fn published_tool_uses_bare_name() {
        let reg = SessionRegistry::new();
        reg.register(
            params("uid-1", "client", vec![("echo", json!({"type": "object"}))]),
            Instant::now(),
            None,
        )
        .unwrap();
        let view = build_proxy_view(&reg);
        assert_eq!(view.published.len(), 1);
        // Голое имя — без префикса `<kind>__`.
        assert_eq!(view.published[0].published_name, "echo");
        assert_eq!(view.published[0].tool_name, "echo");
        assert_eq!(view.published[0].session_ids, vec!["uid-1".to_owned()]);
        assert_eq!(
            view.published[0].input_schema["properties"][SESSION_ID_ARGUMENT]["enum"],
            json!(["uid-1"])
        );
        assert!(view.published[0].input_schema.get("required").is_none());
    }

    #[test]
    fn multiple_equal_schema_sessions_require_explicit_selection() {
        let reg = SessionRegistry::new();
        reg.register(
            params("prod", "client", vec![("echo", json!({"type": "object"}))]),
            Instant::now(),
            None,
        )
        .unwrap();
        reg.register(
            params("dev", "client", vec![("echo", json!({"type": "object"}))]),
            Instant::now(),
            None,
        )
        .unwrap();
        let view = build_proxy_view(&reg);
        assert_eq!(view.published.len(), 1);
        assert_eq!(view.published[0].session_ids, vec!["dev", "prod"]);
        assert_eq!(
            view.published[0].input_schema["required"],
            json!([SESSION_ID_ARGUMENT])
        );

        assert!(matches!(
            resolve_published("echo", &view, None),
            Err(ResolveError::SessionRequired { .. })
        ));
        assert_eq!(
            resolve_published("echo", &view, Some("dev"))
                .unwrap()
                .session_id,
            "dev"
        );
        assert_eq!(
            resolve_published("echo", &view, Some("prod"))
                .unwrap()
                .session_id,
            "prod"
        );
    }

    #[test]
    fn proxy_dedup_groups_by_tool_name_only() {
        // Две сессии разного `kind`, но одинаковый `tool_name` и одинаковая схема —
        // дедуплицируются в один published slot.
        let reg = SessionRegistry::new();
        reg.register(
            params("uid-1", "client", vec![("echo", json!({"type": "object"}))]),
            Instant::now(),
            None,
        )
        .unwrap();
        reg.register(
            params(
                "uid-2",
                "vanessa_test_client",
                vec![("echo", json!({"type": "object"}))],
            ),
            Instant::now(),
            None,
        )
        .unwrap();
        let view = build_proxy_view(&reg);
        assert_eq!(view.published.len(), 1, "single published slot expected");
        let slot = &view.published[0];
        assert_eq!(slot.published_name, "echo");
        assert_eq!(slot.session_ids, vec!["uid-1", "uid-2"]);
        assert_eq!(slot.kinds, vec!["client", "vanessa_test_client"]);
    }

    #[test]
    fn conflicting_schemas_are_published_as_session_specific_one_of() {
        let reg = SessionRegistry::new();
        reg.register(
            params(
                "prod",
                "client",
                vec![(
                    "echo",
                    json!({"type": "object", "properties": {"prod_arg": {"type": "string"}}}),
                )],
            ),
            Instant::now(),
            None,
        )
        .unwrap();
        reg.register(
            params(
                "dev",
                "client",
                vec![(
                    "echo",
                    json!({"type": "object", "properties": {"dev_arg": {"type": "integer"}}}),
                )],
            ),
            Instant::now(),
            None,
        )
        .unwrap();
        let view = build_proxy_view(&reg);
        assert_eq!(view.published.len(), 1);
        let slot = &view.published[0];
        assert_eq!(slot.tool_name, "echo");
        assert_eq!(slot.input_schema["type"], "object");
        let variants = slot.input_schema["oneOf"].as_array().unwrap();
        assert_eq!(variants.len(), 2);
        assert!(variants.iter().all(|variant| {
            variant["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == SESSION_ID_ARGUMENT)
        }));
        let mut selectors = variants
            .iter()
            .map(|variant| {
                variant["properties"][SESSION_ID_ARGUMENT]["enum"][0]
                    .as_str()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        selectors.sort();
        assert_eq!(selectors, vec!["dev", "prod"]);
        assert_eq!(
            resolve_published("echo", &view, Some("prod"))
                .unwrap()
                .session_id,
            "prod"
        );
        assert_eq!(
            resolve_published("echo", &view, Some("dev"))
                .unwrap()
                .session_id,
            "dev"
        );
    }

    #[test]
    fn explicit_session_must_publish_requested_tool() {
        let reg = SessionRegistry::new();
        reg.register(
            params("prod", "client", vec![("echo", json!({"type": "object"}))]),
            Instant::now(),
            None,
        )
        .unwrap();
        reg.register(
            params("dev", "client", vec![("other", json!({"type": "object"}))]),
            Instant::now(),
            None,
        )
        .unwrap();
        let view = build_proxy_view(&reg);
        let err = resolve_published("echo", &view, Some("dev")).unwrap_err();
        assert_eq!(
            err,
            ResolveError::SessionToolMismatch {
                tool_name: "echo".to_owned(),
                session_id: "dev".to_owned(),
            }
        );
    }

    #[test]
    fn resolve_unknown_name_returns_not_proxy_tool() {
        let reg = SessionRegistry::new();
        reg.register(
            params("uid-1", "client", vec![("echo", json!({"type": "object"}))]),
            Instant::now(),
            None,
        )
        .unwrap();
        let view = build_proxy_view(&reg);
        let err = resolve_published("nope", &view, None).unwrap_err();
        assert!(matches!(err, ResolveError::NotProxyTool));
    }

    #[test]
    fn internal_tools_are_absent_from_agent_view_and_resolver() {
        let registry = SessionRegistry::new();
        let mut registration = params(
            "uid-internal",
            "server",
            vec![("mcp_internal_feed", json!({"type": "object"}))],
        );
        registration.tools[0].visibility =
            crate::session_manager::protocol::ToolVisibility::Internal;
        registry
            .register(registration, Instant::now(), None)
            .unwrap();

        let view = build_proxy_view(&registry);
        assert!(proxy_tools(&view).is_empty());
        assert_eq!(
            resolve_published("mcp_internal_feed", &view, Some("uid-internal")),
            Err(ResolveError::NotProxyTool)
        );
    }
}
