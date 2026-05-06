> **Superseded by [ADR-0034](0034-single-tool-mcp-surface.md).** Управляющие managerские tools (`session_call/session_kill/session_spawn/session_swap`) удалены — менеджер публикует только `session_list` и проксирует тулы клиентов.

# ADR-0030: Inline launch-spec в `session.spawn`; `spawn_templates` как опциональные пресеты

- Статус: `accepted`
- Принято: `2026-04-30` (валидация — smoke #31/#32 этапа 6, см. memory `project_session_manager_stage6.md`)
- Дата: `2026-04-29`

## Контекст

Текущая модель (см. ADR-0020 + spec §5.4 редакция до этой ADR): `session.spawn` принимает `template` и `overrides`, реальные параметры запуска (binary, args, env, startup_command) лежат в YAML-конфиге менеджера в секции `spawn_templates`. Менеджер сам подставляет `$VAR` из overrides и формирует командную строку для `1cv8c`.

Эта модель работает для замкнутого окружения, где менеджер заранее знает все клиентские конфигурации. Реальная картина другая:

1. **Окружения разнятся.** AI-агент может работать на хосте с DRIVE devcontainer'ом, на другом контейнере с УНФ, на удалённом Windows-агенте. Каждый знает свои пути к платформе, к ИБ, к креденшелам.
2. **Менеджер не должен быть универсальным реестром всех возможных конфигов 1С.** Это перегружает его конфиг и заставляет обновлять YAML при каждом новом окружении.
3. **Знание контекста — у вызывающей стороны.** AI-агент видит свою файловую систему, имеет свои переменные среды, понимает какую ИБ хочет дёрнуть.

## Решение

Перевернуть контракт: launch-spec приходит **inline** в самом `session.spawn`. YAML-секция `spawn_templates` остаётся как опциональные пресеты для удобства, но не основной путь.

### Новый payload `session.spawn`

```jsonc
{
  "host_id": "1c-ai-sandbox",
  "kind": "yaxunit_runner",
  "mode": "THIN",                    // optional: THIN | DESIGNER | THICK
  "launch": {
    "binary": "/opt/1cv8/x86_64/8.3.27.2074/1cv8c",
    "args": [
      "ENTERPRISE",
      "/S\"onec-infra\\dssl_drive_ai\"",
      "/N\"AgentAI\"",
      "/P\"AgentAI\""
    ],
    "env": {
      "DISPLAY": ":99"
    },
    "startup_command": "RunYaXUnit;configFile=/path/to/yaxunit.json",
    "extra_args": ["/DisableStartupMessages", "/DisableStartupDialogs"]
  },
  "if_exists": "fail",
  "wait_for_register_ms": 60000,

  // Опционально: использовать пресет из YAML вместо inline launch
  "template": "yaxunit_runner",
  "overrides": { ... }
}
```

### Контракт обработки

1. Если присутствует `launch` — он используется как основной источник параметров. `template` игнорируется (или ошибка, если оба заданы — выбираем strict-mode: ошибка).
2. Если присутствует `template` — менеджер ищет его в `spawn_templates` своего YAML, применяет `overrides`, формирует launch-spec. Это backward-compat с ADR-0020.
3. Если ни `launch`, ни `template` — ошибка `400 BadRequest`.

### Поля `launch`

- `binary` — абсолютный путь к исполняемому файлу. Обязательно.
- `args` — массив аргументов до `/C"..."`. По умолчанию пусто.
- `env` — словарь переменных среды для child-процесса. Сливается с env-родителя (kid наследует, потом overlay из `env`).
- `startup_command` — содержимое `/C"..."` без обрамляющих кавычек. Менеджер сам формирует `"/C\"...\""`. Если пусто — `/C` не добавляется.
- `extra_args` — массив аргументов после `/C` (например, `/DisableStartupMessages`). По умолчанию пусто.

Подстановка параметров протокола (`client_uid`, `correlation_id`, `kind`) делается менеджером поверх — он добавляет `/C"client_uid=<uid>;correlation_id=<id>;kind=<kind>"` либо мерджит в существующий `/C` (см. spec §8.4).

### Безопасность (этап 7)

Inline launch-spec даёт вызывающему AI-агенту возможность запустить произвольный бинарь под user-аккаунтом менеджера / sidecar'а. Митигация — на этапе 7:

- **Allowlist бинарей** в YAML менеджера: `session_manager.spawn.allowed_binaries: [...]`. Запрос с binary вне allowlist → отказ.
- **Запрет `..`** в `binary` и в путях `extra_args`/`env` — sanity check.
- **Auth-token на MCP HTTP** менеджера — отдельная мера (в ADR-0027 уже как открытый вопрос).

В этой ADR явно фиксируем: inline launch-spec + allowlist = security-модель MVP. Без allowlist (default config) — режим «trust your AI-agent».

## Альтернативы

1. **Оставить только template-based, расширить overrides до полного launch-spec.** Тот же эффект через workaround. Хуже читается, требует client-side wrapper'а.
2. **Только inline, без template.** Чище, но ломает существующие тесты этапа 4 и теряет преимущество предсказуемых пресетов для типовых окружений.
3. **Полностью клиент-side spawn (без менеджера).** Это вариант D из обсуждения. Теряет origin tracking (ADR-0028) и singleton-инвариант (ADR-0019/spec §5.4 if_exists logic).

## Следствия

### Положительные

- Менеджер де-факто становится «session coordinator», а конфигурация запуска — ответственностью вызывающего. Чище разделение.
- Один менеджер обслуживает несколько окружений с разными конфигами 1С без обновления YAML.
- `spawn_templates` остаются для консервативных сценариев (CI-pipeline с фиксированной матрицей).

### Отрицательные / стоимость

- Расширение security-поверхности — mitigation через allowlist (этап 7).
- Двойной путь обработки в `session.spawn` (inline vs template) → ветвление кода.
- Документация дольше: нужно объяснить когда что использовать.

### Неграницы

- Не описывает поведение при `template` + `overrides` с конфликтующими полями: priority — overrides побеждают (как и в текущей ADR-0020).
- Не описывает версионирование `launch` schema; для MVP — без version-поля, расширяем JSON-Schema по необходимости.

## Ссылки

- ADR-0020 «SessionLaunchParamsCarrier» — отменяемая часть контракта (template-only).
- ADR-0031 «Dual backend» — потребитель `launch` для LocalBackend и RemoteBackend.
- spec §5.4 («`session.spawn`»), §8.3 («`spawn_templates` теперь опционально»), §8.4.2 (`/C"..."` параметры).
