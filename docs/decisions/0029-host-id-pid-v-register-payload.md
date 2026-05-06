# ADR-0029: `host_id` + `pid` + `capabilities` в `session.register`

- Статус: `accepted`
- Принято: `2026-04-30` (валидация — smoke #31/#32 этапа 6, см. memory `project_session_manager_stage6.md`)
- Дата: `2026-04-29`

## Контекст

Менеджер должен уметь:

1. Различать клиентов по host'у, в котором они работают (один devcontainer, другой devcontainer, удалённый Windows-агент). Это критично для маршрутизации `session.spawn` (см. ADR-0031): если запрос на spawn в host_id == manager.host_id, менеджер сам запускает процесс; если в чужой — ищет sidecar в этом host_id.
2. Знать PID каждой зарегистрированной сессии для force-kill в случае, когда WS оторвался, а процесс ещё живой.
3. Понимать, какие capabilities умеет отдельно взятый клиент. Сейчас все компоненты `web-transport-addin` идентичны, но при будущей эволюции возможны разные версии в окружении.

Текущий payload `session.register` несёт только `client_uid`, `kind`, `version`, `tools`. Этого недостаточно для всего вышеуказанного.

## Решение

Расширить payload `session.register`:

```jsonc
{
  "method": "session.register",
  "params": {
    "client_uid": "uuid",
    "kind": "client" | "yaxunit_runner" | "vanessa_manager" | "vanessa_test_client" | "spawner" | ...,
    "version": "1.0",
    "tools": [...],

    // НОВЫЕ ОБЯЗАТЕЛЬНЫЕ
    "host_id": "1c-ai-sandbox",
    "pid": 12345,

    // НОВОЕ ОПЦИОНАЛЬНОЕ
    "capabilities": ["spawn", "kill"]
  }
}
```

### `host_id` — источники (со стороны addin)

В порядке приоритета:

1. ENV `V8_HOST_ID` — явный override. Используется когда несколько окружений делят hostname (host network mode), либо нужна логическая группировка.
2. Linux: `gethostname()` (читает `/etc/hostname`). В контейнере = container name.
3. Windows: переменная `COMPUTERNAME`.

`host_id` определяется один раз при инициализации addin (или при первом `register`) и не меняется в течение жизни процесса.

### Менеджер: свой `host_id`

Менеджер при старте читает свой `host_id` тем же способом и держит как константу. Логирует `mcp_session_manager_started{ host_id }` для диагностики.

В нашем случае: devcontainer DRIVE = `1c-ai-sandbox` (см. ADR-0032).

### `pid` — `std::process::id()`

Тривиально: `std::process::id()` в Rust. Аддин загружен в адресное пространство `1cv8c`, поэтому возвращаемое значение — PID самого 1cv8c-процесса. Подделать значение со стороны клиента-приложения нельзя без компрометации компоненты.

### `capabilities` — текущий и будущий набор

Всегда поддерживаемые в актуальной сборке `web-transport-addin`:

- `spawn` — addin умеет запустить дочерний `1cv8c` через `addin.spawn{ launch_spec }`.
- `kill` — addin умеет убить процесс по PID через `addin.kill{ pid, force }`.

Поле обязательно как массив; пустой массив — клиент не предоставляет system_capability (только участвует как target sessions). Менеджер при выборе sidecar'а фильтрует регистрационные записи по требуемой capability.

### Дополнительно: PID-верификация (опциональная, default off)

Walk-in клиент может соврать про PID/host_id. Если устранять этот класс угрозы:

- Linux: при `host_id == manager.host_id` менеджер читает `/proc/<pid>/comm` или `/proc/<pid>/cmdline`, проверяет наличие `1cv8c`/`thinclient` в имени.
- Windows: `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `QueryFullProcessImageName`.

Управляется feature-flag в конфиге менеджера: `session_manager.verify_register_pid: true|false`. По умолчанию `false` — доверяем addin'у. Включение — этап 7 (security/observability).

## Альтернативы

1. **`host_id` через `/C"host_id=..."` от инициатора.** Хрупко: legacy launch flows этот ключ не передают. Лучше пусть addin сам определяет — детерминированный единый источник.
2. **PID не передавать, ходить за ним только при kill.** Невозможно: при оторванном WS sidecar в чужом host_id уже не дотянется к target — менеджер должен иметь PID до ситуации kill, а не во время.
3. **`capabilities` как struct с детальными полями (`spawn: bool, kill: bool, ...`).** Лишняя многословность, сейчас всегда оба или ни одного. Массив строк — расширяемее.

## Следствия

### Положительные

- Все предусловия для ADR-0031 (dual backend) удовлетворены: менеджер знает host_id и PID каждой сессии.
- Force-kill работает по PID без дополнительных round-trip'ов.
- Forward compatibility: разные версии addin регистрируются с разными `capabilities`, менеджер маршрутизирует осознанно.

### Отрицательные / стоимость

- Изменение протокола `session.register` — minor, но требует синхронной правки addin (#37) и менеджера (#39).
- Серверная сторона должна корректно обрабатывать отсутствие `capabilities` (старый клиент = пустой набор).

### Неграницы

- Не описывает endpoint для динамической смены `capabilities`. Если addin позже отключил `kill` (например, по конфигу) — нужен `notifications/capabilities_changed` или аналог; вне scope этой ADR (этап 7).
- Не описывает PID-верификацию подробно — флаг есть, реализация — отдельная задача в этапе 7.

## Ссылки

- ADR-0027 «System capability vs MCP tools» — `capabilities` определяет участие в system_capability слое.
- ADR-0031 «Dual backend» — потребитель `host_id` и `pid`.
- ADR-0032 «Manager native deploy» — host_id менеджера в нашей среде.
- spec §5.4 (`session.register` payload), §8.4 (delivery параметров).
