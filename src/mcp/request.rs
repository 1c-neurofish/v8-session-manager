//! MCP-facing request DTOs.
//!
//! После урезания менеджера до агрегатора (post-extraction) остался единственный
//! tool — `session.list`. Все DTO для spawn/kill/call/swap удалены вместе с
//! соответствующими механизмами; AI вызывает прокси-tool'ы клиентских сессий
//! напрямую через top-level routing (`<prefix>__<tool>`).

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// MCP request for `session.list` (session-manager management tool).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct McpSessionListRequest {}

/// MCP request for `tools_cache_reset` (ADR-0035).
///
/// Без `config_id` — очистить весь кеш. С `config_id` — удалить только запись
/// с этим `config_id` (среди всех `kind`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(default, rename_all = "snake_case")]
pub struct McpToolsCacheResetRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_id: Option<String>,
}
