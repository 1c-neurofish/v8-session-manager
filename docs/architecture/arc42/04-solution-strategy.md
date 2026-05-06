## 4. Стратегия решения

Архитектура следует слоистой модели оркестрации.

Ключевые решения и целевые контракты:

- CLI и MCP остаются тонкими адаптерами над транспортно-нейтральными use case.
- `v8project.yaml` и typed `AppConfig` являются главным конфигурационным контрактом; unsafe/unsupported combinations должны отклоняться на validation boundary.
- Прямое взаимодействие с инструментами 1С инкапсулировано в выделенных платформенных адаптерах.
- Анализ изменений используется для предпочтения инкрементальной работы вместо полного rebuild.
- Структурированные типы результатов сохраняются до границы адаптера, а затем рендерятся отдельно для CLI и MCP.
- MCP рассматривается не только как транспорт: он добавляет сессии, параллелизм, нормализацию, admission control и обработку транспортных ошибок.
- Публичные команды над одним canonical `workPath` сериализуются через workspace lock; nested flows используют явные unlocked entrypoints только под внешним lock.
- Timeout/cancellation являются общим CLI/MCP целевым контрактом и не должны возвращаться наружу до terminal state underlying operation.
- Full replacement `dump` и `artifacts` публикуются через staging/backup, чтобы platform failure до publish сохранял старый target.
- Runner-like сценарии используют общий execution grammar: pipeline vocabulary, step entries и `ExecutionOutcome<T>` как canonical domain outcome.
- Общая интерактивная EDT-сессия переиспользуется только для живого пути MCP `check_syntax_edt`.
- CLI EDT execution тоже умеет interactive-режим, но не использует MCP shared session manager и остаётся отдельным execution path.
- Архитектура оптимизирована под agent-friendly contracts: use case возвращают transport-neutral DTO и структурированные failure payload, а логика представления остаётся на границе адаптера.

Эта стратегия удерживает публичную поверхность стабильной и позволяет независимо развивать платформенное поведение и транспортные правила.
