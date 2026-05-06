> **Superseded by [ADR-0033](0033-extract-v8-session-manager-from-v8-runner.md).** Документ относится к скоупу форка `v8-runner`, который вырезан из проекта. Текст сохранён как исторический след.

# ADR-0006: Сохранять транспортно-нейтральный use case слой

- Статус: `accepted`
- Дата: `2026-04-20`

## Контекст

CLI и MCP используют одни и те же бизнес-сценарии, но имеют разные public surface, входные DTO, правила presentation и форматы ошибок.
Use case слой уже выделен в `src/use_cases` и содержит requests, `ExecutionContext`, structured results and failures.

Если use case начнут зависеть от `clap`, `Presenter`, `Envelope`, MCP DTO или конкретного transport payload format, orchestration станет трудно переиспользовать между CLI и MCP.
Это также приведет к тому, что изменение одного transport будет ломать другой transport.

## Решение

Сохранять `src/use_cases` как транспортно-нейтральный слой business orchestration.

1. Use case не зависят от `clap`.
2. Use case не зависят от `output::Presenter`.
3. Use case не создают и не принимают CLI `Envelope`.
4. Use case не принимают MCP DTO из `src/mcp/request.rs` и не возвращают MCP DTO из `src/mcp/response.rs`.
5. Use case не знают конкретный CLI/MCP transport payload format.
6. Transport adapters мапят свои входы в `use_cases::request::*` и мапят `UseCaseResult<T>` в свой output contract.
7. `ExecutionContext` может содержать transport-neutral invocation metadata, но не должен становиться контейнером для raw CLI/MCP payload.

## Неграницы (Non-goals)

1. Запрет на transport-specific адаптеры.
2. Запрет на transport metadata в `ExecutionContext`, если metadata не протаскивает raw payload format.
3. Требование иметь одинаковый response shape у CLI и MCP.

## Последствия

1. CLI rendering остается в `src/cli` и `src/output`.
2. MCP request/response mapping остается в `src/mcp`.
3. Новая бизнес-логика сначала проектируется как use case request/result, затем адаптируется к CLI/MCP.
4. Если use case нуждается в новом параметре, параметр добавляется в transport-neutral request model, а не через прямую ссылку на CLI flag или MCP DTO.
5. Тесты use case должны проверять business behavior без запуска CLI/MCP transport.

## План реализации

1. Зафиксировать этот ADR в `docs/decisions`.
2. Поддерживать imports в `src/use_cases` без зависимостей от `crate::cli`, `crate::output` и `crate::mcp`.
3. При добавлении сценариев обновлять:
- `src/use_cases/request.rs`
- соответствующий `src/use_cases/*.rs`
- `src/cli/execute.rs`
- `src/mcp/service.rs`, если сценарий публикуется в MCP по ADR-0005
4. Добавлять unit tests на use case без CLI/MCP presentation.
5. Добавлять adapter tests отдельно для CLI/MCP mapping.

## Верификация

- [x] ADR явно запрещает зависимости use case от `clap`, `Presenter`, `Envelope`, MCP DTO и transport payload format.
- [x] ADR описывает допустимую роль `ExecutionContext`.
- [x] ADR фиксирует responsibility split между use case, CLI и MCP.
- [x] Инвариант добавлен в архитектурную документацию.
