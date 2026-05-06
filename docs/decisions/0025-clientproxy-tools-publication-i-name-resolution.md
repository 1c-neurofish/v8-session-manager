# ADR-0025: ClientProxy tools — публикация в `tools/list` и резолвинг имени

- Статус: `accepted`
- Дата: `2026-04-28`

## Контекст

Спека §4 говорит: AI‑агенту в `tools/list` менеджер показывает не только server tools (`build_project`, `dump_config`, …), но и tools, зарегистрированные клиентами через `session.register{...,tools:[...]}`. ADR‑0019 фиксирует дедупликацию по `(kind, name, schema_hash)`. Спека §4: «Если у этого публичного tool в реестре менеджера сейчас одна сессия — параметр `session_id` опционален».

Текущая интеграция MCP‑server'а (`src/mcp/server.rs`) использует rmcp макрос `#[tool_router]`, дающий **статический** набор tools. Для динамики (tools клиентов появляются/исчезают на register/bye/tools_changed) нужна другая схема.

Дополнительно — ADR‑0019 разрешает деление по schema_hash, но не описывает, что делает менеджер при **конфликте** (одинаковый `(kind, name)`, разный schema_hash от двух клиентов). Сейчас это open question.

## Решение

1. **Что публикуется в `tools/list`** (порядок проверки на каждый именованный tool из registry):
   - Server tools (`build_project`, `run_all_tests`, `dump_config`, …) — без изменений, через rmcp `ToolRouter`. Скоп `Server`.
   - Management tools — `session.list` (этап 1), `session.call` (этап 2 — см. п.5). Скоп `Management`.
   - ClientProxy tools — динамически из `SessionRegistry`. Скоп `ClientProxy`. Имя: **`<kind>__<tool_name>`** (двойное подчёркивание; разрешённый символ в JSON identifier'ах). Описание/schema берутся у первого «канонического» дескриптора в группе.

2. **Дедупликация (ADR‑0019)**:
   - Группа: `(kind, tool_name)`.
   - Подгруппа: `(kind, tool_name, schema_hash)`. Hash — sha256 от канонически отсортированного JSON `input_schema`.
   - В пределах подгруппы — все сессии считаются «equivalent providers» этого tool.
   - В пределах группы (но между подгруппами) **conflict** — см. п.3.

3. **Конфликт schema (одинаковые `(kind, tool_name)`, разные schema_hash)**:
   - Tool **скрывается** из `tools/list` (не публикуется). В audit‑log событие `proxy_tool_hidden{kind, name, sessions:[...], reason:"schema_conflict"}`.
   - Доступ к tool возможен **только** через явный `session.call(session_id, tool, args)` — там клиент уже выбран по `session_id`, и schema конкретной сессии однозначна.
   - Альтернатива «суффикс с коротким хешом» (`client__echo__a3f1`) была рассмотрена: даёт дискаверабельность, но усложняет UX («что выбрать?»), и требует политики устаревания short‑hash при добавлении 3‑й сессии. Скрытие проще и безопаснее, в случае реальной потребности — добавим суффикс отдельным ADR.

4. **Резолвинг при `tools/call(<kind>__<tool>, args)` от AI‑агента**:
   - Если в группе `(kind, tool_name)` ровно одна Active‑сессия → `session_id` resolved к ней автоматически.
   - Если несколько Active‑сессий с одинаковым schema_hash → менеджер выбирает по **round‑robin** среди них (последовательный счётчик per‑group, modulo набор Active). Это даёт справедливое распределение между равнозначными клиентами без необходимости агенту что‑либо знать.
   - Если несколько подгрупп (schema_hash различается) → tool скрыт, см. п.3 (этот путь не срабатывает).
   - Если ни одной Active‑сессии (все ушли в Disconnected/Gone к моменту вызова) → ошибка `-32011 session_gone` с подсказкой «session list is empty for this kind».

5. **Management tool `session.call`**:
   - Подпись: `session.call(session_id: String, tool: String, arguments: Value, deadline_ms?: u64) -> CallToolResult`.
   - Всегда работает, даже если `<kind>__<tool>` скрыт из `tools/list` или kind помечен `publish_named_tools=false`.
   - Это «runaround» для нестандартных кейсов и для kind'ов вроде `vanessa_test_client` (см. §8.1 спеки), где named publication отключена.

6. **Реализация в rmcp**:
   - `#[tool_router]` остаётся для server + management tools.
   - В `ServerHandler` руками переопределяем:
     ```rust
     fn list_tools(&self, ...) -> ... {
         let mut base = self.tool_router.list_tools(...);
         base.extend(client_proxy_tools(&self.session_registry));
         base
     }
     fn call_tool(&self, request, ctx) -> ... {
         if is_client_proxy_name(&request.name) {
             return self.proxy_call(request, ctx).await;
         }
         self.tool_router.call_tool(request, ctx).await
     }
     fn get_tool(&self, name: &str) -> Option<Tool> {
         self.tool_router.get_tool(name).or_else(|| client_proxy_get(&self.session_registry, name))
     }
     ```
   - Это снимает зависимость от макроса `#[tool_handler]` (который автоматически делегирует всё в роутер); приходится написать `ServerHandler` руками. Ущерб локализован одним файлом.

7. **Per‑kind флаг `publish_named_tools`** (§8.1 спеки):
   - `client`, `yaxunit_runner`, `vanessa_manager` → `true`.
   - `vanessa_test_client` → `false` (доступ только через `session.call` или дочерние tools VA‑manager'а).
   - При `false` все tools этого kind **не** попадают в named‑publication, но остаются доступными через `session.call`.

## Следствия

### Положительные

- Внятный contract: AI‑агент видит «один tool — один способ его позвать», без двусмысленности при конфликте схем.
- Round‑robin даёт нагрузочную справедливость без явного знания агента о топологии.
- `session.call` — universal escape hatch и базис для VA‑прогонов с десятками test‑клиентов.

### Отрицательные / стоимость

- При конфликте schema_hash AI‑агент в `tools/list` **не увидит** tool вообще (только через `session.list`+`session.call`). Это сознательный выбор: лучше «не вижу» чем «вижу неоднозначность». Документировать в спеке §4.
- Кастомная имплементация `ServerHandler` вместо `#[tool_handler]` — поддержка: при апгрейде rmcp может потребоваться ручная сверка с новыми методами trait.
- Round‑robin счётчик per group требует атомарности; thread‑safe реализация не сложна, но non‑zero state в роутере.

### Неграницы

- Не описывает приоритезацию сессий («какой именно client выбрать») — только round‑robin. Стратегии типа least‑inflight оставлены на будущее.
- Не описывает per‑session quotas (rate‑limit к одному client'у) — отдельный ADR при потребности.
- Не вводит `tools/list_changed` (см. ADR‑0026).

## Ссылки

- `spec/SESSION_MANAGER.md` §4 «MCP surface», §8 «Kinds и шаблоны».
- ADR‑0019 (Дедупликация client tools).
- ADR‑0023 (Bidirectional) — proxy‑роутер дёргает `ConnectionHandle::call` через диспетчер.
- ADR‑0024 (Per‑session dispatcher) — диспетчер выбранной сессии.
