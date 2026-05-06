# SignCfg

## Назначение

Подписывает конфигурацию или расширение цифровой подписью.

## Синтаксис

```text
config sign-cfg --configuration-type=<extension-configuration|extension-db-configuration|extension-configuration-repository|file>
[--name=<extension name>] [--version=<version>] [--file=<file>] --digi-sign=<file> --signed-file=<file>
```

## Параметры

- `--configuration-type=...` — тип подписываемой конфигурации.
- `--name=<extension name>` — имя расширения.
- `--version=<version>` — версия в хранилище.
- `--file=<file>` — входной файл.
- `--digi-sign=<file>` — файл приватного ключа.
- `--signed-file=<file>` — файл результата.

## Связи

- Работает в группе `config`.
- Близка к batch-командам подписи конфигурации.

## Примечания

- Подтверждено встроенным `help config` в SSH shell `8.5.1.1150`.
