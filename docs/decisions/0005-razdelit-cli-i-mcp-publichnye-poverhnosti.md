> **Superseded by [ADR-0033](0033-extract-v8-session-manager-from-v8-runner.md).** Документ относится к скоупу форка `v8-runner`, который вырезан из проекта. Текст сохранён как исторический след.

# ADR-0005: Разделить CLI и MCP публичные поверхности

- Статус: `accepted`
- Дата: `2026-04-20`

## Контекст

`v8-runner` имеет две публичные поверхности: CLI и MCP.
CLI содержит полный набор пользовательских команд, включая `config init`, `init`, `extensions`, `build`, `load`, `test`, `dump`, `make`/`artifacts`, `syntax`, `launch` и `mcp serve`.
MCP предназначен для управляемого агентского доступа и сейчас публикует более узкий набор tool-операций.

Если считать MCP автоматическим зеркалом CLI, любое CLI-расширение будет неявно становиться MCP-контрактом.
Это повышает риск небезопасного роста поверхности, размывает ожидания MCP-клиентов и усложняет версионирование.

## Решение

Разделить CLI и MCP как самостоятельные публичные поверхности.

1. MCP не зеркалит CLI автоматически.
2. Текущая MCP-поверхность состоит ровно из 8 опубликованных tool-операций:
- `run_all_tests`
- `run_module_tests`
- `build_project`
- `dump_config`
- `launch_app`
- `check_syntax_edt`
- `check_syntax_designer_config`
- `check_syntax_designer_modules`
3. CLI может иметь команды, не опубликованные в MCP.
4. Добавление новой MCP tool-операции, удаление существующей tool-операции, переименование или изменение ее семантики считается изменением MCP public surface.
5. Расширение MCP surface требует отдельного ADR или явного обновления этого ADR с описанием мотивации, DTO, бизнес-ошибок, ограничений исполнения и тестов.
6. MCP DTO используют собственную форму запроса/ответа и не обязаны совпадать с CLI flags или CLI JSON `Envelope`.

## Неграницы (Non-goals)

1. Немедленная публикация всех CLI-команд в MCP.
2. Создание стабильного ABI между CLI flags и MCP request DTO.
3. Запрет на будущие MCP tool-операции; запрещено только неявное расширение без решения.

## Последствия

1. Документация должна явно перечислять опубликованные MCP tools.
2. Реализация MCP должна оставаться в `src/mcp`, а не проходить через `cli::execute`.
3. Новые CLI-команды не требуют MCP-эквивалента по умолчанию.
4. Новые MCP tools должны проектироваться как отдельный контракт: request DTO, response DTO, business failure shape, runtime/admission behavior, telemetry and tests.
5. Агентам нельзя выводить доступность MCP-операции из наличия CLI-команды.

## План реализации

1. Зафиксировать этот ADR в `docs/decisions`.
2. Обновить индекс ADR и arc42 decision list.
3. Зафиксировать invariant в `docs/architecture/invariants.md`.
4. При изменении MCP surface синхронизировать:
- `README.md`
- `docs/CAPABILITIES.md`
- `spec/FUNCTIONAL_CAPABILITIES.md`
- `src/mcp/server.rs`
- `src/mcp/request.rs`
- `src/mcp/response.rs`
- `src/mcp/service.rs`
5. Добавлять/обновлять тесты MCP server/service для каждого изменения surface.

## Верификация

- [x] ADR явно говорит, что MCP не зеркалит CLI.
- [x] ADR перечисляет 8 текущих MCP tool-операций.
- [x] ADR требует отдельного решения для расширения MCP surface.
- [x] Инвариант добавлен в архитектурную документацию.
