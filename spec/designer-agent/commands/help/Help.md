# Help

## Назначение

Показывает общую справку по SSH shell агента или справку по указанной группе команд.

## Синтаксис

```text
help [MODE]
help --version
```

## Параметры

- `MODE` — группа команд, для которой требуется вывести help.
- `--version` — выводит версию платформы агента.

## Связи

- Без аргументов показывает группы `help`, `common`, `options`, `config`, `infobase-tools`.
- Используется для discovery shell-команд агента.

## Примечания

- Подтверждено runtime в SSH shell `designer>` на `8.5.1.1150`.
- При `options set --output-format=json` возвращает JSON-массив.
