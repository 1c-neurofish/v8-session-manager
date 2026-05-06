# ADR-0024: Per‑session dispatcher и его lifecycle

- Статус: `accepted`
- Дата: `2026-04-28`

## Контекст

ADR‑0021 фиксирует инвариант «1 admission slot на сессию». Этот ADR описывает, **где** хранится диспетчер, **как** он живёт через состояния `Active`/`Disconnected`/`Gone`, и **что** происходит с queued/inflight вызовами при разрывах.

Образец, на который опираемся, уже есть в проекте: `src/mcp/edt_session.rs::EdtSessionManager`. Там реализованы:

- `Arc<Semaphore>` (capacity = 1 для shared EDT, для нас — capacity = 1 на каждую сессию);
- `Mutex<VecDeque<Arc<QueuedRequest>>>` + `Notify` (queue_ready);
- состояние per‑request с `OwnedSemaphorePermit` (`RequestState::queued(permit)`), позволяющим освободить слот без race'ов при cancellation/timeout;
- `EdtSessionError` со внятным набором terminal‑состояний (`QueueFull`, `CancelledWhileQueued`, `TimedOutWhileQueued`, `DrainedByRestartOrShutdown` и т.п.).

На уровне MCP в `EdtSessionManager` диспетчер один — он делит shared EDT между всеми вызовами. У нас каждая 1С‑сессия имеет **свой** диспетчер, и его lifecycle привязан к жизни записи в `SessionRegistry`.

## Решение

1. Новая сущность `SessionDispatcher`, ровно один на запись `SessionRecord`:
   ```rust
   pub struct SessionDispatcher {
       admission: Arc<Semaphore>,        // capacity = 1
       queue: Mutex<VecDeque<Arc<QueuedCall>>>,
       queue_ready: Notify,
       inflight: AtomicU32,
       telemetry: Arc<DispatcherTelemetry>,
   }
   ```
   Хранится как `Arc<SessionDispatcher>` в `SessionRecord` (поле `dispatcher`). Создаётся при `register()`/soft‑reconnect, шарится между `Active` и `Disconnected` состояниями (см. п.5).

2. Публичное API диспетчера:
   ```rust
   pub async fn enqueue(
       &self,
       call: ToolCallRequest,
       deadline: tokio::time::Instant,
       cancellation: CancellationToken,
   ) -> Result<ToolCallResponse, DispatcherError>;
   ```
   Внутри:
   - получаем `OwnedSemaphorePermit` (admission);
   - переходим из `Queued` в `Running`, `inflight += 1`, `last_call_at = now`;
   - дёргаем `ConnectionHandle::call("tool.call", ...)` — этот вызов уже async и не блокирует тред;
   - после terminal — `inflight -= 1`, освобождаем permit.

3. Cancellation routing (по ADR‑0021 и спеке §7):
   - Cancel **до** старта (запись ещё в очереди) — выкидываем из `VecDeque`, возвращаем `DispatcherError::CancelledWhileQueued`. Permit не брался — отпускать нечего.
   - Cancel **во время** inflight — шлём клиенту `tool.cancel(id)` через тот же `ConnectionHandle`, **но локально продолжаем ждать terminal** от `ConnectionHandle::call`. Это сознательная гарантия §7.2 спеки: «running call продолжается до terminal state». Клиент сам решает, ответить ли `JsonRpcError{-32800, "cancelled"}` или результат — оба валидны.

4. Telemetry (готовится к этапу 7, но события появляются здесь):
   - `mcp_session_queue_depth{action: enqueue|drain, depth}`,
   - `mcp_session_queue_wait{ms}`,
   - `mcp_session_call_outcome{outcome: ok|cancelled|timeout|error}`.

5. Lifecycle относительно состояний `SessionRecord`:
   - `Active` — диспетчер принимает новые `enqueue`. inflight может быть > 0.
   - `mark_disconnected` — запись переходит в `Disconnected`, `connection = None`. Диспетчер **не уничтожается**: его inflight (если есть) уже завершается ошибкой `-32011 session_gone` через цепочку `ConnectionHandle::drain_pending`. Queue полностью дренируется тем же кодом ошибки. Новые `enqueue` после `Disconnected` отвергаются немедленно (`DispatcherError::SessionGone`) — это согласуется с ADR‑0022 «новые `tool.call` к такой сессии немедленно возвращают `session_gone`, а не висят».
   - `soft reconnect` — записи возвращается состояние `Active`, выдаётся новый `ConnectionHandle`; диспетчер тот же (admission/queue/inflight reset, поскольку драйнились в момент disconnect). Reconnect не восстанавливает результаты ранее inflight‑вызовов — AI‑агент должен повторить (тоже зафиксировано в ADR‑0022 §7).
   - `remove` (после grace timeout / `session.bye` / kill) — `Drop` диспетчера, освобождение Semaphore.

6. **Где живёт состояние.** Диспетчер — owned by реестр (`Arc` внутри `SessionRecord`); внешний код берёт `Arc<SessionDispatcher>` через `SessionRegistry::get_dispatcher(session_id)`. Это позволяет proxy‑роутеру дёргать диспетчер, не зная деталей реестра.

7. **Что НЕ входит в этот ADR**: глобальные лимиты concurrency на менеджер (есть `concurrency_limit` в `McpToolServer`, он остаётся), бюджет на queue‑capacity per session (на этапе 2 — unbounded, c добавлением telemetry; ограничение появится отдельным ADR при фактической потребности).

## Следствия

### Положительные

- Один владелец per‑session FIFO — реестр. Никаких параллельных state‑machine'ов.
- Соответствует §6 спеки и ADR‑0021/0022 без затратной координации между транспортом и диспетчером.
- Поведение при soft reconnect однозначно: новые вызовы — да, восстановление inflight — нет.

### Отрицательные / стоимость

- Аккуратная работа с `OwnedSemaphorePermit` при cancellation/timeout — паттерн нетривиальный (см. EdtSessionManager `release_queued`/`release_running`), требует ревью.
- На каждую сессию создаётся отдельный `Notify`+`Semaphore`. На сотнях параллельных сессий ничего страшного, но overhead non‑zero.

### Неграницы

- Не описывает `if_exists=reuse` semantics (ADR будущего этапа 4).
- Не описывает идемпотентность `tool.call` (если AI‑агент повторил вызов из‑за reconnect) — это контракт уровня агента, не диспетчера.

## Ссылки

- `spec/SESSION_MANAGER.md` §6 «Lifecycle сессии», §7 «Per‑session FIFO».
- ADR‑0021 (Per‑session FIFO как обязательный инвариант) — мотивация.
- ADR‑0022 (Soft reconnect) — поведение диспетчера на disconnect/reconnect.
- ADR‑0023 (Bidirectional control‑plane) — диспетчер дёргает `ConnectionHandle::call`.
- `src/mcp/edt_session.rs` — образец реализации очереди и admission‑slot.
