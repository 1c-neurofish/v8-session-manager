# ADR-0021: Per‑session FIFO как обязательный инвариант

- Статус: `accepted`
- Дата: `2026-04-28`

> **Update 2026-05-06:** ниже упоминается, что Rust-слой addin шлёт
> `session.heartbeat` в фоне без `external_event`. Это рассуждение
> остаётся актуальным как мотивация — но в текущем runtime application-level
> `session.heartbeat` удалён, и роль liveness, не зависящего от BSL,
> играет WS protocol-level Ping/Pong (RFC 6455): tokio-tungstenite в
> addin отвечает Pong автоматически. Аргумент про неблокирующий liveness
> при длинных inflight tool.call — сохраняется и усиливается.

## Контекст

1С‑клиент обрабатывает входящие `MCP_TOOL_CALL` через однопоточный механизм `external_event` (`web-transport-addin/src/mcp/server.rs`, `src/mcp/addin.rs`). Длительность одного вызова **не ограничена 30 секундами**: `response_timeout` в `mcp_start` (default 30 сек) — это окно ожидания **между прогресс‑сигналами**, а не hard‑limit на tool.call. Пока 1С‑код шлёт сигналы прогресса по `request_id`, таймаут сбрасывается; yaxunit‑прогон или VA‑сценарий на минуты — штатный сценарий (см. `wait_for_response` и тест `wait_for_response_resets_timeout_on_progress` в `server.rs`).

Реальная проблема — **однопоточность диспетчера на стороне 1С**: входящие `external_event` сериализуются. Если в одну 1С‑сессию параллельно прилетают два `tool.call`:

1. Они всё равно будут исполняться по очереди — параллельность мнимая.
2. Progress‑reset работает по конкретному `request_id`; одновременный прогресс двух запросов не предусмотрен текущей реализацией addin'а.
3. Порядок сериализации — недетерминированный, и AI‑агент при автоматизации легко генерирует параллельные вызовы. Без явного контроля получим регулярный «тихий» false‑negative и непредсказуемый порядок.

Существующий ориентир в кодовой базе менеджера — `mcp::edt_session::EdtSessionManager`: семафор capacity=1, FIFO‑очередь с deadline и cancellation routing.

Альтернативы:

- Полагаться на 1С: всё, что прилетит, обработать в порядке поступления. Не работает из‑за однопоточности `external_event`.
- Глобальный лимит concurrency на менеджере. Слишком грубо: блокирует параллельные вызовы в **разные** сессии без необходимости.
- Параллелить через несколько WS‑соединений к одной 1С‑сессии. Не решает первопричину — однопоточный dispatcher на стороне 1С.

## Решение

Per‑session FIFO — обязательный инвариант менеджера:

1. На каждую активную сессию выдаётся ровно один admission slot (семафор capacity=1).
2. Все `tool.call` для этой сессии проходят через FIFO‑очередь.
3. Каждая запись в очереди несёт enqueue‑time deadline (`deadline_ms` из `session.spawn` или глобальный default).
4. Cancellation от MCP (cancellation routing) выкидывает запись из очереди до начала исполнения; running call продолжается до terminal state согласно ADR-0014.
5. Очередь и счётчики (`inflight`, `queue_depth`) видны в `session.list`.
6. `session.kill(force=false)` отказывается, если `inflight > 0`. `force=true` обрывает inflight ошибкой `session_gone`.
7. Tracing‑события: `mcp_session_queue_depth`, `mcp_session_queue_wait`, `mcp_session_call_outcome`.
8. Heartbeat и control‑plane сообщения (`session.register`, `session.tools_changed`, `session.bye`) идут отдельным от FIFO путём:
   - На стороне 1С `session.heartbeat` отправляет **Rust‑слой addin** в фоне, **не дёргая 1С‑код через `external_event`**. Поэтому длинный inflight tool.call не блокирует liveness — менеджер видит сессию живой, даже когда 1С‑код считает минутами.
   - На стороне менеджера control‑plane сообщения обрабатываются вне data‑plane очереди и не конкурируют за admission slot tool.call'ов.
   - Это инвариант: heartbeat, который требует ответа от 1С‑кода, был бы антипаттерном — он переставал бы приходить ровно тогда, когда сессия занята полезной работой.
9. Реализация делается на образце `mcp::edt_session::EdtSessionManager`, переиспользуя его примитивы где возможно.

## Следствия

### Положительные

- Гарантированное отсутствие гонок и потерянных вызовов в одну 1С‑сессию.
- Прозрачное наблюдение очереди через `session.list` и tracing.
- Единый стиль с EDT‑sessions упрощает code review и навигацию.
- Корректное взаимодействие с idle‑детектором (`last_call_at` обновляется только реальным data‑plane вызовом, не control‑plane).

### Отрицательные / стоимость

- Latency: при N параллельных запросах в одну сессию N‑й ждёт `(N‑1) × средний exec_time`. Для VA‑прогонов это ожидаемо; для интерактивных сценариев AI‑агенту видна как очередь в `session.list`.
- Дополнительная сложность в `session.kill`: нужно различать `inflight` и `queued`, корректно завершать оба класса.
- Тесты должны покрыть гонки: cancellation в `queued`, cancellation в `running`, kill во время очереди.
- `deadline_ms` в FIFO‑очереди и `response_timeout` в addin — две разные сущности; нельзя их путать. `deadline_ms` — это deadline всего вызова от менеджера; `response_timeout` — окно ожидания прогресс‑сигнала на стороне addin'а. Документация и комментарии должны это явно разделять.

### Неграницы

- ADR не фиксирует значение default `deadline_ms` (см. §7.2 и §5.4 спеки).
- ADR не описывает логику между сессиями — параллельные вызовы в **разные** сессии не сериализуются.
- Не заменяет MCP execution admission из ADR-0013, а вкладывается в него вторым уровнем.

## Ссылки

- `spec/SESSION_MANAGER.md` §7, §5.5, §6.3, §12.
- ADR-0013 (MCP execution admission).
- ADR-0014 (timeout/cancellation policy).
- `mcp::edt_session::EdtSessionManager`.
