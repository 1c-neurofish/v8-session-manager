# ADR-0033: Отделить v8-session-manager от форка v8-runner

- Статус: accepted
- Дата: 2026-05-06
- Контекст задачи: extraction (см. `tasks/session-manager-extraction/`)

## Контекст

Менеджер клиентских сессий 1С зародился как ветка форка `v8-runner` — Rust-CLI для разработки на 1С (build/syntax/dump/launch/mcp). Со временем session-manager превратился в самостоятельный продукт с собственной архитектурой (WS control-plane, per-session dispatcher, soft-reconnect — ADR-0018..ADR-0032), не пересекающейся с разработческим CLI.

Совмещённое содержание форка приводило к проблемам:

- Зависимости разработческого CLI (walkdir, redb, quick-xml, tempfile, camino, libc, regex, gethostname, async-trait, anyhow, assert_cmd, predicates, insta, reqwest) тащились в бинарь менеджера.
- Use-case слой v8-runner (build/syntax/dump/launch/extensions/test/load/artifacts/config_init/init/ibcmd_diagnostics) занимал ~50% кодовой базы и не использовался менеджером.
- CLI-структура с подкомандами `v8-runner session-manager …` маскировала тот факт, что у менеджера ровно один режим работы.
- Конфиг `v8project.yaml` был перегружен полями (`basePath`, `connection`, `format`, `builder`, `source-set`, `templates`, `spawn`, `remote_backend`), нерелевантными для менеджера.
- ADR-0001..ADR-0017 описывали ограничения, которые перестали действовать в session-manager scope.

## Решение

Отделить session-manager в отдельный одиночный crate-проект:

- **Имя бинаря:** `v8-session-manager` (был `v8-runner`).
- **Репозиторий:** `https://github.com/1c-neurofish/v8-session-manager` (отдельный origin; форк-предок `SteelMorgan/v8-client-session-manager` остаётся как `legacy-fork` для исторической справки).
- **Скоуп:** только два транспорта (WS `:4000/sessions`, MCP HTTP `:4001/mcp`) на общий `Arc<SessionRegistry>`, плюс per-session dispatcher.
- **CLI:** плоский clap без подкоманд (`--config / --workdir / --log-level / --bind / --path / --mcp-http`).
- **Конфиг:** плоский `v8project.yaml` с `workPath` + `mcp.{session_manager,http,execution,metrics}`.
- **Удалено:** `src/use_cases/`, `src/parsers/`, `src/change_detection/`, `src/domain/`, `src/platform/`, `src/output/{json,presenter,text}.rs`, `src/cli/{execute,output}.rs`, `src/bin/mock_client.rs`, `src/mcp/{edt_session,edt_syntax,port,service,response,tool_result,context,telemetry,error}.rs`, все `tests/cli_*.rs`, `tests/fixtures/`, `examples/`.
- **Сохранено:** `src/session_manager/`, `src/mcp/{server,request,session_list,common}` (и smoke/transport тесты), `src/config/`, `src/cli/args.rs`, `src/app.rs`, `src/support/exit_codes.rs`.

## Последствия

### Положительные

- Cargo-граф уменьшился вдвое — быстрая сборка, меньше CVE-поверхности.
- Однопроцессный режим явно отражён в CLI: ошибиться с тем, какой бинарь и какую подкоманду запускать, невозможно.
- Конфиг и документация перестают «врать» о возможностях.
- Каждый ADR теперь либо относится к session-manager, либо помечен как `superseded by ADR-0033`.

### Отрицательные / риски

- ADR-0001..ADR-0017 (CLI/build/source-set scope) формально устаревают и переводятся в статус `superseded`. Их содержимое остаётся в репо как исторический след — файлы не удаляются.
- Старый remote `SteelMorgan/v8-client-session-manager` теряет связь с upstream — если кто-то использовал его для обновлений, надо переключиться на `1c-neurofish/v8-session-manager`.
- v8-runner CLI как продукт продолжает существовать в собственном репо родителя (вне нашего скоупа).

## Связанные решения

- Superseded by this ADR: ADR-0001, ADR-0002, ADR-0005, ADR-0006, ADR-0007, ADR-0008, ADR-0010, ADR-0011, ADR-0012, ADR-0015, ADR-0016, ADR-0017 (полностью v8-runner-специфичные).
- Частично superseded: ADR-0003, ADR-0004, ADR-0009, ADR-0013, ADR-0014 — их runtime-составляющая (admission, timeout/cancellation, structured failures) остаётся актуальной для MCP HTTP в session-manager.
- ADR-0034 — следующее решение, фиксирующее минимальную MCP-поверхность менеджера.

## Проверка

- `cargo build --release --bin v8-session-manager` — зелёный.
- `cargo test` — 73 теста зелёных.
- 4 итерации `codex-review` — финальный раунд без замечаний.
- DRIVE Linux→Linux smoke: 1cv8c с `mcpMode=ws` регистрируется, `tools/list` возвращает `session_list` + 18 проксированных `test_client__*`.
