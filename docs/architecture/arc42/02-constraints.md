## 2. Ограничения

### 2.1 Технические ограничения

- Кодовая база реализована на Rust 2021 (`tokio`, `axum`, `rmcp`, `tokio-tungstenite`).
- Один бинарь, два транспорта: WS (`:4000/sessions` по умолчанию) и MCP HTTP (`:4001/mcp` по умолчанию). Оба транспорта работают на общем `Arc<SessionRegistry>`.
- Конфигурация — единственный YAML-файл (`v8project.yaml` или путь из `--config` / `V8SM_CONFIG`). Никаких `base_path` / `connection` / `source-set` / `tools.platform` / `tools.edt-cli` / `tests` — это были поля исторического CLI `v8-runner`, удалены при extraction (ADR-0033).
- Менеджер сам 1С не запускает: `tokio::process::Command`, RAC-интеграция, sidecar-аддин для spawn — нет (ADR-0034).
- Liveness канала держится на WS protocol-level Ping/Pong (RFC 6455). Application-level ping намеренно не делается, чтобы открытый модальный диалог 1С не считался зависанием (см. STACK_OVERVIEW §Liveness).
- Состояние сессий — in-memory. Менеджер stateless по диску: рестарт уничтожает реестр, клиенты переподключаются заново.
- MCP HTTP — streamable transport через `rmcp::transport::StreamableHttpService`; stateful-сессии трекаются по `Mcp-Session-Id`.

### 2.2 Организационные и продуктовые ограничения

- Публичная поверхность менеджера на MCP HTTP — единственный встроенный tool `session_list` плюс проксированные тулы клиентов (ADR-0034). Расширение этой поверхности требует нового ADR, явно отменяющего ADR-0034.
- Lifecycle 1С-клиентов лежит на внешнем оркестраторе. Любые «manager-side spawn templates», «kill matrix», «session swap» — это исторические наработки (ADR-0030, ADR-0031), помеченные как `superseded by ADR-0034`.
- Все изменения архитектурных границ оформляются как новый ADR в `docs/decisions/`, а связанные документы (`README.md`, `docs/CONFIGURATION.md`, `docs/architecture/`) синхронизируются в том же изменении.
