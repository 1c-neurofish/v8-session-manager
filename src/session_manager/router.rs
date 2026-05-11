//! ClientProxy router (ADR‑0025, naming v2 от 2026-05-09).
//!
//! Динамически вычисляет публикуемые tool'ы из `SessionRegistry`. После
//! правки от 2026-05-09 публикуется голое имя `tool_name` без префикса
//! `<kind>__`. Дедупликация выполняется только по `tool_name` + `schema_hash`;
//! `kind` больше не входит в ключ группы.
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
    /// Список `session_id`, удовлетворяющих `(tool_name, schema_hash)`,
    /// в стабильной (отсортированной по uid) последовательности — для
    /// предсказуемости round‑robin'а.
    pub session_ids: Vec<String>,
}

/// Результат группировки реестра по `tool_name`.
#[derive(Debug, Default)]
pub struct ProxyView {
    /// Опубликованные slots — ровно один schema_hash в группе `tool_name`.
    pub published: Vec<ProxySlot>,
    /// Скрытые из `tools/list` группы (конфликт schema). Доступны через `session.call`.
    pub hidden: Vec<HiddenGroup>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HiddenGroup {
    pub tool_name: String,
    pub schema_hashes: Vec<String>,
    pub session_ids: Vec<String>,
    pub kinds: Vec<String>,
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
        if buckets.len() == 1 {
            let (h, mut acc) = buckets.into_iter().next().unwrap();
            acc.session_ids.sort();
            acc.kinds.sort();
            view.published.push(ProxySlot {
                published_name: tool_name.clone(),
                kinds: acc.kinds,
                tool_name,
                schema_hash: h,
                description: acc.description,
                input_schema: acc.input_schema,
                session_ids: acc.session_ids,
            });
        } else {
            let mut hashes: Vec<String> = buckets.keys().cloned().collect();
            hashes.sort();
            let mut session_ids: Vec<String> = Vec::new();
            let mut kinds: Vec<String> = Vec::new();
            for acc in buckets.into_values() {
                session_ids.extend(acc.session_ids);
                for k in acc.kinds {
                    if !kinds.contains(&k) {
                        kinds.push(k);
                    }
                }
            }
            session_ids.sort();
            kinds.sort();
            view.hidden.push(HiddenGroup {
                tool_name,
                schema_hashes: hashes,
                session_ids,
                kinds,
            });
        }
    }
    view.published
        .sort_by(|a, b| a.published_name.cmp(&b.published_name));
    view.hidden.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
    view
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

/// Round‑robin счётчик per‑group `tool_name`.
#[derive(Debug, Default)]
pub struct ProxyRouter {
    counters: Mutex<HashMap<String, AtomicUsize>>,
}

impl ProxyRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Выбирает session_id из равнозначных. Round‑robin по группе `tool_name`.
    /// `session_ids` должен быть непустым, отсортированным.
    pub fn pick(&self, tool_name: &str, session_ids: &[String]) -> Option<String> {
        if session_ids.is_empty() {
            return None;
        }
        let mut guard = self.counters.lock().expect("counters poisoned");
        let counter = guard
            .entry(tool_name.to_owned())
            .or_insert_with(|| AtomicUsize::new(0));
        let n = counter.fetch_add(1, Ordering::Relaxed);
        Some(session_ids[n % session_ids.len()].clone())
    }
}

/// Резолвит `(name, args)` от MCP в выбор сессии + данные для `tool.call`.
///
/// * Если в view есть published slot с `published_name == name`, возвращается
///   round‑robin сессия из его `session_ids`.
/// * Если такого slot нет, но есть hidden‑группа с тем же `tool_name`, —
///   `Err(ResolveError::SchemaConflict)`.
/// * Иначе — `Err(ResolveError::NotProxyTool)` (caller делегирует server‑router).
pub fn resolve_published(
    name: &str,
    view: &ProxyView,
    router: &ProxyRouter,
) -> Result<ResolvedCall, ResolveError> {
    if let Some(slot) = view.published.iter().find(|s| s.published_name == name) {
        let session = router
            .pick(&slot.tool_name, &slot.session_ids)
            .ok_or(ResolveError::NoActiveSessions)?;
        return Ok(ResolvedCall {
            session_id: session,
            tool_name: slot.tool_name.clone(),
        });
    }
    if let Some(hidden) = view.hidden.iter().find(|h| h.tool_name == name) {
        return Err(ResolveError::SchemaConflict {
            tool_name: hidden.tool_name.clone(),
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
    #[error("no active sessions for the requested tool")]
    NoActiveSessions,
    #[error("schema conflict: {tool_name:?} hidden from tools/list — use session.call")]
    SchemaConflict { tool_name: String },
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
            infobase_name: "test_db".to_owned(),
            ib_session_number: 1,
            tools: tools
                .into_iter()
                .map(|(n, schema)| ToolDescriptor {
                    name: n.to_owned(),
                    description: None,
                    input_schema: schema,
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
        assert_eq!(view.hidden.len(), 0);
        // Голое имя — без префикса `<kind>__`.
        assert_eq!(view.published[0].published_name, "echo");
        assert_eq!(view.published[0].tool_name, "echo");
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
        let r1 = resolve_published("echo", &view, &router).unwrap();
        let r2 = resolve_published("echo", &view, &router).unwrap();
        let r3 = resolve_published("echo", &view, &router).unwrap();
        assert_eq!(r1.session_id, "uid-1");
        assert_eq!(r2.session_id, "uid-2");
        assert_eq!(r3.session_id, "uid-1"); // round-robin wraps
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
        assert_eq!(view.hidden.len(), 0);
        let slot = &view.published[0];
        assert_eq!(slot.published_name, "echo");
        assert_eq!(slot.session_ids, vec!["uid-1", "uid-2"]);
        assert_eq!(slot.kinds, vec!["client", "vanessa_test_client"]);
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
        let err = resolve_published("echo", &view, &router).unwrap_err();
        match err {
            ResolveError::SchemaConflict { tool_name } => {
                assert_eq!(tool_name, "echo");
            }
            other => panic!("unexpected: {other:?}"),
        }
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
        let router = ProxyRouter::new();
        let err = resolve_published("nope", &view, &router).unwrap_err();
        assert!(matches!(err, ResolveError::NotProxyTool));
    }
}
