> **Superseded by [ADR-0034](0034-single-tool-mcp-surface.md).** Управляющие managerские tools (`session_call/session_kill/session_spawn/session_swap`) удалены — менеджер публикует только `session_list` и проксирует тулы клиентов.

# ADR-0031: Двойной backend исполнения spawn/kill — `LocalBackend` + `RemoteBackend`; kill matrix

- Статус: `accepted`
- Принято: `2026-04-30` (валидация — smoke #31/#32 этапа 6, см. memory `project_session_manager_stage6.md`)
- Дата: `2026-04-29`

## Контекст

Менеджер сессий должен уметь запускать и убивать 1С-клиенты в произвольных окружениях. Возможные окружения:

1. **Хост менеджера** — внутри того же контейнера/машины, где работает менеджер. Доступ к ОС-API (fork/exec, kill PID) есть напрямую.
2. **Другой контейнер / удалённый сервер** — нет прямого доступа к ОС, но если в этом окружении есть хотя бы один зарегистрированный клиент с `capability=spawn` (см. ADR-0029), менеджер может попросить его выполнить spawn/kill локально для своего окружения.

Если выбрать только первый путь — менеджер ограничен одним host'ом, что не соответствует архитектурной цели «один coordinator на N окружений». Если только второй (всё через sidecar) — bootstrap-проблема: первый клиент в host'е менеджера тоже надо как-то запустить, но sidecar'а ещё нет.

## Решение

Менеджер выставляет наружу один MCP-tool `session.spawn` (и `session.kill`), но внутри маршрутизирует исполнение между двумя реализациями `SpawnBackend` / `KillBackend`:

```rust
trait SpawnBackend {
    async fn spawn(&self, spec: LaunchSpec, expected_uid: ClientUid) -> Result<Pid>;
}

trait KillBackend {
    async fn kill(&self, pid: Pid, force: bool) -> Result<()>;
}
```

### Backends

**`LocalBackend`** — использует ОС-API напрямую:

- spawn: `tokio::process::Command::new(spec.binary).args(...).envs(...).spawn()` (существующий код `spawn.rs` этапа 3).
- kill: `nix::sys::signal::kill(Pid, SIGTERM)` → grace 2s → `SIGKILL` на Linux; `OpenProcess + TerminateProcess` на Windows (существующий код `lifecycle.rs` этапа 3).
- Активен **только** для `host_id == manager.host_id`.

**`RemoteBackend`** — обёртка над `system_capability` слоем (см. ADR-0027):

- spawn: находит в registry активный `SessionRecord` с `host_id == target_host` и `capabilities` содержит `"spawn"`. Шлёт ему `addin.spawn{ launch_spec, expected_uid }` через WS, ждёт `addin.spawn_result{ pid }`.
- kill: аналогично, через `addin.kill{ pid, force }`.
- Активен для любых `host_id != manager.host_id`.

### Алгоритм маршрутизации `session.spawn(host_id, ...)`

```
если host_id == manager.host_id:
    backend = LocalBackend
иначе:
    spawner = registry.find_active(host_id, capability="spawn")
    если spawner отсутствует:
        return Error::NoSpawnerInHost { host_id }
    backend = RemoteBackend(spawner_session_id)
reservation = registry.reserve_spawn(expected_uid, kind, origin=ManagerSpawned)
pid = backend.spawn(launch_spec, expected_uid).await?
reservation.set_pid(pid)
register_record = registry.wait_register(expected_uid, wait_for_register_ms).await?
return SessionSpawned { session_id, kind, host_id, pid, registered_at }
```

При timeout:

```
if pid_known:
    backend.kill(pid, force=true).await   // лучшее усилие, не ретраим
registry.cancel_reservation(reservation)
return Error::SpawnRegisterTimeout
```

### Bootstrap первого клиента

Менеджер **не отвечает** за bootstrap первого клиента в host'е менеджера. Это ответственность инициатора окружения (devcontainer postStart, CI setup, ручной запуск AI-агентом через shell). LocalBackend всегда доступен на host'е менеджера, поэтому курино-яичной проблемы нет: bootstrap для своего host'а делается через `LocalBackend.spawn` напрямую через MCP-вызов AI-агента; для чужого host'а — bootstrap делает агент в том окружении (никакого `RemoteBackend.spawn` без существующего sidecar'а).

### Kill matrix

Первая попытка ВСЕГДА — `session.shutdown` по WS целевому клиенту (graceful, его собственный addin делает `Завершить()`). Force / zombie path подключается только если grace истёк или WS уже мёртв:

| Состояние клиента | host_id == manager | Force/zombie path |
|---|---|---|
| Жив, WS работает | — | graceful по WS, force не нужен |
| ManagerSpawned, PID известен | да | LocalBackend.kill(pid) |
| ManagerSpawned, PID известен | нет | RemoteBackend.kill(pid) через sidecar в host'е target |
| SelfRegistered, PID + host_id из register | да | LocalBackend.kill(pid) |
| SelfRegistered, PID + host_id из register | нет | RemoteBackend.kill(pid) через sidecar в host'е target |
| sidecar отсутствует и host_id != manager.host_id | нет | mark dead + close WS, орфан фиксируется в `WARN: orphan suspected pid=... host_id=...` |

PID для LocalBackend верифицируется (см. ADR-0029, опционально). PID для RemoteBackend верифицируется самим sidecar'ом — addin.kill отказывает если процесс не дочерний для самого 1cv8c с компонентой (см. spec §5.7).

### Самовнимание (selfkill)

Если sidecar = target (попытка убить себя), менеджер не идёт через RemoteBackend. Вместо этого — graceful по WS (`session.shutdown`); если grace истёк — закрывает WS, помечает dead. Force-self-kill через PID не предлагает: нет смысла просить процесс убить себя по PID при живом WS-канале к нему же.

## Альтернативы

1. **Только LocalBackend.** Менеджер ограничен своим host'ом. Не соответствует архитектурной цели multi-environment.
2. **Только RemoteBackend (через sidecar).** Bootstrap-проблема для первого клиента в host'е менеджера; вынуждает вводить «coupon-fallback» (отвергнут на этапе обсуждения).
3. **Pluggable backend через config.** Преждевременная абстракция; концептуально достаточно двух фиксированных backend'ов.
4. **Force-kill через коллатеральный канал (ssh/docker exec).** Требует credentials на сторону менеджера; не масштабируется. Отвергнуто.

## Следствия

### Положительные

- Менеджер обслуживает произвольное число окружений при наличии хотя бы одного клиента-sidecar'а в каждом.
- Reuse существующего кода этапа 3 (`spawn.rs`, `lifecycle.rs`) для LocalBackend — миграция небольшая.
- Чёткие границы ответственности: bootstrap — инициатор окружения, координация — менеджер.

### Отрицательные / стоимость

- Дополнительный код роутинга + обёртка `RemoteBackend` над WS-каналом.
- Force-kill в чужом host'е без sidecar'а невозможен — орфаны на совести инициатора. Документируется явно.
- Тесты: matrix-style (LocalBackend × ManagerSpawned/SelfRegistered, RemoteBackend × ManagerSpawned/SelfRegistered, Refusal cases).

### Неграницы

- Не описывает реализацию `addin.spawn`/`addin.kill` со стороны addin'а — это задача #38.
- Не описывает observability метрик для каждого backend'а (`mcp_session_spawn{backend, outcome}`) — этап 7.
- Не описывает retry-policy при transient ошибках RemoteBackend — MVP без retry, ошибка пробрасывается AI-агенту.

## Ссылки

- ADR-0027 «System capability vs MCP tools» — слой `addin.*`, который использует RemoteBackend.
- ADR-0028 «Session origin tracking» — поле `origin` для kill matrix.
- ADR-0029 «host_id + pid + capabilities» — данные для маршрутизации.
- ADR-0030 «Inline launch-spec» — `launch_spec`, который backend исполняет.
- spec §5.4 (`session.spawn` алгоритм), §5.5 («Kill matrix»), §5.7 (`addin.*` контракт).
