# Designer Startup Specs

Краткий индекс параметров запуска `1cv8` для AI-агентов.

## Базовый контракт

- Режимы запуска: `DESIGNER`, `ENTERPRISE`, `CREATEINFOBASE`.
- Обзор выбора режима: `mode-selection/ZIF1.md`.
- Машиночитаемый индекс: `manifest.json`.
- Смежный batch-справочник: `../designer-batch/README.md`.

## Группы параметров

### Использование клиентских сертификатов (только для тонкого клиента)

- `HttpsCA` -> `common-parameters/client-certificates/HttpsCA.md`
- `HttpsCert` -> `common-parameters/client-certificates/HttpsCert.md`
- `HttpsForceTLS1_0` -> `common-parameters/client-certificates/HttpsForceTLS1_0.md`
- `HttpsForceTLS1_1` -> `common-parameters/client-certificates/HttpsForceTLS1_1.md`
- `HttpsForceTLS1_2` -> `common-parameters/client-certificates/HttpsForceTLS1_2.md`

### Настройка аутентификации

- `AccessToken` -> `common-parameters/authentication/AccessToken.md`
- `Authoff` -> `common-parameters/authentication/Authoff.md`
- `EmailAuth` -> `common-parameters/authentication/EmailAuth.md`
- `N` -> `common-parameters/authentication/N.md`
- `NoProxy` -> `common-parameters/authentication/NoProxy.md`
- `OIDA` -> `common-parameters/authentication/OIDA.md`
- `P` -> `common-parameters/authentication/P.md`
- `Proxy` -> `common-parameters/authentication/Proxy.md`
- `ResetSavedAuth` -> `common-parameters/authentication/ResetSavedAuth.md`
- `SAOnRestart` -> `common-parameters/authentication/SAOnRestart.md`
- `WA` -> `common-parameters/authentication/WA.md`
- `WSA` -> `common-parameters/authentication/WSA.md`
- `WSN` -> `common-parameters/authentication/WSN.md`
- `WSP` -> `common-parameters/authentication/WSP.md`

### Настройки интерфейса

- `DisableHomePageForms` -> `common-parameters/interface/DisableHomePageForms.md`
- `i85` -> `common-parameters/interface/i85.md`
- `iTaxi` -> `common-parameters/interface/iTaxi.md`
- `itdi` -> `common-parameters/interface/itdi.md`
- `TechnicalSpecialistMode` -> `common-parameters/interface/TechnicalSpecialistMode.md`

### Настройки локализации

- `L` -> `common-parameters/localization/L.md`
- `VL` -> `common-parameters/localization/VL.md`

### Настройки отладки

- `Debug` -> `common-parameters/debugging/Debug.md`
- `DebuggerURL` -> `common-parameters/debugging/DebuggerURL.md`
- `DisplayPerformance` -> `common-parameters/debugging/DisplayPerformance.md`
- `SimulateServerCallDelay` -> `common-parameters/debugging/SimulateServerCallDelay.md`

### Настройки тестирования

- `TestClient` -> `common-parameters/testing/TestClient.md`
- `TestManager` -> `common-parameters/testing/TestManager.md`
- `UILogRecorder` -> `common-parameters/testing/UILogRecorder.md`

### Определение режима запуска

- `AppArch` -> `common-parameters/launch-mode/AppArch.md`
- `AppAutoCheckMode` -> `common-parameters/launch-mode/AppAutoCheckMode.md`
- `AppAutoCheckVersion` -> `common-parameters/launch-mode/AppAutoCheckVersion.md`
- `RunModeManagedApplication` -> `common-parameters/launch-mode/RunModeManagedApplication.md`
- `RunModeOrdinaryApplication` -> `common-parameters/launch-mode/RunModeOrdinaryApplication.md`

### Проверки во время работы клиентского приложения

- `EnableCheckExtensionsAndAddInsSyncCalls` -> `common-parameters/runtime-checks/EnableCheckExtensionsAndAddInsSyncCalls.md`
- `EnableCheckModal` -> `common-parameters/runtime-checks/EnableCheckModal.md`
- `EnableCheckScriptCircularRefs` -> `common-parameters/runtime-checks/EnableCheckScriptCircularRefs.md`
- `EnableCheckServerCalls` -> `common-parameters/runtime-checks/EnableCheckServerCalls.md`

### Прочие параметры

- `@` -> `common-parameters/misc/at.md`
- `AllowExecuteScheduledJobs` -> `common-parameters/misc/AllowExecuteScheduledJobs.md`
- `AppAutoInstallLastVersion` -> `common-parameters/misc/AppAutoInstallLastVersion.md`
- `C` -> `common-parameters/misc/C.md`
- `ClearCache` -> `common-parameters/misc/ClearCache.md`
- `DisableBackgroundIndexBuild` -> `common-parameters/misc/DisableBackgroundIndexBuild.md`
- `DisableSplash` -> `common-parameters/misc/DisableSplash.md`
- `DisableStartupDialogs` -> `common-parameters/misc/DisableStartupDialogs.md`
- `DisableStartupMessages` -> `common-parameters/misc/DisableStartupMessages.md`
- `DisableUnrecoverableErrorMessage` -> `common-parameters/misc/DisableUnrecoverableErrorMessage.md`
- `DisplayUserNotificationList` -> `common-parameters/misc/DisplayUserNotificationList.md`
- `Execute` -> `common-parameters/misc/Execute.md`
- `Out` -> `common-parameters/misc/Out.md`
- `RunShortcut` -> `common-parameters/misc/RunShortcut.md`
- `TComp` -> `common-parameters/misc/TComp.md`
- `UC` -> `common-parameters/misc/UC.md`
- `URL` -> `common-parameters/misc/URL.md`
- `UseHwLicenses` -> `common-parameters/misc/UseHwLicenses.md`
- `UsePrivilegedMode` -> `common-parameters/misc/UsePrivilegedMode.md`

### Указание параметров подключения

- `F` -> `common-parameters/connection/F.md`
- `IBConnectionString` -> `common-parameters/connection/IBConnectionString.md`
- `IBName` -> `common-parameters/connection/IBName.md`
- `O` -> `common-parameters/connection/O.md`
- `S` -> `common-parameters/connection/S.md`
- `SLev` -> `common-parameters/connection/SLev.md`
- `WS` -> `common-parameters/connection/WS.md`
- `Z` -> `common-parameters/connection/Z.md`

## Правила чтения

- В каждой карточке оставлены назначение, синтаксис, связи и важные примечания.
- `Связи` фиксируют ограничения, режимы совместного использования и значения по умолчанию.
- Метаданные источника (group/pagePath/sourceUrl) сохранены в `manifest.json`.
