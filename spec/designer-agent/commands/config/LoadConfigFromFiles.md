# LoadConfigFromFiles

## Назначение

Загружает конфигурацию или расширение из XML-файлов из SSH shell агента.

## Синтаксис

```text
config load-config-from-files --dir=<path> [--archive=<path>] [--extension=<extension name>]
[--all-extensions] [--files=<files>] [--list-file=<path>] [--format=<hierarchical|plain>]
[--update-config-dump-info] [--update-config-dump-info-archive=<value>] [--no-check] [--partial]
```

## Параметры

- `--dir=<path>` — каталог загрузки.
- `--archive=<path>` — ZIP-архив загрузки.
- `--files=<files>` — список файлов через запятую.
- `--list-file=<path>` — файл со списком файлов.
- `--partial` — частичная загрузка.

## Связи

- Алиас: `load-files`.
- Парная команда: `config dump-config-to-files`.

## Примечания

- Подтверждено встроенным `help config` в SSH shell `8.5.1.1150`.
