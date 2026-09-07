# Заказ M4-A — DOM shim и асинхронный `FileReader` для memory Blob

| Поле | Значение |
|---|---|
| ID | `M4A-DOM-FILEREADER-ASYNC` |
| Исполнитель | кодовый агент; архитектура, scope, tests и приёмка ниже фиксированы |
| Основание | `TZ_boa_fapi_FileAPI.md`: §2.3–2.4, §3.3–3.4, §4.4, §5.1/5.5, §6.4–6.5, §7.1–7.5, §9.2, §10–15; приняты M1–M3-B |
| База | M3-B final handoff commit `040211d`; начинать только после подтверждения владельцем его acceptance |
| Ветка | создать `task/m4a` от указанной базы; не работать на `main`, `task/m3` или `task/m3b` |
| Предельный размер | около 3000 строк production+test diff. Если это невозможно, зафиксировать причину в `QUESTIONS.md` и остановиться |

## 1. Цель

Добавить к существующему `boa_fapi` минимальный self-contained `dom-shim` и
полностью JS-наблюдаемый **асинхронный** `FileReader` для уже поддерживаемых
memory-backed `Blob`/`File`. Реализовать все пять методов `FileReader`,
state machine, события, generation-защиту от stale completion и очередь File
Reading tasks, доставляемую через обычный цикл `Context::run_jobs()`.

Это первый, намеренно ограниченный срез M4. Он создаёт нормативную
асинхронную state machine, но **не** реализует `FileReaderSync`, worker
environment descriptors, filesystem-backed sources, blob URL, clone или WPT
harness. Они являются отдельными будущими заказами и не должны появиться в
этом diff.

## 2. Неизменяемые правила

1. Не менять этот заказ, ТЗ, принятые заказы M1–M3-B, thresholds, trace IDs,
   CI semantics, существующие acceptance conditions или результаты тестов.
   Нельзя подгонять код/тесты/документы под зелёный результат, ослаблять
   assertion, удалять неприятный сценарий или объявлять PASS для
   незапущенной, упавшей либо заблокированной проверки.
2. Нельзя скрывать defect через `#[ignore]`, `cfg`/feature disablement,
   `allow`, исключение из coverage, mock вместо реального Boa Context,
   фальшивую очередь, ручной вызов JS handler из Rust или fabricated evidence.
   Tests обязаны наблюдать JS-visible state/events после реального
   `context.run_jobs()`.
3. Соблюдать `AGENTS.md`: Rust 2024, safe Rust only; без `unsafe`,
   `unsafe impl Trace`, raw pointers, fabricated references, production
   `unwrap`/`expect`/`panic` и blanket allow. Публичные элементы документировать.
4. Новая dependency допустима только для `encoding_rs` 0.8 и `base64` 0.22,
   уже разрешённых ТЗ §2.3. До использования добавить по абзацу ADR в
   `docs/DECISIONS.md` (поддержка, распространённость, permissive license).
   Иные dependencies запрещены.
5. Сохранять атомарность регистрации: любой conflict, missing/disabled shim,
   invalid config или ошибка install оставляет `globalThis` таким, каким он
   был. Не изменять public core API, кроме явно названных ниже primitives.
6. После выполнения заказа остановиться и передать handoff на независимое
   review. Не начинать M4-B/M5 и не публиковать изменения.

## 3. Жёсткая граница M4-A

### Входит

* feature `dom-shim` (default-on) с минимальными `EventTarget`, `Event`,
  `ProgressEvent`, `DOMException`; registration capability и fail-fast,
  если shim отключён;
* `FileReader` constructor/prototype, native state, all async read methods,
  constants, readonly attributes и six `on*` handlers;
* отдельная FIFO File Reading task abstraction поверх Boa jobs для memory
  `BlobReader`, generation/cancellation, quota и delivery events;
* incremental `readAsText` decoding с encoding labels, exact binary-string
  and bounded data-URL packaging;
* migration M3 promise-read failure mapping с plain `Error`/`RangeError` на
  соответствующие `DOMException`, поскольку DOMException появился в M4;
