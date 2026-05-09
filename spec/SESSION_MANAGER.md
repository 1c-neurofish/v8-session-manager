# Менеджер клиентских сессий — спецификация

> Статус: черновик. Источник принятых решений и открытых вопросов для трансформации `v8-runner` в менеджер клиентских сессий веб-сокет-клиентов.
>
> Связанные документы: `ARCHITECTURE.md`, `spec/MCP_IMPLEMENTATION_PLAN.md`. Связанный внешний проект: `web-transport-addin` (1С‑расширение, выступает клиентом менеджера).
>
> ⚠️ **Naming v2 (2026-05-09).** §4–§5 описывают историческую схему публикации `<kind>__<tool_name>`, которая пересмотрена. Актуальная схема: `published_name = <tool_name>` (голое имя), дедуп по `(tool_name, schema_hash)`, поля `infobase_name` и `ib_session_number` в `session.register` — обязательные. Полное описание: README §«Naming contract (v2)». Не доверяй упоминаниям префикса `<kind>__` ниже без сверки с README.

## 1. Цели и не-цели

### Цели

- Превратить `v8-runner` в Rust‑сервис, который одновременно:
  1. сохраняет существующий MCP‑контракт (CLI + stdio/HTTP MCP) для серверных операций (build/dump/syntax);
  2. выступает **прокси MCP‑сервером** для AI‑агента, агрегируя tools из подключённых 1С‑клиентов;
  3. умеет порождать и убивать клиентские 1С‑сессии (`1cv8c`) по запросу AI‑агента.
