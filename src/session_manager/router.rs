//! ClientProxy router (ADR‑0025).
//!
//! Динамически вычисляет публикуемые tool'ы из `SessionRegistry`, разруливает
//! имя `<kind>__<tool>` обратно к конкретной сессии и `name` для `tool.call`.
//!
//! Pure функции: всё состояние (round‑robin счётчики) живёт в [`ProxyRouter`],
//! который держит `McpToolServer`. Без зависимости от rmcp internals: возвращает
//! `Vec<rmcp::model::Tool>` и принимает `arguments: JsonObject`.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rmcp::model::Tool;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::session_manager::registry::{SessionRegistry, SessionState};

/// Разделитель в имени публикации `<kind>__<tool>`.
pub const PROXY_NAME_SEPARATOR: &str = "__";

/// Per‑kind флаг публикации именованных tools (§8.1 спеки, ADR‑0025).
fn is_publishing_kind(kind: &str) -> bool {
    !matches!(kind, "vanessa_test_client")
}

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

/// Описание одного слота публикации `<kind>__<tool>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxySlot {
    pub kind: String,
    pub tool_name: String,
    pub published_name: String,
    pub schema_hash: String,
    pub description: Option<String>,
    pub input_schema: Value,
    /// Список `session_id`, удовлетворяющих `(kind, tool_name, schema_hash)`,
    /// в стабильной (отсортированной по uid) последовательности — для
    /// предсказуемости round‑robin'а.
    pub session_ids: Vec<String>,
}

/// Результат группировки реестра по `(kind, tool_name)`.
#[derive(Debug, Default)]
pub struct ProxyView {
    /// Опубликованные slots — ровно один schema_hash в группе `(kind, tool_name)`.
    pub published: Vec<ProxySlot>,
    /// Скрытые из `tools/list` группы (конфликт schema). Доступны через `session.call`.
    pub hidden: Vec<HiddenGroup>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HiddenGroup {
    pub kind: String,
    pub tool_name: String,
    pub schema_hashes: Vec<String>,
    pub session_ids: Vec<String>,
}

/// Группирует Active‑записи реестра в slots ClientProxy.
pub fn build_proxy_view(registry: &SessionRegistry) -> ProxyView {
    let mut groups: HashMap<(String, String), HashMap<String, GroupAccumulator>> = HashMap::new();

    for rec in registry.snapshot() {
        if rec.state != SessionState::Active {
            continue;
        }
        if !is_publishing_kind(&rec.kind) {
            continue;
        }
        for tool in &rec.tools {
            let h = schema_hash(&tool.input_schema);
            let group = groups
                .entry((rec.kind.clone(), tool.name.clone()))
                .or_default();
            let acc = group.entry(h.clone()).or_insert_with(|| GroupAccumulator {
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
                session_ids: Vec::new(),
            });
            acc.session_ids.push(rec.session_id.clone());
        }
    }

    let mut view = ProxyView::default();
    for ((kind, tool_name), buckets) in groups {
        if buckets.len() == 1 {
            let (h, mut acc) = buckets.into_iter().next().unwrap();
            acc.session_ids.sort();
            view.published.push(ProxySlot {
                published_name: format!("{kind}{PROXY_NAME_SEPARATOR}{tool_name}"),
                kind,
                tool_name,
                schema_hash: h,
                description: acc.description,
                input_schema: acc.input_schema,
                session_ids: acc.session_ids,
            });
        } else {
            let mut hashes: Vec<String> = buckets.keys().cloned().collect();
            hashes.sort();
            let mut session_ids: Vec<String> =
                buckets.into_values().flat_map(|a| a.session_ids).collect();
            session_ids.sort();
            view.hidden.push(HiddenGroup {
                kind,
                tool_name,
                schema_hashes: hashes,
                session_ids,
            });
        }
    }
    view.published
        .sort_by(|a, b| a.published_name.cmp(&b.published_name));
    view.hidden.sort_by(|a, b| {
        (a.kind.as_str(), a.tool_name.as_str()).cmp(&(b.kind.as_str(), b.tool_name.as_str()))
    });
    view
}

#[derive(Debug)]
struct GroupAccumulator {
    description: Option<String>,
    input_schema: Value,
    session_ids: Vec<String>,
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
                slot.description
                    .clone()
                    .unwrap_or_else(|| format!("ClientProxy tool from kind={}", slot.kind)),
                Arc::new(object),
            )
        })
        .collect()
}

