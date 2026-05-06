# v8-session-manager

Менеджер клиентских сессий 1С с двумя транспортами (WS + MCP HTTP) в одном
бинарнике. Принимает входящие WS-подключения от 1С-клиентов, агрегирует их
MCP-инструменты и отдаёт единый MCP-эндпоинт для AI-агентов и IDE.

> Проект — форк, отвязанный от `v8-runner`. После extraction CLI-инструменты
> разработки 1С (build/syntax/dump/launch и т.п.) полностью удалены; здесь
> остался только агрегатор клиентских MCP-сессий.

## Зачем это нужно

AI-агенту (Claude Code, Cursor, любой MCP-клиент) удобно говорить с одной
точкой доступа. На практике один и тот же агент работает сразу с несколькими
запущенными 1С-клиентами (тестовый сеанс, сеанс под другой учёткой,
несколько баз). v8-session-manager выступает посредником:

- 1С-клиенты подключаются к нему по WebSocket и публикуют свой набор тулов;
- AI-агент видит на MCP-эндпоинте один агрегированный `tools/list`, где
  тулы разных сессий разнесены по префиксам.

Менеджер сам 1С не запускает. Запуск 1С-клиентов (с параметром `mcpMode=ws`
и адресом менеджера) — забота внешнего оркестратора, например BSL-расширения
`client_mcp` со стороны прикладной конфигурации.

## Архитектура

```
   1С-клиент A                                      AI-агент / IDE
   (mcpMode=ws)                                     (MCP client)
        │                                                  │
        │ WebSocket                                        │ HTTP (streamable)
        │ register + tools/publish                         │ initialize, tools/list,
        ▼                                                  ▼ tools/call
  ┌─────────────────────────────────────────────────────────────┐
  │              v8-session-manager (single binary)             │
  │                                                             │
  │   WS :4000/sessions          MCP HTTP :4001/mcp             │
  │           │                          │                      │
  │           └────────► Arc<SessionRegistry> ◄─────────┘       │
  │                          │                                  │
  │              ┌───────────┴───────────┐                      │
  │              │                       │                      │
  │      SessionDispatcher A     SessionDispatcher B  …         │
  │      (FIFO очередь tool-вызовов, inflight, idle-sweep)      │
  └─────────────────────────────────────────────────────────────┘
            ▲                                ▲
            │ WebSocket                      │ HTTP
            1С-клиент B                AI-агент / IDE
```

Оба транспорта работают на общем `Arc<SessionRegistry>`. Регистрация
сессии, публикация её тулов и вызов проксированного тула — единые операции
над этим реестром.

## Возможности

- **Single binary, два порта.** WS-транспорт на `:4000/sessions` для
  1С-клиентов, MCP HTTP (streamable) на `:4001/mcp` для AI-агентов.
- **Встроенный tool `session_list`.** Возвращает активные сессии с полями
  `id`, `prefix`, `last_call_at`, `inflight`. Это единственный «свой» тул
  менеджера — всем остальным управлением жизненным циклом занимается
  клиент или внешний оркестратор.
- **Проксирование клиентских тулов.** Каждый зарегистрированный клиент
  публикует свой набор инструментов. На MCP HTTP они отображаются с
  префиксом `<session_prefix>__<tool_name>` (см. ADR-0025). Вызов такого
  тула диспатчится в нужную сессию.
- **Per-session FIFO.** На сессию создаётся `SessionDispatcher` —
  последовательная очередь tool-вызовов с inflight-счётчиком. Это
  гарантия, что параллельные запросы агента к одной сессии не перетасуются
  на стороне 1С (ADR-0021).
- **Soft-reconnect.** Если клиент 1С переоткрыл WS (упало соединение,
  перезапуск тонкого клиента), сессия восстанавливается по `client_uid` и
  `generation` в пределах `reconnection_grace_secs`, не теряя prefix и
  опубликованные тулы (ADR-0022).
- **Idle-sweeper.** Сессии без активности дольше `idle_timeout_secs`
  чистятся автоматически.
- **Дедупликация тулов.** Совпадающие `(kind, name, schema_hash)` от
  разных клиентов сводятся в один публичный тул, а конфликты по схеме
  скрываются с предупреждением (ADR-0019).

> Удалены (по сравнению с предыдущей итерацией) `session_call`,
> `session_kill`, `session_spawn`, `session_swap` — управление жизненным
> циклом сессий полностью на стороне клиента / внешнего оркестратора.

## Быстрый старт

Сборка:

```bash
cargo build --release
```

Минимальный `v8project.yaml`:

