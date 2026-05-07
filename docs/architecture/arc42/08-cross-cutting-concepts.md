## 8. Сквозные концепции

Свод обязательных правил вынесен в [архитектурные инварианты](../invariants.md). Этот раздел даёт обзорный срез сквозных тем.

### 8.1 Конфигурация

- Один YAML-файл (`v8project.yaml` по умолчанию). Структура — `workPath` + `mcp: { session_manager, http, execution, metrics }`.
- Источник правды и дефолты — `src/config/model.rs`.
- CLI-флаги (`--bind`, `--path`, `--mcp-http`, `--workdir`) применяются как override поверх загруженного YAML.
- Полный справочник — [`docs/CONFIGURATION.md`](../../CONFIGURATION.md).

### 8.2 Identity сессий и тулов

- Сессия идентифицируется устойчивым `client_uid` (UUID, генерируется/задаётся клиентом). Soft-reconnect использует `(client_uid, generation)`.
- Каждая сессия публикует `prefix` — namespace, под которым её tools видны на MCP HTTP. Имя проксированного tool: `<prefix>__<tool>`. Особый `kind = vanessa_test_client` отключает префикс.
- Дедупликация client tools — по триплету `(kind, name, schema_hash)`.

### 8.3 Liveness и таймауты

- WS protocol-level Ping/Pong (RFC 6455) с интервалом `ws_ping_interval_ms` и таймаутом `ws_ping_timeout_ms`. Намеренно не application-level: открытый модальный диалог 1С — легитимное состояние.
- `idle_timeout_secs` — окно неактивности сессии после последнего tool-вызова. Idle-sweeper удаляет запись.
- `reconnection_grace_secs` — окно soft-reconnect: после disconnect запись держится этот интервал и удаляется только если клиент не вернулся.
- `mcp.http.idle_ttl_secs` — TTL stateful MCP HTTP-сессии без активности.

### 8.4 Логирование

- Логирование — `tracing`. Уровень задаётся `--log-level`. Вывод идёт в stdout/stderr; путь до постоянного хранилища определяется системой инициализации (journald на Linux, file-redirect через NSSM на Windows, unified log на macOS через launchd).

### 8.5 Метрики

- Prometheus exporter — опциональный, конфигурируется `mcp.metrics.bind_address`. Если поле отсутствует или пустое — exporter не поднимается.

### 8.6 Graceful shutdown

- На SIGTERM `app.rs` ставит CancellationToken; оба транспорта дренируют inflight-вызовы в пределах `mcp.execution.shutdown_grace_period_secs` и завершаются.

### 8.7 MCP HTTP guardrails

- `max_sessions` ограничивает stateful HTTP-сессии. Reservation/confirm/release flow: переполнение = `503` для нового `initialize`, stateful не-`initialize` без `Mcp-Session-Id` = `400`.
- Lazy pruning по `idle_ttl_secs` освобождает слот при следующей попытке reserve.
