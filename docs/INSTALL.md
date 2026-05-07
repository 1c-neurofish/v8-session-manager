# Установка `v8-session-manager` как сервиса

Документ описывает установку `v8-session-manager` с автозапуском на трёх ОС. Бинарь и конфиг везде одинаковые; различается способ управления процессом.

| ОС | Способ автозапуска | Раздел |
|----|-------------------|--------|
| Linux | systemd unit | [Linux (systemd)](#linux-systemd) |
| Windows | Windows Service через [NSSM](https://nssm.cc/) (рекомендуется) или нативный `sc.exe` | [Windows](#windows) |
| macOS | launchd (LaunchAgent или LaunchDaemon) | [macOS (launchd)](#macos-launchd) |

После установки — обязательно [Проверка работоспособности](#проверка-работоспособности).

---

## Linux (systemd)

Установка под выделенным системным пользователем `v8sm` с конфигом в `/etc/v8-session-manager/v8sm.yaml` и рабочим каталогом `/var/lib/v8-session-manager`.

### 1. Сборка

```bash
cargo build --release
```

### 2. Установка бинаря и юнита

```bash
sudo install -m 0755 target/release/v8-session-manager /usr/local/bin/
sudo install -m 0644 systemd/v8-session-manager.service /etc/systemd/system/
```

### 3. Системный пользователь и каталоги

```bash
sudo useradd -r -s /usr/sbin/nologin -d /var/lib/v8-session-manager v8sm
sudo install -d -o v8sm -g v8sm /var/lib/v8-session-manager
sudo install -d -o root  -g v8sm -m 0750 /etc/v8-session-manager
```

### 4. Конфиг

```bash
sudo install -m 0640 -o root -g v8sm \
    etc/v8-session-manager/v8sm.yaml \
    /etc/v8-session-manager/v8sm.yaml
```

Содержимое `etc/v8-session-manager/v8sm.yaml` — production-baseline: bind на loopback, `workPath: /var/lib/v8-session-manager`, метрики выключены. Подробности — [`docs/CONFIGURATION.md`](CONFIGURATION.md).

### 5. Запуск и проверка

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now v8-session-manager
sudo systemctl status v8-session-manager
journalctl -u v8-session-manager -f
```

### Обновление

```bash
cargo build --release
sudo install -m 0755 target/release/v8-session-manager /usr/local/bin/
sudo systemctl restart v8-session-manager
```

---

## Windows

Рекомендуемый путь — [NSSM (Non-Sucking Service Manager)](https://nssm.cc/). NSSM умеет редиректить stdout/stderr в файлы, что критично, потому что `v8-session-manager` логирует в stdout.

### 1. Сборка

```text
cargo build --release
```

Бинарь: `target\release\v8-session-manager.exe`.

### 2. Раскладка файлов

Стандартные пути (можно скорректировать под политику организации):

```text
C:\Program Files\v8-session-manager\v8-session-manager.exe
C:\ProgramData\v8-session-manager\v8sm.yaml
C:\ProgramData\v8-session-manager\logs\
C:\ProgramData\v8-session-manager\state\
```

```text
mkdir "C:\Program Files\v8-session-manager"
mkdir "C:\ProgramData\v8-session-manager"
mkdir "C:\ProgramData\v8-session-manager\logs"
mkdir "C:\ProgramData\v8-session-manager\state"

copy target\release\v8-session-manager.exe "C:\Program Files\v8-session-manager\"
copy etc\v8-session-manager\v8sm.yaml      "C:\ProgramData\v8-session-manager\v8sm.yaml"
```

Пример конфига для Windows (`C:\ProgramData\v8-session-manager\v8sm.yaml`):

```yaml
workPath: C:\ProgramData\v8-session-manager\state

mcp:
  session_manager:
    bind_address: "127.0.0.1:4000"
    path: "/sessions"
    idle_timeout_secs: 1800
    reconnection_grace_secs: 30
    ws_ping_interval_ms: 20000
    ws_ping_timeout_ms: 30000

  http:
    bind_address: "127.0.0.1:4001"
    path: "/mcp"
    stateful_sessions: true
    max_sessions: 64
    idle_ttl_secs: 900

  execution:
    shutdown_grace_period_secs: 30
```

### 3. Установка через NSSM

```text
nssm install V8SessionManager "C:\Program Files\v8-session-manager\v8-session-manager.exe"
nssm set V8SessionManager AppParameters --config "C:\ProgramData\v8-session-manager\v8sm.yaml"
nssm set V8SessionManager AppDirectory  "C:\ProgramData\v8-session-manager"
nssm set V8SessionManager Start         SERVICE_AUTO_START
nssm set V8SessionManager AppStdout     "C:\ProgramData\v8-session-manager\logs\v8sm.out.log"
nssm set V8SessionManager AppStderr     "C:\ProgramData\v8-session-manager\logs\v8sm.err.log"
nssm set V8SessionManager AppRotateFiles 1
nssm set V8SessionManager AppRotateBytes 10485760
nssm set V8SessionManager AppExit       Default Restart
```

Запуск:

```text
nssm start V8SessionManager
nssm status V8SessionManager
```

Просмотр логов: открыть `C:\ProgramData\v8-session-manager\logs\v8sm.out.log` и `v8sm.err.log`.

Удаление:

```text
nssm stop V8SessionManager
nssm remove V8SessionManager confirm
```

### 4. Альтернатива: нативный `sc.exe`

Работает, но не имеет родной поддержки stdout-в-файл — логи будут идти в Event Log только если процесс сам пишет туда (а он пишет в stdout). Без NSSM или аналогичного шима эта схема пригодна только для разработческой проверки:

```text
sc create V8SessionManager ^
    binPath= "\"C:\Program Files\v8-session-manager\v8-session-manager.exe\" --config \"C:\ProgramData\v8-session-manager\v8sm.yaml\"" ^
    start= auto ^
    DisplayName= "v8-session-manager"
sc start V8SessionManager
```

Для production предпочтительнее NSSM.

---

## macOS (launchd)

На macOS используется launchd. Два варианта: `~/Library/LaunchAgents/...plist` (для текущего пользователя) или `/Library/LaunchDaemons/...plist` (system-wide, требует root). Ниже — system-wide.

### 1. Сборка

```bash
cargo build --release
```

### 2. Раскладка файлов

```bash
sudo install -m 0755 target/release/v8-session-manager /usr/local/bin/

sudo mkdir -p /usr/local/etc/v8-session-manager
sudo install -m 0644 etc/v8-session-manager/v8sm.yaml \
    /usr/local/etc/v8-session-manager/v8sm.yaml

sudo mkdir -p /usr/local/var/v8-session-manager
sudo mkdir -p /usr/local/var/log/v8-session-manager
```

В `/usr/local/etc/v8-session-manager/v8sm.yaml` поправьте `workPath: /usr/local/var/v8-session-manager`.

### 3. plist

Сохранить как `/Library/LaunchDaemons/com.v8sm.session-manager.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.v8sm.session-manager</string>

    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/v8-session-manager</string>
        <string>--config</string>
        <string>/usr/local/etc/v8-session-manager/v8sm.yaml</string>
    </array>

    <key>WorkingDirectory</key>
    <string>/usr/local/var/v8-session-manager</string>

    <key>RunAtLoad</key>
    <true/>

    <key>KeepAlive</key>
    <true/>

    <key>StandardOutPath</key>
    <string>/usr/local/var/log/v8-session-manager/v8sm.out.log</string>

    <key>StandardErrorPath</key>
    <string>/usr/local/var/log/v8-session-manager/v8sm.err.log</string>
</dict>
</plist>
```

Права:

```bash
sudo chown root:wheel /Library/LaunchDaemons/com.v8sm.session-manager.plist
sudo chmod 0644       /Library/LaunchDaemons/com.v8sm.session-manager.plist
```

### 4. Запуск

```bash
sudo launchctl bootstrap system /Library/LaunchDaemons/com.v8sm.session-manager.plist
sudo launchctl enable system/com.v8sm.session-manager
sudo launchctl kickstart -k system/com.v8sm.session-manager
```

Проверка статуса:

```bash
sudo launchctl print system/com.v8sm.session-manager | grep -E "state|pid|last exit"
tail -f /usr/local/var/log/v8-session-manager/v8sm.out.log
```

Остановка / удаление:

```bash
sudo launchctl bootout system/com.v8sm.session-manager
sudo rm /Library/LaunchDaemons/com.v8sm.session-manager.plist
```

### Вариант LaunchAgent (для одного пользователя)

Положить plist в `~/Library/LaunchAgents/com.v8sm.session-manager.plist`, заменить `system` на `gui/$(id -u)` в командах `launchctl`. Пути workPath/конфига перенести в `~/Library/Application Support/v8-session-manager/` и `~/.config/v8-session-manager/`.

---

## Проверка работоспособности

После запуска (на любой ОС) — два запроса MCP HTTP:

```bash
# 1. initialize
curl -sS -X POST http://127.0.0.1:4001/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize",
       "params":{"protocolVersion":"2025-03-26","capabilities":{},
                 "clientInfo":{"name":"curl","version":"0"}}}'

# 2. tools/list — без подключённых клиентов вернётся только session_list
curl -sS -X POST http://127.0.0.1:4001/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
```

Ожидаемый ответ на `tools/list` (фрагмент):

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "tools": [
      {
        "name": "session_list",
        "description": "..."
      }
    ]
  }
}
```

Если 1С-клиент уже подключён к `:4000/sessions` с `mcpMode=ws`, в `tools/list` появятся проксированные тулы с префиксом `<prefix>__<tool>`.

Бинарный ping WS-эндпоинта простым curl невозможен — там WebSocket handshake; проверьте подключением реального клиента или `wscat`:

```bash
wscat -c ws://127.0.0.1:4000/sessions
```

Если приходит первый кадр от менеджера (или соединение держится без ошибок) — WS-листенер живой.
