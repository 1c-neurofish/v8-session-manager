# Конфигурация

Подробный справочник по `v8project.yaml` и CLI-флагам бинарника `v8-session-manager`.

Источник правды:

- структура и дефолты — `src/config/model.rs`;
- CLI-флаги — `src/cli/args.rs`;
- production-baseline — `etc/v8-session-manager/v8sm.yaml`;
- dev-baseline — `examples/local-dev.yaml` и `v8project.yaml` в корне репо.

## Минимальный конфиг

```yaml
workPath: /var/lib/v8-session-manager
```

Этого достаточно: все секции `mcp.*` имеют дефолты. Менеджер поднимется на `127.0.0.1:4000/sessions` (WS) и `127.0.0.1:4001/mcp` (HTTP).

## Полный пример

```yaml
workPath: /var/lib/v8-session-manager

mcp:
  session_manager:
    bind_address: "127.0.0.1:4000"
    path: "/sessions"
    heartbeat_interval_ms: 15000
    idle_timeout_secs: 1800
    reconnection_grace_secs: 30
    graceful_kill_grace_ms: 5000
    ws_ping_interval_ms: 20000
    ws_ping_timeout_ms: 30000

  http:
    bind_address: "127.0.0.1:4001"
    path: "/mcp"
    stateful_sessions: true
    max_sessions: 64
    idle_ttl_secs: 900
    auth_token: null

  execution:
    shutdown_grace_period_secs: 30

  metrics:
    bind_address: "127.0.0.1:9100"
```

## Корневые ключи

### `workPath` (обязателен)

Рабочий каталог менеджера: лог-файлы, runtime-данные. Должен быть доступен на запись пользователю, под которым работает сервис. Создаётся заранее (для systemd-инсталляции — пакетным скриптом или вручную).

## Секция `mcp.session_manager`

WS-транспорт для входящих подключений 1С-клиентов (`mcpMode=ws`).

| Ключ | Тип | По умолчанию | Назначение |
|------|-----|--------------|------------|
| `bind_address` | `host:port` | `127.0.0.1:4000` | Bind WS-листенера. Для production обычно loopback за reverse-proxy. |
| `path` | string | `/sessions` | URL path WS-эндпоинта. |
| `heartbeat_interval_ms` | u64 | `15000` | Информационное значение, анонсируется в `session.register.result` (используется devkit'ом для собственного keepalive, не транспортом менеджера). |
| `idle_timeout_secs` | u64 | `1800` | Idle-таймаут сессии: запись удаляется, если `last_call_at` старше этого окна (idle-sweeper). |
| `reconnection_grace_secs` | u64 | `30` | Окно soft-reconnect: после disconnect запись помечается как `Disconnected` и удаляется не сразу, а через grace (даёт клиенту шанс переподключиться по тому же `client_uid`). |
| `graceful_kill_grace_ms` | u64 | `5000` | Grace на корректное закрытие WS перед принудительным aborter'ом writer-таска. |
| `ws_ping_interval_ms` | u64 | `20000` | Период WS protocol-level Ping (RFC 6455 opcode 0x9) от менеджера к клиенту. `0` — Ping отключён. Tokio-tungstenite в addin отвечает Pong автоматически без участия BSL. |
| `ws_ping_timeout_ms` | u64 | `30000` | Таймаут отсутствия любых входящих фреймов (Pong / Text). По истечении соединение закрывается, сессия → `Disconnected`. Должен быть `>= ws_ping_interval_ms`. |

> Важно: `ws_ping_*` — это liveness транспортного канала, а не application-level reachability BSL. Открытый модальный диалог 1С — легитимное состояние, при котором Pong продолжает приходить от tokio-worker'а addin'а. Менеджер намеренно не делает application-level ping. См. STACK_OVERVIEW §Liveness.

## Секция `mcp.http`

MCP HTTP transport (streamable) для AI-агентов и IDE.

| Ключ | Тип | По умолчанию | Назначение |
|------|-----|--------------|------------|
| `bind_address` | `host:port` | `127.0.0.1:4001` | Bind HTTP-листенера. |
| `path` | string | `/mcp` | URL path MCP-эндпоинта. |
| `stateful_sessions` | bool | `true` | Включить stateful HTTP-сессии MCP (через `Mcp-Session-Id`). |
| `max_sessions` | usize | `64` | Лимит одновременных stateful HTTP-сессий. При исчерпании новый `initialize` получает `503`. |
| `idle_ttl_secs` | u64 | `900` | TTL stateful HTTP-сессии без активности. |
| `auth_token` | string \| null | `null` | Bearer-токен для MCP HTTP. Если задан — каждый запрос обязан содержать `Authorization: Bearer <token>`. |

## Секция `mcp.execution`

Общие параметры исполнения MCP-вызовов. Сейчас менеджер длительных tool-вызовов сам не выполняет (только `session_list` + проксирование), поэтому остался единственный параметр.

| Ключ | Тип | По умолчанию | Назначение |
|------|-----|--------------|------------|
| `shutdown_grace_period_secs` | u64 | `30` | Время на graceful shutdown tokio-runtime: дренируются inflight-вызовы и WS-сокеты, после чего процесс завершается. |

## Секция `mcp.metrics`

Prometheus exporter.

| Ключ | Тип | По умолчанию | Назначение |
|------|-----|--------------|------------|
| `bind_address` | string \| null | `127.0.0.1:9100` | Bind для Prometheus `/metrics`. Пустая строка или `null` — exporter отключён. |

## CLI-флаги

Подкоманд нет, всё плоско (`src/cli/args.rs`):

| Флаг | Тип | Назначение |
|------|-----|------------|
| `--config <PATH>` | path | Путь к YAML-конфигу. Env: `V8SM_CONFIG`. По умолчанию `./v8project.yaml`. |
| `--workdir <DIR>` | path | Переопределить рабочий каталог (используется для разрешения относительных путей и для логов). |
| `--log-level <LEVEL>` | enum | `error`, `warn`, `info`, `debug`, `trace`. По умолчанию `info`. |
| `--bind <HOST:PORT>` | string | Override `mcp.session_manager.bind_address`. |
| `--path <PATH>` | string | Override `mcp.session_manager.path`. |
| `--mcp-http <HOST:PORT>` | string | Override `mcp.http.bind_address`. |

## Раскладка конфигов в репозитории

| Файл | Назначение | Запуск |
|------|------------|--------|
| `v8project.yaml` | Дефолтный dev-конфиг, подхватывается `cargo run` без флагов. | `cargo run --release` |
| `examples/local-dev.yaml` | Расширенный dev-конфиг: bind на `0.0.0.0`, метрики выключены. | `./target/release/v8-session-manager --config examples/local-dev.yaml` |
| `etc/v8-session-manager/v8sm.yaml` | Production-baseline для systemd. | через `systemd/v8-session-manager.service` (см. [INSTALL.md](INSTALL.md)) |
