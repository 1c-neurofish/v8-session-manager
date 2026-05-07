# Архитектурные решения (ADR)

Этот каталог хранит архитектурные решения проекта в формате ADR.

> История форка `v8-runner` зафиксирована в **ADR-0033 (extraction)**. После
> отделения этого репозитория ADR-0001..0017 и ADR-0027, относившиеся к
> CLI-функционалу `v8-runner`, удалены — на новый код они не действуют. Если
> нужна их история, она доступна в исходном репозитории `v8-runner` или в
> git-логе до коммита `chore: drop v8-runner ADRs`.

## Актуальные решения (session-manager)

- [ADR-0035: Кеш проксированных тулов с TTL и `config_id`](0035-tools-cache-with-ttl-and-config-id.md) — `proposed`, `2026-05-06`
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

## Правила обновления

- Для изменений архитектурных ограничений добавляйте новый ADR или обновляйте существующий с явным указанием статуса.
- При обновлении публичного контракта синхронизируйте связанные документы (`README.md`, `docs/architecture/`, `docs/CONFIGURATION.md` если он актуален).
- Архитектурные инварианты, которые должны соблюдаться агентами и контрибьюторами, перечислены в [docs/architecture/invariants.md](../architecture/invariants.md).
