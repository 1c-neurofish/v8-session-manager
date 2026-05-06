# Handoff: переход в контекст DRIVE

> Назначение: документ для свежей сессии Claude Code, запущенной из `/workspaces/work/repos/1C Projects/DSSL DRIVE` с `--add-dir` к этому репо и к `web-transport-addin`. Восстанавливает контекст обсуждения, в котором была написана `SESSION_MANAGER.md`, фиксирует принятый режим работы и стартовые шаги.
>
> Перед началом работы: прочитать в указанном порядке — `spec/SESSION_MANAGER.md`, `ARCHITECTURE.md` (выборочно), этот файл, `web-transport-addin/AGENT.md`, `web-transport-addin/docs/{mcp,ws,http}.md`.

## 1. Что мы делаем

Превращаем `v8-runner` (Rust CLI + MCP сервер для операций над 1С‑конфигурацией) в **менеджер клиентских сессий веб‑сокет‑клиентов**. Параллельно дорабатываем `web-transport-addin` — 1С‑расширение на Rust, которое играет роль клиента менеджера.

Целевая картина:

```
AI‑агент ──MCP(stdio|http)──▶ Менеджер ──WS JSON‑RPC tunnel──▶ 1cv8c #1..N
                                  │                              │
                                  ├── Server tools (как сейчас)  ├── 1С‑код через addin
                                  ├── Management tools           │   регистрирует свои tools
                                  └── Client tools (proxy)       │   в локальном MCP, addin
                                                                 │   проксирует их по WS
```

Подробно — `spec/SESSION_MANAGER.md`. Этот документ — единственный источник истины по проектированию.

## 2. Точно где какой код

| Что | Где |
|---|---|
| Менеджер сессий (этот репо) | `/workspaces/work/repos/1C Framework/v8-client-session-manager/` |
| 1С‑расширение (Rust + .bsl) | `/workspaces/work/repos/1C Framework/web-transport-addin/` |
| Тестовый стенд (DRIVE) | `/workspaces/work/repos/1C Projects/DSSL DRIVE/` (cwd новой сессии) |
| VA framework (для шаблона `vanessa_manager`) | `<framework_repos>/vanessa-automation/dist/vanessa-automation/` (см. `.claude/skills/vanessa-run/SKILL.md` в DRIVE) |

## 3. Зафиксированные архитектурные решения

Всё это уже отражено в `SESSION_MANAGER.md` соответствующими разделами. Ниже — выжимка для быстрого восстановления контекста; **детали и обоснования читать в спеке**.

| # | Решение | Раздел спеки |
|---|---|---|
| 1 | Старые MCP tools раннера остаются. Делятся на Server (`build_project`, `dump_config`, `check_syntax_*`) и ClientLocal (`launch_app`, `run_*_tests`). К ним добавляются Management (`session.*`) и ClientProxy (динамические). | §5.1, §14.2 |
| 2 | Транспорт addin↔manager — **WS‑tunnel**: один двунаправленный JSON‑RPC канал и для control‑plane (register/heartbeat/list_changed), и для data‑plane (`tool.call`). Локальный HTTP MCP в addin для нашего сценария не используется. | §4 |
| 3 | `client_uid` приходит от 1С‑кода в `session.register`. Менеджер использует его как `session_id`, при коллизии активной — отказ; при коллизии с `Disconnected` — soft reconnect. | §4.6, §6.4 |
| 4 | Дедупликация client tools: однотипные регистрации схлопываются в один публичный tool `<kind>__<tool_name>`. AI‑агент адресует сессию параметром `session_id` (опционален при единственном кандидате, обязателен при двух+). Для разных схем — раздельные tools с дизамбигуирующим суффиксом. | §5.3 |
| 5 | Per‑session FIFO обязателен (1С обрабатывает `external_event` однопоточно с дефолт 30s timeout). Реализация по образцу `mcp::edt_session::EdtSessionManager`. | §7 |
| 6 | Idle‑таймаут — 30 мин по последнему `tool.call`, настраивается. Heartbeat — 15 сек, настраивается. RAC‑интеграция отложена. | §6.3, §1 non‑goals |
| 7 | `session.spawn` с `if_exists: fail | reuse | replace`. Singleton kinds: `yaxunit_runner`, `vanessa_manager`. `vanessa_test_client` запускается VA‑manager'ом сам, не публикуется в `tools/list` (`publish_named_tools=false`). | §5.4, §8.1 |
| 8 | `session.swap` — best‑effort, без отката при падении spawn нового. | §5.6 |
| 9 | Resources/prompts pass‑through на уровне протокола с MVP, дедупликация ленивая до первого реального use‑case'а. | §10.2 |
| 10 | Кросс‑платформенная передача параметров запускаемому 1cv8c — trait `SessionLaunchParamsCarrier` с `EnvCarrier` (Unix default), `ParamsFileCarrier`, `CompositeCarrier` (Windows default). 1С‑код читает через единый `ПолучитьПараметрыСессии()` в addin. | §8.4 |
| 11 | Динамические tools публикуются через MCP `notifications/tools/list_changed`. Несовместимым клиентам — fallback `session.list` + `session.call`. | §5.2, §10.1 |
| 12 | Менеджер ephemeral — реестр сессий не персистится. Клиенты переподключаются и регистрируются заново. | §1 non‑goals |
| 13 | WS‑сервер — отдельный `axum::Server` на отдельном порту (не делит state с MCP HTTP). Default `0.0.0.0:4000`. | §11, §14.1 |

