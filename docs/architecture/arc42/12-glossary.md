## 12. Глоссарий

| Термин | Значение |
| --- | --- |
| 1С | Локальная корпоративная платформа и утилиты, которыми управляет система |
| Designer | Конфигуратор 1С и соответствующий формат исходников / backend |
| EDT | 1C:Enterprise Development Tools и формат EDT-проектов |
| MCP | Model Context Protocol, через который ассистенты вызывают инструменты |
| `source-set` | Логическая группа исходников; поддержанные типы: `CONFIGURATION`, `EXTENSION`, `EXTERNAL_DATA_PROCESSORS`, `EXTERNAL_REPORTS` |
| External source-set | `source-set` типа `EXTERNAL_DATA_PROCESSORS` или `EXTERNAL_REPORTS`, используемый для публикации внешних `.epf`/`.erf` артефактов |
| YaXUnit | Фреймворк тестирования, используемый для запуска и отчётности по unit-тестам 1С |
| `workPath` | Каталог времени выполнения для логов, temp-файлов, состояния и сгенерированных артефактов |
| Workspace lock | Advisory lock по canonical `workPath`, который сериализует публичные CLI/MCP команды над одним runtime root |
| IBCMD | Командная утилита 1С, используемая как альтернативный backend для части операций |
| Структурированная бизнес-ошибка | Контролируемая ошибка, возвращаемая как часть контракта CLI/MCP-операции |
| Execution Context | Transport-neutral invocation metadata, описывающая команду, transport и дополнительные execution flags |
| Execution Outcome | `ExecutionOutcome<T>`, доменная форма результата runner-like/pipeline-like сценария со статусом, errors, diagnostics, metrics, artifacts и typed payload |
| Pipeline block | Крупный шаг use-case pipeline: validation, resolve target, prepare workspace, platform command, parse output, publish, cleanup или diagnostics |
| MCP execution admission | Лимит одновременных MCP tool executions, общий для stdio и HTTP transport |
| HTTP session capacity | Отдельный лимит tracked stateful HTTP sessions, не равный execution admission |
| Critical phase | Участок mutating operation, где default hard kill запрещён и cancellation/timeout ждёт terminal outcome |
| Staging/backup publication | Контракт публикации full replacement target через sibling staging path, backup старого target, rollback attempt и metadata-based cleanup |
