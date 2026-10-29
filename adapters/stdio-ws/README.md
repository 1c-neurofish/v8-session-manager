# SDK stdio ↔ WS adapters

Canonical Python adapter для двух внешних SDK:

- Testpilot: official MCP SDK stdio → WS registration → v8-session-manager → DataMask.
- Debugger: тот же transport/lifecycle и полный опубликованный inventory16 из отдельного `1c-debug-mcp` SDK под именами `debug_*`.

Rust manager core не меняется. Адаптер не запускает1C и не содержит бизнеслогику. Зависимости Python: `mcp==1.30.0`, `jsonschema==4.26.0`, `websockets==15.0.1`; установлены отдельно по [requirements.txt](requirements.txt). Происхождение, лицензия и внешние SDK: [NOTICE.md](NOTICE.md).

## Расположение и qualified deployment

Исходники находятся в `v8-session-manager/adapters/stdio-ws`. В GBIG PAM старый `scripts/testpilot-adapter` является относительным compatibility directory symlink на этот каталог. Это сохраняет прежние imports/команды/installed launchers без второй реализации.

Текущий deployment квалифицирован для GBIG DEV; relocation не обобщает эти параметры:

| Параметр | Текущее значение/источник |
|---|---|
| DEV base/cluster server | `gbig_pam_ai` / `onec-infra` |
| Debug server / alias | `http://onec-infra:1550` / проверенный private binding |
| Debugger SDK | отдельный `/workspaces/work/repos/1C Framework/1c-debug-mcp/dist/index.js` |
| Manager URL/auth | существующая таблица `v8-session-manager` в `/home/vscode/.codex/config.toml` |
| Testpilot state | `/home/vscode/.local/state/testpilot-adapter/gbig-pam` |
| Debugger private state | owner-managed private directory вне repository |
| Testpilot SDK/venv | `/home/vscode/.local/share/1c-testpilot/venv` |

Private configs, receipts, credentials, dictionaries и runtime payloads в repository не входят. Существующий private Testpilot canary `profiles_file` имеет прежний exact path/digest guard и зависит от GBIG project CWD; запуск этого optional режима сохраняет прежний CWD. Сам private файл не переносится. Runtime/установленные units и supervisor не изменяются при relocation.

## Testpilot

Default CLI profile — Testpilot. Публичные группы: `testpilot_session`, `testpilot_app`, `testpilot_window`, `testpilot_field`, `testpilot_doc`, `testpilot_table`, `testpilot_calendar`, `testpilot_form`, `testpilot_scenario`, `testpilot_find`. Upstream `tc_*` имена остаются внутри SDK/action policy. Default action policy — deny-all; caller не может выбрать чужой TestClient.

```sh
PY=/home/vscode/.local/share/1c-testpilot/venv/bin/python
# Можно использовать absolute canonical path либо прежний GBIG compatibility path.
"$PY" adapters/stdio-ws/provider.py --config /absolute/private/adapter.json
```

Формат service bootstrap: [service-config.example.json](service-config.example.json). Activation config дополнительно содержит qualified binding/owner receipts и action-policy file. Receipts связывают настоящий native UID/IBSID и OWN PID/birth/listener; Testpilot `connection_id` не является native session number. Пустой connection_id допускается только до штатного connect. Перед register и effect независимо проверяются private frozen receipts и fresh public manager session_list.

Постоянный `service.py serve` держит OWN SDK в STANDBY без запуска native1C. `activate` требует свежие private owner/binding receipts; второй owner получает BUSY. STANDBY не означает готовность UI или классификации DataMask.

```sh
"$PY" adapters/stdio-ws/service.py health
"$PY" adapters/stdio-ws/service.py activate --config /absolute/private/adapter.json
```

Для standalone Testpilot provider общий project lock требует штатный `pause` → `stop`/reap существующего service. `resume` и start hook выполняются владельцем только после завершения standalone. У relocation нет оснований выполнять эти операции. Прежние GBIG deployment команды и ownership protocol остаются в project `docs/tooling/testpilot.md`.

## Debugger: полный native API

```sh
"$PY" adapters/stdio-ws/provider.py --profile debugger --config /absolute/private/debugger-config.json
```

[debugger-config.example.json](debugger-config.example.json) содержит намеренно неактивируемые pin placeholders. Actual полный SDK descriptor digest считается `provider.descriptor_digest` (tools сортируются по name), entry pin — SHA256 actual dist/index.js. Смена inventory/schema закрывает route. Metadata source paths `ONEC_CF_PATH`, `ONEC_CFE_PATHS`, `ONEC_EPF_PATHS` переиспользуются из existing SDK config для reload; произвольный env не наследуется.