/// Round‑robin счётчик per‑group (kind, tool_name).
#[derive(Debug, Default)]
pub struct ProxyRouter {
    counters: Mutex<HashMap<(String, String), AtomicUsize>>,
}

impl ProxyRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Выбирает session_id из равнозначных. Round‑robin по группе `(kind, tool_name)`.
    /// `session_ids` должен быть непустым, отсортированным.
    pub fn pick(&self, kind: &str, tool_name: &str, session_ids: &[String]) -> Option<String> {
        if session_ids.is_empty() {
            return None;
        }
        let mut guard = self.counters.lock().expect("counters poisoned");
        let counter = guard
            .entry((kind.to_owned(), tool_name.to_owned()))
            .or_insert_with(|| AtomicUsize::new(0));
        let n = counter.fetch_add(1, Ordering::Relaxed);
        Some(session_ids[n % session_ids.len()].clone())
    }
}

/// Распознаёт имя `<kind>__<tool>` → `(kind, tool_name)`. Возвращает `None`
/// для имён без separator'а или с пустыми частями.
pub fn parse_proxy_name(name: &str) -> Option<(String, String)> {
    let mut split = name.splitn(2, PROXY_NAME_SEPARATOR);
    let kind = split.next()?.to_owned();
    let tool = split.next()?.to_owned();
    if kind.is_empty() || tool.is_empty() {
        return None;
    }
    Some((kind, tool))
}

/// Резолвит `(name, args)` от MCP в выбор сессии + данные для `tool.call`.
///
/// * Если имя — `<kind>__<tool>`, ищет slot в view (round‑robin по published).
/// * Если slot не найден, но в view есть hidden‑группа с тем же `(kind, tool_name)` —
///   вернёт `Err(ResolveError::SchemaConflict)` (агент пусть использует `session.call`).
/// * Если имя не proxy — `Err(ResolveError::NotProxyTool)` — caller делегирует
///   в server‑router.
pub fn resolve_published(
    name: &str,
    view: &ProxyView,
    router: &ProxyRouter,
) -> Result<ResolvedCall, ResolveError> {
    let (kind, tool_name) = parse_proxy_name(name).ok_or(ResolveError::NotProxyTool)?;
    if let Some(slot) = view
        .published
        .iter()
        .find(|s| s.kind == kind && s.tool_name == tool_name)
    {
        let session = router
            .pick(&slot.kind, &slot.tool_name, &slot.session_ids)
            .ok_or(ResolveError::NoActiveSessions)?;
        return Ok(ResolvedCall {
            session_id: session,
            tool_name: slot.tool_name.clone(),
        });
    }
    if view
        .hidden
        .iter()
        .any(|h| h.kind == kind && h.tool_name == tool_name)
    {
        return Err(ResolveError::SchemaConflict { kind, tool_name });
    }
    Err(ResolveError::Unknown { kind, tool_name })
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
    #[error("no active sessions for the requested tool")]
    NoActiveSessions,
    #[error("schema conflict: {kind:?}/{tool_name:?} hidden from tools/list — use session.call")]
    SchemaConflict { kind: String, tool_name: String },
    #[error("unknown proxy tool {kind:?}/{tool_name:?}")]
    Unknown { kind: String, tool_name: String },
}

