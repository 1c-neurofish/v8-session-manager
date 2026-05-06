# ADR-0032: Менеджер сессий нативно в основном devcontainer

- Статус: `accepted`
- Дата: `2026-04-29`

## Контекст

Этап 6.8 контейнеризовал менеджер в отдельный Docker-контейнер `v8-session-mgr` (192.168.0.50 в сети `infra`). Это упростило production-deploy, но создало проблему для ADR-0031 (LocalBackend):

1. В контейнере `v8-session-mgr` нет платформы 1С — это `debian:bookworm-slim` + Rust-бинарь.
2. Даже если бы платформа была — клиенты, спавненые внутри изолированного контейнера, не имеют доступа к хостовой DRIVE-базе (`onec-infra\dssl_drive_ai`), к user-кэшу `~/.1cv8/`, к DISPLAY для UI.
3. Cross-container spawn через docker-API из Rust-кода менеджера — отдельная инфра-задача с credentials, не соответствует MVP-цели.

Поэтому LocalBackend в контейнерном деплое бесполезен, остаётся только RemoteBackend через sidecar — что превращает bootstrap первого клиента в нетривиальную задачу.

## Решение

Менеджер сессий разворачивается **нативно** в основном devcontainer'е разработческого окружения — `1c-ai-sandbox` (IP 192.168.0.10 в сети `infra`). `host_id` менеджера = `1c-ai-sandbox`.

### Установка

- Бинарь: `~/.local/bin/v8-runner` (симлинк на `target/release/v8-runner` из репозитория `v8-client-session-manager`).
- Конфиг: `~/.config/v8-runner/v8project.yaml` с секцией `mcp.session_manager` (bind 0.0.0.0:4000, /sessions) и `mcp.http` (bind 0.0.0.0:4001, /mcp).
- Управление через скрипты в `~/.local/bin/`:
  - `v8-session-mgr-start.sh` — идемпотентный запуск через `nohup`, PID в `/tmp/v8-session-mgr.pid`, логи `/tmp/v8-session-mgr.log`.
  - `v8-session-mgr-stop.sh` — SIGTERM → grace 2s → SIGKILL.
  - `v8-session-mgr-status.sh` — проверка PID + tail логов.

### Сетевая модель

Bind на `0.0.0.0` оба порта — менеджер доступен:

- Из самого devcontainer'а: `ws://127.0.0.1:4000/sessions`, `http://127.0.0.1:4001/mcp`.
- Из других контейнеров сети `infra`: `ws://1c-ai-sandbox:4000/sessions` (DNS) или `ws://192.168.0.10:4000/sessions` (IP).
- С хоста разработчика: через VS Code port forward, без жёсткой привязки.

### Жизненный цикл

Менеджер живёт всё время существования devcontainer'а. Между перезапусками devcontainer'а — поднимается вручную через `v8-session-mgr-start.sh` (автоматизация через `postStartCommand` — отдельная задача, см. «Открытые вопросы»).

### Что демонтировано

- Контейнер `v8-session-mgr` остановлен и удалён.
- Volume `v8-client-session-manager_v8-session-mgr-data` удалён (старые actions.log не нужны).
- `docker-compose.yml` репозитория менеджера остаётся как production-deploy reference, но помечается комментарием как «не для local dev».

## Альтернативы

1. **Оставить менеджер в контейнере + добавить hostmount + DBus.** Серьёзная инфра-работа, ломает изоляцию контейнеров, привязывает к версии платформы.
2. **Менеджер на хосте разработчика (вне devcontainer'а).** Сложнее интеграция с remote-Cursor / VS Code remote — менеджер должен быть видим оттуда же, где живёт DRIVE-thinclient.
3. **Менеджер как user-systemd unit внутри devcontainer'а.** В нашем devcontainer'е нет полноценного init (`systemctl --user` без `systemd-logind` не работает гарантированно). nohup-обёртка проще и работает везде.

## Следствия

### Положительные

- LocalBackend полностью функционален — менеджер сам спавнит `1cv8c` с доступом к ИБ, DISPLAY, ExtCompT-кэшу.
- Нет необходимости bootstrap'ить sidecar для smoke-тестов на этапе 6.6.
- Sole source of truth: один менеджер, один host_id, прямой доступ ко всему.

### Отрицательные / стоимость

- Production-deploy ≠ dev-deploy. Это ожидаемо для MVP, но фиксируется как технический долг: production-режим (контейнер) после ADR-0027/0029/0031 потребует sidecar в окружении 1С.
- Нет автостарта — между ребилдом devcontainer'а менеджер придётся поднимать руками.

### Неграницы

- ADR не описывает production-deploy после реализации sidecar в `web-transport-addin` (#37/#38). Когда sidecar готов, контейнерный режим возвращается с двумя host_id: `v8-session-mgr` (менеджер) + `1c-ai-sandbox` (sidecar). LocalBackend в контейнере остаётся бесполезным, RemoteBackend используется для всего.
- Не описывает миграцию обратно (если потребуется откатиться к контейнерному dev-deploy) — обратное действие тривиально по тем же шагам в обратном порядке.

## Открытые вопросы

- **Автостарт.** В репозитории не нашлось `.devcontainer/devcontainer.json` (devcontainer-конфигурация управляется извне). После того как путь к devcontainer-конфигурации найдётся — добавить `postStartCommand: "/home/vscode/.local/bin/v8-session-mgr-start.sh"`. До этого — ручной запуск.

## Ссылки

- ADR-0029 «host_id + pid» — менеджер при старте читает свой `host_id` тем же способом, что addin.
- ADR-0031 «Dual backend» — почему LocalBackend требует совмещённого хоста.
- Память агента: `project_session_manager_stage6.md` — раздел «Активные ресурсы».
- spec §8.3 (deploy options).
