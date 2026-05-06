> **Superseded by [ADR-0033](0033-extract-v8-session-manager-from-v8-runner.md).** Документ относится к скоупу форка `v8-runner`, который вырезан из проекта. Текст сохранён как исторический след.

# ADR-0010: Разделить CLI output для человека и AI-агента

- Статус: `accepted`
- Дата: `2026-04-20`

## Контекст

CLI `v8-runner` будет потребляться двумя разными аудиториями:

1. Человек-разработчик запускает команды вручную и должен быстро увидеть значимые места: что изменилось, что пропущено, что требует внимания, где ошибка и какой следующий шаг.
2. AI-агент запускает CLI как инструмент и должен получать краткий, предсказуемый вывод без лишнего шума. Если всё хорошо и ошибок нет, агенту не нужен подробный рассказ о каждом успешном шаге.

Текущий `--output text|json` задаёт формат сериализации, но не полностью описывает audience intent.
`json` полезен для машинной обработки, но не должен автоматически означать "полный verbose dump", если потребитель — агент, которому нужен минимальный сигнал.

## Решение

Разделить требования к CLI presentation для человека и AI-агента.

1. Human-oriented CLI output должен акцентировать значимые места:
- итог команды;
- ошибки и предупреждения;
- skipped/degraded/partial behavior;
- изменённые или созданные артефакты;
- важные пути к логам или отчётам;
- следующий actionable hint, если без него человеку сложно продолжить.
2. Agent-oriented CLI output должен быть кратким:
- не выводить подробные успешные шаги, если нет ошибок, предупреждений, degraded behavior или созданных артефактов, которые агент должен использовать дальше;
- при успехе без важных деталей возвращать минимальный success signal;
- при ошибке возвращать только код/класс ошибки, краткое сообщение, affected target and next actionable diagnostic location;
- не дублировать данные, которые уже доступны в structured result или логах.
3. Формат вывода и аудитория вывода являются разными осями проектирования.
4. `--output json` остаётся structured format, но не является синонимом verbose output.
5. `--output text` остаётся human-readable format, но его текст не должен становиться единственным источником машинно значимой информации.
6. Use case слой не знает, для человека или агента рендерится результат; audience-specific rendering остаётся в CLI/output adapter.
7. Любой новый CLI output должен проверяться на два вопроса:
- какую значимую информацию это даёт человеку;
- нужен ли этот сигнал агенту при successful/no-warning path.

## Неграницы (Non-goals)

1. Немедленное изменение всех текущих CLI сообщений в рамках этого ADR.
2. Замена MCP на CLI для агентской автоматизации.
3. Превращение human text output в стабильный machine protocol.
4. Скрытие ошибок, предупреждений, degraded behavior или путей к диагностике ради краткости.

## Последствия

1. CLI presentation должен иметь явную модель audience/profile или эквивалентное правило, чтобы не смешивать human highlights and agent minimalism.
2. По умолчанию human output может оставаться удобным для ручного запуска, но agent-oriented режим должен быть доступен явно и документирован.
3. Успешный happy path для agent output не должен печатать пошаговый журнал.
4. Ошибки в agent output должны быть достаточно короткими для LLM context, но достаточно точными для следующего действия.
5. Structured data for automation should remain in JSON/MCP contracts, while text highlights remain presentation only.

## План реализации

1. Зафиксировать этот ADR в `docs/decisions`.
2. Добавить invariant в `docs/architecture/invariants.md`.
3. При реализации audience split обновить:
- `src/cli/args.rs`
- `src/output/presenter.rs`
- `src/output/text.rs`
- `src/output/json.rs`, если меняется JSON compact/full policy
- `src/cli/execute.rs`
- `docs/CAPABILITIES.md`
- `README.md`
4. Ввести тесты CLI output для двух классов потребителей:
- human text output highlights errors/warnings/degraded/artifacts;
- agent output stays minimal on clean success and still reports errors/warnings/actionable diagnostics.
5. При добавлении новой CLI команды или нового result field обновлять rendering tests before exposing the behavior.

## Верификация

- [x] ADR фиксирует, что CLI output потребляется человеком и AI-агентом.
- [x] ADR фиксирует краткость agent output и отсутствие лишнего вывода при чистом успехе.
- [x] ADR фиксирует human output emphasis on significant places.
- [x] ADR сохраняет транспортно-нейтральный use case слой согласно ADR-0006.
