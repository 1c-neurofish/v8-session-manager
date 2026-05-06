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