### Открытые вопросы (закрываются при реализации)

- **Q‑R1.** Формат `extra_args`/`startup_command` под Linux/Windows — закрыть при реализации первого реального шаблона `yaxunit_runner` (этап 6).
- **Q‑R2.** Переиспользование `use_cases::launch_app` в spawn — определить на этапе 3.
- **Q‑R3.** Дедупликация resources/prompts — при первом 1С use‑case'е.

## 4. Принятый режим работы

Полное обсуждение — в исходной переписке (потеряна при переходе). Краткая выжимка:

1. **PR per stage**, не один большой. Этапов 7 (см. §15 спеки), частота merge ≈ раз в 3‑5 дней.
2. **ADR на каждое нетривиальное решение** в `docs/decisions/`. Запланированные ADR (черновики писать **до** имплементации, мерджить после ревью):
   - `WS‑tunnel вместо HTTP back‑connect`
   - `Дедупликация client tools по (kind, name, schema_hash)`
   - `SessionLaunchParamsCarrier как абстракция доставки параметров`
   - `Per‑session FIFO как обязательный инвариант`
   - `Soft reconnect по client_uid`
3. **Mock‑клиент в `tests/mock_client/`** — Rust‑бинарь, эмулирует addin для e2e тестов менеджера без 1С. Закрывает ~80% сценариев и должен появиться в этапе 1.
4. **JSON Schema протокола** в `spec/protocol/` — выпустить параллельно с этапом 1, гонять валидацию в CI на отправляемых/принимаемых сообщениях.
5. **Acceptance‑чеклист** в каждом PR — конкретные «Дано/Когда/Тогда», не «работает». Образцы — в моём ответе перед handoff'ом, в новой сессии воспроизвести по ходу.
6. **Smoke на DRIVE** — на этапах 3, 5, 6 минимум. С наличием доступа к DRIVE‑MCP (yaxunit‑runner, 1c‑mcp, lsp‑bsl‑bridge, 1c‑platform‑context) — гонять самостоятельно, ставя пользователю только утверждать ADR и решения уровня контракта.
7. **TaskCreate на каждый этап** — сразу после старта новой сессии.

## 5. Этапы внедрения (из §15 спеки)

1. WS‑transport + protocol + registry (без spawn). Mock‑клиент. `session.list`.
2. Прокси `tool.call` через WS, per‑session FIFO, list_changed, ClientProxy tools, `session.call`.
3. Spawn/kill: `session.spawn` поверх `launch_app` с удержанием `Child`, `session.kill` (graceful + force), idle‑sweeper.
4. Шаблоны и kinds, `if_exists`, `session.swap`, classification.
5. Доработки в `web-transport-addin`: tunnel‑режим, авто‑reconnect, client_uid, `ПолучитьПараметрыСессии()`. Параллельно с этапами 1‑2.
6. Интеграция с DRIVE: spawn‑шаблоны для `yaxunit_runner` и `vanessa_manager`, smoke по реальным сценариям. **Это первый честный e2e**, ожидаются сюрпризы.
7. Observability/correlation_id, документация и ADR финализация.

## 6. Что нужно сделать первым делом в новой сессии

1. Прочитать в порядке: `spec/SESSION_MANAGER.md` → этот файл (`spec/HANDOFF.md`) → `ARCHITECTURE.md` (только разделы про MCP boundary, command boundary, MCP execution policy) → `web-transport-addin/AGENT.md` + `docs/{mcp,ws,http}.md`.
2. Завести `spec/IMPLEMENTATION_BACKLOG.md` — таблица «этап → acceptance‑сценарии → smoke‑команда → owner». Образцы acceptance‑сценариев для этапа 2 уже накиданы в исходной переписке (мой ответ «Acceptance‑сценарии вместо "работает"»). Восстановить по памяти/смыслу.
3. Создать TaskCreate‑задачи на 7 этапов, статус `pending`, и стартовать этап 1 (`in_progress`).
4. Написать **черновики ADR** для всех 5 заявленных решений в `docs/decisions/`. Каждый — 1 страница (контекст / решение / следствия). Прислать пользователю на ревью **до** того, как написана хоть строчка кода менеджера.
5. После апрува ADR — стартовать этап 1: WS‑сервер, протокол, registry, mock‑клиент, JSON Schema. PR в feature‑ветке. Не мерджить до зелёного CI и подтверждения пользователем.

## 7. Что доступно в DRIVE‑окружении

