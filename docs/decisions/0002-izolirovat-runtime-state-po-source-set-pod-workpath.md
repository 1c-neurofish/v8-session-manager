> **Superseded by [ADR-0033](0033-extract-v8-session-manager-from-v8-runner.md).** Документ относится к скоупу форка `v8-runner`, который вырезан из проекта. Текст сохранён как исторический след.

# ADR-0002: Изолировать runtime state по source-set под workPath

- Статус: `accepted`
- Дата: `2026-04-20`

## Контекст

`v8-runner` выполняет операции над основной конфигурацией, расширениями, EDT-проектами и сгенерированными Designer-файлами.
Эти операции используют анализ изменений, временные списки partial load/dump, платформенные логи и служебные рабочие каталоги.

Если хранить состояние изменений глобально на весь проект или писать runtime-артефакты рядом с исходниками, появляются риски:

1. смешать состояние основной конфигурации, расширений и generated Designer output;
2. принять partial/full load decision по неправильному представлению исходников;
3. повредить пользовательские исходники временными файлами;
4. потерять различие между EDT-файлами как source of truth и Designer-файлами как форматом загрузки в ИБ.

## Решение

Принять `source-set` как минимальную единицу оркестрации, а `workPath` как единственный корень runtime-состояния и временных артефактов.

### DESIGNER format

Для `format=DESIGNER` состояние изменений одноуровневое:

```text
Designer source-set
  -> redb state: designer-<sourceSetName>
  -> partial/full load decision
  -> load via DESIGNER or IBCMD
```

На каждый `source-set` используется один логический `redb` context, потому что исходники уже находятся в формате, который может быть загружен backend-ом.

### EDT format

Для `format=EDT` состояние изменений двухуровневое:

```text
EDT source-set
  -> redb state: edt-<sourceSetName>
  -> decide whether EDT export is needed
  -> export to workPath/designer/<sourceSetName>
  -> redb state: designer-<sourceSetName>
  -> partial/full load decision
  -> load generated Designer files via DESIGNER or IBCMD
```

Правила:

1. `edt-<sourceSetName>` хранит состояние основных EDT-исходников из `basePath/source-set.path`.
2. `edt-<sourceSetName>` используется только для решения, нужна ли конвертация/export EDT source-set в Designer-формат.
3. `designer-<sourceSetName>` хранит состояние generated Designer-файлов под `workPath/designer/<sourceSetName>`.
4. `designer-<sourceSetName>` используется для решения, какие Designer-файлы грузить: partial или full.
5. Partial/full load decision всегда принимается по Designer-format context, потому что backend загрузки работает с Designer-представлением.

### workPath

Все runtime-артефакты должны находиться под `workPath`, включая:

1. platform logs;
2. temp-файлы partial load/dump;
3. YaXUnit и Vanessa Automation run artifacts;
4. `redb` hash storages;
5. generated Designer output для EDT flow.

## Неграницы (Non-goals)

1. Не вводить единый глобальный hash storage на весь проект.
2. Не писать runtime-артефакты в `basePath` или каталоги пользовательских исходников.
3. Не считать `workPath/designer/<sourceSetName>` пользовательскими исходниками.
4. Не обещать атомарность `build` по нескольким `source-set`.
5. Не менять публичную YAML-модель `source-set` без отдельного решения.

## Последствия

1. Изменения в layout `workPath` или именовании change-detection contexts являются архитектурными изменениями и должны синхронизироваться с этим ADR.
2. `source-set.name` влияет на runtime paths и имена storage contexts, поэтому имена должны оставаться безопасными для путей и уникальными.
3. EDT flow обязан различать исходные EDT-файлы и generated Designer-файлы: первый контур управляет export step, второй контур управляет load step.
4. При сбоях или небезопасных условиях partial load должен деградировать в full load, а не пытаться выполнить потенциально неполную загрузку.

## План реализации

Текущее состояние кода уже следует этому решению:

1. `src/config/model.rs` описывает `source-set`, `workPath`, `format` и `builder`.
2. `src/config/validate.rs` валидирует уникальность и безопасность `source-set` и рабочие ограничения.
3. `src/change_detection/source_sets.rs` создает контексты `designer-<sourceSetName>` и `edt-<sourceSetName>`.
4. `src/change_detection/hash_storage.rs` хранит состояние в `redb`.
5. `src/change_detection/partial_load.rs` принимает partial/full decision по Designer-файлам.
6. `src/use_cases/build_project.rs` использует EDT context для export decision и Designer context для load decision.

При дальнейших изменениях:

1. новые сценарии, которым нужно состояние изменений, должны использовать per-source-set context, а не глобальный storage;
2. новые runtime-артефакты должны размещаться под `workPath`;
3. EDT-сценарии должны явно выбирать, работают они с `edt-*` context или `designer-*` context.

## Верификация

- [x] Для `format=DESIGNER` существует один Designer context на `source-set`.
- [x] Для `format=EDT` существуют два context на `source-set`: `edt-<sourceSetName>` и `designer-<sourceSetName>`.
- [x] Generated Designer output для EDT находится под `workPath/designer/<sourceSetName>`.
- [x] Partial/full load decision выполняется по Designer-format context.
- [x] `redb` storage используется как per-context persisted state, а не как единый глобальный индекс.