```yaml
workPath: /tmp/v8-session-manager-dev
mcp:
  session_manager:
    bind_address: "0.0.0.0:4000"
    path: "/sessions"
  http:
    bind_address: "0.0.0.0:4001"
    path: "/mcp"
```

Запуск:

```bash
./target/release/v8-session-manager --config v8project.yaml
```

Проверка через MCP HTTP (две команды JSON-RPC по протоколу MCP):

```bash
# 1. initialize
curl -sS -X POST http://127.0.0.1:4001/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize",
       "params":{"protocolVersion":"2025-03-26","capabilities":{},
                 "clientInfo":{"name":"curl","version":"0"}}}'

# 2. tools/list — пока ни одного клиента не подключено,
#    вернётся только встроенный session_list
curl -sS -X POST http://127.0.0.1:4001/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
```

После того как 1С-клиент с `mcpMode=ws` подключится к `:4000/sessions`,
повторный `tools/list` покажет встроенный `session_list` плюс
проксированные тулы клиентов с префиксом `<prefix>__<tool>`.

## Параметры запуска 1С-клиента (`/C`)

Менеджер сам 1С не запускает; к нему подключаются 1С-клиенты, которые
получили адрес и режим через `/C` при старте платформы. Эти ключи
обрабатываются BSL-расширением `client_mcp` (см.
[`onec-client-mcp-devkit`](../onec-client-mcp-devkit)) — менеджеру
важно лишь, какие значения он увидит в `session.register` и какие
последствия будут на стороне реестра сессий.

Формат: пары `key=value`, разделённые `;`. Пример:

```text
/C"mcpMode=ws;manager_url=ws://127.0.0.1:4000/sessions;kind=client_drive;client_uid=...;mcp_log_level=info"
```

