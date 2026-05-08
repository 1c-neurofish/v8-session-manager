# Архитектурные инварианты

Правила, которые должны оставаться верными при развитии `v8-session-manager`. Если изменение нарушает инвариант — нужен новый ADR, явно заменяющий или уточняющий текущее решение.

## Цель и публичная поверхность

1. Менеджер — агрегатор клиентских MCP-сессий: WS-транспорт для 1С-клиентов и MCP HTTP для AI-агентов в одном бинарнике.
2. Менеджер не запускает 1С-процессы. Lifecycle `1cv8c` лежит на стороне внешнего оркестратора. См. ADR-0034.
3. Публичная MCP-поверхность менеджера — единственный встроенный tool `session_list` плюс проксированные тулы клиентов с префиксом `<prefix>__<tool>`. Любые `session.spawn/kill/call/swap` менеджерскими тулами запрещены (ADR-0034).

## Per-session FIFO

1. На каждую сессию создаётся `SessionDispatcher` с FIFO-очередью tool-вызовов.
2. Параллельные `tools/call` к одной сессии не должны перетасовываться на стороне 1С: dispatcher выполняет их строго последовательно.
3. Inflight-счётчик и `last_call_at` обновляются исключительно через dispatcher, не напрямую из транспорта.

См. [ADR-0021](../decisions/0021-per-session-fifo-kak-obyazatelnyy-invariant.md), [ADR-0024](../decisions/0024-per-session-dispatcher-i-lifecycle.md).

## Soft-reconnect по `client_uid`

1. Сессия идентифицируется устойчивым `client_uid` (UUID, генерируется/задаётся клиентом).
2. При потере WS запись помечается `Disconnected`, но сохраняется до истечения `reconnection_grace_secs`.
3. Повторное подключение с тем же `client_uid` и большим `generation` re-attach'ит сессию: prefix и опубликованные tools сохраняются.
4. Generation монотонно растёт, защищает от гонок между свежим коннектом и обработкой старого `mark_disconnected`.

См. [ADR-0022](../decisions/0022-soft-reconnect-po-client-uid.md).

## Дедупликация client tools

1. Tools, опубликованные разными клиентами, дедуплицируются по триплету `(kind, name, schema_hash)`.
2. Совпадающие триплеты — один публичный тул в агрегированном `tools/list`.
3. Конфликт по схеме (одинаковые `kind` и `name`, разные `schema_hash`) — тул скрывается с предупреждением; ни одна из версий не публикуется как «победитель».

См. [ADR-0019](../decisions/0019-deduplikatsiya-client-tools-po-kind-name-schema-hash.md).

## Single-tool MCP surface

1. Менеджер публикует один собственный tool — `session_list`, который возвращает активные сессии (`id`, `prefix`, `last_call_at`, `inflight`).
2. Менеджер не публикует tools для управления жизненным циклом сессий (`session.spawn`, `session.kill`, `session.call`, `session.swap`). Любая такая логика лежит на стороне клиента или внешнего оркестратора.
3. Изменение этого контракта требует нового ADR, явно отменяющего ADR-0034.

См. [ADR-0034](../decisions/0034-single-tool-mcp-surface.md).

## Bidirectional control plane

1. Один WebSocket несёт оба направления control-plane: `client → manager` (`session.register`, `session.bye`, `tools/publish`) и `manager → client` (`tools/list_changed` и т.п.).
2. Tool-вызовы (`tools/call` ↔ `tools/result`) идут по тому же сокету как обычные JSON-RPC сообщения, не требуя отдельного back-connect HTTP.
3. На стороне менеджера control-plane и data-plane разделяются на уровне диспатчера, а не транспорта.

См. [ADR-0018](../decisions/0018-ws-tunnel-vmesto-http-back-connect.md), [ADR-0023](../decisions/0023-bidirectional-control-plane-manager-client.md).

## `tools/list_changed` notify policy

1. Менеджер шлёт MCP HTTP-клиентам `tools/list_changed` при любом изменении агрегированного `tools/list`: регистрация/disconnect клиента, публикация новых тулов, изменение конфигурации tools-cache.
2. Клиент-агент в ответ должен пере-пулить `tools/list`. Менеджер не дублирует payload в нотификации.

См. [ADR-0026](../decisions/0026-tools-list-changed-notify-policy.md).

## Origin tracking

1. Каждая запись в `SessionRegistry` помечается `origin` (`SelfRegistered` или исторический `ManagerSpawned`).
2. После принятия ADR-0034 (single-tool surface) idle-sweeper больше не фильтрует записи по origin: чистятся все сессии без активности дольше `idle_timeout_secs`.

См. [ADR-0028](../decisions/0028-session-origin-tracking.md), [ADR-0034](../decisions/0034-single-tool-mcp-surface.md).

## Tools cache (TTL + `config_id`)

1. Менеджер кэширует агрегированный `tools/list` per-session с TTL и инвалидирует кэш по изменению `config_id` клиента.
2. `config_id` приезжает в `session.register` и `tools/publish`; он защищает от гонки «старые тулы из памяти после перерегистрации».

См. [ADR-0035](../decisions/0035-tools-cache-with-ttl-and-config-id.md) (`proposed`).

## HTTP session capacity

1. `mcp.http.max_sessions` ограничивает только трекинг stateful HTTP-сессий, не command execution.
2. `initialize` использует reservation/confirm/release flow; overload — `503`, stateful non-`initialize` POST без `Mcp-Session-Id` — `400`.
3. `idle_ttl_secs` — TTL HTTP-сессии без активности; expiry освобождает слот в `max_sessions` лениво (при следующей попытке reserve).

## Configuration boundary

1. `v8project.yaml` (или путь, переданный в `--config` / `V8SM_CONFIG`) — единственный источник конфигурации.
2. Никаких `base_path` / `connection` / `source-set` / `tools.platform` / `tools.edt-cli` / `tests` / `build` — это были поля исторического CLI `v8-runner`, удалены при extraction (ADR-0033).
3. Дефолты заданы в `src/config/model.rs::Default impl`. CLI-override (`--bind`, `--path`, `--mcp-http`, `--workdir`) применяется поверх загруженного конфига.

См. [ADR-0033](../decisions/0033-extract-v8-session-manager-from-v8-runner.md).

## Identity клиентских tools

1. Имя проксированного tool в публичном `tools/list` всегда имеет вид `<prefix>__<tool>`, где `prefix` зафиксирован при `session.register` (поле `prefix`, см. также `kind`).
2. Особое значение `kind = vanessa_test_client` отключает префикс (исторический совместимый канал; см. `router.rs`).
3. Резолв `<prefix>__<tool>` обратно в `(session_id, tool_name)` — на стороне `router`, не транспорта.

См. [ADR-0025](../decisions/0025-clientproxy-tools-publication-i-name-resolution.md).
