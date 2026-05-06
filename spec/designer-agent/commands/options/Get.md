# Get

## Назначение

Возвращает значение указанной shell-настройки агента.

## Синтаксис

```text
options get [--output-format] [--show-prompt] [--notify-progress] [--notify-progress-interval]
```

## Параметры

- `--output-format` — формат вывода shell.
- `--show-prompt` — наличие prompt `designer>`.
- `--notify-progress` — вывод информации о прогрессе.
- `--notify-progress-interval` — интервал обновления прогресса.

## Связи

- Связана с `options list` и `options set`.

## Примечания

- Подтверждено runtime на `8.5.1.1150` для `--output-format` и `--show-prompt`.
- Остальные ключи подтверждены встроенным `help options`.