- Поддерживать одновременно несколько клиентских сессий с независимыми наборами tools (например, тестовый клиент + VA‑менеджер + два VA‑test‑client'а).
- Обеспечивать честную маршрутизацию вызовов AI‑агента в нужную клиентскую сессию по её идентификатору.

### Не-цели (на текущей итерации)

- RAC‑интеграция для детектирования зависших сеансов 1С. (Откладываем; idle определяем по проксированным вызовам.)
- Активные use‑case'ы для resources/prompts (1С‑код их пока не публикует), но pass‑through на уровне протокола поддерживается с MVP — см. §10.5.
- Персистентность реестра сессий между перезапусками менеджера. (Менеджер ephemeral; клиенты переподключаются и регистрируются заново.)
- Классический `Auth` (mTLS/JWT). На текущей итерации — bind на loopback или приватной сети, опциональный bearer‑токен в WS handshake (см. §11).
- Агрегация tools из разных сессий в один tool с union‑схемой. Каждая сессия — отдельные именованные tools плюс универсальный `session.call`.

## 2. Глоссарий

| Термин | Значение |
|---|---|
| **Менеджер** | Этот сервис (`v8-runner` после доработок). |
| **Клиент / клиентская сессия** | Запущенный процесс `1cv8c`, в котором загружено 1С‑расширение `web-transport-addin`, инициализировавшее WS‑клиент к менеджеру. |
| **`client_uid`** | Идентификатор клиентской сессии. Заявляется самим клиентом при регистрации. По договорённости — `Новый УникальныйИдентификатор()` или композит `{НомерСеанса}:{СоединениеUID}`. |
| **`session_id`** | Идентификатор сессии в реестре менеджера. Менеджер выдаёт сам при `register`, возвращает клиенту. Всегда совпадает с `client_uid`, если коллизий нет; иначе менеджер отказывает в регистрации. |
| **`kind`** | Тип сессии: `client` \| `yaxunit_runner` \| `vanessa_manager` \| `vanessa_test_client`. Декларируется клиентом, опционально проверяется менеджером по spawn‑шаблону. |
| **Server tool** | MCP‑tool, реализованный в самом менеджере и не зависящий от клиентских сессий (build/dump/syntax). Доступен AI‑агенту всегда. |
| **Client tool** | MCP‑tool, проксируемый из конкретной клиентской сессии. Появляется/исчезает по мере жизни сессии. |
| **Management tool** | MCP‑tool менеджера для управления сессиями (`session.spawn`, `session.kill`, `session.list`, `session.call`). Доступен всегда. |
| **Idle‑таймаут** | Время бездействия сессии (без проксированных вызовов), после которого менеджер её убивает. Default 30 минут. |
| **Heartbeat** | WS‑ping/pong для liveness. Default раз в 15 секунд, настраиваемо. |

## 3. Акторы и общая схема

```
                     ┌────────────────────────────────────┐
                     │             AI‑агент               │
                     │  (Claude Code, Codex CLI и т.п.)   │
                     └────────────────┬───────────────────┘
                                      │ MCP (stdio | http)
                                      ▼
   ┌─────────────────────────────────────────────────────────────┐
   │                     v8-runner / Менеджер                    │
   │                                                             │
   │   ┌──────────────┐   ┌────────────────┐   ┌──────────────┐  │
   │   │  Server tools│   │ Management tools│   │ Client tools │  │
   │   │ (build, dump │   │ (spawn/kill/list│   │ (динамически │  │
   │   │  syntax,…)   │   │  /call, …)      │   │  из сессий)  │  │
   │   └──────────────┘   └────────────────┘   └──────┬───────┘  │
   │                                                  │          │
   │   ┌──────────────────────────────────────────────▼──────┐   │
   │   │  Реестр сессий + Per-session FIFO + Тunnel router   │   │
   │   └──────────────────────────────────────────────┬──────┘   │
   │                                                  │          │
   │                    WS‑сервер (axum) ◀────────────┘          │
   └──────────┬──────────────────────────────────────────────────┘
              │ JSON‑RPC поверх WS (control + data, двунаправл.)
              │
   ┌──────────▼─────────┐  ┌────────────────────┐  ┌──────────────────────┐
   │  1cv8c #1          │  │  1cv8c #2          │  │  1cv8c #N            │
   │  + addin (ws)      │  │  + addin (ws)      │  │  + addin (ws)        │
   │  kind=client       │  │  kind=vanessa_mgr  │  │  kind=vanessa_test…  │
   └────────────────────┘  └────────────────────┘  └──────────────────────┘
```

Отношения 1‑к‑1: каждый процесс `1cv8c` с расширением — отдельный WS‑клиент, отдельная сессия в реестре, отдельный набор client tools.

## 4. Транспорт между addin и менеджером (WS‑tunnel)

### 4.1 Решение

WS используется как **двунаправленный JSON‑RPC канал**: и регистрация (control‑plane), и MCP‑вызовы (data‑plane) идут по одному соединению. Liveness канала — WS protocol-level Ping/Pong (RFC 6455), без application-level heartbeat. У addin **не задействуется HTTP MCP‑сервер**; `mcp`‑класс addin'а используется только для регистрации tools со стороны 1С‑кода и dispatch‑пайплайна — но точкой входа для удалённых MCP‑запросов становится WS‑сообщение, а не HTTP.

Аргументы:

- Один исходящий коннект с 1С‑машины → нет проблем NAT/firewall и контейнеризации.
- Один порт у менеджера.
- Маршрутизация по `session_id` ↔ WS‑соединению однозначна.

Стоимость — доработки в addin, см. §13.

### 4.2 Фрейминг

Каждый WS Text Frame — один JSON‑объект, JSON‑RPC 2.0:

```json
{ "jsonrpc": "2.0", "id": "<string|null>", "method": "<name>", "params": { … } }
{ "jsonrpc": "2.0", "id": "<string>", "result": { … } }
{ "jsonrpc": "2.0", "id": "<string>", "error": { "code": -32000, "message": "…", "data": {…} } }
```

ID — opaque строки, генерируются стороной‑инициатором. Сторона‑получатель не интерпретирует.

### 4.3 Методы (направление: client → manager)

| Method | Когда | Params | Result |
|---|---|---|---|
| `session.register` | Сразу после WS handshake | `{ client_uid, kind, version, host_id, pid, capabilities?, tools, resources?, prompts?, extras? }` | `{ session_id, server_version, heartbeat_interval_ms, idle_timeout_secs }` |
| `session.tools_changed` | Если 1С‑код перерегистрировал tools | `{ tools: [...] }` | `{}` |
| `session.bye` | Грейс‑шатдаун клиента | `{ reason }` | `{}` |

> Application-level heartbeat (`session.heartbeat`) убран. Liveness канала
> держится на WS protocol-level Ping/Pong (RFC 6455) из менеджера; tokio-
> tungstenite на стороне addin отвечает Pong автоматически — BSL не
> задействуется. Параметры — `ws_ping_interval_ms` / `ws_ping_timeout_ms`
> (см. §8.3). Поле `heartbeat_interval_ms` в `session.register.result`
> оставлено для совместимости wire-контракта, но клиенты его не используют.

`tools` — массив с MCP tool‑descriptor: `{ name, description?, input_schema (JSON Schema) }`.

**Поля идентичности окружения (ADR‑0029):**

- `host_id` (обязательно) — детерминированный идентификатор окружения, в котором живёт клиент. Источники в порядке приоритета: ENV `V8_HOST_ID` → `gethostname()` (Linux, читает `/etc/hostname`) → `COMPUTERNAME` (Windows). В контейнере = container name. Менеджер при старте читает свой `host_id` тем же способом и сравнивает при маршрутизации (см. §5.4, §5.5).
- `pid` (обязательно) — `std::process::id()` внутри addin. Это PID самого процесса `1cv8c` с загруженной компонентой. Менеджер использует его для force‑kill через `LocalBackend` (если `host_id == manager.host_id`) или через `RemoteBackend` (sidecar в нужном `host_id`).
- `capabilities` (опционально, массив строк) — какие операции `system_capability` (см. §5.7) умеет данный клиент. Текущая сборка `web-transport-addin` всегда поддерживает `["spawn", "kill"]`; поле сохраняется ради forward compatibility между версиями addin. Отсутствие поля или пустой массив — клиент не участвует в `system_capability` слое (только target sessions).

PID‑верификация менеджером (через `/proc/<pid>/cmdline` на Linux / `QueryFullProcessImageName` на Windows) — feature‑flag `session_manager.verify_register_pid` (default off, этап 7).

### 4.4 Методы (направление: manager → client)

| Method | Когда | Params | Result |
|---|---|---|---|
| `tool.call` | Прокси вызова от AI‑агента | `{ tool_name, arguments, deadline_ms }` | `{ content: [...] }` (MCP tool result) или JSON‑RPC error |
| `session.shutdown` | Менеджер инициирует kill | `{ reason, grace_ms }` | `{}` |
| `ping` | Опциональный двусторонний liveness-probe (помимо WS Ping/Pong, см. §10) | `{}` | `{ ts }` |

### 4.5 Жизненный цикл соединения

1. WS handshake. Опционально — `Authorization: Bearer …` (см. §11).
2. Клиент вызывает `session.register`. До получения `result` менеджер не считает сессию активной.
3. Менеджер записывает сессию в реестр, эмитит MCP `notifications/tools/list_changed` AI‑агенту.
4. Двунаправленный обмен: `tool.call` от менеджера, `session.tools_changed` от клиента. Liveness — WS Ping/Pong (см. §10).
5. Завершение:
   - клиент шлёт `session.bye` → менеджер удаляет сессию, эмитит `tools/list_changed`;
   - менеджер шлёт `session.shutdown` → клиент завершается, посылает `session.bye`, закрывает WS;
   - WS‑разрыв без `session.bye` → менеджер ставит сессию в `Disconnected`, ждёт N секунд reconnection grace; если reconnect не пришёл — удаляет.

### 4.6 Ошибки

- Регистрация с `client_uid`, который уже **активен** (`Active`, не `Disconnected`) → `error.code = -32010 ("session_uid_collision")`. Клиент должен сменить uid.
- Регистрация с `client_uid`, который числится в `Disconnected` (внутри `reconnection_grace_secs`) → **soft reconnect** (см. §6.4): менеджер привязывает новый WS к существующей записи, сохраняет `session_id`, набор tools, очередь, `last_call_at`. В ответе помимо обычных полей возвращается `{ "reconnected": true }`. При включённом auth‑токене это безопасно (uid угадать без токена нельзя).
- `tool.call` к сессии, которая уже мертва → `error.code = -32011 ("session_gone")`.
- Превышение `deadline_ms` на стороне клиента → `error.code = -32012 ("client_timeout")`.

## 5. MCP API менеджера для AI‑агента

### 5.1 Что публикуется

- **Server tools** — все существующие, без изменений: `build_project`, `dump_config`, `check_syntax_designer_*`, `check_syntax_edt`. Внутренний `Scope::Server` помечает их в реестре tool'ов.
- **Existing client‑local tools** — `launch_app`, `run_all_tests`, `run_module_tests`. Помечаются `Scope::ClientLocal`. Сохраняются как «низкоуровневые» tools для случаев, когда менеджер сессий не нужен (синхронный yaxunit happy‑path и т.п.).
- **Management tools** (новые):
  - `session.list` → `[{ session_id, kind, host_id, pid, origin, registered_at, last_call_at, inflight, tools: [name…] }]`
  - `session.spawn` → запускает `1cv8c` по inline launch‑spec в указанном `host_id`, ждёт регистрации, возвращает `{ session_id }`. См. §5.4.
  - `session.kill` → грейс‑шатдаун (`force?: bool`); матрица backend'ов в §5.5.
  - `session.swap` → завершить все активные сессии указанных kinds и/или поднять новый набор по списку spawn‑команд; атомарность лучшего‑возможного (см. §6.4).
  - `session.call` → универсальный прокси: `{ session_id, tool_name, arguments }` → результат MCP tool.
- **Client tools** (динамически по подключённым сессиям). Дедуплицируются по сигнатуре, AI‑агент видит **по одному tool на kind**, выбор конкретной сессии — параметром. Подробно в §5.3.

> **Двухслойный API (ADR‑0027).** Methods вида `addin.spawn` / `addin.kill` относятся к слою `system_capability` и **не публикуются** в MCP‑каталоге. Они используются менеджером изнутри при обработке `session.spawn`/`session.kill` для удалённых host'ов. См. §5.7.

### 5.2 Динамика: `notifications/tools/list_changed`

При изменении набора client tools (сессия зарегистрировалась/ушла/прислала `tools_changed`) менеджер эмитит MCP‑нотификацию. Совместимый клиент (Claude Code) перечитает `tools/list`. Несовместимый — пользуется `session.list` + `session.call`.

### 5.3 Дедупликация client tools и адресация по `session_id`

**Идея:** не показывать AI‑агенту по N однотипных tools на каждую сессию, а схлопывать однотипные регистрации в один публичный tool. Конкретную сессию агент адресует параметром.

#### 5.3.1 Правила дедупликации

Ключ дедупликации = `(kind, tool_name, schema_hash)`, где `schema_hash` — стабильный хэш `input_schema`.

- Если несколько сессий **одного и того же kind** регистрируют tool с **одинаковыми именем и схемой** → публикуется **один tool**: `<kind>__<tool_name>` (например `vanessa_test_client__screenshot`).
- Если у этого публичного tool в реестре менеджера сейчас **одна сессия** — параметр `session_id` опционален (по умолчанию = эта единственная сессия).
- Если **две и более** — параметр `session_id` обязателен; вызов без него возвращает структурную ошибку с подсказкой и списком кандидатов из `session.list`.
- Если две сессии того же kind зарегистрировали один и тот же `tool_name`, но с **разными схемами**, дедупликация невозможна → tools публикуются раздельно с дизамбигуирующим суффиксом: `<kind>__<tool_name>__s<short_id>`. Это edge case; ожидаем, что в норме один kind = один набор сигнатур.

#### 5.3.2 Injection параметра `session_id` в схему

Менеджер берёт оригинальную `input_schema` от клиента и оборачивает:

```jsonc
{
  "type": "object",
  "properties": {
    "session_id": {
      "type": "string",
      "description": "ID сессии. Опционален, если активна ровно одна сессия kind=<kind>; иначе обязателен. Список — через session.list."
    },
    // ...все original.properties целиком...
  },
  "required": [ /* original.required + "session_id" если сессий >1 */ ]
}
```

Когда количество активных сессий kind переходит между «1» и «много», `required` пересчитывается и эмитится `tools/list_changed`.

#### 5.3.3 Маршрутизация

При вызове `<kind>__<tool_name>`:
1. Менеджер вынимает `session_id` из аргументов (или подставляет единственного кандидата).
2. Удаляет `session_id` из аргументов перед отправкой клиенту.
3. Отправляет `tool.call` по WS этой сессии с оригинальным `tool_name` (не публичным `<kind>__<tool_name>`).
4. Возвращает результат AI‑агенту.

#### 5.3.4 Универсальный fallback

`session.call(session_id, tool_name, arguments)` остаётся всегда — для сценариев:
- MCP‑клиент не понимает `tools/list_changed`,
- AI‑агент хочет вызвать tool, который попал в edge‑case (разные схемы, см. выше),
- скриптовая автоматизация без чтения `tools/list`.

#### 5.3.5 Server‑ и Management‑tools — без дедупликации

`build_project`, `dump_config`, `check_syntax_*`, `launch_app`, `run_all_tests`, `run_module_tests`, `session.*` публикуются как есть (kind в их имени не нужен, потому что они не клиентские).

### 5.4 `session.spawn`

```jsonc
// input — inline launch‑spec (основной путь, ADR‑0030)
{
  "host_id": "1c-ai-sandbox",            // обязательный — целевое окружение
  "kind": "yaxunit_runner",
  "mode": "THIN",                        // optional: THIN | DESIGNER | THICK
  "launch": {                            // inline спецификация запуска
    "binary": "/opt/1cv8/x86_64/8.3.27.2074/1cv8c",
    "args": [
      "ENTERPRISE",
      "/S\"onec-infra\\dssl_drive_ai\"",
      "/N\"AgentAI\"", "/P\"AgentAI\""
    ],
    "env": { "DISPLAY": ":99" },
    "startup_command": "RunYaXUnit;configFile=/path/to/yaxunit.json",
    "extra_args": ["/DisableStartupMessages", "/DisableStartupDialogs"]
  },
  "if_exists": "fail" | "reuse" | "replace",   // default "fail"
  "wait_for_register_ms": 60000          // сколько ждать пока 1С зарегистрируется

  // ИЛИ альтернатива — пресет из YAML (опционально, см. §8.3)
  // "template": "yaxunit_runner",
  // "overrides": { "YAXUNIT_CONFIG": "/path/to/yaxunit.json" }
}
```

```jsonc
// output (success)
{
  "session_id": "…",
  "kind": "yaxunit_runner",
  "host_id": "1c-ai-sandbox",
  "pid": 12345,
  "origin": "ManagerSpawned",            // ADR‑0028
  "registered_at": "2026-04-29T15:21:33Z"
}
```

#### 5.4.1 Источник параметров запуска

- Если задан `launch` — он используется напрямую. `template` игнорируется.
- Если задан `template` (без `launch`) — менеджер ищет пресет в `spawn_templates` своего YAML (§8.3) и применяет `overrides`. Это backward‑compat путь.
- Ни `launch`, ни `template` — ошибка `400 BadRequest`.

Подстановка протокольных параметров (`client_uid`, `correlation_id`, `kind`) делается менеджером поверх итогового launch‑spec'а — он добавляет/мерджит `/C"client_uid=<uid>;correlation_id=<id>;kind=<kind>"` (см. §8.4.2).

#### 5.4.2 Маршрутизация исполнения (ADR‑0031)

Менеджер выбирает backend по `host_id` запроса:

```
если host_id == manager.host_id:
    backend = LocalBackend            // tokio::process::Command, см. §6.2
иначе:
    spawner = registry.find_active(host_id, capability="spawn")
    если spawner отсутствует:
        return Error::NoSpawnerInHost { host_id }
    backend = RemoteBackend(spawner)  // через addin.spawn, см. §5.7

reservation = registry.reserve_spawn(expected_uid, kind, origin=ManagerSpawned)
pid = backend.spawn(launch_spec, expected_uid).await?
reservation.set_pid(pid)
record  = registry.wait_register(expected_uid, wait_for_register_ms).await?
return SessionSpawned{ session_id, kind, host_id, pid, origin, registered_at }
```

При timeout: `backend.kill(pid, force=true)` (best‑effort), отмена reservation, ответ `Error::SpawnRegisterTimeout`.

**Bootstrap первого клиента в чужом `host_id`** не входит в обязанности менеджера. Инициатор окружения (devcontainer postStart, CI setup, агент через shell) поднимает первый клиент с `capabilities=["spawn"]` сам. После этого — все последующие spawn в этом host_id через `RemoteBackend`. Для `host_id == manager.host_id` `LocalBackend` доступен всегда — bootstrap не нужен.

#### 5.4.3 `if_exists`

- `fail` — если есть активная сессия с тем же `kind` (для `vanessa_manager`/`yaxunit_runner` — синглтон по умолчанию), вернуть ошибку.
- `reuse` — вернуть существующую сессию.
- `replace` — выполнить `session.kill(force=false)` для всех существующих с этим kind, дождаться завершения, потом spawn.

Список kinds, для которых действует синглтон: задаётся в конфиге (по умолчанию `vanessa_manager`, `yaxunit_runner`). `vanessa_test_client` и `client` — множественные.

### 5.5 `session.kill`

```jsonc
// input
{ "session_id": "…", "force": false, "grace_ms": 10000 }
```

#### 5.5.1 Базовая последовательность

1. Если `force=false` и `inflight > 0` — отказ с сообщением «in‑flight calls: N, retry with force=true or wait».
2. **Graceful попытка ВСЕГДА первая.** Менеджер шлёт целевому клиенту `session.shutdown(grace_ms)` по WS. Параллельно — отсчёт `grace_ms`.
3. Если за `grace_ms` клиент вызвал `session.bye` или процесс завершился — kill завершён успехом. Конец.
4. Если grace истёк или WS уже мёртв на момент шага 2 — переход к **force/zombie path** (§5.5.2).
5. `force=true` — пропустить шаги 1‑3 и сразу 5.5.2.

#### 5.5.2 Force / zombie path — выбор backend (ADR‑0031)

Менеджер выбирает backend по `host_id` целевой сессии и наличию sidecar'а:

| Состояние сессии | `host_id == manager.host_id` | Backend |
|---|---|---|
| ManagerSpawned, PID известен | да | `LocalBackend.kill(pid)` — `SIGTERM` → 2с → `SIGKILL` (Linux); `TerminateProcess` (Windows) |
| ManagerSpawned, PID известен | нет | `RemoteBackend.kill(pid)` через sidecar в нужном `host_id` (см. §5.7) |
| SelfRegistered, PID + host_id из `register` | да | `LocalBackend.kill(pid)` |
| SelfRegistered, PID + host_id из `register` | нет | `RemoteBackend.kill(pid)` |
| sidecar отсутствует и `host_id != manager.host_id` | нет | mark dead + close WS, орфан логируется (`WARN: orphan suspected pid=… host_id=…`) |

**Self‑kill (target == sidecar).** Менеджер не запрашивает sidecar убить себя по PID — graceful по WS уже отправлен в §5.5.1. Если grace истёк — менеджер закрывает WS, помечает запись dead. Force через PID для self‑kill не предлагается (нет точки приложения).

PID‑верификация перед `LocalBackend.kill` — feature‑flag (см. §4.3).

### 5.6 `session.swap`

```jsonc
// input
{
  "kill_kinds": ["vanessa_manager", "vanessa_test_client", "client"],
  "spawn": [
    { "host_id": "1c-ai-sandbox", "kind": "vanessa_manager", "launch": { … } }
  ],
  "force": false
}
```

Семантика: сначала kill всех сессий из `kill_kinds` (с учётом `force` и kill matrix §5.5.2), потом последовательно spawn по списку (через §5.4). Если хоть один spawn падает — ошибка возвращается с тем, что уже было сделано (best‑effort, без отката). Каждый элемент `spawn[]` — полноценный payload `session.spawn` (inline `launch` или `template`+`overrides`).

### 5.7 `system_capability` — внутренний слой менеджер↔addin (ADR‑0027)

`system_capability` — контракт **между менеджером и addin'ом**, реализованный поверх того же WS‑канала, что `session.*` и `tool.call`. **НЕ публикуется** в MCP‑каталоге наружу AI‑агенту. Вызывается изнутри менеджера при обработке `session.spawn` / `session.kill` для удалённых host'ов.

#### 5.7.1 Методы (направление: manager → addin)

| Method | Params | Result |
|---|---|---|
| `addin.spawn` | `{ launch_spec, expected_uid, correlation_id? }` | `{ pid }` или error `addin_spawn_failed` |
| `addin.kill` | `{ pid, force }` | `{ ok: bool }` или error `addin_kill_failed` |

`launch_spec` — структура из §5.4 (`binary`, `args`, `env`, `startup_command`, `extra_args`). `expected_uid` — `client_uid` будущей сессии; addin должен подмешать `/C"client_uid=<uid>"` в командную строку.

#### 5.7.2 Notifications (направление: addin → manager)

| Method | Params | Назначение |
|---|---|---|
| `addin.child_exited` | `{ pid, code, signal? }` | Дочерний процесс, ранее запущенный через `addin.spawn`, завершился. Менеджер чистит соответствующую запись registry. |

#### 5.7.3 Авторизация и ограничения

- Метод `addin.*` от менеджера принимается addin'ом только при наличии `capability` в его `register` (см. §4.3).
- Addin может убивать **только** дочерние процессы, которые сам запустил (по pid → handle в локальном map). Запрос `addin.kill` для произвольного PID (не из своего pool) — error `addin_kill_not_owned`.
- Этап 7: allowlist бинарей в `launch_spec.binary` на стороне менеджера + auth‑token на MCP HTTP.

#### 5.7.4 Implementation notes (informative)

- Addin держит `Mutex<HashMap<Pid, Child>>` процессов, supervisor task через `Child::wait()` эмитит `addin.child_exited`.
- На Linux `addin.kill` использует `nix::sys::signal::kill(Pid::from_raw(pid), SIGTERM)`; на Windows — `OpenProcess(PROCESS_TERMINATE) + TerminateProcess`.
- Sleep между `SIGTERM` и `SIGKILL` — 2с. На Windows force = немедленный `TerminateProcess` (нет грейс‑модели в WinAPI).

## 6. Жизненный цикл сессии

### 6.0 Origin сессии (ADR‑0028)

Каждая запись в реестре несёт явный `origin`:

| Origin | Когда | Поведение idle‑sweeper |
|---|---|---|
| `ManagerSpawned` | Запись создана через `reserve_spawn` в рамках `session.spawn` | Подвержена idle‑kill, spawn‑timeout, force‑kill при таймауте |
| `SelfRegistered` | `register` без предварительной reservation (walk‑in: интерактивный клиент, sidecar, поднятый setup‑скриптом) | НЕ подвержена idle‑kill. Force‑kill только через явный `session.kill` |

`session.list` возвращает `origin` в выводе. `session.kill` обрабатывает оба origin одинаково (явное действие AI‑агента не должно различать источник).

### 6.1 Состояния

```
[Spawning] ──register──▶ [Active] ──no calls 30m──▶ [IdleKilling] ──exit──▶ [Gone]
   │            │            │
   │            │            ├── session.kill ──▶ [Killing] ──exit──▶ [Gone]
   │            │            │
   │            │            └── ws disconnect ──▶ [Disconnected] ──reconnect──▶ [Active]
   │            │                                       │
   │            │                                       └── grace timeout ──▶ [Gone]
   │            │
   │            └── register timeout ──▶ [SpawnFailed] ──▶ [Gone]
   │
   └── process spawn err ──▶ [SpawnFailed] ──▶ [Gone]
```

### 6.2 Spawning

1. Менеджер берёт шаблон, формирует `ProcessRequest` (по образцу `use_cases::launch_app`).
2. **Удерживает `tokio::process::Child`** (не `spawn` без handle), чтобы можно было сделать `child.kill().await`.
3. PID, child handle, kind, expected_uid (если шаблон навязывает) пишутся во временную запись с состоянием `Spawning`.
4. Ждёт `wait_for_register_ms`. Если за это время приходит `session.register` с подходящим контекстом — переход в `Active`. Иначе — kill процесса и `SpawnFailed`.
5. Состояние `Active` живёт в реестре, доступно через `session.list`.

### 6.3 Idle‑детектирование

Реестр для каждой сессии хранит `last_call_at` — время начала последнего проксированного `tool.call` от AI‑агента. Heartbeat **не** обновляет `last_call_at`. Inflight‑вызов **обновляет** `last_call_at` (и пока он inflight — сессия активна).

Фоновый сборщик (tokio interval) каждые N секунд (default 30) ищет сессии с `origin == ManagerSpawned && now - last_call_at > idle_timeout_secs && inflight == 0` → запускает `session.kill(force=false)` для них. Сессии с `origin == SelfRegistered` (walk‑in, sidecar, интерактивные) idle‑sweeper'ом не сканируются — их жизненный цикл управляется инициатором (см. §6.0).

### 6.4 Disconnect / reconnect

При разрыве WS:
- `session_id` остаётся в реестре в состоянии `Disconnected`.
- В `session.list` отражается флаг + `disconnected_since`.
- Все inflight `tool.call` к этой сессии завершаются ошибкой `session_gone`.
- Если в течение `reconnection_grace_secs` (default 30) приходит новый WS с `session.register` от того же `client_uid` → **soft reconnect**: реестр сохраняет `session_id`, набор tools, очередь, счётчики; новый WS привязывается к существующей записи, состояние возвращается в `Active`. В ответе на `session.register` менеджер указывает `reconnected: true`. При включённом auth‑токене это безопасно (uid угадать без токена невозможно). При выключенном auth — riski минимальный (надо угадать uid за 30 сек), но фиксируется в audit log.
- Иначе — менеджер пытается `child.kill()` (если процесс ещё жив) и удаляет запись.

## 7. Per‑session FIFO

### 7.1 Зачем

1С обрабатывает `MCP_TOOL_CALL` через однопоточный `external_event` с дефолтным таймаутом 30 сек (см. `web-transport-addin/src/mcp/server.rs:639` и `src/mcp/addin.rs:614`). Параллельные вызовы в один и тот же 1С‑клиент → таймауты или сериализация на стороне 1С.

### 7.2 Реализация

Образец — существующий `mcp::edt_session::EdtSessionManager`:

- На сессию выдаётся 1 admission slot (семафор capacity = 1).
- Очередь FIFO с enqueue‑time deadline (`deadline_ms` из `session.spawn`/глобального дефолта).
- Отмена клиентом MCP (cancellation routing) выкидывает запись из очереди до начала исполнения; running call продолжается до terminal state.
- Tracing events: `mcp_session_queue_depth`, `mcp_session_queue_wait`, `mcp_session_call_outcome`.

### 7.3 Inflight‑видимость

- В `session.list` поле `inflight: u32`.
- `session.kill(force=false)` отказывается, если `inflight > 0`.
- `session.kill(force=true)` обрывает inflight ошибкой `session_gone` для соответствующего MCP‑вызова.

## 8. Классификация сессий и spawn‑шаблоны

### 8.1 Kinds (MVP)

| Kind | Описание | Singleton | Спавнится менеджером? |
|---|---|---|---|
| `client` | Просто 1С‑клиент с расширением, без тестового сценария | нет | да (на запрос) |
| `yaxunit_runner` | 1С‑клиент с конфигом yaxunit, исполняющий тесты | да (по умолчанию) | да |
| `vanessa_manager` | 1С‑процесс с `vanessa-automation-single.epf`, дирижирует прогоном | да (по умолчанию) | да |
| `vanessa_test_client` | 1С‑клиент, который VA‑manager сам поднял для прогона test‑клиента | нет | **нет** (запускает VA‑manager, мы только видим в реестре) |

Список kinds расширяемый (конфигом). Для каждого kind в конфиге задаётся флаг `publish_named_tools: bool` — публиковать ли его tools как именованные `<kind>__<tool_name>` в `tools/list` AI‑агента. По умолчанию:

- `client`, `yaxunit_runner`, `vanessa_manager` → `true`;
- `vanessa_test_client` → `false` (доступен только через `session.call`, чтобы не засорять `tools/list` при больших VA‑прогонах с десятками test‑клиентов; AI‑агент находит их через `va.list_test_clients` от VA‑manager или через `session.list`).

### 8.2 Источники типизации

- **Менеджер‑порождённые сессии**: kind задаётся выбранным spawn‑template'ом. В spawn‑команде клиенту передаётся ENV `V8RUNNER_SESSION_KIND=...`, и 1С‑код в `session.register` обязан повторить это значение.
- **Сторонние сессии** (`vanessa_test_client`, ad‑hoc): клиент сам декларирует kind в register.
- При несоответствии менеджер‑порождённого kind и заявленного клиентом — ошибка регистрации.

### 8.3 Spawn‑шаблоны (опционально, ADR‑0030)

> **Изменение модели.** Основной путь `session.spawn` — inline launch‑spec от вызывающего AI‑агента (см. §5.4). `spawn_templates` ниже остаются как **опциональные пресеты** для фиксированных CI‑pipeline'ов или сценариев, где конфигурация запуска предсказуема и не меняется. Если в `session.spawn` пришёл `template` без `launch` — менеджер ищет ключ в этом разделе и применяет `overrides`.

В `v8project.yaml`:

```yaml
mcp:
  session_manager:
    bind_address: "0.0.0.0:4000"
    path: "/sessions"
    heartbeat_interval_ms: 15000
    idle_timeout_secs: 1800
    reconnection_grace_secs: 30
    register_timeout_ms: 60000
    auth_token: "${V8RUNNER_SESSION_TOKEN:-}"   # опционально
    singleton_kinds: ["yaxunit_runner", "vanessa_manager"]
    spawn_templates:
      yaxunit_runner:
        kind: yaxunit_runner
        mode: THIN
        connection: "${connection_default}"
        extra_args: ["/DisableStartupMessages", "/DisableStartupDialogs"]
        startup_command: "C\"RunYaXUnit;configFile=$YAXUNIT_CONFIG\""
      vanessa_manager:
        kind: vanessa_manager
        mode: THIN
        connection: "${connection_default}"
        extra_args:
          - "/TESTMANAGER"
          - "/DisableStartupMessages"
          - "/Execute$VA_EPF_PATH"
        startup_command: "C\"StartFeaturePlayer;workspaceRoot=$WORKSPACE;VBParams=$VA_PARAMS\""
```

Параметры шаблона (`$VAR`) подставляются из `overrides` в `session.spawn`. Базовые connection‑строки берутся из существующего `connection`‑контракта.

### 8.4 Доставка параметров клиенту (см. ADR‑0020)

Контракт между менеджером и 1С‑клиентом — единый и кросс‑платформенный. Trait `SessionLaunchParamsCarrier` и временные params‑file отменены. См. ADR‑0020.

#### 8.4.1 `manager_url` — зашит в клиенте

- Default `ws://127.0.0.1:4000/sessions` захардкожен в коде расширения `web-transport-addin` и совпадает с default‑bind менеджера (§8.3). Один источник истины.
- Переопределение — только через константу расширения `WebTransportSessionManagerURL`. При первом запуске расширения константа автозаполняется default'ом.
- Менеджер `manager_url` через CLI **не передаёт**: смена адреса — однократное действие на стороне ИБ.

Это работает одинаково и для клиентов, спавненых менеджером, и для клиентов, поднятых VA‑manager'ом или запущенных пользователем интерактивно.

#### 8.4.2 Опциональные параметры через `/C"key=value ..."`

Менеджер при `session.spawn` может добавить в командную строку `1cv8c` ключ `/C"..."` со следующим ограниченным набором:

| Параметр | Когда передавать | Назначение |
|---|---|---|
| `correlation_id` | при спавне менеджером | сквозной trace в логах 1С + менеджер |
| `kind` | редко, как override эвристики | страховка для нестандартных шаблонов запуска |

Других параметров в контракте нет. Auth‑token не передаётся.

#### 8.4.3 Чтение на стороне 1С (через addin)

Метод addin'а `ПолучитьПараметрыСессии()` возвращает структуру `{ manager_url, client_uid, kind, host_id, pid, capabilities, correlation_id? }`:

1. **Парсинг `ПараметрЗапуска`** — через обёртку БСП в общем модуле `CommonClientServer` / `ОбщегоНазначенияКлиентСервер`. Точная сигнатура и имя метода фиксируются на этапе 5 проверкой против установленной в DRIVE версии БСП. Если подходящего метода в конкретной БСП нет — в общий модуль расширения добавляется тонкая утилитарная функция (парная en/ru) — это не «свой парсер», а локальная обёртка над `ПараметрЗапуска`/`StartupParameter` в стиле БСП.
2. **`manager_url`:** только из константы `WebTransportSessionManagerURL`; пустая → default. `/C"manager_url=..."` игнорируется.
3. **`client_uid`:** `Новый УникальныйИдентификатор()` / `New UUID()`.
4. **`kind`:** `/C"kind=..."` имеет приоритет; иначе вычисляется по `СтрокаЗапуска()` / `LaunchString()` (`/TESTMANAGER` → `vanessa_manager`, `/TESTCLIENT` → `vanessa_test_client`, `RunYaXUnit` в строке `/C` → `yaxunit_runner`, иначе → `client`).
5. **`correlation_id`:** напрямую из `/C`; отсутствует — пустая строка.
6. **`host_id` (ADR‑0029):** ENV `V8_HOST_ID` → `gethostname()` (Linux) → `COMPUTERNAME` (Windows). Определяется однократно при инициализации addin.
7. **`pid` (ADR‑0029):** `std::process::id()` — PID самого процесса `1cv8c`.
8. **`capabilities` (ADR‑0029):** массив строк, в текущей сборке `web-transport-addin` всегда `["spawn", "kill"]`. Передаётся в `session.register`; оставляет место для forward compatibility между версиями.

#### 8.4.4 Безопасность

- Auth‑token из контракта удалён (см. §11 — модель «доверенный dev‑контур»).
- В `ps`/`tasklist` могут быть видны `correlation_id` и `kind` — оба нечувствительные.
- Audit log менеджера фиксирует `client_uid`, `kind`, заявленный набор tools и peer address.

## 9. Сценарии асинхронного VA

### 9.1 Запуск VA‑прогона

1. AI‑агент → `session.spawn(template="vanessa_manager", overrides={ scenarios: "...", params_path: "..." }, if_exists="replace")`.
2. Менеджер: kill старого VA‑manager (если был, граефул через `session.kill`), затем spawn `1cv8c /TESTMANAGER /Execute…`.
3. 1С‑код в VA‑manager (через `web-transport-addin`):
   - в стартовом обработчике (или в начале сценария) вызывает `Подключиться(URL)` на addin.ws с URL из ENV;
   - регистрирует свой набор tools: `va.run_status`, `va.current_scenario`, `va.abort_run`, `va.list_test_clients`, `va.dump_run_artifacts`, и т.п.;
   - отправляет `session.register`.
4. Менеджер возвращает AI‑агенту `session_id`.
5. AI‑агент держит активный мониторинг через `session.call(session_id, "va.run_status")` или через `s_<id>__va_run_status` (если list_changed поддержан).
6. По мере прогона VA сама стартует test‑клиенты — каждый из них регистрируется отдельной сессией `vanessa_test_client` и появляется в `session.list`. Их `client_uid` VA‑manager может перечислить через свой `va.list_test_clients`.

### 9.2 Диагностика зависания

- AI‑агент видит, что `va.run_status` не меняется → дёргает у конкретного `vanessa_test_client` его tools (`client.screenshot`, `client.dump_active_form`, …) — то, что 1С‑код заявит как доступное.
- При полной потере связи (WS reconnect не сработал, addin не отвечает) — `session.kill(force=true)`.

### 9.3 Дедупликация

Поведение `if_exists` — основной рычаг. По умолчанию `vanessa_manager` синглтон → попытка повторного спавна без `replace` возвращает ошибку с подсказкой.

## 10. Динамика tools, resources, prompts

### 10.1 Совместимость с MCP‑клиентами

- Поддерживающие `notifications/tools/list_changed` (Claude Code, актуальный rmcp‑совместимый клиент): получают живой набор client tools.
- Не поддерживающие: пользуются `session.list` + `session.call` без list_changed. Эта пара публикуется всегда и достаточна для всего функционала. Документируется как fallback.

### 10.2 Pass‑through для resources и prompts

Хотя в текущем 1С‑коде resources/prompts не используются, протокол менеджера поддерживает их с MVP — это копеечная доработка поверх уже описанного `tool.call`‑роутинга. На стороне клиента в `session.register` принимаются опциональные поля `resources` и `prompts` (см. §4.3).

Менеджер в `tools/list_changed` стиле эмитит:
- `notifications/resources/list_changed` при изменении набора ресурсов;
- `notifications/prompts/list_changed` при изменении набора промптов.

Через WS‑tunnel поддерживаются дополнительные направления `manager → client`:

| Method | Params | Result |
|---|---|---|
| `resource.read` | `{ uri }` | `{ contents: [...] }` |
| `prompt.get` | `{ name, arguments? }` | `{ messages: [...], description? }` |

Дедупликация для resources — по `(kind, uri, content_hash?)` (если 1С‑коды ленивые и hash не передают, дедупликация по `(kind, uri)`). Для prompts — по `(kind, name, schema_hash)`. AI‑агент обращается к `<kind>__<name>` или `<kind>__<uri-suffix>`, при коллизиях — fallback на `session.read_resource(session_id, uri)` / `session.get_prompt(session_id, name, args)`.

Подробная спецификация resource/prompt дедупликации — when first 1С‑use‑case появится; на MVP протокол готов, реализация — ленивая.

## 11. Безопасность

- **Bind address** — настраивается. На dev‑машине рекомендуем `127.0.0.1:4000`. В контейнере — внутренний bridge или `0.0.0.0` за firewall.
- **Auth token** (опц.): bearer‑токен в WS handshake (`Authorization: Bearer ...`) и/или в query (`?token=...`). Сравнивается constant‑time. Если токен в конфиге не задан — проверка отключена.
- **Изоляция от MCP HTTP**: WS‑сервер живёт на отдельном `axum::Server` (отдельный bind), не делит `LocalSessionManager`/`max_sessions` с `mcp.http`.
- **Audit log**: каждая регистрация/kill/spawn пишется в `tracing::info!` со структурой (`session_id`, `kind`, `client_uid`, `pid`, `actor`).

## 12. Наблюдаемость

- Tracing events (по образцу `mcp/telemetry.rs`):
  - `mcp_session_register{ session_id, kind, client_uid, outcome }`
  - `mcp_session_unregister{ session_id, kind, reason }`
  - `mcp_session_spawn{ template, kind, pid, outcome, elapsed_ms }`
  - `mcp_session_call{ session_id, tool_name, outcome, queue_wait_ms, exec_ms }`
  - `mcp_session_queue_depth{ session_id, depth }`
  - `mcp_session_idle_kill{ session_id, idle_secs }`
- `correlation_id` (UUID) добавляется к каждому пути «MCP‑запрос → tool.call → tool result» и пробрасывается в WS‑сообщение `tool.call.params.correlation_id` для сквозного трейса в 1С‑логах.

## 13. Доработки в `web-transport-addin`

Чтобы поддержать дизайн (B), в addin вносятся следующие изменения. Ниже — контракт, не патч.

1. **Режим WS‑tunnel в addin.mcp** или **проксирование MCP в ws‑классе**.
   Текущий `mcp::server::dispatch_request` (`src/mcp/server.rs:639`) уже умеет роутить incoming JSON в зарегистрированные tools. Нужно:
   - Поднять модуль, который умеет **читать MCP JSON‑RPC из WS‑text‑frame** и отправлять ответ обратно через тот же WS.
   - Использовать тот же `Registry` и тот же external_event‑dispatch, что и HTTP MCP.
   - Простой вариант: в существующем `ws_client.rs` добавить «server mode» / «tunnel mode», где принятые сообщения интерпретируются как JSON‑RPC и пробрасываются в общий MCP dispatcher; а исходящие control‑сообщения (`session.register`, `session.tools_changed`, ...) формируются вызовами 1С‑кода.

2. **Авто‑реконнект** в WS‑клиенте.
   Сейчас `Подключиться` — однократный (`src/ws_client.rs:85`). Добавить параметры:
   - `Подключиться(URL, Заголовки, Таймаут, БэкоффМс, МаксПопыток, ПереподключатьсяАвтоматически)` или отдельный метод `ВключитьАвтопереподключение(...)`.
   - При разрыве — экспоненциальный backoff с потолком; уведомление 1С через ВнешнееСобытие `WS_RECONNECT_STATE`.

3. **Получение параметров и `client_uid`** в register.
   Метод `ПолучитьПараметрыСессии()` (см. §8.4.3) возвращает структуру `{ manager_url, client_uid, kind, correlation_id? }`, скрывая источник: константа `WebTransportSessionManagerURL` (с default), парсинг `ПараметрЗапуска` через обёртку БСП (англо‑/русскоязычная), `client_uid` от `Новый УникальныйИдентификатор()`, `kind` по `СтрокаЗапуска()` или `/C"kind=..."`. Дальше метод `ЗапуститьСессионнуюИнтеграцию()` дёргает WS‑connect, посылает `session.register` и эмитит ВнешнееСобытие при изменениях. Auth‑token из контракта удалён.

4. **Сквозная пробрасываемость correlation_id** в payload `MCP_TOOL_CALL` (поле `correlationId`), чтобы 1С‑код мог записывать его в свои логи.

Эти доработки делаются параллельно с менеджером и согласуются через JSON‑RPC контракт §4.

## 14. Доработки в v8-runner

### 14.1 Новый модуль `src/session_manager/`

- `transport.rs` — axum/ws сервер (отдельный `axum::Server` на `mcp.session_manager.bind_address`).
- `protocol.rs` — типы JSON‑RPC сообщений §4.
- `registry.rs` — реестр сессий (in‑memory `RwLock<HashMap<SessionId, Session>>`, с `tokio::process::Child`).
- `router.rs` — диспетчер: `tool.call` от MCP → per‑session FIFO → WS отправка → ожидание ответа.
- `lifecycle.rs` — стейт‑машина §6, idle‑sweeper, reconnection grace.
- `spawn.rs` — обёртка над `use_cases::launch_app` для шаблонов.
- `telemetry.rs` — tracing‑события из §12.

### 14.2 Изменения в `src/mcp/`

- В `McpToolServer` ввести `Scope { Server, ClientLocal, Management, ClientProxy }`.
- `tools/list` объединяет:
  - Server + ClientLocal + Management — статически из реестра менеджера;
  - ClientProxy — динамически из `session_manager::registry`.
- При изменении registry эмитится rmcp `notifications/tools/list_changed`.
- `tools/call`:
  - Server / ClientLocal / Management — как сейчас (через `McpService`);
  - ClientProxy и `session.call` — через `session_manager::router`.

### 14.3 Изменения в `src/config/model.rs`

- Новая структура `McpSessionManagerConfig` (см. §8.3).
- В `McpConfig` добавить `pub session_manager: Option<McpSessionManagerConfig>`.

### 14.4 Изменения в `src/platform/process.rs`

- `spawn()` в дополнение к `SpawnResult { pid }` должен иметь альтернативу, возвращающую `tokio::process::Child` (или хранить внутри shared map). Используется реестром сессий для надёжного kill и для wait‑on‑exit.

### 14.5 Существующие tools

- `launch_app`, `run_all_tests`, `run_module_tests` остаются без изменений как `ClientLocal` tools (для сценариев, где менеджер сессий не нужен).
- `build_project`, `dump_config`, `check_syntax_*` остаются без изменений как `Server` tools.

## 15. Этапы внедрения (предлагаемый порядок)

1. **WS‑transport + protocol + registry (без spawn)**.
   - WS‑сервер на порту, JSON‑RPC, `session.register`/`session.bye`, in‑memory registry, `session.list` как management tool.
   - Mock‑клиент на Rust для e2e.
2. **Прокси `tool.call`** через WS, per‑session FIFO, list_changed, ClientProxy tools, `session.call`.
3. **Spawn/kill**: `session.spawn` поверх `launch_app` с удержанием `Child`, `session.kill` (graceful + force), idle‑sweeper.
4. **Шаблоны и kinds**, `if_exists`, `session.swap`, classification.
5. **Доработки в addin** (параллельно с этапами 1–2): tunnel‑режим, reconnect, client_uid.
6. **Интеграция с DRIVE**: spawn‑шаблоны для `yaxunit_runner` и `vanessa_manager`, smoke по реальным сценариям.
7. **Observability/tracing**, correlation_id, документация и ADR.

## 16. Зафиксированные решения и оставшиеся вопросы

### Решено (закрыто на этой итерации)

- **`session.swap`** — best‑effort: kill всех указанных kinds, потом последовательный spawn по списку; при падении spawn возвращается ошибка с описанием уже выполненных шагов, отката нет (см. §5.6).
- **Reconnect по `client_uid`** — мягкий: совпадение uid с записью в `Disconnected` интерпретируется как reconnect, запись восстанавливается со всем состоянием (см. §4.6, §6.4). Безопасность — за счёт auth‑токена.
- **Дедупликация client tools** — однотипные регистрации схлопываются в один публичный tool `<kind>__<tool_name>`, AI‑агент адресует сессию параметром `session_id` (опционален при единственном кандидате, обязателен при двух+). См. §5.3.
- **Видимость `vanessa_test_client`** — не публикуются в `tools/list` (флаг `publish_named_tools=false`), только через `session.call` и `va.list_test_clients`. См. §8.1.
- **Resources/prompts pass‑through** — поддерживаются в протоколе с MVP, дедупликация ленивая до первого реального use‑case'а. См. §10.2.
- **Доставка параметров клиенту** — `manager_url` зашит в клиенте через константу расширения `WebTransportSessionManagerURL` (с default `ws://127.0.0.1:4000/sessions`); опциональные `correlation_id` и `kind` — через `/C"key=value ..."` при спавне менеджером; парсинг через обёртку БСП (англо/русско). Никаких trait'ов и временных файлов. Auth‑token не используется. См. §8.4 и ADR‑0020.

### Открыто (требует исследования при реализации)

- **Q‑R1.** Конкретный формат `extra_args`/`startup_command` в spawn‑шаблонах для Linux и Windows (DISPLAY, экранирование, кириллица в `/C"..."`, ANSI vs UTF‑8). Закрывается при реализации первого реального шаблона `yaxunit_runner` на DRIVE.
- **Q‑R2.** Совместное использование `use_cases::launch_app` с шаблонами spawn'а: переиспользовать целиком (через адаптацию `LaunchModeRequest`) или вводить параллельный путь. Решение по результатам §15 этап 3.
- **Q‑R3.** Дедупликация resources/prompts: что считать «эквивалентным» ресурсом — только URI или URI+content_hash. Закрывается при первом use‑case'е от 1С‑кода.

---

## 17. Observability, Security, Retry (Этап 7)

### 17.1 Correlation ID (задача 7.2)

`correlation_id` — опциональный UUID, прокидываемый сквозь `session.spawn` / `session.kill`:

- Передаётся в `McpSessionSpawnRequest.correlation_id` и `McpSessionKillRequest.correlation_id`.
- Если не задан — менеджер генерирует `uuid::Uuid::new_v4()`.
- Пробрасывается через `SpawnRouter.spawn(correlation_id)` → `SpawnBackend.spawn(correlation_id)`.
- В `LocalBackend`: добавляется как `correlation_id=<value>` в `/C"..."` startup-param (рядом с `client_uid`, `kind`).
- В `RemoteBackend`: передаётся как поле `correlation_id` в JSON-RPC params `addin.spawn`.
- Весь поток обёрнут в `tracing::info_span!("session_spawn", correlation_id, host_id, kind)`.
- Возвращается в response: `spawn` → поле `correlation_id`, `kill` → поле `correlation_id`.

### 17.2 Метрики Prometheus (задача 7.3)

Используется crate `metrics = "0.23"` + `metrics-exporter-prometheus = "0.15"`.

#### Счётчики и гистограмма

| Метрика | Тип | Лейблы |
|---------|-----|--------|
| `mcp_session_spawn_total` | Counter | `backend` (local\|remote), `outcome` (success\|timeout\|reservation_conflict\|backend_error\|kind_mismatch) |
| `mcp_session_kill_total` | Counter | `backend`, `outcome` (graceful\|force\|already_dead\|orphan\|backend_error) |
| `mcp_session_spawn_duration_seconds` | Histogram | `backend` |

#### Конфигурация

```yaml
mcp:
  metrics:
    bind_address: "127.0.0.1:9100"  # отключить — убрать поле или оставить пустым
```

Default: `127.0.0.1:9100`. При старте `serve_http`/`serve_session_manager` Prometheus exporter запускается автоматически.  
Если `bind_address` отсутствует или пусто — exporter не запускается.

### 17.3 PID Verification (задача 7.4)

```yaml
mcp:
  session_manager:
    spawn:
      verify_pid_via_proc: false   # default
```

Если `true` и платформа Linux: после spawn читается `/proc/<pid>/cmdline`, `argv[0]` сравнивается с `LaunchSpec.binary`. При несовпадении — `SpawnError::PidVerificationFailed` (spawn fail).  
На не-Linux: флаг принимается, но verification пропускается с `warn!`.

Инжектируемый trait `CmdlineProvider` позволяет unit-тестировать без реального `/proc`.

### 17.4 Allowlist бинарей (задача 7.5)

```yaml
mcp:
  session_manager:
    spawn:
      allowed_binaries: []    # пусто = разрешено всё (WARN при старте)
```

`LocalBackend.spawn` проверяет `binary` против списка до запуска процесса.  
При непустом списке и бинаре вне него — `SpawnError::BinaryNotAllowed` (постоянная ошибка, не retryable).  
`RemoteBackend` не дублирует allowlist — ответственность на sidecar в том хосте.

### 17.5 Auth-token Bearer на MCP HTTP (задача 7.6)

```yaml
mcp:
  http:
    auth_token: "secret"    # если не задано — open endpoint с WARN при старте
```

При наличии `auth_token`: все входящие запросы к MCP HTTP проверяются на `Authorization: Bearer <token>`.  
Неверный/отсутствующий заголовок → HTTP 401 Unauthorized.  
Реализовано как axum middleware в `HttpMcpService::handle`.

### 17.6 Retry policy для RemoteBackend (задача 7.7)

```yaml
mcp:
  session_manager:
    remote_backend:
      max_attempts: 3          # включая первую попытку
      base_backoff_ms: 200     # 200ms, 400ms, 800ms, ...
```

Transient-ошибки (`RemoteTimeout`, общие `Remote` RPC-ошибки, обрыв WS): retry с exponential backoff.  
Permanent-ошибки (`NoSpawner`, `BinaryNotAllowed` на стороне sidecar): без retry.  
Логи: `warn!` на каждой попытке, `error!` на финальной неудаче.

Аналогичная политика применяется к `kill`-операциям через `RemoteBackend`.
