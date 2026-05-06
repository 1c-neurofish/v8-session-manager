# Протокол менеджера клиентских сессий

JSON Schema, описывающие WS JSON‑RPC 2.0 контракт между менеджером и 1С‑клиентом
(см. `spec/SESSION_MANAGER.md` §4).

## Файлы

- `messages.schema.json` — единая schema со всеми сообщениями этапа 1 в
  `$defs`. Покрывает:
  - JSON‑RPC envelope (request / response / notification / error);
  - control‑plane: `session.register`, `session.heartbeat`, `session.bye`,
    `session.tools_changed`, `ping`;
  - вспомогательные типы: `tool-descriptor`.

## Эволюция

Schema эволюционирует по этапам. Этап 2 добавит `tool.call` (manager → client),
этап 3 — `session.shutdown`, этап 7 финализирует resources/prompts pass‑through
(§10.2 спеки).

## Использование

Каждое отправляемое и принимаемое сообщение менеджера и mock‑клиента в CI
валидируется против соответствующего `$defs/<message>` через `serde_json` +
`jsonschema` crate.
