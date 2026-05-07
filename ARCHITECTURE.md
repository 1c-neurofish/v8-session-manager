# Архитектура

> Обзорный документ верхнего уровня по `v8-session-manager`. Полная схема стека (включая клиентскую часть `client_mcp` / addin) — в [`docs/architecture/STACK_OVERVIEW.md`](docs/architecture/STACK_OVERVIEW.md). Архитектурные решения — в [`docs/decisions/`](docs/decisions/README.md), инварианты — в [`docs/architecture/invariants.md`](docs/architecture/invariants.md).

## Что это

`v8-session-manager` — однобинарный агрегатор клиентских MCP-сессий. Принимает входящие WebSocket-подключения от 1С-клиентов (через расширение `client_mcp` + транспортный addin) и одновременно отдаёт MCP HTTP (streamable) единым эндпоинтом для AI-агентов и IDE.

Менеджер сам 1С не запускает: lifecycle `1cv8c` лежит на стороне внешнего оркестратора (deploy-скрипт, IDE-плагин, BSL-расширение прикладной конфигурации). См. ADR-0034.

## Компоненты

```
                              v8-session-manager
                       ┌────────────────────────────────┐
   1С-клиенты (WS)  ──►│  WS transport :4000/sessions   │
                       │            │                   │
                       │            ▼                   │
                       │   Arc<SessionRegistry>         │
                       │            ▲                   │
                       │            │                   │
   AI-агенты (HTTP) ──►│  MCP HTTP transport :4001/mcp  │
                       └────────────────────────────────┘
                                    │
                            per-session SessionDispatcher
                            (FIFO очередь tool-вызовов)
```

| Модуль | Источник | Ответственность |
|--------|----------|------------------|
| `src/session_manager/transport.rs` | WS-транспорт | axum + tokio-tungstenite на `:4000/sessions`. Принимает WS, ведёт reader/writer таски, шлёт RFC 6455 Ping для liveness, дёргает реестр на регистрацию/disconnect/reconnect. |
| `src/session_manager/registry.rs` | `SessionRegistry` | In-memory реестр сессий: `client_uid` → `SessionRecord` (prefix, generation, tools, статус, `last_inbound_at`, `last_call_at`). Под `Arc`, шарится между транспортами. |
| `src/session_manager/dispatcher.rs` | per-session FIFO | `SessionDispatcher` для каждой сессии: последовательная очередь tool-вызовов, inflight-счётчик, idle-bump (ADR-0021, ADR-0024). |
| `src/session_manager/protocol.rs` | JSON-RPC 2.0 | Envelope + методы control-plane: `session.register`, `session.bye`, `tools/publish`, `tools/list_changed` (ADR-0023). |
| `src/session_manager/lifecycle.rs` | sweepers | Idle-sweeper по `idle_timeout_secs`, grace-sweeper по `reconnection_grace_secs` для удаления отключённых записей. |
| `src/session_manager/router.rs` | резолв префиксов | Маппинг `<prefix>__<tool>` ↔ `(session_id, tool_name)` при `tools/list` и `tools/call` (ADR-0025). |
| `src/session_manager/notify.rs` | bidi notifications | `tools/list_changed` → подписчикам MCP HTTP (ADR-0026). |
| `src/mcp/server.rs` | MCP-фасад | Реализация `rmcp` сервера: `initialize`, `tools/list` (агрегированный), `tools/call` (проксирование в нужную сессию), встроенный tool `session_list` (ADR-0034). |
| `src/mcp/` (HTTP transport) | streamable HTTP | `axum` + `rmcp::transport::StreamableHttpService` на `:4001/mcp`, поддержка stateful-сессий, лимит `max_sessions`, idle TTL. |
| `src/cli/args.rs` | CLI | Плоский `clap`-парсер: `--config`, `--workdir`, `--log-level`, `--bind`, `--path`, `--mcp-http`. Никаких подкоманд. |
| `src/config/model.rs` | конфиг | `AppConfig` = `workPath` + `mcp: { session_manager, http, execution, metrics }`. |

## Поток выполнения

### Регистрация клиента

1. 1С-клиент стартует с `mcpMode=ws;manager_url=…` в `/C` → addin открывает WS на `:4000/sessions`.
2. devkit BSL шлёт `session.register` (`client_uid`, `prefix`, `host_id`, `pid`, capabilities).
3. `SessionRegistry::register_or_reattach` — либо новая запись, либо soft-reconnect по `client_uid` + `generation` (ADR-0022).
4. Создаётся `SessionDispatcher`. `notify` рассылает MCP HTTP-клиентам `tools/list_changed`.

### Tool-вызов

1. AI-агент → `POST /mcp` `tools/call` с именем `<prefix>__<tool>`.
2. `router` резолвит prefix → session.
3. Вызов кладётся в `SessionDispatcher` (FIFO + inflight).
4. WS-фрейм → addin → devkit BSL → handler в прикладном расширении.
5. Результат поднимается обратно по той же цепочке.

### Liveness

WS protocol-level Ping/Pong (RFC 6455) с интервалом `ws_ping_interval_ms` и таймаутом `ws_ping_timeout_ms`. Реализация — в `transport.rs`, обрабатывается tokio worker'ом в addin без участия BSL (ADR-0024 §liveness, STACK_OVERVIEW).

### Закрытие

- Корректное: `session.bye` от клиента → `remove_if_generation`.
- Аварийное: idle-sweeper удаляет по `last_call_at + idle_timeout_secs` (ADR-0028 — фильтр по origin снят в ADR-0034).
- Disconnect: `mark_disconnected_if_generation`, далее grace-sweeper.

## Ключевые инварианты

См. [`docs/architecture/invariants.md`](docs/architecture/invariants.md). Кратко:

- per-session FIFO (ADR-0021);
- soft-reconnect по `client_uid` + monotonic `generation` (ADR-0022);
- единственный публичный manager-tool — `session_list` (ADR-0034);
- `tools/list` менеджера — агрегированный, имена клиентских тулов с префиксом `<prefix>__<tool>` (ADR-0025);
- дедупликация tools по `(kind, name, schema_hash)` (ADR-0019);
- bidirectional control-plane поверх одного WS (ADR-0023).

## Связанные документы

- [`docs/architecture/STACK_OVERVIEW.md`](docs/architecture/STACK_OVERVIEW.md) — полная схема L0..L4.
- [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md) — справочник по `v8project.yaml`.
- [`docs/INSTALL.md`](docs/INSTALL.md) — установка как сервис на Linux/Windows/macOS.
- [`docs/decisions/`](docs/decisions/README.md) — ADR-0018..0026, 0028, 0029, 0034, 0035.
