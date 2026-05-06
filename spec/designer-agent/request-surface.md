# Request Surface

Матрица подтвержденной поверхности запроса для `1cv8 DESIGNER /AgentMode`.

Ниже намеренно разделены:

- transport/auth observations на `8.3.27.1859`;
- shell/session observations на `8.5.1.1150`.

## Матрица

| Surface | Пример | Prerequisite | Observed output channel | Observed artifact channel | Status |
| --- | --- | --- | --- | --- | --- |
| Startup с явным host key, `8.3.27.1859` | `1cv8 DESIGNER /F <ib> /AgentMode /AgentListenAddress 127.0.0.1 /AgentPort 1543 /AgentBaseDir <dir> /AgentSSHHostKey <key>` | scratch file ИБ | процесс держит SSH порт `127.0.0.1:1543` | custom `AgentBaseDir` на старте пуст | confirmed-runtime |
| Startup с auto host key, `8.3.27.1859` | `1cv8 DESIGNER /F <ib> /AgentMode /AgentPort 1544 /AgentSSHHostKeyAuto` | scratch file ИБ | процесс держит SSH порт `127.0.0.1:1544` | серверный host key совпадает с `~/.1cv8/1C/1cv8/host_id` | confirmed-runtime |
| SSH handshake без явного username, `8.3.27.1859` | `ssh -p 1544 127.0.0.1` | агент запущен | OpenSSH отправляет локального пользователя `alko`; сервер отвечает `password` auth | нет | confirmed-runtime |
| SSH handshake с `PreferredAuthentications=none`, `8.3.27.1859` | `ssh -o PreferredAuthentications=none -p 1544 127.0.0.1 true` | агент запущен | `Permission denied (password)` | нет | confirmed-runtime |
| SSH password auth с пустым паролем, `8.3.27.1859` | `ssh -o PreferredAuthentications=password -p 1544 127.0.0.1` + empty password | агент запущен | повторный password prompt, вход не выполняется | нет | confirmed-runtime |
| Startup с явным host key, `8.5.1.1150` | `1cv8 DESIGNER /F <ib> /AgentMode /AgentListenAddress 127.0.0.1 /AgentPort 1552 /AgentBaseDir <dir> /AgentSSHHostKey <key>` | scratch file ИБ | процесс держит SSH порт `127.0.0.1:1552` | артефакты не создаются до команд | confirmed-runtime |
| SSH shell без PTY, `8.5.1.1150` | `ssh -T -p 1552 agenttest@127.0.0.1` | агент запущен, валидные SSH/ИБ credentials | banner `1C Designer Shell`, prompt `designer>` | нет | confirmed-runtime |
| SSH shell с PTY, `8.5.1.1150` | `ssh -tt -p 1551 agenttest@127.0.0.1` | агент запущен | `PTY allocation request failed on channel 0` | нет | confirmed-runtime |
| Второй SSH shell-клиент, `8.5.1.1150` | второй `ssh -T -p 1553 ...` при уже открытом shell | один shell уже подключен | `shell request failed on channel 0` | нет | confirmed-runtime |
| `help`, `8.5.1.1150` | `help` в `designer>` | shell session | stdout shell | нет | confirmed-runtime |
| `help --version`, `8.5.1.1150` | `help --version` | shell session | stdout shell, text или JSON | нет | confirmed-runtime |
| `options list/get/set`, `8.5.1.1150` | `options list`, `options get --output-format`, `options set --output-format=json` | shell session | stdout shell | нет | confirmed-runtime |
| JSON output switch, `8.5.1.1150` | `options set --output-format=json` | shell session | JSON array в stdout shell | нет | confirmed-runtime |
| Hidden prompt mode, `8.5.1.1150` | `options set --show-prompt=no` | shell session | ответы продолжают идти в stdout без `designer>` | нет | confirmed-runtime |
| Unknown command error, `8.5.1.1150` | `abracadabra` | shell session, JSON output | JSON array с `type: error`, `error-type: CommandFormatError` | нет | confirmed-runtime |
| `common connect-ib`, `8.5.1.1150` | `common connect-ib` | shell session | JSON success в stdout shell | нет | confirmed-runtime |
| Повторный `common connect-ib`, `8.5.1.1150` | `common connect-ib` при уже активном подключении | shell session, JSON output | JSON error `DesignerAlreadyConnectedToInfoBase` | нет | confirmed-runtime |
| `infobase-tools debug-info`, `8.5.1.1150` | `infobase-tools debug-info` | shell session после `common connect-ib` | JSON success с `enabled`, `protocol`, `server-address` | нет | confirmed-runtime |
| `common disconnect-ib`, `8.5.1.1150` | `common disconnect-ib` | shell session | JSON success в stdout shell | нет | confirmed-runtime |
| `common disconnect-ib` без подключения, `8.5.1.1150` | `common disconnect-ib` без активного подключения | shell session, JSON output | JSON error `DesignerNotConnectedToInfoBase` | нет | confirmed-runtime |
| `common shutdown`, `8.5.1.1150` | `common shutdown` | shell session | SSH session closes after command | нет | confirmed-runtime |
| SFTP session, `8.5.1.1150` | `sftp -P 1552 agenttest@127.0.0.1` | агент запущен, валидные credentials | `Connected to 127.0.0.1`, `Remote working directory: /` | каталог `/`, `ls` в пустом стенде без вывода | confirmed-runtime |
| Два SFTP-клиента одновременно, `8.5.1.1150` | два параллельных `sftp -P 1555 ...` | агент запущен, валидные credentials | оба клиента получают `sftp>` и `Remote working directory: /` | каталог `/` для обоих клиентов | confirmed-runtime |
| `config ...` help surface, `8.5.1.1150` | `help config` | shell session | stdout shell | нет | confirmed-runtime |
| `infobase-tools ...` help surface, `8.5.1.1150` | `help infobase-tools` | shell session | stdout shell | нет | confirmed-runtime |
| JSON request/response file mode | неизвестно | shell session или docs/help | не достигнуто | не достигнуто | unconfirmed |

## Ключевые наблюдения

- На `8.3.27.1859` SSH banner сервера: `libssh_0.9.6`.
- Для `/AgentSSHHostKey` сервер публикует ED25519 host key из явно переданного файла.
- Для `/AgentSSHHostKeyAuto` сервер публикует RSA host key из `~/.1cv8/1C/1cv8/host_id`.
- В проверенном стенде агент не предоставил anonymous auth и не принял пустой пароль.
- На `8.5.1.1150` shell доступен только без PTY и работает как stdin/stdout session с prompt `designer>`.
- На `8.5.1.1150` интерактивный shell фактически одиночный: второй SSH shell-клиент не допускается, но SFTP-сеансы можно держать параллельно.
- После `options set --output-format=json` shell возвращает JSON-массивы, а не одиночные JSON-объекты.
- После `options set --show-prompt=no` приглашение исчезает, но stdin/stdout контракт сохраняется.
- `help config` выводит предупреждение `The infobase user is not authenticated`, но help по командам группы при этом все равно доступен.
- `common connect-ib` в проверенном стенде завершился успешно без отдельного prompt к учетным данным ИБ внутри shell.

## Вывод для spec

- Для `8.3.27.1859` подтверждена только transport/auth часть startup и SSH/SFTP handshake.
- Для `8.5.1.1150` подтвержден полноценный shell-like request surface агента и базовый набор команд `help`, `common`, `options`, `infobase-tools`.
- Для `config/*` и разрушающих `infobase-tools/*` пока подтверждена только help/runtime-discovery surface, а не выполнение самих операций.