/// Хелпер: канонический JSON‑object для `arguments` в `tool.call`.
pub fn arguments_to_value(arguments: Option<rmcp::model::JsonObject>) -> Value {
    match arguments {
        Some(map) => Value::Object(map),
        None => json!({}),
    }
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
            tools: tools
                .into_iter()
                .map(|(n, schema)| ToolDescriptor {
                    name: n.to_owned(),
                    description: None,
                    input_schema: schema,
                })
                .collect(),
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
    fn published_slot_for_single_session() {
        let reg = SessionRegistry::new();
        reg.register(
            params("uid-1", "client", vec![("echo", json!({"type": "object"}))]),
            Instant::now(),
            None,
        )
        .unwrap();
        let view = build_proxy_view(&reg);
        assert_eq!(view.published.len(), 1);
        assert_eq!(view.hidden.len(), 0);
        assert_eq!(view.published[0].published_name, "client__echo");
        assert_eq!(view.published[0].session_ids, vec!["uid-1".to_owned()]);
    }

    #[test]
    fn round_robin_distributes_across_equal_schema_sessions() {
        let reg = SessionRegistry::new();
        reg.register(
            params("uid-1", "client", vec![("echo", json!({"type": "object"}))]),
            Instant::now(),
            None,
        )
        .unwrap();
        reg.register(
            params("uid-2", "client", vec![("echo", json!({"type": "object"}))]),
            Instant::now(),
            None,
        )
        .unwrap();
        let view = build_proxy_view(&reg);
        assert_eq!(view.published.len(), 1);
        assert_eq!(view.published[0].session_ids, vec!["uid-1", "uid-2"]);

        let router = ProxyRouter::new();
        let r1 = resolve_published("client__echo", &view, &router).unwrap();
        let r2 = resolve_published("client__echo", &view, &router).unwrap();
        let r3 = resolve_published("client__echo", &view, &router).unwrap();
        assert_eq!(r1.session_id, "uid-1");
        assert_eq!(r2.session_id, "uid-2");
        assert_eq!(r3.session_id, "uid-1"); // round-robin wraps
    }

    #[test]
    fn conflict_schema_hides_tool_from_list() {
        let reg = SessionRegistry::new();
        reg.register(
            params("uid-1", "client", vec![("echo", json!({"type": "object"}))]),
            Instant::now(),
            None,
        )
        .unwrap();
        reg.register(
            params("uid-2", "client", vec![("echo", json!({"type": "string"}))]),
            Instant::now(),
            None,
        )
        .unwrap();
        let view = build_proxy_view(&reg);
        assert_eq!(view.published.len(), 0);
        assert_eq!(view.hidden.len(), 1);
        let h = &view.hidden[0];
        assert_eq!(h.kind, "client");
        assert_eq!(h.tool_name, "echo");
        assert_eq!(h.schema_hashes.len(), 2);
    }

    #[test]
    fn resolve_returns_schema_conflict_for_hidden() {
        let reg = SessionRegistry::new();
        reg.register(
            params("uid-1", "client", vec![("echo", json!({"a": 1}))]),
            Instant::now(),
            None,
        )
        .unwrap();
        reg.register(
            params("uid-2", "client", vec![("echo", json!({"a": 2}))]),
            Instant::now(),
            None,
        )
        .unwrap();
        let view = build_proxy_view(&reg);
        let router = ProxyRouter::new();
        let err = resolve_published("client__echo", &view, &router).unwrap_err();
        match err {
            ResolveError::SchemaConflict { kind, tool_name } => {
                assert_eq!(kind, "client");
                assert_eq!(tool_name, "echo");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn vanessa_test_client_kind_is_not_published_named() {
        let reg = SessionRegistry::new();
        reg.register(
            params(
                "uid-vt",
                "vanessa_test_client",
                vec![("step.run", json!({"type": "object"}))],
            ),
            Instant::now(),
            None,
        )
        .unwrap();
        let view = build_proxy_view(&reg);
        assert!(view.published.is_empty());
        assert!(view.hidden.is_empty());
    }

    #[test]
    fn parse_proxy_name_handles_double_underscore() {
        assert_eq!(
            parse_proxy_name("client__echo"),
            Some(("client".to_owned(), "echo".to_owned()))
        );
        assert_eq!(parse_proxy_name("no_separator"), None);
        assert_eq!(parse_proxy_name("__"), None);
    }
}
