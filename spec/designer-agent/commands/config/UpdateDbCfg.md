# UpdateDbCfg

## Назначение

Обновляет конфигурацию базы данных из SSH shell агента.

## Синтаксис

```text
config update-db-cfg [--dynamic-enable] [--dynamic-disable] [--warnings-as-errors]
[--prompt-confirmation] [--session-terminate=<disable|prompt|force>]
[--session-terminate-message=<value>] [--background-start] [--background-cancel]
[--background-finish] [--background-suspend] [--background-resume]
[--server] [--extension=<extension name>]
```

## Параметры

- `--dynamic-enable` — сначала попытаться выполнить динамическое обновление.
- `--dynamic-disable` — запретить динамическое обновление.
- `--warnings-as-errors` — считать предупреждения ошибками.
- `--background-start|cancel|finish|suspend|resume` — управление фоновым обновлением.
- `--session-terminate=<disable|prompt|force>` — политика завершения активных сеансов.

## Связи

- Работает в группе `config`.
- В `V8Update.htm` и shell-help согласованы `--background-finish` и `--session-terminate`.

## Примечания

- Подтверждено встроенным `help config` в SSH shell `8.5.1.1150`.
- Разрушающее выполнение не проверялось в этом стенде.
