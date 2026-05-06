# Designer Agent Specs

Краткий runtime-ориентированный справочник по `1cv8 DESIGNER /AgentMode`.

Раздел теперь опирается на два слоя проверки:

- startup и transport contract для `8.3.27.1859`;
- SSH shell и agent-команды для `8.5.1.1150`, которую вы отдельно указали использовать для запуска агента.

## Базовый контракт

- Точка входа: `1cv8 DESIGNER /F <file-ib> /AgentMode [...]`.
- Транспорт: SSH и SFTP.
- Один процесс агента обслуживает одну информационную базу.
- Для интерактивной SSH shell-сессии агент не запрашивает PTY; рабочий вариант клиента в стенде: `ssh -T`.
- Машиночитаемый индекс: `manifest.json`.
- Подробная матрица проверок: `request-surface.md`.

## Что подтверждено

### Startup contract

- Scratch file ИБ создается headless через `CREATEINFOBASE 'File=<dir>;' /DisableStartupDialogs /Out <file>`.
- `DESIGNER /AgentMode` стартует headless на Linux без `DISPLAY`; в лог попадают только предупреждения `colord`, порт при этом открывается.
- Подтверждены параметры `/AgentMode`, `/AgentPort`, `/AgentListenAddress`, `/AgentSSHHostKey`, `/AgentSSHHostKeyAuto`, `/AgentBaseDir`.
- Подтверждены значения по умолчанию:
  - `/AgentPort` -> `1543`.
  - `/AgentListenAddress` -> `127.0.0.1`.
  - `/AgentSSHHostKeyAuto` на Linux -> `~/.1cv8/1C/1cv8/host_id`.
  - `/AgentBaseDir` по локальной справке -> `~/.1cv8/1C/1cv8/<UUID>/sftp`.

### Transport and session contract

- Агент `8.3.27.1859` поднимает SSH-сервер с banner `libssh_0.9.6`.
- Агент `8.5.1.1150` поднимает SSH shell с banner `1C:Enterprise 8.5 1C Designer Shell © 1C-Soft LLC 1996-2025` и prompt `designer>`.
- При запуске с `/AgentSSHHostKey <file>` сервер публикует host key из указанного файла.
- При запуске с `/AgentSSHHostKeyAuto` сервер публикует RSA host key из `/home/alko/.1cv8/1C/1cv8/host_id`.
- Подключение без явного `username` в клиенте не является anonymous mode: OpenSSH автоматически отправляет локального пользователя (`alko` в стенде).
- На этапе `userauth` агент объявляет только метод `password`.
- Пустой пароль отвергается.
- Успешный SSH login в стенде выполнен учетными данными пользователя ИБ `agenttest/agenttest`; это подтверждает, что агент принимает SSH username/password как учетные данные пользователя информационной базы.
- Для SSH shell на `8.5.1.1150` PTY не выделяется: `ssh -tt` получает `PTY allocation request failed on channel 0`, а `ssh -T` работает штатно.
- Пока один SSH shell-клиент удерживает сессию, второй `ssh -T` получает `shell request failed on channel 0`.
- Одновременно могут жить несколько SFTP-сеансов: на `1555` были подтверждены два параллельных `sftp`-клиента с успешным `pwd`.
- В SFTP-сеансе рабочий каталог агента в проверенном стенде равен `/`.

## Request Surface

- Для `8.3.27.1859` подтвержден только transport/auth слой.
- Для `8.5.1.1150` подтвержден shell-like request surface через `ssh -T`, prompt `designer>`, текстовый и JSON-вывод, а также SFTP-вход.
- Подтвержденный runtime-факт: shell принимает команды в форме `<group> <command> [options]` и `help [mode]`.
- Подробная матрица с примерами вынесена в `request-surface.md`.

## Команды агента

Подтвержденные группы SSH shell на `8.5.1.1150`:

- `help`
- `common`
- `options`
- `config`
- `infobase-tools`

Подтвержденные runtime-команды:

- `help`
- `help --version`
- `common connect-ib`
- `common disconnect-ib`
- `common shutdown`
- `options list`
- `options get`
- `options set`
- `infobase-tools debug-info`

Подтвержденные встроенным shell-help команды:

- `config dump-config-to-files` (`dump-files`)
- `config load-config-from-files` (`load-files`)
- `config dump-external-data-processor-or-report-to-files` (`dump-ext-files`)
- `config load-external-data-processor-or-report-from-files` (`load-ext-files`)
- `config update-db-cfg`
- `config mobile-app-write-file`
- `config mobile-client-digi-sign`
- `config mobile-client-write-file`
- `config dump-cfg`
- `config load-cfg`
- `config manage-cfg-support`
- `config sign-cfg`
- `config generation-id`
- `config extensions`
- `infobase-tools data-separation-common-attributes-list`
- `infobase-tools dump-ib`
- `infobase-tools restore-ib`
- `infobase-tools erase-data`

### Максимально подробный список команд

`help`

- `help` — общая справка по shell-командам агента.
- `help --version` — версия платформы агента.
- `help <mode>` — help по группе команд.

`common`

- `common connect-ib` — подключить shell-сеанс к ИБ.
- `common disconnect-ib` — отключить shell-сеанс от ИБ.
- `common shutdown` — завершить агент.