* unit, JS integration, deterministic race/property/guard tests, trace rows,
  docs, validation, audit, CI test step and handoff.

### Не входит

* `FileReaderSync`, `DedicatedWorker`/`SharedWorker`/service-worker mode или
  любой sync API;
* `fs`, paths, snapshots, host file import, background threads/futures,
  blob URL, structured clone, URL shim, full DOM/HTML, WPT harness;
* full EventTarget/DOM surface (tree dispatch, capture/bubble phases,
  composed paths, CustomEvent, AbortSignal), full Streams или изменение M3-B
  stream semantics;
* изменение Blob/File/FileList construction, M1 limits except the explicit
  checked Data-URL output use, либо новый public byte/segment accessor.

Если обязательная часть потребует любого пункта из «не входит», нового core
API или API Boa, отсутствующего в закреплённой версии, записать exact blocker
в `QUESTIONS.md` и остановиться без substitute implementation.

## 4. Обязательная архитектура

### 4.1. DOM shim и registration

1. Добавить feature `dom-shim`, default-on. `FileApiExtensionBuilder` получает
   documented `dom_shim(bool)`. При выключенном flag или Cargo feature
   `register()` возвращает typed `RegisterError::DomShimDisabled` до build,
   preflight и любого изменения global.
2. Shim устанавливает только `EventTarget`, `Event`, `ProgressEvent`,
   `DOMException`, `FileReader` как writable/non-enumerable/configurable
   globals. Проверить все новые имена в одном preflight с уже существующими
   globals и включить их в rollback. Совместимого host DOM adapter в M4-A нет:
   existing own global name — `NameConflict`, не duck-typing и не overwrite.
3. `EventTarget.prototype` предоставляет только `addEventListener`,
   `removeEventListener`, `dispatchEvent`; listener deduplication использует
   tuple `(type, callback, capture)`; capture принимается, но dispatch только
   at-target. Listener exception report-ится как JS job error и не прекращает
   dispatch других listeners. `dispatchEvent` синхронен, возвращает
   `!defaultPrevented`; для FileReader events `bubbles=false`,
   `cancelable=false`, поэтому preventDefault не меняет result.
4. `Event` имеет readonly `type`, `target`, `currentTarget`, `bubbles`,
   `cancelable`, `defaultPrevented`, `timeStamp`; `preventDefault` и
   `stopImmediatePropagation`. `ProgressEvent` наследует `Event` и имеет
   readonly `lengthComputable`, `loaded`, `total`. Objects/events branded;
   borrowed/forged receiver даёт synchronous `TypeError`.
5. `DOMException` — branded constructor/prototype with readonly `name`,
   `message`, correct `Error` inheritance, `[object DOMException]`; required
   names at least `InvalidStateError`, `NotReadableError`, `AbortError`,
   `EncodingError`, `SecurityError`, `NotFoundError`, `QuotaExceededError`.
   Один central mapping преобразует `FileApiError`: NotFound→NotFoundError,
   UnsafeFile/TooManyReads/PermissionDenied→SecurityError,
   SnapshotChanged/FileLocked/InvalidRange/Internal→NotReadableError,
   ResourceLimit→QuotaExceededError (это фиксированный выбор M4-A),
   Cancelled→AbortError. Не включать path, byte content или source details в
   message.

### 4.2. `FileReader` Web IDL surface

1. `new FileReader()` разрешён только с `new`; constructor `name="FileReader"`,
   `length=0`; prototype inherits exactly from `EventTarget.prototype`, has
   `Symbol.toStringTag="FileReader"`. Illegal construction/receiver gives
   `TypeError`.
2. Expose exactly `readAsArrayBuffer(blob)`, `readAsBinaryString(blob)`,
   `readAsText(blob, encoding?)`, `readAsDataURL(blob)`, `abort()` plus
   readonly `readyState`, `result`, `error`, and writable handlers
   `onloadstart`, `onprogress`, `onabort`, `onerror`, `onload`, `onloadend`.
   All method names, lengths, ownership and descriptors must be tested.
