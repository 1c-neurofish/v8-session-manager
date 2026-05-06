# ADR-0009: Разделить structured business failures и transport/runtime failures

- Статус: `accepted`
- Дата: `2026-04-20`

## Контекст

Одни ошибки являются ожидаемыми business failures сценария: validation errors, platform command failure with structured payload, test/syntax failures.
Другие ошибки являются transport/runtime failures: invalid adapter usage, MCP runtime bootstrap failure, join failure, cancelled/timeout outside business response contract, serialization/handler faults.

Если эти категории смешать, MCP-клиент может получить runtime fault как business payload, CLI может потерять корректный exit code, а use case orchestration начнет знать детали CLI/MCP serialization.

## Решение

Разделить structured business failures и transport/runtime failures.

1. Use case возвращают `UseCaseResult<T> = Result<T, UseCaseFailure<T>>`.
2. `UseCaseFailure<T>` содержит transport-neutral `UseCaseError` and optional structured payload.
3. MCP service мапит ожидаемые business failures в `McpBusinessFailure<T>`.
4. MCP service/transport мапит adapter/runtime misuse and infrastructure faults в `McpInternalError` or protocol-level error.
5. CLI adapter мапит `UseCaseFailure<T>` в CLI exit code and text/JSON rendering.
6. Orchestration не знает конкретный формат ответа CLI/MCP.
7. Business failure payload может сохранять частичный structured result, если это полезно клиенту.

## Неграницы (Non-goals)

1. Единый serialized error shape для CLI и MCP.
2. Сокрытие всех platform failures как internal errors.
3. Запрет на partial structured payload при business failure.
4. Использование MCP business response для ошибок неправильного transport/runtime usage.

## Последствия

1. CLI and MCP adapters remain responsible for final error rendering.
2. MCP tool result has an explicit success/business-failure payload shape.
3. Internal MCP errors must not pretend to be successful tool payloads.
4. Use cases can be tested without transport serialization.
5. Новый use case должен определить, какие ошибки являются business failures с payload, а какие являются failures without payload.

## План реализации

1. Зафиксировать этот ADR в `docs/decisions`.
2. Поддерживать failure contracts в:
- `src/use_cases/result.rs`
- `src/mcp/error.rs`
- `src/mcp/tool_result.rs`
- `src/cli/execute.rs`
- `src/mcp/service.rs`
- `src/mcp/server.rs`
3. При добавлении нового MCP tool явно тестировать:
- success payload
- business failure payload
- internal/runtime error path
4. При добавлении нового use case тестировать `UseCaseFailure<T>` with and without payload.

## Верификация

- [x] ADR фиксирует `UseCaseFailure<T>`.
- [x] ADR фиксирует различие `McpBusinessFailure<T>` and `McpInternalError`.
- [x] ADR запрещает orchestration знать конкретный CLI/MCP response format.
- [x] Инвариант добавлен в архитектурную документацию.
