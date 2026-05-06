# Set

## Назначение

Изменяет shell-настройки агента для текущей SSH-сессии.

## Синтаксис

```text
options set [--output-format=<text|json>] [--show-prompt=<yes|no>]
```

## Параметры

- `--output-format=<text|json>` — формат результата команд.
- `--show-prompt=<yes|no>` — показывать или скрывать prompt `designer>`.
- `--notify-progress=<yes|no>` — включить сообщения о прогрессе.
- `--notify-progress-interval=<time interval>` — интервал обновления прогресса.

## Связи

- Используется для переключения shell между text и JSON output.
- Связана с `options list` и `options get`.

## Примечания

- Подтверждено runtime на `8.5.1.1150` для `--output-format=text|json`.
- После `--output-format=json` агент возвращал JSON-массивы.
