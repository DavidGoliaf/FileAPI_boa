# Заказ M4-B — `FileReaderSync` для worker environments

| Поле | Значение |
|---|---|
| ID | `M4B-FILEREADER-SYNC` |
| База | Принятый M4-A final commit `88fe48725e473f4a6ce62e46fe9e77d78a3e5de2` |
| Ветка | `task/m4b`, от `main` через базу M4-A; не работать напрямую в `main` |
| Нормативная база | `TZ_boa_fapi_FileAPI.md` §5.6, §7.6, §9, §10.2, §12.4, §15 |
| Зависимости | M4-A DOM shim, `DOMException`, `BlobData::materialize`, existing limits/brands |
| Результат | Worker-only `FileReaderSync` для memory-backed `Blob`/`File`, без изменения async `FileReader` |

## 1. Цель

Добавить ограниченную нормативную поверхность `FileReaderSync`. Она
доступна только в явно заданных `DedicatedWorker` и `SharedWorker`
environment descriptors. В `Window` и `ServiceWorker` глобальное имя
`FileReaderSync` отсутствует. Хост не имеет права выводить worker-режим из
потока исполнения или включать его автоматически.

Заказ продолжает memory-only границу M4-A. Filesystem-backed sources,
snapshot validation, Blob URL, structured clone, полный DOM/Workers runtime,
WPT harness и M5 не входят в scope.

## 2. Жёсткие ограничения

1. Не менять `TZ_boa_fapi_FileAPI.md`, M1–M4-A orders, уже принятые matrix
   IDs и acceptance thresholds.
2. Не ослаблять M4-A tests, guards, generation/event semantics или central
   DOMException mapping. Async `FileReader` должен остаться совместимым по
   наблюдаемой семантике.
3. Не добавлять `unsafe`, production `unwrap`/`expect`/`panic`, background
   threads, Tokio, собственный executor или JS-вызовы из source completion.
4. Не добавлять filesystem, URL, clone или full-DOM API под видом подготовки
   к будущим этапам.
5. Новая dependency запрещена без ADR в `docs/DECISIONS.md`; предпочтительно
   переиспользовать `encoding_rs`, `base64` и существующие core primitives.
6. Отключение `dom-shim` или compile-time отсутствие этой feature должно
   сохранять рабочий `cargo hack --feature-powerset --depth 2` и не создавать
   частично зарегистрированный global.

## 3. Environment capability

Добавить публично документированный host-controlled descriptor с четырьмя
значениями: `Window`, `DedicatedWorker`, `SharedWorker`, `ServiceWorker`.
Название и форму типа можно выбрать в коде, но решение зафиксировать ADR и
соблюсти следующие правила:

- default остаётся `Window`, чтобы существующие пользователи M4-A не получили
  новый global без изменения конфигурации;
- builder получает явный setter для environment descriptor;
- descriptor хранится в зарегистрированном `FileApiHandle`/context state и
  не определяется по thread ID, `Context` типу или наличию host callback;
- регистрация проверяет конфигурацию атомарно до изменения `globalThis`;
- при `Window`/`ServiceWorker` `FileReaderSync` не устанавливается, а
  `globalThis.FileReaderSync` не появляется даже как `undefined`-shim;
- при `DedicatedWorker`/`SharedWorker` устанавливается только нормативный
  `FileReaderSync`; service-worker capability явно запрещена;
- конфликт имени, нерасширяемый global, повторная регистрация и отключённый
  `dom-shim` используют существующий fail-fast/rollback contract.

## 4. JavaScript surface

В worker environments реализовать ровно:

```webidl
[Exposed=(DedicatedWorker,SharedWorker)]
interface FileReaderSync {
  constructor();
  ArrayBuffer readAsArrayBuffer(Blob blob);
  DOMString readAsBinaryString(Blob blob);
  DOMString readAsText(Blob blob, optional DOMString encoding);
  DOMString readAsDataURL(Blob blob);
};
```

Обязательные проверки:

- `new FileReaderSync()` разрешён, вызов без `new` синхронно даёт `TypeError`;
- constructor/prototype/name/length/descriptors и `Symbol.toStringTag`
  проверяются так же строго, как M4-A surface;
- prototype содержит только четыре метода; `FileReaderSync` не имеет
  `readyState`, `result`, `error`, `abort`, event handlers или Promise API;
- методы выполняют brand check для настоящего Blob/File и дают `TypeError` на
  forged receiver/argument;
- method `length`: `1`, `1`, `1`, `1`; optional encoding не меняет Web IDL
  length;
- `FileReaderSync` не регистрируется в `Window`/`ServiceWorker`, а guards
  запрещают случайное появление async-only или M5 API.

## 5. Synchronous read semantics

Каждый метод выполняется полностью на текущем вызове и возвращает результат
либо бросает same-realm `DOMException`. Promise, `FileReader`, ProgressEvent,
Boa job, File Reading task и `context.run_jobs()` внутри метода запрещены.

Перед materialization каждая операция обязана:

1. проверить Blob/File brand и размер;
2. отклонить `size > max_sync_read_bytes` с фиксированным документированным
   лимитным DOMException (`QuotaExceededError` по принятому M4-A mapping);
3. не делать `ByteSource::read_range` и не выделять итоговый буфер до
   успешной preflight-проверки;
4. использовать существующий checked/fallible core path и не отдавать partial
   JS result;
5. не занимать и не менять async `max_concurrent_reads_per_global` quota.

Результаты должны совпадать с M4-A packaging:

