# DumpExternalDataProcessorOrReportToFiles

## Назначение

Выгружает внешнюю обработку или отчет в XML-файлы.

## Синтаксис

```text
config dump-external-data-processor-or-report-to-files --file=<file> --ext-file=<file>
[--format=<hierarchical|plain>]
```

## Параметры

- `--file=<file>` — корневой XML-файл выгрузки.
- `--ext-file=<file>` — исходный `.epf` или `.erf`.
- `--format=<hierarchical|plain>` — формат выгрузки.

## Связи

- Алиас: `dump-ext-files`.
- Парная команда: `config load-external-data-processor-or-report-from-files`.

## Примечания

- Подтверждено встроенным `help config` в SSH shell `8.5.1.1150`.