3. Define numeric `EMPTY=0`, `LOADING=1`, `DONE=2` as readonly constants on
   both constructor and prototype. Initial state exactly
   `(EMPTY, null, null)`; `result` can only be `null`, DOMString or a fresh
   ArrayBuffer; `error` only `null` or same-realm DOMException.
4. Argument conversion/brand check happens synchronously. Non-Blob or missing
   argument throws `TypeError` without changing a previous operation. Calling
   any read while LOADING throws same-realm `InvalidStateError` synchronously,
   preserving generation, `result`, `error` and queued work.

### 4.3. File Reading task source and state machine

1. Implement a private per-Context FIFO `FileReadingQueue` held in traced
   context data. A read operation enqueues exactly one initial FileReading
   job. That job may enqueue the next job for the same operation; it must not
   call `run_jobs()`, call JS directly from source completion, or execute a
   later reader ahead of an earlier queued reader. All resolver/event values
   captured by jobs are directly traced; native shared state contains no
   untraced `JsObject`, `JsValue`, closures or `Context`.
2. On successful read invocation set `(LOADING, null, null)`, allocate a
   monotonically increasing nonzero generation, reserve one
   `max_concurrent_reads_per_global` slot and create a bounded `BlobReader`.
   The 65th active reader with default limit fails as `SecurityError` through
   normal FileReader error/loadend path; every terminal/abort/stale path
   releases exactly one slot.
3. Read one configured chunk per FileReading job. The first successful read,
   including immediate EOF of empty Blob, queues `loadstart`. Each generation
   has monotonically increasing `loaded <= blob.size`; queue a final
   `progress(loaded=total)` before `load`. Non-terminal progress is no more
   often than once per 50 ms according to the injected `Clock`, except one
   per chunk if chunks arrive less often. Do not use system time in tests.
4. Success: set `DONE`, package result, then dispatch `load`, then
   `loadend` only when the same generation remains terminal after `load`
   dispatch. Error: set `DONE`, `result=null`, mapped DOMException, then
   `error`, then conditionally `loadend`. At most one terminal event per
   generation and never `progress` after terminal state.
5. `abort()` in EMPTY/DONE sets `result=null`, returns `undefined`, does not
   alter `error` and queues no event. In LOADING it increments/invalidate
   generation before cancellation, releases its slot once, sets
   `(DONE,null,null)`, then queues `abort` followed conditionally by
   `loadend`. Every late job/completion whose generation differs is a strict
   no-op: it cannot read, mutate state, free a new generation slot or emit an
   event. Reentrant handler that starts a new read suppresses only the old
   generation's following `loadend`, never the new operation's events.

### 4.4. Result packaging

1. `readAsArrayBuffer`: exact Blob bytes, fresh independent `ArrayBuffer`.
2. `readAsBinaryString`: one JS code unit U+0000..U+00FF per input byte;
   embedded NULs preserved.
3. `readAsText`: absent label defaults UTF-8; label parsing/decoding follows
   Encoding Standard through `encoding_rs`, uses replacement for malformed
   sequences, preserves split multibyte sequences and BOM handling. Unknown
   or unsupported label terminates with `EncodingError`; no partial result.
4. `readAsDataURL`: exact `data:<media-type>;base64,<payload>`; empty media
   type becomes `data:;base64,<payload>`; standard base64 only (no whitespace,
   CR/LF or charset parameter). Calculate encoded and final length with
   checked arithmetic and reject before allocation when it exceeds
   `max_data_url_output`, using `QuotaExceededError`.
5. Memory use stays O(chunk size + final result), no unbounded intermediate
   copies, whole-Blob materialization before `max_materialize_bytes` check,
   or partial JS result on any error.

### 4.5. M3 promise-read migration

