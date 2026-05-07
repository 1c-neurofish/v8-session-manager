## 4. Стратегия решения

Архитектура построена вокруг общего `Arc<SessionRegistry>`, который шарится между двумя транспортами.

Ключевые решения и целевые контракты:

- **Один бинарь, два транспорта.** WS для входящих 1С-клиентов и MCP HTTP для AI-агентов делят процесс, конфиг и реестр сессий. См. ADR-0018 (WS вместо HTTP back-connect).
- **Bidirectional control plane поверх одного WS.** `client → manager` (`session.register`, `tools/publish`) и `manager → client` (`tools/list_changed` и т.п.) идут через тот же сокет; back-connect не используется. См. ADR-0023.
- **Per-session FIFO как обязательный инвариант.** На каждую сессию — `SessionDispatcher` с последовательной очередью tool-вызовов. См. ADR-0021, ADR-0024.
- **Soft-reconnect по `client_uid`.** Сессия живёт через краткие потери WS; `generation` защищает от гонок свежего коннекта и обработки старого `mark_disconnected`. См. ADR-0022.
- **Минимальная MCP-поверхность.** Менеджер публикует только `session_list` плюс проксированные тулы клиентов. Никакого `session.spawn/kill/call/swap` (ADR-0034).
- **Дедупликация тулов.** Одинаковые тулы от разных клиентов сводятся в один публичный по триплету `(kind, name, schema_hash)`; конфликты по схеме скрываются с предупреждением. См. ADR-0019.
- **Liveness через WS Ping/Pong.** Не application-level: модальный диалог 1С — не «зависание». См. STACK_OVERVIEW §Liveness.
- **Stateless по диску.** Менеджер не хранит persistent state; рестарт = чистый реестр, клиенты переподключаются заново.
- **Transport-agnostic registry.** `SessionRegistry`, `SessionDispatcher`, `router` не знают о том, кто их вызывает (WS handler vs MCP HTTP tool dispatch); вся логика — над общим реестром.
