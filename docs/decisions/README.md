# Архитектурные решения (ADR)

Этот каталог хранит архитектурные решения проекта в формате ADR.

> **Watershed: ADR-0033 (extraction).** Часть ранних ADR-0001..0017 описывает CLI-форка `v8-runner`, от которого этот проект отделён. Они помечены как `superseded by ADR-0033` и сохраняются в репо как исторический след — на новый код они уже не действуют.

## Актуальные решения (session-manager)

- [ADR-0034: Минимальная MCP-поверхность менеджера — только `session_list` плюс проксированные тулы клиентов](0034-single-tool-mcp-surface.md) — `accepted`, `2026-05-06`
- [ADR-0033: Отделить v8-session-manager от форка v8-runner](0033-extract-v8-session-manager-from-v8-runner.md) — `accepted`, `2026-05-06`
- [ADR-0032: Менеджер сессий нативно в основном devcontainer](0032-manager-natively-v-osnovnom-devcontainer.md) — `accepted`, `2026-04-29`
- [ADR-0029: `host_id` + `pid` + `capabilities` в `session.register`](0029-host-id-pid-v-register-payload.md) — `proposed`, `2026-04-29`
- [ADR-0028: Origin tracking сессий и scope idle-sweeper'а](0028-session-origin-tracking.md) — `proposed`, `2026-04-29` *(обновлено ADR-0034: фильтр по origin в idle-sweeper снят)*
- [ADR-0026: Политика `tools/list_changed` уведомлений](0026-tools-list-changed-notify-policy.md) — `accepted`, `2026-04-28`
- [ADR-0025: ClientProxy tools — публикация в `tools/list` и резолвинг имени](0025-clientproxy-tools-publication-i-name-resolution.md) — `accepted`, `2026-04-28`
- [ADR-0024: Per‑session dispatcher и его lifecycle](0024-per-session-dispatcher-i-lifecycle.md) — `accepted`, `2026-04-28`
- [ADR-0023: Bidirectional control‑plane (manager ↔ client) поверх одного WS](0023-bidirectional-control-plane-manager-client.md) — `accepted`, `2026-04-28`
- [ADR-0022: Soft reconnect по `client_uid`](0022-soft-reconnect-po-client-uid.md) — `accepted`, `2026-04-28`
- [ADR-0021: Per‑session FIFO как обязательный инвариант](0021-per-session-fifo-kak-obyazatelnyy-invariant.md) — `accepted`, `2026-04-28`
- [ADR-0020: Доставка `manager_url` через константу расширения, опциональные `/C`-параметры](0020-sessionlaunchparamscarrier-kak-abstraktsiya-dostavki-parametrov.md) — `accepted`, `2026-04-28`
- [ADR-0019: Дедупликация client tools по `(kind, name, schema_hash)`](0019-deduplikatsiya-client-tools-po-kind-name-schema-hash.md) — `accepted`, `2026-04-28`
- [ADR-0018: WS-tunnel вместо HTTP back-connect](0018-ws-tunnel-vmesto-http-back-connect.md) — `accepted`, `2026-04-28`

## Superseded решения (managerские tools для spawn/kill/call/swap)

- [ADR-0031: Двойной backend исполнения spawn/kill — `LocalBackend` + `RemoteBackend`; kill matrix](0031-dual-backend-local-remote-i-kill-matrix.md) — `superseded by ADR-0034`
- [ADR-0030: Inline launch-spec в `session.spawn`; `spawn_templates` как опциональные пресеты](0030-inline-launch-spec-v-session-spawn.md) — `superseded by ADR-0034`
- [ADR-0027: Двухслойный API менеджера — `system_capability` vs `mcp_tools`](0027-system-capability-vs-mcp-tools-layering.md) — `superseded by ADR-0034`

## Частично актуальные решения (часть пунктов снята, часть остаётся)

- [ADR-0014: Единая timeout/cancellation policy для CLI и MCP команд](0014-edinaya-timeout-cancellation-policy-dlya-cli-i-mcp-komand.md) — `accepted`, *CLI-часть superseded by ADR-0033; MCP-часть актуальна*
- [ADR-0013: MCP execution admission, timeout/cancellation routing и HTTP session capacity](0013-mcp-execution-admission-timeout-cancellation-routing-i-http-session-capacity.md) — `accepted`, *runtime-часть актуальна для MCP HTTP*
- [ADR-0009: Разделить structured business failures и transport/runtime failures](0009-razdelit-business-i-transport-runtime-failures.md) — `accepted`, *MCP-составляющая актуальна*
- [ADR-0004: Автообнаруживать компоненты платформы 1С по версии-маске](0004-avtoobnaruzhivat-komponenty-platformy-1s-po-versii-maske.md) — *вне scope менеджера, но используется внешним оркестратором запуска `1cv8c`*
- [ADR-0003: Поддерживать серверные ИБ для всех инструментов](0003-podderzhivat-servernye-ib-dlya-vseh-instrumentov.md) — *вне scope менеджера; зона ответственности внешнего оркестратора*

## Superseded by ADR-0033 (исторический форк-слой v8-runner)

- [ADR-0001: Границы поддержки IBCMD как ограниченного backend](0001-granitsy-podderzhki-ibcmd-kak-ogranichennogo-backend.md) — `superseded by ADR-0033`
- [ADR-0002: Изолировать runtime state по source-set под workPath](0002-izolirovat-runtime-state-po-source-set-pod-workpath.md) — `superseded by ADR-0033`
- [ADR-0005: Разделить CLI и MCP публичные поверхности](0005-razdelit-cli-i-mcp-publichnye-poverhnosti.md) — `superseded by ADR-0033`
- [ADR-0006: Сохранять транспортно-нейтральный use case слой](0006-sohranyat-transportno-neytralnyy-use-case-sloy.md) — `superseded by ADR-0033`
- [ADR-0007: Свести EDT execution к one-shot и shared interactive режимам](0007-vydelit-otdelnyy-pereklyuchatel-dlya-shared-edt.md) — `superseded by ADR-0033`
- [ADR-0008: Держать платформенные backend DSL отдельно от orchestration](0008-derzhat-platformennye-backend-dsl-otdelno-ot-orchestration.md) — `superseded by ADR-0033`
- [ADR-0010: Разделить CLI output для человека и AI-агента](0010-razdelit-cli-output-dlya-cheloveka-i-ai-agenta.md) — `superseded by ADR-0033`
- [ADR-0011: Эксклюзивное владение `workPath` на время команды](0011-eksklyuzivnoe-vladenie-workpath-na-vremya-komandy.md) — `superseded by ADR-0033`
- [ADR-0012: On-demand change detection и файловая partial-load стратегия](0012-on-demand-change-detection-i-faylovaya-partial-load-strategiya.md) — `superseded by ADR-0033`
- [ADR-0015: Атомарная публикация dump/artifacts через staging/backup](0015-atomarnaya-publikatsiya-dump-artifacts-cherez-staging-backup.md) — `superseded by ADR-0033`
- [ADR-0016: Единый `ExecutionOutcome` и pipeline steps для runner-like сценариев](0016-edinyy-executionoutcome-i-pipeline-steps-dlya-runner-like-stsenariev.md) — `superseded by ADR-0033`
- [ADR-0017: `v8project.yaml` / `source-set` как главный конфигурационный контракт](0017-v8project-yaml-source-set-kak-glavnyy-konfiguratsionnyy-kontrakt.md) — `superseded by ADR-0033`

## Правила обновления

- Для изменений архитектурных ограничений добавляйте новый ADR или обновляйте существующий с явным указанием статуса.
- При обновлении публичного контракта синхронизируйте связанные документы (`README.md`, `docs/architecture/`, `docs/CONFIGURATION.md` если он актуален).
- Архитектурные инварианты, которые должны соблюдаться агентами и контрибьюторами, перечислены в [docs/architecture/invariants.md](../architecture/invariants.md).
