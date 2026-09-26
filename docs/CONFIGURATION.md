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

masking:
  enabled: false
  socket_path: /run/1c-masking/internal.sock
  internal_listen_path: /run/1c-masking/manager-internal.sock
  preflight_timeout_ms: 3000
  finalize_timeout_ms: 15000
  internal_call_timeout_ms: 10000
  service_expected_uid: 994
  broker_public_key_path: /etc/v8-session-manager/broker-ed25519.pub.pem
  broker_issuer: trusted-mcp-broker
  broker_audience: v8-session-manager
  broker_max_assertion_ttl_secs: 300
  conversation_assertion_header: x-v8-conversation-assertion
  internal_tools:
    - mcp_internal_masking_metadata_feed
    - mcp_internal_masking_dictionary_feed
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

## Секция `masking`

По умолчанию интеграция выключена. При `enabled: true` обязательны UDS сервиса,
UDS internal endpoint менеджера, UID peer-а сервиса и Ed25519 public key
trusted broker-а. Manager принимает
`conversation_id` только из JWT/JWS header, проверяя `iss`,
`aud=v8-session-manager`, `iat`, `exp` и максимальный TTL. Tool arguments
и MCP `_meta` источниками chat identity не являются.

При `enabled: true` **каждый** публичный proxy `tools/call` проходит через
сервис маскирования (preflight + finalize) — единая точка контроля.
Классификация инструментов (`data-mask`, `no-mask`,
`deny-pending-review`) настраивается в административном API сервиса, а не
конфигом менеджера: неизвестный сервису инструмент отклоняется до вызова 1С
как `deny-pending-review` и автоматически попадает в очередь классификации.
Сессия идентифицируется координатами ИБ из `session.register`
(`cluster_server`/`infobase_name`); при RAS-резолюции — парой GUID-ов
кластера и ИБ. Сессия без `cluster_server`+`infobase_name` отклоняется
`DATABASE_IDENTITY_UNVERIFIED` до проверки conversation assertion.

`managed_tools` — **устаревшее** поле: оставлено для совместимости со
старыми конфигами, на маршрут не влияет и не валидируется; непустое
значение порождает предупреждение в логе при старте gate.

`internal_tools` — ровно два internal tool адаптера
(`mcp_internal_masking_metadata_feed`, `mcp_internal_masking_dictionary_feed`),
через которые сервис маскирования загружает словарь. Метка `Internal`
расставляется менеджером по имени из конфига; adapter-provided visibility
не доверяется (adapter-declared `Internal` сохраняется как fail-safe,
`Public` для configured internal-имени игнорируется). Internal tools
исключены из `tools/list`, agent resolver, persistent cache и `session_list`.

### Internal endpoint менеджера

`internal_listen_path` — UDS в том же shared volume, что и socket сервиса.
На нём менеджер принимает `POST /internal/v1/tools/call`
(`{cluster_server, infobase_name, [cluster_guid, infobase_guid], name,
arguments}`) только от peer-а с UID `service_expected_uid`; unknown tool,
чужой UID и неоднозначный target отклоняются фиксированными error-кодами.
`internal_call_timeout_ms` — таймаут dispatch в сессию 1С.

WS registration не аутентифицируется отдельным криптографическим слоем —
это operational channel внутри принятой доверенной сетевой границы
(VPN/LAN/tunnel). Поэтому WS endpoint нельзя публиковать за пределами
этой границы без отдельного transport-auth слоя.

### Опознание баз через RAS (env)

При `masking.enabled: true` менеджер при `session.register` резолвит
координаты ИБ (`cluster_server`/`infobase_name`) в GUID кластера и GUID
инфобазы через RAS-агент кластера 1С (`rac`). Настройки — только в env
процесса менеджера (в YAML не выносятся, т.к. содержат креды):

| Env | По умолчанию | Назначение |
|-----|--------------|------------|
| `V8SM_RAC_PATH` | `/opt/1cv8/current/rac` | Путь к исполняемому `rac` платформы 1С. |
| `V8SM_RAS_ADDRESS` | — | Фиксированный адрес RAS (`host:port`); без него `rac` вызывается по `cluster_server` (Srvr строки соединения) как есть — порт rmngr ≠ порт RAS, поэтому для серверных баз переменную нужно задать. |
| `V8SM_RAS_CLUSTER_USER` | — | Пользователь кластера 1С (опционально). |
| `V8SM_RAS_CLUSTER_PASSWORD` | — | Пароль кластера 1С (опционально; не логируется). |

Пустые/отсутствующие `V8SM_RAS_CLUSTER_USER`/`V8SM_RAS_CLUSTER_PASSWORD` —
не ошибка: RAS без пароля — штатная схема, флаги `--cluster-user`/
`--cluster-pwd` в этом случае в `rac` не передаются.

`V8SM_RAS_CLUSTER_PASSWORD` передаётся `rac` аргументом командной строки
(`--cluster-pwd`), поэтому на время вызова виден в argv процесса `rac`
(`ps`, `/proc/<pid>/cmdline`). Если это неприемлемо — используйте RAS
без пароля или изолируйте хост менеджера.

Ключ идентичности базы — непрозрачная строка `instance_id`
(`ras:<cluster_guid>:<infobase_guid>` при успешной RAS-резолюции,
`gen:<srvr>/<ref>` verbatim при недоступном RAS): совпадение только
точное, нормализации и координатных фолбэков нет — склейка баз
невозможна по построению. Если `rac` отсутствует, RAS недоступен по
сети или отказал — регистрация не блокируется: сессия получает
`gen:`-ключ, а сервис маскирования при первом обращении создаёт под него
ненастроенную запись. Автоматического перехода `gen:` → `ras:` нет:
настройки переносятся между записями экспортом/импортом; переименование
или перенос базы при `gen:`-ключе даёт новый ключ и новую запись.

Timeout defaults: `preflight_timeout_ms=3000`, `finalize_timeout_ms=15000`,
`internal_call_timeout_ms=10000`.
Finalize может один раз повторить только transport failure с тем же `call_id`.
Masking response никогда не добавляет agent-facing receipt/history ID.

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