After DOMException exists, replace only M3 failure settlement mapping:
`ResourceLimit` becomes a rejected same-realm `QuotaExceededError`; other
core failures become the central mapped DOMException. Promise timing,
fulfilment, byte isolation, stream errors and every non-error M3 behavior do
not change. Update affected tests and trace rows; do not retain obsolete
claims that M3 promises reject plain `Error` or `RangeError`.

## 5. Обязательные tests

Create `crates/boa_fapi/tests/m4_filereader_async.rs`. Every integration test
creates a fresh actual `boa_engine::Context`, registers extension, runs JS and
drives `context.run_jobs()` until quiescent. Assertions inspect JS-visible
objects/events; Rust-only state inspection is supplementary only.

1. **DOM/registration.** Exact globals/descriptors/prototypes/brands;
   EventTarget listener dedupe/removal/order/exception isolation; event and
   ProgressEvent attributes; DOMException same-realm `instanceof`, name and
   Error inheritance. Test name conflicts for each new global, non-extensible
   global, install rollback and `dom_shim(false)`/`--no-default-features`
   fail before mutation.
2. **FileReader surface.** Constructors, constants on constructor/prototype,
   descriptors, `FileReader.prototype instanceof EventTarget`, initial values,
   all `on*` properties, brand/receiver/missing/invalid Blob failures.
3. **Four representations.** Empty, ASCII, NUL/high-byte binary data,
   composed/sliced Blob and File. Prove exact ArrayBuffer fresh backing;
   binary code units; text UTF-8/BOM/replacement and split 2/3/4-byte
   boundaries; data URL exact normal/empty media type, no whitespace.
4. **State/event model.** For empty and multichunk Blob assert full sequence
   `loadstart`, throttled zero-or-more `progress`, final progress, `load`,
   `loadend`; every event target/currentTarget is reader, flags and progress
   fields exact, `readyState/result/error` correct at each handler. Use a
   fake Clock to prove the 50-ms throttle and slow-chunk exception.
5. **Synchronous LOADING guard/reentrancy.** A second read during LOADING
   throws `InvalidStateError` and preserves first read. A `load` handler that
   begins a new read suppresses only old `loadend`; analogous `error` and
   `abort` cases. Handler-set property and addEventListener both participate
   once in deterministic order.
6. **Abort races.** Deterministically abort before first job, after
   loadstart/between chunks, after final progress but before load, and race a
   stale completion with a new operation. Assert exactly abort/loadend for
   aborted generation, no stale progress/load/error/loadend, new result intact
   and quota returned. Include `force_collect()` before queued jobs and prove
   callbacks/state survive GC without unsafe ignored JS captures.
7. **Failure/quota/limits.** Controlled private test `ByteSource` proves
   short/long/source failures map to `NotReadableError`, no partial result and
   `error` then `loadend`; unknown encoding maps to `EncodingError`; Data URL
   boundary `==limit` succeeds and `+1` fails before allocation; 64 active
   reads succeed and 65th emits SecurityError, then slots recover through
   success/error/abort. Do not expose a production test hook.
8. **M3 regression.** Existing promise tests prove rejection after jobs is
   now same-realm `DOMException` with fixed mapping and unchanged async/FIFO
   success behavior. M1/M2/M3-A/M3-B suites and guards remain green.
9. **Property/model test.** Generate bounded operation sequences
   (`start`, `run one job`, `abort`, handler-start-new, stale completion) and
   compare JS-observed state/event log against a small pure expected model.
   Cover at least EMPTY/LOADING/DONE, every terminal kind and generation
   replacement; no `#[ignore]`/reduced corpus.
10. **Negative API guards.** Verify FileReaderSync, fs/Path, URL/clone/WPT,
    full DOM APIs and new public raw-byte/core accessors are absent. Extend
    source scans for `filereader.rs`, `dom.rs` and task module.

## 6. Documentation, trace, CI

1. Add `M4-DOM-01..05` and `M4-FR-01..10` rows to `docs/spec-matrix.md`: each
   has normative rule, exact `file:symbol`, normal/error/boundary or race
   test and status. Update M3 rejection rows to DOMException mapping.