- `readAsArrayBuffer` — свежий независимый `ArrayBuffer` с точными байтами;
- `readAsBinaryString` — один code unit `U+0000..U+00FF` на байт, включая NUL;
- `readAsText` — default UTF-8, один leading UTF-8 BOM удаляется, malformed
  input заменяется `U+FFFD`, известные labels разрешаются через
  `encoding_rs`, неизвестный label синхронно бросает `EncodingError`;
- `readAsDataURL` — ровно `data:<type>;base64,<payload>` либо
  `data:;base64,<payload>`, без whitespace/charset; checked arithmetic и
  `max_data_url_output` выполняются до output allocation.

Ошибки source (`short`, `long`, explicit failure, cancellation/invalid range)
должны синхронно отображаться в тот же DOMException class mapping, без
раскрытия path, source details или partial bytes. Неизвестная внутренняя
ошибка не должна завершать процесс.

## 6. Required implementation shape

- Вынести общий memory-backed packaging/preflight в переиспользуемый private
  helper либо безопасно переиспользовать существующие функции; не копировать
  четыре алгоритма из `filereader.rs` с расхождением семантики.
- Сохранить `BlobData`/`ByteSource` Boa-free и существующие public core API.
- Добавить отдельный модуль/часть модуля для sync binding, а регистрацию
  включать через capability descriptor и существующий `dom-shim` feature.
- Публичные типы, builder setter и новые методы документировать rustdoc-ом.
- При выборе имени descriptor, helper boundaries или error policy добавить
  один ADR; не добавлять необоснованные поля для M5 filesystem/URL.

## 7. Tests and traceability

Создать `crates/boa_fapi/tests/m4_filereader_sync.rs` с отдельным свежим
`Context` для каждого integration test и реальным JavaScript. Минимальный
набор:

1. capability matrix: `Window` absent, `ServiceWorker` absent,
   `DedicatedWorker` present, `SharedWorker` present;
2. exact descriptors/prototype/constructor/brand/illegal invocation;
3. all four methods on empty Blob, NUL/high-byte data, composed/sliced Blob и
   File; verify fresh ArrayBuffer independence;
4. UTF-8 BOM, malformed input, multibyte cases, supported labels and unknown
   label → `EncodingError`;
5. exact Data URL with empty/non-empty MIME and boundary
   `max_data_url_output` (`==` succeeds, `+1` fails);
6. `max_sync_read_bytes` boundary (`==` succeeds, `+1` fails), with a
   controlled private `ByteSource` proof that rejected preflight performs zero
   source reads;
7. short/long/failing source mapping to same-realm `NotReadableError`, no
   partial result and no process panic;
8. no Promise/events/jobs: method returns before any queued job can affect the
   context and no `FileReader` event handler is involved;
9. concurrent async reads still obey M4-A quota while sync reads do not consume
   a slot; run the existing M4-A regression suite;
10. feature powerset and negative guards: no sync global when capability is
    absent, no filesystem/URL/clone/full-DOM surface.

Добавить trace rows `M4B-FRS-01..08` в `docs/spec-matrix.md`, каждый с точным
`file:symbol` и test names. Не заменять существующие M4-A rows.

## 8. Docs, CI and handoff

Обновить только relevant current-scope documentation:

- `README.md` и `docs/architecture.md`: worker-only sync capability,
  explicit descriptor, memory-only boundary и omitted M5+ APIs;
- `docs/DECISIONS.md`: ADR для descriptor/registration и любого packaging
  boundary choice;
- `.github/workflows/ci.yml`: запускать новый M4-B integration test после
  M4-A на Ubuntu и Windows; сохранить существующие jobs и deny fetch semantics;
- `docs/m4b-validation.md`, `docs/m4b-final-audit.md`,
  `docs/reviews/M4B-handoff.md`: exact local commands/results, coverage,
  retrospective bug-find findings, actual CI status и deviations;
- не создавать `docs/security.md`, WPT harness или filesystem docs в этом
  заказе без прямой необходимости реализованного public API.

## 9. Required validation

Запустить из чистого checkout/worktree на финальном M4-B commit в этом порядке:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test m2_blob_file_filelist -- --nocapture
cargo test --package boa_fapi --test m3_promise_blob_reads -- --nocapture
cargo test --package boa_fapi --test m3_blob_streams -- --nocapture
cargo test --package boa_fapi --test m4_filereader_async -- --nocapture
cargo test --package boa_fapi --test m4_filereader_sync -- --nocapture
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo test --package boa_fapi --doc
cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85
cargo hack check --feature-powerset --depth 2
cargo deny check
git diff --check
```

`cargo deny check` должен быть записан с фактическим exit/result. Network-
blocked advisory DB — `BLOCKED`, не PASS; зелёный CI не переписывает локальный
результат. Coverage для `boa_fapi` должен оставаться не ниже 85%, а все M4-A
tests должны остаться зелёными.

## 10. Completion and stop condition

Перед commit выполнить обязательный retrospective bug-find из `AGENTS.md`:
проверить scope boundaries, panic/unsafe/unwrap/expect, stale generation или
source-read races, no-op/rollback, exact descriptors, negative surface guards,
качество тестов и правдивость документов. Исправить все находки и повторить
затронутую валидацию до отсутствия unresolved item.

Создать один imperative commit на `task/m4b`, subject <=72 characters, затем
написать `docs/reviews/M4B-handoff.md` с base/final commit, implemented и
omitted surface, командами, trace/ADR links, coverage, audit findings,
actual local results и honest CI status.

После handoff остановиться. Не начинать M5, Blob URL, structured clone, WPT
или full Workers runtime. Не изменять этот заказ и не принимать его самому.
