> **Superseded by [ADR-0033](0033-extract-v8-session-manager-from-v8-runner.md).** Документ относится к скоупу форка `v8-runner`, который вырезан из проекта. Текст сохранён как исторический след.

# ADR-0008: Держать платформенные backend DSL отдельно от orchestration

- Статус: `accepted`
- Дата: `2026-04-20`

## Контекст

`v8-runner` вызывает несколько платформенных инструментов 1С: Designer, IBCMD, EDT CLI and Enterprise.
У каждого инструмента свои executable lookup rules, process arguments, logs, timeouts, error forms and parsing details.

Если эти детали попадут в use case orchestration, бизнес-сценарии станут зависеть от конкретного backend process protocol.
Это усложнит добавление новых backend, поддержку server/file infobase combinations and reuse between CLI/MCP.

## Решение

Держать платформенные backend DSL отдельно от orchestration.

1. Низкоуровневые process details живут в `src/platform`.
2. `DesignerDsl` отвечает за команды Designer.
3. `IbcmdDsl` отвечает за команды `ibcmd`.
4. `EdtDsl` и interactive executor отвечают за `1cedtcli` one-shot/interactive execution.
5. `EnterpriseDsl` отвечает за запуск Enterprise client.
6. `platform::locator` and process facade отвечают за executable discovery and process execution.
7. Use case orchestration выбирает backend, вызывает domain-level methods and interprets `PlatformCommandResult`.
8. Use case orchestration не должна собирать сырые process command lines выше платформенного слоя, кроме диагностического/тестового кода, который явно проверяет adapter behavior.

## Неграницы (Non-goals)

1. Запрет на выбор backend в use case.
2. Запрет на анализ `PlatformCommandResult` в use case.
3. Требование сделать единый polymorphic trait для всех backend прямо сейчас.
4. Запрет на backend-specific tests.

## Последствия

1. Новый платформенный backend добавляется как отдельный adapter/DSL в `src/platform`.
2. Backend-specific gaps фиксируются в ADR/docs, а не маскируются в orchestration.
3. Use case code должен оставаться читаемым как сценарий: analyze changes, export/import/build/test/dump, map results.
4. Process spawning, command rendering, log reading and startup probing не должны размазываться по CLI/MCP adapters.
5. Ошибки платформенного уровня мапятся вверх как structured use-case failures или runtime/internal failures согласно ADR-0009.

## План реализации

1. Зафиксировать этот ADR в `docs/decisions`.
2. При добавлении backend или process mode обновлять:
- `src/platform/*`
- `src/platform/locator.rs`
- `src/platform/process.rs` or `src/platform/interactive.rs`, если меняется execution facade
- соответствующие use cases only at orchestration selection points
3. Не добавлять новые прямые вызовы `std::process` outside `src/platform` без отдельного решения.
4. Проверять tests at both levels:
- adapter tests for command construction/process mapping
- use case tests for orchestration behavior and result mapping

## Верификация

- [x] ADR явно перечисляет `DesignerDsl`, `IbcmdDsl`, `EdtDsl`, interactive executor, `EnterpriseDsl`, locator/process facade.
- [x] ADR запрещает протаскивать process details выше платформенного слоя.
- [x] ADR описывает допустимую роль orchestration.
- [x] Инвариант добавлен в архитектурную документацию.