**MCP‑серверы (см. `/workspaces/work/repos/1C Projects/DSSL DRIVE/.mcp.json`)**:

- `yaxunit-runner` — критичен для acceptance этапа 6.
- `1c-mcp`, `1c-copilot-proxy`, `1c-log-checker` — работа с конфигурацией и логами.
- `lsp-bsl-bridge` — BSL LSP при правке `.bsl` обвязки.
- `1c-platfom-context` — контекст платформы для написания `.bsl`.
- `chrome-devtools`, `time`, `infostart-kb` — utility.

**Платформа 1С**: `/opt/1cv8/x86_64/8.3.27.2074/1cv8c`, `vrunner`, `ibcmd`. Linux + Xvfb (`DISPLAY=:99`).

**Skills DRIVE**: `.claude/skills/vanessa-run/SKILL.md`, `.claude/skills/vanessa-diagnostics/SKILL.md`, `.claude/skills/vanessa-authoring/SKILL.md`.

**Конфиг тестового стенда**: `configs/yaxunit-runner.yml` (connection string, user, password, platform‑version).

## 8. Что мне (новой сессии) делать НЕ автоматически, а спросить пользователя

1. **Изменения в конфигурации DRIVE** (новые общие модули, обработчики `ВнешнееСобытие`, подключение нашего расширения к рабочей конфигурации). До первого такого вмешательства — спросить и предупредить, что одной кнопкой не откатить.
2. **Любые `git push`, `gh pr create`** в чужие репозитории.
3. **Изменения внешнего MCP‑контракта менеджера** (новые management tools, переименования, изменения формата `session.list`). Это влияет на AI‑агентов, которые могут уже использоваться.
4. **Force kill 1С‑процессов**, не порождённых самим менеджером. Например, если в DRIVE уже есть запущенный 1cv8c — не убивать без спроса.
5. **Решение «реальное 1С ведёт себя не так, как в spec»** — приходить с двумя‑тремя вариантами обхода, выбор за пользователем.

## 9. Риски, на которые сразу закладываем буфер

- **Addin × 1С — чёрный ящик в части `external_event`**. На этапе 5 могут вылезти сюрпризы (таймауты, потеря событий, кодировки). Закладываем +неделю на этом этапе.
- **VA‑менеджер не загрузит наше расширение**, или загрузит, но не вызовет стартовый код. Точка входа не решена. Если VA не даёт hook — искать через плагин VA или `БДДРаннер` hook. Это блокер этапа 6, надо решать заранее, желательно одновременно с этапом 5.
- **Параллелизм в 1С**. Допущение «однопоточно» — эмпирически подтвердить на этапе 5 простым тестом (два mock‑агентских вызова в одну сессию, посмотреть на сериализацию).
- **Windows‑совместимость** `ParamsFileCarrier` — без Windows‑машины подтверждения нет; помечаем как «to be validated».

## 10. Пользовательский контекст

- Email: `gbig_opus@yahoo.com`
- Задача — инфраструктурная для команды, занимающейся тестированием 1С‑конфигурации DRIVE через AI‑агентов. Менеджер сессий — необходимое звено для work‑flow «агент запускает yaxunit/VA и взаимодействует с тестовым клиентом».
- Стиль работы: пользователь хорошо ориентируется в архитектурных решениях, ценит чёткие развилки и конкретные acceptance‑критерии. Не любит решения «на авось». Просит фиксировать решения в документах (отсюда `SESSION_MANAGER.md` и этот handoff).
- Язык переписки — русский, документы — русский, код‑комментарии — на усмотрение, но идентификаторы и API — английский.

## 11. Memory новой сессии

Текущая `memory/` директория этого проекта пуста. **При первом контакте в новой сессии** записать:

- `user_role.md`: разработчик инфраструктуры тестирования 1С через AI‑агентов; работает на Linux + Xvfb (DRIVE devcontainer).
- `feedback_explicit_decisions.md`: пользователь предпочитает зафиксированные решения в `.md`‑документах вместо «договорённостей в чате»; ADR‑first перед кодом.
- `project_session_manager.md`: ссылка на `spec/SESSION_MANAGER.md` как источник истины + статус (этап 1 в работе, остальные `pending`).

## 12. Команда старта

В новой сессии Claude Code, запущенной как:

```bash
cd "/workspaces/work/repos/1C Projects/DSSL DRIVE"
claude \
  --add-dir "/workspaces/work/repos/1C Framework/v8-client-session-manager" \
  --add-dir "/workspaces/work/repos/1C Framework/web-transport-addin"
```

первое сообщение пользователя предполагается такое:

> Прочитай `/workspaces/work/repos/1C Framework/v8-client-session-manager/spec/SESSION_MANAGER.md` и `spec/HANDOFF.md`. Создай `IMPLEMENTATION_BACKLOG.md`, заведи задачи на 7 этапов, напиши черновики ADR из §4 handoff'а и принеси на ревью. Этап 1 не стартуй до апрува ADR.