`options`

- `options list` — вывести текущие shell-настройки.
- `options get` — получить значение настройки.
- `options set` — изменить настройку.

Подтвержденные настройки:

- `output-format` — `text|json`.
- `show-prompt` — `yes|no`.
- `notify-progress` — показывать progress, если применимо.
- `notify-progress-interval` — период обновления progress.

`config`

- `dump-config-to-files` (`dump-files`) — выгрузить конфигурацию в XML-файлы.
- `load-config-from-files` (`load-files`) — загрузить конфигурацию из XML-файлов.
- `dump-external-data-processor-or-report-to-files` (`dump-ext-files`) — выгрузить внешнюю обработку или отчет в XML.
- `load-external-data-processor-or-report-from-files` (`load-ext-files`) — загрузить внешнюю обработку или отчет из XML.
- `update-db-cfg` — обновить конфигурацию базы данных.
- `mobile-app-write-file` — сохранить мобильное приложение в XML.
- `mobile-client-digi-sign` — подписать мобильный клиент.
- `mobile-client-write-file` — сохранить мобильный клиент в XML.
- `dump-cfg` — сохранить конфигурацию или расширение в файл.
- `load-cfg` — загрузить конфигурацию или расширение из файла.
- `manage-cfg-support` — изменить параметры поддержки конфигурации.
- `sign-cfg` — подписать конфигурацию или расширение.
- `generation-id` — получить generation ID конфигурации.
- `extensions` — submode управления расширениями: `create`, `delete`, `properties get`, `properties set`.

`infobase-tools`

- `data-separation-common-attributes-list` — вывести разделители ИБ.
- `debug-info` — вывести настройки отладчика.
- `dump-ib` — выгрузить ИБ в файл.
- `restore-ib` — восстановить ИБ из файла.
- `erase-data` — удалить данные ИБ.

## Success and error surface

- Позитивный startup-кейс: агент открывает TCP-порт и отвечает SSH banner.
- Негативный транспортный кейс: `PreferredAuthentications=none` завершается `Permission denied (password)`.
- Негативный auth-кейс: пустой пароль отклоняется повторным password prompt.
- Позитивный shell-кейс на `8.5.1.1150`: `options set --output-format=json` возвращает JSON-массив с `type: "success"`.
- Позитивный shell-кейс на `8.5.1.1150`: `infobase-tools debug-info` возвращает JSON-object с полями `enabled`, `protocol`, `server-address`.
- Негативный shell-кейс на `8.5.1.1150`: неизвестная команда возвращает JSON-массив с логами help и завершается `type: "error"`, `error-type: "CommandFormatError"`, `message: "Invalid command format"`.
- Негативный shell-кейс на `8.5.1.1150`: повторный `common connect-ib` возвращает `DesignerAlreadyConnectedToInfoBase`.
- Негативный shell-кейс на `8.5.1.1150`: `common disconnect-ib` без активного подключения возвращает `DesignerNotConnectedToInfoBase`.
- При `options set --show-prompt=no` shell перестает печатать `designer>`, но команды продолжают работать через stdin/stdout.
- `DESIGNER --help` без `DISPLAY` в этом окружении не дал usable output и не использовался как источник истины.

## Расхождения Docs/Help/Runtime

| Спорный пункт | Docs/help | Observed runtime | Verdict |
| --- | --- | --- | --- |
| `/AgentSSHHostKeyAuto` на Linux использует `~/.1cv8/1C/1cv8/host_id` | Локальная help-страница `zif3_AgentSSHHostKeyAuto` | Host key fingerprint агента на `:1544` совпал с `host_id` | confirmed-runtime |
| Подключение "без пользователя" | Help этого не обещает; SSH-клиент может не указывать `username` явно | Клиент все равно отправляет локального пользователя `alko` | runtime wins |
| Подключение "без пароля" | Help этого не описывает | Агент предлагает только `password`; пустой пароль отвергается | rejected-for-spec |
| Shell с PTY | Локальная справка и приведенный текст рекомендуют не выделять PTY | `ssh -tt` получает `PTY allocation request failed on channel 0`, `ssh -T` дает prompt `designer>` | confirmed-runtime |
| Команды shell на `8.3.27.1859` | Текст справки описывает `designer>` shell и команды | Агент `8.3.27.1859` обрывал конфигурационные операции с сообщением о требовании версии `8.5.1+` | runtime wins |
| `AgentBaseDir` сразу создает `agentbasedir.json` | В текущем help-viewer это не подтверждено отдельной страницей | На старте без успешной auth/команд никаких файлов в custom `AgentBaseDir` не появилось | unconfirmed |

## Гипотезы и блокеры

- Для `8.3.27.1859` shell-команды по-прежнему не подтверждены runtime: transport/auth есть, но конфигурационная shell-поверхность фактически уперлась в version gate.
- Для `8.5.1.1150` остаются частичные блокеры:
  - не подтверждены файловые артефакты `config/*` команд;
  - не проверялись разрушающие команды `restore-ib`, `erase-data`, `update-db-cfg`;
  - не снимался progress/output для длительных операций.
- Все shell-команды ниже нужно трактовать как подтвержденные для `8.5.1.1150`, а не ретроактивно для `8.3.27.1859`.