2. Update README and `docs/architecture.md`: installed M4-A surface, explicit
   `context.run_jobs()` task contract, memory-only boundary, `dom-shim`
   capability and omitted M4-B features. Do not claim full DOM or File API.
3. Add ADRs for minimal DOM ownership/capability negotiation, FileReading FIFO
   + generation lifetime, result/error mapping, and each new dependency.
4. Create `docs/m4a-validation.md`, `docs/m4a-final-audit.md`, and later
   `docs/reviews/M4A-handoff.md`. They may state only actual final data,
   commands and exit codes. CI is `awaiting customer verification` unless the
   owner supplies an explicit verified run; never invent links/IDs/status.
5. Append the M4-A integration test to both OS jobs after M3-B; retain all
   existing steps and test names.

## 7. Обязательная локальная validation

Run in this exact order after the last production change and record each exit
code as PASS/FAIL/BLOCKED (not a desired result):

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test m2_blob_file_filelist
cargo test --package boa_fapi --test m3_promise_blob_reads
cargo test --package boa_fapi --test m3_blob_streams
cargo test --package boa_fapi --test m4_filereader_async
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo test --package boa_fapi --doc
cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85
cargo hack check --feature-powerset --depth 2
cargo deny check
git diff --check
```

`cargo deny` without a locally available advisory DB is `BLOCKED`, not PASS;
refresh it only through the documented project mechanism. Any
FAIL/BLOCKED/N-A/UNVERIFIED/FALSE PASS means the work order is not ready for
acceptance. The owner, not executor, verifies final Windows and Ubuntu CI.

## 8. Приёмка

M4-A is ready for independent review only if all statements are true:

* exact minimal DOM + FileReader surface is registered atomically and no
  excluded API appears;
* all methods, representations, DOMException mapping, event order/state,
  quota, throttle and reentrancy rules above are JS-proven;
* abort/stale generation races are deterministic, GC-safe, no event/state or
  quota leak occurs, and no terminal duplication is possible;
* FileReader uses real FIFO jobs and never invokes JS from source completion;
* M3 promise errors migrated exactly once and M1–M3-B behavior regresses not;
* every §7 command is actual PASS, coverage meets threshold, final external
  CI result is later confirmed by owner;
* trace, ADRs, docs, validation, audit and handoff contain reproducible,
  truthful evidence; no TODO/FIXME or undocumented deviation remains.

## 9. Обязательный отдельный final audit-pass

After the first complete green local run, stop implementation and conduct a
fresh bug-find pass. Record it in `docs/m4a-final-audit.md`.

1. For every `M4-DOM-*`, `M4-FR-*`, and every numbered requirement above,
   record exact `file:symbol`, normal/error/boundary/race evidence and verdict.
2. Re-read registration/rollback, descriptors/brands, GC traces and every job
   capture; verify no untraced JS data in native state and no stale generation
   can affect new operation/quota.
3. Re-run event ordering under empty/multichunk/error/abort/reentrant cases;
   check first/last/final progress, 50-ms boundaries, source error and all
   four packagers with checked arithmetic.
4. Inspect test quality: force a representative assertion failure mentally or
   via mutation, confirm it fails; ensure tests do not merely inspect private
   state or call implementation helpers.
5. Search for out-of-scope APIs, forbidden safety escapes, stale M3 error
   claims, false PASS/coverage/CI facts, and untracked generated artefacts.
   Fix every finding, repeat the affected validation, update evidence, then
   repeat this audit until it has no unresolved item.

## 10. Завершение и handoff

Before commit perform the retrospective required by `AGENTS.md` and fix all
findings. Make one logical commit on `task/m4a`, imperative subject <=72
characters with body explaining why. Write `docs/reviews/M4A-handoff.md` with
exact base/final commit, implemented and explicitly omitted surface, demo
commands, trace/ADR links, coverage, audit findings/fixes, actual local
results, honest CI status and deviations (`None` expected).

Stop. Do not alter the work order or its acceptance criteria, do not start
M4-B/M5, and do not self-accept the work.
