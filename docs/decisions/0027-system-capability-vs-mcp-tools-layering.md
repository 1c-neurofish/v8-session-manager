> **Superseded by [ADR-0034](0034-single-tool-mcp-surface.md).** Управляющие managerские tools (`session_call/session_kill/session_spawn/session_swap`) удалены — менеджер публикует только `session_list` и проксирует тулы клиентов.

# ADR-0027: Двухслойный API менеджера — `system_capability` vs `mcp_tools`

- Статус: `superseded` (см. парные ADR-0003 в `onec-client-mcp-devkit` и ADR-0005 в `web-transport-addin`)
- Изначально принято: `2026-04-30`
- Superseded: `2026-05-05` — вместо отдельного слоя `system_capability` в Rust addin spawn/kill живут как обычные MCP-tools (`system_spawn_1c_client` / `system_kill_pid`) в прикладном расширении test_client; маршрутизация менеджера идёт по имени tool в каталоге сессии, отдельный механизм `capabilities` упразднён.
- Дата: `2026-04-29`

## Контекст

Менеджер сессий вырастает в двух направлениях одновременно:

1. **Наружу к AI-агенту** — публикует MCP-каталог (management-tools, server-tools, проксированные клиентские tools).
2. **Внутрь к 1С-клиентам** — расширенный контракт для удалённого spawn/kill процессов через addin-side (см. ADR-0029, ADR-0031).

Без явного разделения этих слоёв возникает соблазн: либо дать AI-агенту прямой вызов `addin.spawn` (нарушение изоляции — агент не должен думать про процессы 1С), либо сделать `addin.spawn` обычным MCP-tool в каталоге менеджера (тогда `tools/list` загрязняется внутренней механикой).

## Решение

Два независимых API-слоя с разной аудиторией и разной плоскостью транспорта.

### Слой `system_capability` (внутренний)

Контракт менеджер↔addin поверх уже установленного WS-канала к клиенту с `capabilities = ["spawn", "kill"]`. **Не публикуется** в MCP-каталоге наружу.

Минимальный набор методов:

| Метод | Направление | Назначение |
|---|---|---|
| `addin.spawn{ launch_spec, expected_uid, correlation_id? }` | manager → addin | Запустить дочерний `1cv8c` процесс на хосте sidecar'а |
| `addin.spawn_result{ pid }` или `addin.spawn_error{ code, message }` | addin → manager | Результат |
| `addin.kill{ pid, force }` | manager → addin | Убить процесс по PID |
| `addin.kill_result{ ok }` | addin → manager | Результат |
| `addin.child_exited{ pid, code, signal? }` (notification) | addin → manager | Дочерний процесс завершился — менеджер чистит registry |

JSON-RPC envelope тот же, что у существующих `session.*` сообщений (см. ADR-0023 «Bidirectional control plane»).

### Слой `mcp_tools` (внешний)

Каталог, который видит AI-агент через MCP HTTP / stdio:

- **management:** `session.spawn`, `session.kill`, `session.list`, `session.call`, `session.swap`
- **server-tools:** `build_project`, `dump_config`, `check_syntax_*`, `launch_app`, `run_all_tests`, `run_module_tests`
- **proxy от клиентов:** агрегация из `register.tools` живых сессий по правилам ADR-0019/0025

`addin.*` методы здесь **никогда** не появляются. Их использует менеджер изнутри при обработке `session.spawn`/`session.kill` для удалённых host'ов (см. ADR-0031).

### Tools-publishing protocol (двунаправленный)

Стандартный MCP-flow в обе стороны:

1. Клиент → менеджер:
   - Initial set — в payload `session.register{ tools: [...] }`.
   - Update — клиент шлёт `notifications/tools/list_changed`; менеджер дёргает `tools/list` к этому клиенту, обновляет registry.
2. Менеджер → AI-агент:
   - Уже реализовано через `notifications/tools/list_changed` поверх MCP-канала менеджера (см. ADR-0026).
   - Любое изменение клиентского реестра прозрачно превращается в notification наружу (с дебаунс-окном из ADR-0026).

## Следствия

### Положительные

- AI-агент видит чистый каталог из бизнес-tools, без процессного контроля. Меньше поверхность атаки.
- `system_capability` развивается независимо: добавление новых методов (например, `addin.health`, `addin.list_children`) не ломает MCP-каталог.
- Тестируемость: каждый слой — отдельный набор контрактных тестов.

### Отрицательные / стоимость

- Два слоя протокола = два набора схем, два набора enum-кодов ошибок, два места для документации.
- Менеджер становится «тонким посредником» для удалённого spawn — добавляется код роутинга (см. ADR-0031).

### Неграницы

- ADR-0027 не описывает реализацию `addin.spawn`/`addin.kill` со стороны addin'а — это ADR-0031 (backend) и задачи #38 (web-transport-addin spawn/kill capability).
- Не описывает security-модель доступа к `system_capability` (auth-token, allowlist бинарей) — этап 7.

## Ссылки

- ADR-0023 «Bidirectional control plane» — JSON-RPC envelope.
- ADR-0026 «tools/list_changed notify policy» — debounce наружу.
- ADR-0029 «Host identity & PID protocol» — `capabilities` в `register`.
- ADR-0031 «Dual backend & kill matrix» — кто использует `addin.*`.
- spec §5.7 (новый) «Внутренний слой `system_capability`».
