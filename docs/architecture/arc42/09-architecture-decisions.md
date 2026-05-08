## 9. Архитектурные решения

Источник истины — [`docs/decisions/`](../../decisions/README.md). Этот раздел перечисляет ADR, актуальные после extraction (ADR-0033). Исторические ADR-0001..0017 и ADR-0027 относились к CLI `v8-runner` и удалены.

| ADR | Статус / дата | Краткое значение для архитектуры |
| --- | --- | --- |
| [ADR-0018: WS-tunnel вместо HTTP back-connect](../../decisions/0018-ws-tunnel-vmesto-http-back-connect.md) | `accepted`, `2026-04-28` | Один WS несёт control- и data-plane; HTTP back-connect не используется. |
| [ADR-0019: Дедупликация client tools по `(kind, name, schema_hash)`](../../decisions/0019-deduplikatsiya-client-tools-po-kind-name-schema-hash.md) | `accepted`, `2026-04-28` | Одинаковые тулы от разных клиентов сводятся в один публичный; конфликты по схеме скрываются. |
| [ADR-0020: `SessionLaunchParamsCarrier`](../../decisions/0020-sessionlaunchparamscarrier-kak-abstraktsiya-dostavki-parametrov.md) | `accepted`, `2026-04-28` | Доставка `manager_url` через константу расширения, опциональные `/C`-параметры. |
| [ADR-0021: Per-session FIFO как обязательный инвариант](../../decisions/0021-per-session-fifo-kak-obyazatelnyy-invariant.md) | `accepted`, `2026-04-28` | На каждую сессию — последовательная очередь tool-вызовов. |
| [ADR-0022: Soft-reconnect по `client_uid`](../../decisions/0022-soft-reconnect-po-client-uid.md) | `accepted`, `2026-04-28` | Сессия переживает потерю WS в пределах `reconnection_grace_secs`. |
| [ADR-0023: Bidirectional control-plane](../../decisions/0023-bidirectional-control-plane-manager-client.md) | `accepted`, `2026-04-28` | `tools/list_changed` и т.п. идут через тот же WS, без HTTP back-connect. |
| [ADR-0024: Per-session dispatcher и lifecycle](../../decisions/0024-per-session-dispatcher-i-lifecycle.md) | `accepted`, `2026-04-28` | `SessionDispatcher` владеет очередью + inflight + last_call_at. |
| [ADR-0025: ClientProxy tools — публикация и резолвинг](../../decisions/0025-clientproxy-tools-publication-i-name-resolution.md) | `accepted`, `2026-04-28` | Имя `<prefix>__<tool>` на MCP HTTP; резолв в `(session_id, tool_name)`. |
| [ADR-0026: Политика `tools/list_changed` уведомлений](../../decisions/0026-tools-list-changed-notify-policy.md) | `accepted`, `2026-04-28` | Менеджер шлёт notify, не дублируя payload; клиент пере-пуливает `tools/list`. |
| [ADR-0028: Origin tracking сессий](../../decisions/0028-session-origin-tracking.md) | `proposed`, `2026-04-29` | Поле `origin` на записи. Фильтр idle-sweeper по origin снят в ADR-0034. |
| [ADR-0029: `host_id` + `pid` + `capabilities` в `session.register`](../../decisions/0029-host-id-pid-v-register-payload.md) | `proposed`, `2026-04-29` | Идентификация хоста в payload регистрации (для логов и диагностики). |
| [ADR-0030: Inline launch-spec в `session.spawn`](../../decisions/0030-inline-launch-spec-v-session-spawn.md) | `superseded by ADR-0034` | Менеджер сессии не запускает. |
| [ADR-0031: Двойной backend исполнения spawn/kill](../../decisions/0031-dual-backend-local-remote-i-kill-matrix.md) | `superseded by ADR-0034` | LocalBackend/RemoteBackend исключены из публичной поверхности. |
| [ADR-0032: Менеджер нативно в основном devcontainer](../../decisions/0032-manager-natively-v-osnovnom-devcontainer.md) | `accepted`, `2026-04-29` | Деплой бинаря, биндинг на `0.0.0.0:4000`/`:4001`. |
| [ADR-0033: Отделить v8-session-manager от форка v8-runner](../../decisions/0033-extract-v8-session-manager-from-v8-runner.md) | `accepted`, `2026-05-06` | Extraction; ADR-0001..0017, ADR-0027 удалены вместе с CLI. |
| [ADR-0034: Single-tool MCP surface](../../decisions/0034-single-tool-mcp-surface.md) | `accepted`, `2026-05-06` | Только `session_list` + проксированные тулы клиентов. |
| [ADR-0035: Кеш проксированных тулов с TTL и `config_id`](../../decisions/0035-tools-cache-with-ttl-and-config-id.md) | `proposed`, `2026-05-06` | Per-session кеш `tools/list` с инвалидацией по `config_id`. |

### Правила актуализации

- При добавлении или изменении ADR синхронизировать этот раздел и затронутые arc42-разделы, а не только список ссылок.
- При изменении любого инварианта сначала обновлять соответствующий ADR или добавлять новый ADR, который явно заменяет старое решение.
- Если реализация временно расходится с принятым ADR, фиксировать это как implementation gap в разделе 11.
