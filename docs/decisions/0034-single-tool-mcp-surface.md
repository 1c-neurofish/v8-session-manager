# ADR-0034: Минимальная MCP-поверхность менеджера — только `session_list` плюс проксированные тулы клиентов

- Статус: accepted
- Дата: 2026-05-06
- Контекст задачи: extraction (см. `tasks/session-manager-extraction/`)

## Контекст

В предыдущих итерациях менеджер публиковал в `tools/list` свой набор управляющих тулов:

- `session_list` — список активных сессий;
- `session_call` — вызов tool в указанной сессии;
- `session_kill` — корректное закрытие/убийство сессии;
- `session_spawn` — порождение новой сессии (через LocalBackend / RemoteBackend, см. ADR-0030, ADR-0031);
- `session_swap` — атомарная замена сессии на новую инкарнацию.

Параллельно менеджер уже публиковал тулы клиентов как top-level `<prefix>__<tool>` (ADR-0025). Это давало два конкурирующих способа дотянуться до той же логики:

1. через managerский `session_call("<session_id>", "<tool>", args)`;
2. через `<prefix>__<tool>(args)` (адресация по prefix).

В реальной интеграции с DRIVE это выявилось как анти-паттерн:

- `session_call/session_kill/session_swap` дублируют то, что и так выражается проксированием — клиенту 1С нет нужды экспонировать свои тулы дважды.
- `session_spawn` смешивал два уровня (manager как aggregator vs. manager как orchestrator). Управление жизненным циклом 1С-клиента — забота внешнего оркестратора (BSL-расширение `client_mcp` в DRIVE, deploy-скрипт, IDE-плагин), а не менеджера. Решения о том, какой `1cv8c` запускать, какие `/C`-параметры подкладывать и кто гасит зомби-процесс, лежат в зоне ответственности оркестратора, а не агрегатора.
- Каждый из управляющих тулов тянул собственный JSON Schema, валидаторы, обработчики ошибок, тесты. Для AI-агента это шум в `tools/list`, который никогда не используется напрямую.

## Решение

Менеджер публикует в `tools/list` ровно **один встроенный tool**:

- `session_list` — read-only сводка по активным сессиям (id, prefix, last_call_at, inflight, generation).

Всё остальное — это **проксированные тулы клиентов** в виде top-level `<session_prefix>__<tool>` с адресацией через `SessionDispatcher` (ADR-0024, ADR-0025).

Удалены: `session_call`, `session_kill`, `session_spawn`, `session_swap` и связанные `McpSessionCallRequest / McpSessionKillRequest / McpSessionSpawnRequest / McpSessionSwapRequest / SpawnSpecInput / IbSpecInput / LaunchSpecInput / IfExistsPolicy`.

### Управление жизненным циклом сессии — снаружи менеджера

- **Запуск 1С-клиента:** обязанность внешнего оркестратора. Клиент стартует с параметром `mcpMode=ws` и адресом менеджера; BSL-расширение `client_mcp` со стороны 1С регистрируется через WS.
- **Закрытие сессии:** клиент отправляет `SESSION_BYE` и закрывается (idle-timeout / kill процесса). Менеджер гарантирует только корректное удаление записи из реестра по generation.
- **Reconnect:** soft-reconnect по `client_uid` (ADR-0022) — без участия менеджера-tool'ов.
- **Health/idle:** на стороне менеджера — встроенный idle-sweeper (ADR-0028, обновлён); таймауты конфигурируются.

## Последствия

### Положительные

- AI-агент видит в `tools/list` только то, что реально полезно: проксированные тулы клиентов + `session_list` для отладки/наблюдаемости.
- Чёткое разграничение слоёв: менеджер = aggregator/registry/dispatcher; orchestrator = lifecycle owner.
- Минус ~2000 LOC в `src/mcp/server.rs` и связанных файлах; меньше входных JSON Schema → быстрее холодный старт rmcp.
- Тестов меньше, но покрытие осмысленнее: исчезают тесты «менеджер сам себе клиент».

### Отрицательные / риски

- Внешние интеграции, опиравшиеся на `session_call/session_kill/session_spawn/session_swap`, теперь должны:
  - (a) переключиться на top-level `<prefix>__<tool>` для бизнес-вызовов;
  - (b) самостоятельно управлять lifecycle 1C-клиента.
- В DRIVE/`client_mcp` BSL-расширении надо подтвердить, что соответствующие сценарии не зависят от старого managerского API. На момент принятия ADR — подтверждено только для smoke (DRIVE Linux→Linux).

## Связанные решения

- ADR-0025 — публикация client tools и резолвинг имени (остаётся базой).
- ADR-0024 — per-session dispatcher и lifecycle (остаётся базой).
- Superseded by this ADR: ADR-0027 (`system_capability` vs `mcp_tools` — оба слоя сводятся к одному), ADR-0030 (inline launch-spec в `session.spawn` — нет такого tool), ADR-0031 (dual backend spawn/kill — нет такого tool).
- Частично переосмысливает ADR-0028 (origin tracking): в idle-sweeper фильтр по `SessionOrigin` снят, sweeper применяется ко всем сессиям единообразно.

## Проверка

- `tools/list` менеджера на свежем менеджере без клиентов: `[session_list]`.
- При регистрации одного клиента c `prefix=test_client` и 18 тулами: `tools/list` = `session_list + 18× test_client__*` (smoke прошёл).
- Любая попытка вызвать `session_call/session_kill/session_spawn/session_swap` через MCP HTTP возвращает «method not found» — управляющие тулы не зарегистрированы.
