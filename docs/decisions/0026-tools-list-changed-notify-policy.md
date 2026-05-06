# ADR-0026: Политика `tools/list_changed` уведомлений

- Статус: `accepted`
- Дата: `2026-04-28`

## Контекст

После каждого изменения публичного набора client tools AI‑агент должен узнать об этом и пере‑прочитать `tools/list`. Это делается стандартным MCP notification `notifications/tools/list_changed` (`peer.notify_tool_list_changed()` в rmcp 1.2). Capability включается через `ServerCapabilities::enable_tool_list_changed()`.

События, вызывающие изменение публичного набора:

- `session.register` (новые tools могут появиться в `tools/list`);
- `session.bye` или grace‑remove (tools могут исчезнуть, либо измениться выбор round‑robin);
- `session.tools_changed` (клиент переопределил свой набор);
- soft reconnect, если клиент при reconnect указал отличающийся набор tools (см. ADR‑0022 «`tools` обновляются»).

Проблема: при VA‑прогонах эти события идут пачками — VA‑manager поднимает 10‑30 test‑клиентов за 5‑10 секунд. Уведомление AI‑агента **на каждый** register даст 10‑30 ре‑чтений `tools/list` за тот же интервал, каждое — round‑trip + JSON serialization.

Альтернативы:

1. **Notify сразу, без throttling.** Простая логика, но флуд при VA.
2. **Дебаунс по таймеру.** Все события за окно `T` (200 мс кажется разумным) сливаются в одно notification.
3. **Coalescing по эпохе.** Каждое изменение увеличивает счётчик; фоновая задача шлёт notification только когда «есть что отправлять» (epoch_seen < epoch_current). Эквивалентно дебаунсу, но без таймера.
4. **Pull‑модель**: не шлём notification вообще, AI‑агент периодически опрашивает `tools/list`. Антипаттерн в MCP.

## Решение

Дебаунс с окном **200 мс** + coalescing (вариант 2 + элементы 3):

1. **Источник событий — `SessionRegistry`.** При `register`/`mark_disconnected`/`remove`/`update_tools` registry увеличивает `AtomicU64 epoch` и сигналит общий `Arc<Notify>` (`tools_changed_signal`).
2. **Notifier‑task** в MCP‑слое (отдельная tokio‑задача, поднимается при инициализации `McpToolServer`):
   ```text
   loop {
       wait_for_notify(tools_changed_signal).await;
       sleep(200 ms);                              // debounce window
       drain_notify(tools_changed_signal);          // схлопывает все накопленные
       broadcast_to_active_peers(notify_tool_list_changed).await;
   }
   ```
   Окно 200 мс — компромисс: при VA‑прогоне 30 register‑ов за 5 сек дают **~25 уведомлений вместо 30** (мелкая выгода) — но ключевой эффект в другом сценарии: пакетный кластер (например, 10 register'ов в течение 50 мс) сольётся в **одно** уведомление вместо 10. Окно меньше 200 мс не ловит реалистичные кластеры; больше 500 мс — уже заметная задержка появления новых tools для агента.
3. **Capability:** `McpToolServer::get_info()` отдаёт `ServerCapabilities::builder().enable_tools().enable_tool_list_changed().build()`.
4. **Множественные peer'ы.** На HTTP streamable transport может быть несколько MCP‑клиентов одновременно (один AI‑агент = один peer). Решение: notifier держит `Arc<Mutex<Vec<Weak<Peer<RoleServer>>>>>`. Peer регистрируется при первом успешном `list_tools`/`initialize` (взять из `RequestContext::peer`), `Weak` чтобы не держать висящие peer'ы. На notify — пробежать вектор, на каждом `weak.upgrade()`, на nil‑upgrade — компактификация. Это типовой fanout‑pattern.
5. **stdio transport.** Один peer на процесс, простая `Option<Peer>`; та же логика, размер `Vec` ≤ 1.
6. **Failure handling.** Если `peer.notify_tool_list_changed()` возвращает ошибку (peer disconnected, transport closed) — пометить `Weak` как dead в next pass. Не ретраить, не логировать на error‑level (это нормальный сценарий при отключении агента).

## Следствия

### Положительные

- Кластерные события (VA‑burst, reconnect storms) сливаются в одно уведомление.
- 200 мс — приемлемая задержка появления новых tools для агента; не воспринимается как лаг.
- Отдельная задача с понятной ответственностью; не подмешиваем notify в горячий путь register/bye.

### Отрицательные / стоимость

- 200 мс задержка — единичная regression‑правка появится в `tools/list` не мгновенно. Для UX MCP‑клиентов это незаметно, но фиксируем как известный trade‑off.
- Хранение `Vec<Weak<Peer>>` требует thread‑safe компактификации (раз в N итераций или при notify).
- Усложняет smoke‑дебаг: для воспроизведения «вижу tool сразу» придётся ждать debounce окно.

### Неграницы

- Не описывает `notifications/resources/list_changed` или `notifications/prompts/list_changed` — эти контуры на этапе 2 не вводятся (resources/prompts от 1С‑клиента в спеке упомянуты как опциональные, но нашему MVP не нужны).
- Не описывает push отдельных `tools/updated` событий — стандарт MCP такого не вводит, мы тоже не вводим.
- Не вводит throttling на источнике (`SessionRegistry`); registry просто сигналит. Decoupling чище.

## Параметры

- **Дебаунс‑окно:** 200 мс. Не выносится в config на этапе 2 (на 7‑м, observability, при необходимости — в `mcp.session_manager.tools_list_changed_debounce_ms`).
- **Capability:** `enable_tool_list_changed()`.

## Ссылки

- `spec/SESSION_MANAGER.md` §4 «MCP surface».
- ADR‑0019 (Дедупликация) — изменения дедупликации тоже триггерят events.
- ADR‑0022 (Soft reconnect) — на reconnect tools могут смениться.
- ADR‑0025 (ClientProxy publication) — что именно перечитывает агент.
- rmcp 1.2: `ServerCapabilities::enable_tool_list_changed()`, `Peer::notify_tool_list_changed()`.
