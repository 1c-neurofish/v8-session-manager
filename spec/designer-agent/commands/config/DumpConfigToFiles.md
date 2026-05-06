# DumpConfigToFiles

## Назначение

Выгружает конфигурацию или расширение в XML-файлы из SSH shell агента.

## Синтаксис

```text
config dump-config-to-files --dir=<path> [--extension=<extension name>] [--all-extensions]
[--format=<hierarchical|plain>] [--update] [--get-changes=<path>]
[--config-dump-info-for-changes=<path>] [--config-dump-info-only] [--force]
[--list-file=<path>] [--threads=<n>] [--archive=<path>] [--ignore-unresolved-refs]
```

## Параметры

- `--dir=<path>` — каталог выгрузки.
- `--extension=<extension name>` — имя расширения.
- `--all-extensions` — обработать все расширения.
- `--format=<hierarchical|plain>` — формат выгрузки.
- `--update` — обновить существующую выгрузку.
- `--list-file=<path>` — файл со списком объектов.

## Связи

- Алиас: `dump-files`.
- Парная команда: `config load-config-from-files`.

## Примечания

- Подтверждено встроенным `help config` в SSH shell `8.5.1.1150`.
