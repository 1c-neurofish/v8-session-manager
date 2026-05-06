//! Менеджер клиентских 1С‑сессий.
//!
//! Принимает входящие WebSocket‑соединения от 1С‑клиентов с расширением
//! `web-transport-addin`, ведёт реестр активных сессий и проксирует MCP
//! `tool.call` от AI‑агента в нужную сессию через `ProxyRouter`.
//!
//! После урезания (#5/post-extraction) менеджер выполняет ТОЛЬКО роль агрегатора
//! и точки доступа к проксированным tool'ам клиентов:
//! WS‑транспорт, JSON‑RPC протокол, in‑memory registry, soft reconnect по
//! `client_uid`, MCP management tool `session.list`. Без spawn/kill/swap/call.

pub mod connection;
pub mod dispatcher;
pub mod env_carrier;
pub mod lifecycle;
pub mod management;
pub mod metrics;
pub mod notify;
pub mod protocol;
pub mod registry;
pub mod router;
pub mod transport;