Private binding0600 содержит schema_version1, generation, expires_at, native_session_id, infobase_name, cluster_server, ib_session_number, endpoint, alias, endpoint_dev_verified и allowed_target_ids. Endpoint/alias→DEV mapping подтверждается владельцем; native session проверяется через manager. Frozen receipt не изменяется на месте: новое admission требует нового поколения. SDK RDBG UI session ID хранится private; public attach возвращает status.

Полный inventory:

| SDK | Public manager |
|---|---|
| attach | debug_attach |
| detach | debug_detach |
| get_targets | debug_get_targets |
| set_breakpoints | debug_set_breakpoints |
| clear_breakpoints | debug_clear_breakpoints |
| continue | debug_continue |
| step_in | debug_step_in |
| step_out | debug_step_out |
| pause | debug_pause |
| wait_for_stop | debug_wait_for_stop |
| raw_request | debug_raw_request |
| reload_metadata | debug_reload_metadata |
| get_call_stack | debug_get_call_stack |
| get_variables | debug_get_variables |
| evaluate | debug_evaluate |
| get_performance_measurement | debug_get_performance_measurement |

Readonly discovery показывает metadata собственной проверенной DEV alias/UI session через DataMask. Пустой private target allowlist сохраняет discovery, но не разрешает typed control/inspection targets. Caller не меняет endpoint/alias/credentials/base. Native schemas сохраняются; attach arguments подставляются private. `get_performance_measurement.targetId` optional и не требует breakpoint/previous stop.

Raw request сохраняет native cmd/xml schema и namespace declarations. Alias/UI fields и dbgui подставляются из private fixed DEV/actual SDK attach UUID; caller не обязан знать этот UUID. Остальной XML и measureModeSeanceID сохраняются. ON/OFF measurement выполняются существующим `setMeasureMode` (OFF без measureModeSeanceID), async результат хранится existing SDK cache и возвращается getter. Нового action-wrapper вместо native API нет. Cleanup собственной активной measurement пытается OFF, затем обязательно own detach; native1C/чужие процессы не завершаются.

## Маскирование и lifecycle

Все data-bearing представления результата собираются в один `schema_version=1/result/field_sources={}` text envelope для штатного manager finalize/DataMask. Raw дубль вне конверта и unmasked fallback отсутствуют. Source/lineage по переменным не выводится, unknown source не означает blanket mask. Actual JSON keys сохраняются для existing secret-name cut; BSL Variable.name не превращается в sensitive JSON key.

Embedded JSON/XML раскрываются ограниченно: результат1MiB, строка256KiB, depth24,20000nodes. XML становится JSON с decoded text/attributes; DTD/entity declarations и binary/resource blocks отклоняются. Dictionary применяется к строковым values, не к JSON keys/числовым scalar; произвольное намеренное кодирование выражения не обещается.

Correlation/cancel используют фактические SDK request IDs. Timeout/cancel закрывает dispatch без автоматического повторения эффектов. Exceptions имеют fixed safe messages; child stderr направлен в devnull. Сам debugger SDK имеет собственный file logger: devnull его не отключает, adapter этот файл не читает/не публикует. SDK async measurement cache должен быть reviewed memory-only build; raw payload до DataMask не сохраняется этим adapter.

## Offline verification

Из repository root, в окружении с requirements:

```sh
python -m unittest discover -s adapters/stdio-ws/tests -p 'test_*.py'
python adapters/stdio-ws/tests/check_debugger_profile.py
# Дополнительно фактический public tools/list snapshot отдельного SDK:
python adapters/stdio-ws/tests/check_debugger_profile.py --sdk-inventory /path/to/public-inventory.json
```

41 существующий test и64 функциональные проверки используют настоящий MCP ClientSession над memory streams. Они не запускают manager/SDK child/1C и не читают private configs. Bundled methods-inventory.json исключает зависимость тестов от GBIG tasks. Actual inventory snapshot optional; он не входит в source и не требует private fixture.

`deploy/` содержит существующие qualified container templates/patches и launchers. Не применять целиком Dockerfile/entrypoint snapshots поверх чужого WIP. Installed launchers пока используют GBIG compatibility path; отдельное runtime deployment выполняет владелец. Offline relocation verification не является новым reboot/runtime acceptance.
