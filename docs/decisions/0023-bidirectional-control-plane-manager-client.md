# ADR-0023: Bidirectional control‑plane (manager ↔ client) поверх одного WS

- Статус: `accepted`
- Дата: `2026-04-28`

## Контекст

Этап 1 транспорта обрабатывает только направление `client → manager`: `session.register`, `session.heartbeat`, `session.bye`, `session.tools_changed`. Входящие `WireMessage::Response` сейчас игнорируются (`session_manager/transport.rs::dispatch()` явно их `debug!`-логирует и не использует).

Этап 2 (proxy `tool.call`) требует обратного направления: менеджер шлёт клиенту `Request{ id, method:"tool.call", params }`, ждёт `Response{ id, result|error }` от того же клиента. Значит per‑session нужен:

- генератор `Id` для outbound‑запросов (монотонный, не пересекается с client‑Id);
- таблица `pending_outbound: HashMap<Id, oneshot::Sender<Result<Value, JsonRpcError>>>`;
- обработка ветки `WireMessage::Response` в существующем dispatch‑цикле — раутинг по `id` в pending‑таблицу;
- API уровня транспорта: `Connection::send_request(method, params, deadline) -> Result<Value, JsonRpcError>`.

Альтернативы:

1. **Второй WS‑канал** «manager → client» поверх отдельного TCP. Простая mental model, но удваивает количество соединений на одну 1С‑сессию, нагружает однопоточный addin лишним сокетом, ломает соответствие 1‑к‑1 между процессом `1cv8c` и записью реестра (§3 спеки).
2. **Long‑poll**: клиент сам периодически шлёт `tool.poll`, менеджер кладёт следующий вызов в ответ. Латентность +50 мс на каждый round‑trip, плюс заметный overhead на пустой кор‑контур.
3. **Bidirectional на одном WS** с id‑correlation. Соответствует JSON‑RPC 2.0 спецификации (двунаправленное peer-to-peer), не плодит соединений.

## Решение

Bidirectional JSON‑RPC поверх **одного** WS (вариант 3):

1. На каждый WS‑коннект менеджер заводит `Arc<ConnectionHandle>` со следующим состоянием:
   - `outbound_tx: mpsc::UnboundedSender<WireMessage>` — единый канал в writer‑task (правка к нынешнему writer'у не нужна, он уже мультиплексирует).
   - `pending: Mutex<HashMap<Id, oneshot::Sender<Result<Value, JsonRpcError>>>>` — таблица ожидающих ответов.
   - `next_id: AtomicU64` — генератор outbound id'ов в неймспейсе менеджера. Префикс `mgr-<u64>` — гарантирует, что серверные `Id::String` не пересекутся с произвольными клиентскими (например, числовыми или произвольно строковыми).
2. Реестр `SessionRegistry` хранит `Arc<ConnectionHandle>` в `SessionRecord` (поле `connection: Option<Arc<ConnectionHandle>>`; `None` для `Disconnected`-записей).
3. В `transport::dispatch()` ветка `WireMessage::Response` больше не игнорируется: ищет `id` в `pending`, шлёт результат в `oneshot::Sender`. Если `id` неизвестен — `warn!`+drop (нарушение протокола клиентом, но менеджер устойчив).
4. Публичное API:
   ```rust
   impl ConnectionHandle {
       pub async fn call(
           &self,
           method: &str,
           params: serde_json::Value,
           deadline: tokio::time::Instant,
       ) -> Result<serde_json::Value, ConnectionCallError>;
   }
   ```
   `ConnectionCallError` различает: `Timeout`, `Disconnected` (`-32011 session_gone`), `Rejected(JsonRpcError)`, `Cancelled`.
5. На `mark_disconnected` все pending'и завершаются `Disconnected` (одно прокручивание hashmap, очистка таблицы). На soft reconnect — pending пуст по построению (мы только что прошли через mark_disconnected); новый `ConnectionHandle` получает свежий `next_id` и `pending`.
6. Контракт неймспейсов методов:
   - `client → manager`: `session.*` (register/heartbeat/bye/tools_changed) — есть на этапе 1.
   - `manager → client`: `tool.*` (`tool.call`, `tool.cancel`) — детали shape см. в спеке §4.x (обновляется параллельно ADR‑0023) и в JSON Schema.
   - Двусторонний `ping` — обе стороны могут инициировать.

## Следствия

### Положительные

- Соответствует JSON‑RPC 2.0 (peer‑to‑peer), без выдумок над транспортом.
- Один TCP/WS на сессию — соответствует §3 спеки и снижает overhead 1С‑addin'а.
- pending‑таблица и outbound id'ы локальны для коннекта, без глобального state.
- Внятный механизм на disconnect: единая точка drain'а всех ожидающих вызовов с кодом `-32011`.

### Отрицательные / стоимость

- Усложнение transport.rs: новая ветка обработки Response, новая структура `ConnectionHandle`, владение pending‑таблицей с правильным locking'ом.
- Mock‑клиент (`src/bin/mock_client.rs`) обязан уметь принимать `Request` от менеджера и отвечать `Response` — иначе сломаются интеграционные тесты этапа 2.
- `web-transport-addin` (этап 5) должен реализовать обратную сторону: принимать `Request` от менеджера, диспатчить `tool.call` через `external_event`, формировать `Response`. Это уже планировалось, но ADR фиксирует контракт.

### Неграницы

- Не вводит auth‑token (см. ADR‑0022 §6 — токен опциональный, в loopback‑dev отсутствует).
- Не описывает streaming‑responses (`tool.call_progress` и подобные). При первой необходимости — отдельный ADR.
- Не описывает ordering‑guarantees между outbound calls — это задача ADR‑0024 (FIFO).

## Ссылки

- `spec/SESSION_MANAGER.md` §4 «Транспорт и протокол».
- ADR‑0021 (Per‑session FIFO) — потребитель этого механизма.
- ADR‑0022 (Soft reconnect) — pending drain на mark_disconnected согласован с lifecycle сессии.
