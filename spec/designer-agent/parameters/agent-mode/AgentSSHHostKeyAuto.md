# AgentSSHHostKeyAuto

## Назначение

Переключает агент на автоматический выбор server host key из стандартного расположения платформы.

## Синтаксис

```text
/AgentSSHHostKeyAuto
```

## Параметры

Нет.

## Связи

- Для Linux локальная help-страница указывает путь `~/.1cv8/1C/1cv8/host_id`.
- Если файл отсутствует, help обещает автоматическое создание RSA-ключа длиной `2048` бит.
- Runtime подтвердил, что fingerprint host key агента совпадает с `/home/alko/.1cv8/1C/1cv8/host_id`.

## Примечания

- `host_id` участвует как host key сервера и сам по себе не снимает требование SSH userauth `password`.