| Ключ | Значения | Назначение | По умолчанию |
|------|----------|------------|--------------|
| `mcpMode` | `ws`, `http`, `auto` | Транспорт. `ws` — подключиться к менеджеру; `http` — поднять локальный HTTP MCP-сервер (legacy); `auto` — сначала WS, при неудаче за `mcp_ws_timeout_ms` падать на HTTP. Без ключа — старая ветка `runMcp=` для HTTP. | (нет) |
| `manager_url` | `ws://host:port/sessions` | URL WS-менеджера для режимов `ws`/`auto`. | `ws://127.0.0.1:4000/sessions` |
| `client_uid` | UUID-строка | Стабильный идентификатор клиента; используется менеджером для soft-reconnect (ADR-0022). При повторном открытии того же клиента **обязан** совпадать. | автогенерация UUID |
| `kind` | произвольная строка-идентификатор | Namespace для публикации тулов на MCP HTTP (`<kind>__<tool>`), а также бизнес-роль клиента. Особое значение `vanessa_test_client` — тулы не публикуются под префиксом (см. `router.rs`). | `1c-client` |
| `corr_id` | произвольная строка | Correlation id для трассировки запуска в логах менеджера (полезно при spawn'е цепочек). | (пусто) |
| `mcp_log_level` | `off`, `error`, `warn`, `info`, `debug`, `trace` | Уровень логирования BSL-логгера и транспортной компоненты. Девкит подаёт это же значение во внешнюю компоненту через `НастроитьЛогирование(level)` — она маппит его на свой `tracing`-фильтр. На уровнях `trace/debug/info` логгер девкита также дублирует записи в панель `Сообщить` (UI-диагностика); на `warn/error/off` — пишет только в файл `%TEMP%\mcp-client.log`. | `off` (логи выключены) |
| `mcp_ws_timeout_ms` | целое число, мс | Таймаут установления WS-сессии в режиме `auto` (после которого включается HTTP fallback). | `1000` |
| `runMcp` | пусто или путь к JSON-конфигу | Legacy: поднять локальный HTTP MCP-сервер. Сосуществует с `mcpMode=http`/`auto`. | (нет) |
| `mcpPort` | целое число, порт | Legacy: переопределение порта локального HTTP MCP-сервера. | `8080` |

> Полный разбор парсинга — `onec-client-mcp-devkit/exts/client-mcp/.../Мсп_ПараметрыЗапускаКлиент/Module.bsl`,
> функции `РазобратьПараметрЗапуска`, `ИзвлечьПараметрыWS`,
> `ПрименитьЛогированиеИзПараметраЗапуска`.

## Конфиг (`v8project.yaml`)

Плоский YAML, секции `mcp.execution` и `mcp.metrics` опциональны.

| Ключ | Назначение | По умолчанию |
|------|------------|--------------|
| `workPath` | Рабочий каталог (логи, рантайм-данные) | — (обязателен) |
| `mcp.session_manager.bind_address` | Bind WS-транспорта для 1С-клиентов | `127.0.0.1:4000` |
| `mcp.session_manager.path` | WS path | `/sessions` |
| `mcp.session_manager.heartbeat_interval_ms` | Анонс интервала heartbeat в `session.register.result` (информационно) | `15000` |
| `mcp.session_manager.idle_timeout_secs` | Idle-таймаут сессии | `1800` |
| `mcp.session_manager.reconnection_grace_secs` | Окно soft-reconnect | `30` |
| `mcp.session_manager.graceful_kill_grace_ms` | Grace на корректное закрытие WS | `5000` |
| `mcp.session_manager.ws_ping_interval_ms` | Период WS protocol-level Ping (RFC 6455) от менеджера к клиенту (`0` — отключено) | `20000` |
| `mcp.session_manager.ws_ping_timeout_ms` | Таймаут отсутствия любых входящих фреймов (Pong / Text). По истечении соединение закрывается, сессия → `Disconnected` | `30000` |
| `mcp.http.bind_address` | Bind MCP HTTP для AI-агентов | `127.0.0.1:4001` |
| `mcp.http.path` | MCP HTTP path | `/mcp` |
| `mcp.http.stateful_sessions` | Включить stateful HTTP-сессии MCP | `true` |
| `mcp.http.max_sessions` | Лимит одновременных HTTP-сессий | `64` |
| `mcp.http.idle_ttl_secs` | Idle TTL HTTP-сессии | `900` |
| `mcp.http.auth_token` | Bearer-токен для MCP HTTP (если задан) | `null` |
| `mcp.execution.shutdown_grace_period_secs` | Grace на graceful shutdown | `30` |
| `mcp.metrics.bind_address` | Prometheus `/metrics` (пусто = выкл.) | `127.0.0.1:9100` |

Источник правды: `src/config/model.rs`.

## CLI

Подкоманд нет, опции плоские (`src/cli/args.rs`):

| Флаг | Назначение |
|------|------------|
| `--config <PATH>` | Путь к YAML-конфигу. Env: `V8SM_CONFIG`. По умолчанию `./v8project.yaml`. |
| `--workdir <DIR>` | Переопределить рабочий каталог. |
| `--log-level <LEVEL>` | `error`, `warn`, `info`, `debug`, `trace`. По умолчанию `info`. |
| `--bind <HOST:PORT>` | Override WS bind поверх конфига. |
| `--path <PATH>` | Override WS path. По умолчанию `/sessions`. |
| `--mcp-http <HOST:PORT>` | Override MCP HTTP bind поверх конфига. |

## Docker

В репозитории есть `Dockerfile` и `docker-compose.yml`. Compose ожидает
внешнюю сеть `infra` и пробрасывает порты `4000`/`4001`:

```bash
# при необходимости один раз создать сеть:
# docker network create infra

docker compose up -d
docker compose logs -f v8-session-mgr
```

## Документация

- [`docs/architecture/STACK_OVERVIEW.md`](docs/architecture/STACK_OVERVIEW.md) —
  полная архитектурная схема стека: транспортное ядро (Rust addin) →
  devkit BSL (`onec-client-mcp-devkit`) → прикладные расширения
  (`test_client`, `VAExtension`, YaxUnit-runner) → менеджер → AI-агент.
  Mermaid-диаграмма + lifecycle сессии + таблица ответственностей слоёв.
- `docs/decisions/` — архитектурные решения (ADR). Релевантные для
  текущего менеджера: ADR-0018 (WS-туннель), ADR-0019 (дедупликация
  тулов), ADR-0020 (`SessionLaunchParamsCarrier`), ADR-0021 (per-session
  FIFO), ADR-0022 (soft-reconnect), ADR-0023 (bidirectional control
  plane), ADR-0024 (per-session dispatcher), ADR-0025 (публикация и
  резолвинг имён тулов), ADR-0026 (`tools/list_changed`), ADR-0028
  (session origin tracking), ADR-0029 (host_id/pid в register payload),
  ADR-0030 (inline launch spec).
- `docs/architecture/arc42/` — описание архитектуры в формате arc42.
- `ARCHITECTURE.md` — обзорный документ верхнего уровня.

> ADR-0001..0017 относятся к историческому v8-runner CLI и помечены
> как `superseded` либо неактуальны для текущего бинарника. ADR-0027
> (system capability vs MCP tools) переведён в `superseded` после
> урезания менеджера до агрегатора.

## Лицензия

GNU Affero General Public License v3.0. См. `LICENSE`.
