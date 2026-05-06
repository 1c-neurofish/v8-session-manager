# LoadExternalDataProcessorOrReportFromFiles

## Назначение

Загружает внешнюю обработку или отчет из XML-файлов.

## Синтаксис

```text
config load-external-data-processor-or-report-from-files --file=<file> --ext-file=<file>
```

## Параметры

- `--file=<file>` — корневой XML-файл.
- `--ext-file=<file>` — результирующий `.epf` или `.erf`.

## Связи

- Алиас: `load-ext-files`.
- Парная команда: `config dump-external-data-processor-or-report-to-files`.

## Примечания

- Подтверждено встроенным `help config` в SSH shell `8.5.1.1150`.
