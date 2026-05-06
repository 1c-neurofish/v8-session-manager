# Backlog реализации менеджера клиентских сессий

> Источник истины по проектированию — `spec/SESSION_MANAGER.md`.
> Режим работы и риски — `spec/HANDOFF.md`.
> Этот документ — операционный backlog: «этап → acceptance‑сценарии → smoke‑команда → owner». Уровень детализации acceptance растёт по мере приближения этапа к старту.

## Статус по этапам

| # | Этап | Статус | Owner | Гейт старта |
|---|------|--------|-------|-------------|
| 1 | WS‑transport + protocol + registry (без spawn) | `done` (зелёный 2026-04-28) | claude | ADR‑0018..0022 — `accepted` ✅ |
| 2 | Прокси `tool.call` + per‑session FIFO + list_changed + ClientProxy tools | `done` (зелёный 2026-04-28) | claude | ADR‑0023..0026 — `accepted` ✅ |
| 3 | Spawn/kill + idle‑sweeper | `done` (зелёный 2026-04-28) | claude | ADR‑0023..0026 ✅ |
| 4 | Шаблоны, kinds, `if_exists`, `session.swap` | `done` (зелёный 2026-04-28) | claude | Зелёный этап 3 ✅ |
| 5 | Доработки `web-transport-addin` (tunnel, reconnect, uid, ПолучитьПараметрыСессии) | `done` (зелёный 2026-04-28) | claude | 7 PR смержены в `web-transport-addin` (PR #1..#7), 100/100 lib-тестов; bind в 1С‑addin класс (метод `ЗапуститьСессионнуюИнтеграцию`) — на этапе 6 |
| 6 | Интеграция с DRIVE: spawn‑шаблоны yaxunit/VA, smoke | `pending` | TBA | Зелёный 3+5 |
| 7 | Observability, correlation_id, финализация ADR/доки | `pending` | TBA | Зелёный 6 |

## Сквозные требования ко всем этапам

- PR per stage, ≤ 3‑5 дней между merge.
- Acceptance‑сценарии в каждом PR — формат «Дано / Когда / Тогда», не «работает».
- Smoke‑команда для ручного прогона приведена явно.
- JSON Schema протокола (§4 спеки) в `spec/protocol/` — версионируется и валидируется в CI.
- Mock‑клиент в `src/bin/mock_client.rs` — обновляется с каждым изменением протокола (исходно планировалось `tests/mock_client/`, перенесено в `src/bin/` из‑за конфликта с auto‑detect integration‑tests target).

---

## Этап 1 — WS‑transport + protocol + registry (без spawn)

**Прогресс по подзадачам:**

| # | Подзадача | Статус | Артефакт |
|---|---|---|---|
| 1.1 | `McpSessionManagerConfig` в `config/model.rs` + 2 unit‑теста | ✅ done (`6b12790`) | `src/config/model.rs`, `src/config/loader.rs` |
| 1.2 | JSON Schema протокола | ✅ done (`6b12790`) | `spec/protocol/messages.schema.json`, `README.md` |
| 1.3 | `src/session_manager/protocol.rs` — wire types + 6 unit‑тестов | ✅ done (`6b12790`) | `src/session_manager/protocol.rs` |
| 1.4 | `src/session_manager/registry.rs` — реестр + soft reconnect | ✅ done (`e389ae0`) | 8 unit‑тестов: create, uid‑collision, disconnect, soft reconnect, sweep grace, remove, update_tools, snapshot |
| 1.5 | `src/session_manager/transport.rs` — axum WS server | ✅ done | 9 интеграционных тестов через tokio‑tungstenite: register, collision, disconnect+grace+sweeper, reconnect, ping, parse_error, bye, tools_changed, unknown_method |
| 1.6 | `src/bin/mock_client.rs` — Rust‑бинарь (handshake → register → heartbeat → bye) | ✅ done | Размещён в `src/bin/` (не `tests/`, чтобы избежать конфликта с auto‑detect integration‑tests). Smoke вручную через `cargo run --bin mock_client` |
| 1.7 | MCP `session.list` + acceptance #1 E2E test | ✅ done (минимально) | `src/session_manager/management.rs` (DTO + `list()`), tool `session_list` в `McpToolServer`, acceptance‑сценарий #1 как `tokio::test`. Acceptance #2..#5 покрыты тестами `transport`/`registry`. **Отложено в этап 2:** CLI subcommand `v8-runner session-manager` (требует общего entry point с proxy `tool.call`) |

**Цель.** Менеджер принимает WS‑коннекты от 1С‑клиентов (или от mock'а), регистрирует сессии, отдаёт `session.list` через MCP. Без spawn'а, без проксирования `tool.call`.

**Артефакты:**

- `src/session_manager/{transport,protocol,registry,telemetry}.rs`.
- `tests/mock_client/` — Rust‑бинарь, эмулирующий addin (handshake → register → heartbeat → bye).
- `spec/protocol/*.schema.json` — JSON Schema на сообщения §4.2‑4.4 спеки.
- ADR‑0018, ADR‑0022 в статусе `accepted`.

**Acceptance‑сценарии:**

1. *Регистрация и видимость.*
   - Дано: менеджер запущен с `bind_address=127.0.0.1:4000` и пустым реестром.
   - Когда: mock‑клиент открывает WS, шлёт `session.register{client_uid="A", kind="client"}`.
   - Тогда: ответ содержит `session_id="A"`, `heartbeat_interval_ms`, `idle_timeout_secs`; MCP `session.list` возвращает запись с `session_id="A"`, `kind=client`, `tools=[]`, `inflight=0`.
2. *Коллизия активного uid.*
   - Дано: сессия `A` активна.
   - Когда: второй mock пытается зарегистрироваться с `client_uid="A"`.
   - Тогда: ответ — JSON‑RPC error `-32010 session_uid_collision`, в `session.list` всё ещё одна запись `A`.
3. *Грейс‑шатдаун клиента.*
   - Дано: сессия `B` активна.
   - Когда: mock шлёт `session.bye{reason="test"}`, закрывает WS.
   - Тогда: запись `B` удалена из реестра в течение 1 сек, эмитнут `notifications/tools/list_changed`.
4. *Heartbeat и WS‑drop без reconnect.*
   - Дано: сессия `C` активна, `reconnection_grace_secs=2`.
   - Когда: mock закрывает WS без `session.bye`.
   - Тогда: в `session.list` `C` сразу в состоянии `Disconnected` с `disconnected_since`; через ~2 сек `C` исчезает.
5. *Soft reconnect (ADR‑0022).*
   - Дано: сессия `D` в состоянии `Disconnected`, прошло < grace.
   - Когда: новый WS открывается, шлёт `session.register{client_uid="D"}`.
   - Тогда: ответ содержит `reconnected: true`, `session_id="D"`; в `session.list` `D` снова `Active`, `tools` не сброшены.

**Smoke‑команда:**

```bash
cargo run -p v8-runner -- session-manager --config configs/session-manager.dev.yml &
cargo run -p v8-runner --bin mock_client -- --url ws://127.0.0.1:4000/sessions --kind client --uid demo
# в третьем терминале — MCP-запрос session.list через curl/mcp-client
```

---

## Этап 2 — Прокси `tool.call` + per‑session FIFO + list_changed + ClientProxy tools

**Цель.** AI‑агент через MCP может вызывать tools, зарегистрированные клиентами, с дедупликацией по `(kind, name, schema_hash)`.

**Гейт старта:** ADR‑0023..0026 в статусе `accepted` (drafts оформлены 2026‑04‑28, ждут апрува).

**Подзадачи (план до апрува ADR):**

| # | Задача | ADR | Артефакт |
|---|---|---|---|
| 2.1 | CLI subcommand `v8-runner session-manager` (отложено из этапа 1.7): общий entry point с `Arc<SessionRegistry>`, MCP HTTP server + WS transport, SIGINT shutdown | — | `src/cli/session_manager.rs`, точка маршрутизации в `main.rs` |
| 2.2 | `ConnectionHandle` — outbound id‑correlation, pending‑таблица, drain at disconnect | ADR‑0023 | `src/session_manager/connections.rs` + правки `transport.rs` |
| 2.3 | Контракт `tool.call`/`tool.cancel` (manager → client): протокольные типы + JSON Schema | ADR‑0023 | расширение `src/session_manager/protocol.rs`, `spec/protocol/messages.schema.json` |
| 2.4 | `SessionDispatcher` — admission slot, FIFO queue, cancellation routing, telemetry events | ADR‑0024 | `src/session_manager/dispatcher.rs` |
| 2.5 | ClientProxy tools — override `ServerHandler::list_tools`/`call_tool`, дедупликация по schema_hash, round‑robin при multi‑session, скрытие при конфликте схем; management tool `session.call` | ADR‑0025 | `src/session_manager/router.rs` + `src/mcp/server.rs` |
| 2.6 | `tools/list_changed` notifier — debounce 200 мс, multi‑peer fanout, capability `enable_tool_list_changed()` | ADR‑0026 | `src/session_manager/notify.rs` |
| 2.7 | Расширение mock_client: реализация ответа на `tool.call` (echo arguments) + интеграционные тесты на acceptance #1‑#6 | — | `src/bin/mock_client.rs`, тесты в `src/session_manager/router.rs` |

**Acceptance‑сценарии:**

1. *Дедупликация при единственной сессии.* Один mock kind=client с tool `echo` → AI‑агент видит `client__echo`, вызов без `session_id` проходит.
2. *Round‑robin при двух+ сессиях с одинаковой схемой.* Два mock'а kind=client с одинаковым `echo` (равный schema_hash) → `client__echo` опубликован один раз; последовательные вызовы без `session_id` уходят в разных клиентов по round‑robin (счётчик per‑group).
3. *FIFO внутри сессии.* Параллельно отправляются 3 `tool.call` в одну сессию, mock отдаёт каждый за 200 мс → менеджер обрабатывает последовательно, `inflight` в `session.list` не превышает 1.
4. *Cancellation в queued.* `tool.call` №2 отменяется до старта → клиент его не получает; `tool.call` №3 проходит штатно.
5. *Конфликт схем — tool скрыт.* Два mock'а kind=client с tool `echo` разной `input_schema` → `client__echo` **не** появляется в `tools/list`; в audit‑log `proxy_tool_hidden{reason:"schema_conflict"}`. Tool по‑прежнему вызывается через `session.call(session_id, "echo", args)`.
6. *`session.call` fallback.* Тот же tool вызывается через `session.call(session_id, "echo", args)` без публичного имени (например, для kind=`vanessa_test_client` с `publish_named_tools=false`).
7. *`tools/list_changed` debounce.* Поднимаются 5 сессий с интервалом 50 мс → AI‑агент получает **одно** уведомление в окне 200 мс после последнего register'а, не 5.
8. *Disconnect drain.* Сессия с 1 inflight + 2 queued получает разрыв WS → inflight завершается ошибкой `-32011 session_gone`, queued завершаются той же ошибкой; реестр переходит в `Disconnected`.

**Smoke‑команда (после 2.1):**

```bash
v8-runner session-manager --bind 127.0.0.1:4000 --mcp-http 127.0.0.1:4001 &
mock_client --url ws://127.0.0.1:4000/sessions --kind client --uid demo --tools echo &
# AI-агент: MCP HTTP на :4001, видит client__echo, дёргает его
```

---

## Этап 3 — Spawn/kill + idle‑sweeper ✅

**Цель.** Менеджер сам запускает `1cv8c` по шаблону, удерживает `Child`, гасит idle‑сессии. Q‑R2 закрывается здесь.

**Прогресс по подзадачам:**

| # | Подзадача | Статус | Артефакт |
|---|---|---|---|
| 3.1 | `SpawnTemplate` config + `EnvCarrier` MVP | ✅ done (PR #5) | `config/model.rs`, `session_manager/env_carrier.rs` |
| 3.2 | Протокол: `session.shutdown` + mock_client читает env | ✅ done (PR #6) | `session_manager/protocol.rs`, `bin/mock_client.rs` |
| 3.3 | Registry: `Spawning` state + `reserve_spawn` | ✅ done (PR #7) | `session_manager/registry.rs` |
| 3.4 | `spawn.rs` — Child + ожидание register | ✅ done (PR #8) | `session_manager/spawn.rs` |
| 3.5 | `lifecycle.rs` — graceful/force kill + idle‑sweeper | ✅ done (PR #9) | `session_manager/lifecycle.rs` |
| 3.6 | MCP tools `session.spawn` + `session.kill` | ✅ done (PR #10) | `mcp/server.rs`, `mcp/request.rs` |
| 3.7 | Acceptance‑тесты с реальным mock_client процессом | ✅ done | `session_manager/acceptance_stage3.rs` |

**Артефакты:**

- `src/session_manager/{env_carrier,spawn,lifecycle}.rs` — реализация spawn/kill/idle.
- `src/session_manager/acceptance_stage3.rs` — e2e‑прогон (WS transport + LifecycleManager + mock_client как реальный процесс).
- `SessionLaunchParamsCarrier` (ADR‑0020) — `EnvCarrier` MVP реализован.

**Acceptance‑сценарии:**

| # | Сценарий | Покрытие |
|---|---|---|
| 1 | Spawn → register → list | `acceptance_stage3::acceptance_spawn_register_list_then_graceful_kill` (e2e через mock_client процесс) + `spawn::tests::spawn_session_succeeds_when_external_actor_registers` |
| 2 | Spawn timeout | `acceptance_stage3::acceptance_spawn_timeout_cleans_registry_and_kills_child` + `spawn::tests::spawn_session_times_out_when_child_never_registers` |
| 3 | Graceful kill | `lifecycle::tests::graceful_kill_sends_session_shutdown_and_kills_after_ack` |
| 4 | Force kill при inflight | `dispatcher::tests::cancel_during_inflight_sends_tool_cancel_then_waits` + `lifecycle::tests::force_kill_removes_session_immediately` |
| 5 | Idle sweep | `acceptance_stage3::acceptance_idle_sweep_kills_idle_session` (e2e) + `lifecycle::tests::idle_sweeper_kills_active_session_past_timeout` |
| 6 | Sirota cleanup | `acceptance_stage3::acceptance_sirota_cleanup_after_external_kill` (внешний SIGKILL → idempotent kill_session) |

---

## Этап 4 — Шаблоны, kinds, `if_exists`, `session.swap` ✅

**Цель.** Декларативные шаблоны spawn'а, singleton kinds, `session.swap`.

**Прогресс:**

| # | Подзадача | Статус | Артефакт / PR |
|---|---|---|---|
| 4.1 | `SpawnTemplate.singleton` + `IfExistsPolicy` enum + request fields | ✅ done | PR #12 |
| 4.2 | Registry kind mismatch check | ✅ done | PR #13 |
| 4.3 | `if_exists` flow в `session.spawn` | ✅ done | PR #14 |
| 4.4 | MCP tool `session.swap` | ✅ done | PR #15 |
| 4.5 | Acceptance‑тесты | ✅ done | `acceptance_stage4.rs` |

**Acceptance:**

| # | Сценарий | Тест |
|---|---|---|
| 1 | `if_exists=fail` блокирует | `acceptance_stage4::acceptance_if_exists_fail_blocks_second_spawn` |
| 2 | `if_exists=reuse` возвращает существующий | `acceptance_stage4::acceptance_if_exists_reuse_returns_same_session` |
| 3 | `if_exists=replace` kills + spawn | `acceptance_stage4::acceptance_if_exists_replace_kills_old_then_spawns_new` |
| 4 | `session.swap` kill_kinds + spawn | `find_active_session_by_kind` + lifecycle::tests::force_kill + spawn::tests (косвенно через MCP tool) |
| 5 | Kind mismatch при register | `acceptance_stage4::acceptance_kind_mismatch_blocks_register` + `registry::tests::register_with_wrong_kind_after_reserve_spawn_returns_mismatch` |

---

## Этап 5 — Доработки `web-transport-addin` (параллельно с 1‑2)

**Цель.** Реальный 1С‑клиент умеет работать с менеджером.

**Артефакты:**

- WS‑tunnel в `src/ws_client.rs`: incoming JSON‑RPC → `mcp::dispatch_request`.
- Авто‑reconnect с экспоненциальным backoff, событие `WS_RECONNECT_STATE` для 1С.
- Метод `ЗапуститьСессионнуюИнтеграцию(URL, ClientUID, Kind, Token?)`.
- Метод `ПолучитьПараметрыСессии()` (ADR‑0020), читающий ENV → params‑file → CLI.
- Проброс `correlation_id` в payload `MCP_TOOL_CALL`.

**Acceptance‑сценарии (черновик):**

1. Подключение и регистрация из 1С‑кода реальной DRIVE‑конфигурации с тестовым общим модулем.
2. Auto‑reconnect: разрыв сети на 5 сек → клиент восстанавливает сессию soft reconnect'ом.
3. `ПолучитьПараметрыСессии()` под Linux/ENV возвращает корректный набор; под Windows/composite — то же.
4. Эмпирическое подтверждение однопоточности `external_event` (риск §9 handoff): два параллельных `tool.call` сериализуются на стороне 1С.

---

## Этап 6 — Интеграция с DRIVE (yaxunit_runner + vanessa_manager smoke)

**Цель.** Первый честный e2e. Q‑R1 закрывается здесь.

**Acceptance‑сценарии (черновик):**

1. *yaxunit smoke.* `session.spawn(template=yaxunit_runner)` поднимает 1cv8c с RunYaXUnit, регистрируется, возвращает session_id; `session.call(session_id, "yaxunit.run_module_tests", {...})` запускает тесты, результат корректен.
2. *VA smoke.* `session.spawn(template=vanessa_manager, overrides={scenarios=…})` поднимает VA‑manager; в `session.list` появляются `vanessa_test_client` сессии по мере прогона; `va.run_status` отдаёт прогресс.
3. *Reconnect под нагрузкой.* Принудительный WS‑drop в середине прогона → grace срабатывает, прогон не падает.
4. *Idle kill VA.* После завершения прогона VA‑manager уходит по idle.

---

## Этап 7 — Observability, correlation_id, финализация ADR

**Цель.** Готовность к передаче.

**Acceptance‑сценарии (черновик):**

1. Все события из §12 спеки эмитятся, видны в JSON‑логах с `correlation_id`.
2. `correlation_id` сквозной: AI‑агент → MCP → menedjer → WS → 1С‑лог.
3. ADR‑0018..0022 переведены в `accepted`. README/ARCHITECTURE синхронизированы.
4. Документация по конфигу `mcp.session_manager.*` — отдельной страницей.

---

## Связи

- Спека: `spec/SESSION_MANAGER.md`.
- Handoff: `spec/HANDOFF.md`.
- ADR: `docs/decisions/0018..0022`.
- Существующий backlog CLI: `spec/IMPLEMENTATION_BACKLOG.md` (не трогаем — другой scope).
