# ADR-0028: Origin tracking сессий и scope idle-sweeper'а

- Статус: `accepted`
- Принято: `2026-04-30` (валидация — smoke #31/#32 этапа 6, см. memory `project_session_manager_stage6.md`)
- Дата: `2026-04-29`

## Контекст

Менеджер обслуживает два класса сессий с принципиально разным жизненным циклом:

1. **Manager-spawned** — менеджер запросил у LocalBackend / RemoteBackend (см. ADR-0031) запуск процесса, удерживает PID, отвечает за уход (idle-kill, timeout, ошибка регистрации).
2. **Walk-in / self-registered** — клиент пришёл сам, менеджер о его существовании узнаёт только из `session.register`. Это интерактивные клиенты, которых поднял пользователь, и sidecar'ы, поднятые setup-скриптом окружения.

Текущий `idle-sweeper` (ADR-0022, lifecycle.rs) применяет idle-timeout ко всем сессиям подряд. Для walk-in клиентов это неправильно: пользователь не ожидает, что его интерактивный 1С-клиент закроется по причине «менеджер посчитал тебя idle». Особенно болезненно для sidecar-клиентов, которые специально живут долго.

## Решение

Каждая запись в `SessionRegistry` несёт явный `origin`:

```rust
pub enum SessionOrigin {
    /// Menager initiated this session via reserve_spawn → spawn (Local/Remote backend).
    /// PID known. Subject to idle-sweep, spawn-timeout, force-kill on timeout.
    ManagerSpawned,
    /// Client connected and registered without prior reservation.
    /// Walk-in: interactive, sidecar, manually started.
    /// NOT subject to idle-sweep. Force-kill only on explicit session.kill.
    SelfRegistered,
}
```

### Где проставляется

- `reserve_spawn(uid, kind)` создаёт `SessionRecord` с `origin = ManagerSpawned`. На последующем `register` от того же uid — origin сохраняется.
- `register` без предварительной reservation (uid не найден в `Spawning`-state) → создаётся новая запись с `origin = SelfRegistered`.

Отличается **только** наличие предшествующей reservation. Поведение `register` в остальном идентично.

### Где читается

- **`idle_sweeper`** в `lifecycle.rs`: фильтр `origin == ManagerSpawned`. Walk-in клиенты не сканируются.
- **`session.list`** опционально показывает `origin` в выводе (полезно для диагностики).
- **Метрики:** `mcp_session_register{ origin }` — для observability на этапе 7.

### Force-kill через `session.kill`

`session.kill` работает для **обоих** origin одинаково (это явное действие AI-агента, не автоматическое). Отказы в kill walk-in'у быть не должно — пользователь имеет право убить любую сессию явно.

## Альтернативы

1. **Origin не вводить, фильтровать idle-sweep по наличию PID.** Сейчас walk-in тоже передают PID (см. ADR-0029), фильтр развалится.
2. **Origin не вводить, отдельный TTL на walk-in.** Усложняет конфиг и не отражает реального инварианта (walk-in нужно НЕ убивать, а не убивать по другому таймауту).
3. **Origin как boolean `manager_spawned: bool`.** Эквивалентно, но enum читается лучше и расширяется (добавится `SystemDaemon` — для long-living системных процессов в будущем).

## Следствия

### Положительные

- Walk-in клиенты живут до явного `session.bye` или собственной смерти. Никакой неожиданной пропажи.
- Sidecar-клиенты (см. ADR-0031) корректно работают как long-living infrastructure без дополнительной конфигурации.
- Чёткая ответственность: ManagerSpawned → менеджер чистит; SelfRegistered → инициатор отвечает за жизненный цикл.

### Отрицательные / стоимость

- +1 поле в `SessionRecord`, миграция тестов.
- Дополнительный путь в `register`: «нашёл reservation» vs «не нашёл — walk-in».

### Неграницы

- ADR-0028 не вводит origin для дочерних сессий VA-manager'а (`vanessa_test_client`). Они приходят как `SelfRegistered`, потому что менеджер их не спавнит — спавнит VA-manager сам. Это согласуется с моделью.
- Не описывает поведение `session.swap` относительно origin — это уточняется в задаче #39.

## Ссылки

- ADR-0021 «Per-session FIFO» — registry-инварианты.
- ADR-0022 «Soft reconnect» — `register` flow.
- ADR-0031 «Dual backend» — sidecar'ы как `SelfRegistered`.
- spec §5.4, §11 (idle-sweeper).
