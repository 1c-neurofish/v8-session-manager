## 12. Глоссарий

| Термин | Значение |
| --- | --- |
| 1С-клиент | Запущенный экземпляр `1cv8c` (тонкий клиент) с расширением `client_mcp` и транспортным addin'ом, подключающимся к менеджеру по WS. |
| addin | Транспортный Rust-аддин (`web-transport-addin` / `session_y8`), внешняя компонента 1С — `.so` / `.dll`. Держит WS-сокет к менеджеру. |
| AI-агент | MCP-клиент: Claude Code, Codex, Cursor, любой другой потребитель MCP HTTP. |
| `client_mcp` | BSL-расширение из `onec-client-mcp-devkit`. Реализует MCP framing поверх addin'а на стороне 1С. |
| `client_uid` | Стабильный идентификатор клиента (UUID); используется для soft-reconnect. |
| `generation` | Монотонный счётчик инкарнаций сессии. Защищает от гонок свежего коннекта и обработки старого `mark_disconnected`. |
| MCP | Model Context Protocol — протокол, через который AI-агенты вызывают tools. |
| MCP HTTP | Streamable HTTP-транспорт MCP. Эндпоинт менеджера: `:4001/mcp`. |
| `prefix` | Namespace, под которым tools клиента видны на MCP HTTP. Имя проксированного tool: `<prefix>__<tool>`. |
| `kind` | Произвольный строковый идентификатор бизнес-роли клиента. Особый `vanessa_test_client` отключает префикс. |
| `SessionRegistry` | In-memory реестр сессий. `client_uid` → `SessionRecord`. |
| `SessionRecord` | Состояние сессии: `prefix`, `generation`, `tools`, статус (`Reserved`/`Active`/`Disconnected`), `last_inbound_at`, `last_call_at`, `origin`. |
| `SessionDispatcher` | Per-session FIFO очередь tool-вызовов с inflight-счётчиком. |
| Idle-sweeper | Асинхронный таск, удаляющий записи с `last_call_at + idle_timeout_secs < now`. |
| Grace-sweeper | Асинхронный таск, удаляющий записи в статусе `Disconnected` после истечения `reconnection_grace_secs`. |
| Soft-reconnect | Восстановление сессии тем же `client_uid` после краткой потери WS, без сброса prefix и tools. |
| RFC 6455 Ping/Pong | WS protocol-level liveness; обрабатывается tokio worker addin'а без участия BSL. |
| `tools/list_changed` | MCP-нотификация менеджера → агенту. Триггер пере-пулинга `tools/list`. |
| `auth_token` | Опциональный Bearer-токен для MCP HTTP (`mcp.http.auth_token`). |
| `session_list` | Единственный встроенный tool менеджера. Возвращает активные сессии. |
| ADR | Architecture Decision Record. Каталог `docs/decisions/`. |
