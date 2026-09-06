# Заказ M3-A — Promise-чтение `Blob`: `text()`, `arrayBuffer()`, `bytes()`

| Поле | Значение |
|---|---|
| ID | `M3A-PROMISE-BLOB-READS` |
| Исполнитель | кодовый агент; архитектура, scope, проверки и приёмка ниже фиксированы |
| Основание | `TZ_boa_fapi_FileAPI.md`: §5.1, §6.4, §6.6, §7.1, §7.5, §9.2, §10.2, §13–15; M1 и M2 приняты |
| База | принятый M2 commit `5620dc466a2286cfb9924044f35fdad9aa363685` |
| Ветка | создай `task/m3` от указанной базы; не работай на `master`/`main` или `task/m2` |

## 1. Цель

Реализуй в `boa_fapi` promise-returning методы `Blob.prototype.text()`,
`Blob.prototype.arrayBuffer()` и `Blob.prototype.bytes()` для `Blob` и
наследующего от него `File`.

Каждый метод обязан немедленно вернуть pending `Promise`, а чтение,
упаковка и settlement должны происходить только из job, поставленной в
очередь Boa. Вызов `Context::run_jobs()` — явная граница выполнения для
embedder и тестов. Синхронный resolve/reject внутри вызова JS-метода
запрещён, в том числе для пустого memory Blob.

Это **M3-A**, а не весь M3 из ТЗ: задача намеренно не включает Streams.
Она должна остаться меньше примерно 3000 строк production+test diff. Если
это невозможно без изменения требований, зафиксируй причину в
`QUESTIONS.md` и остановись.

## 2. Неизменяемые условия задачи

1. Не меняй `TZ_boa_fapi_FileAPI.md`, этот заказ, M1/M2-заказы, thresholds,
   CI workflow, trace IDs или условия приёмки. Не подгоняй результаты,
   не жульничай и не объявляй незапущенную/заблокированную проверку PASS.
2. Соблюдай `AGENTS.md`: safe Rust, `#![deny(unsafe_code)]`, rustdoc для
   public API, typed errors, без production `unwrap`/`expect`/`panic`/`todo`/
   `unimplemented`, без blanket `allow` и без снижения lint/coverage.
3. `boa_fapi_core` остаётся полностью свободен от Boa, JS, DOM, URL, fs,
   thread/runtime и `Context`/`JsValue`/`JsObject`. Boa-зависимости остаются
   только в `boa_fapi`.
4. Не вводи `unsafe`, `unsafe impl Trace`, fabricated JS references,
   background thread, `tokio`, blocking I/O, filesystem/path/snapshot,
   сеть или произвольный host callback. Любая новая dependency требует ADR
   до её добавления; предпочтительное решение — **без новых dependencies**.
5. Не открывай публичный raw-segment accessor, Arc/address identity probe,
   mutable byte accessor, test hook или неограниченный `read_all`. M2 guard
   fixed `BlobData` API нельзя ослаблять.
6. Promise-результат не может разделять изменяемую JS backing memory с
   исходным BufferSource, другим Promise-вызовом или предыдущим result.
   `File` использует ровно тот же Blob-brand путь.
7. Обрабатывай все `u64 -> usize`/capacity/length преобразования checked и
   fallibly до allocation. Переполнение/лимит не должны panic-нуть,
   wrap-нуться, частично resolve-нуть Promise или оставить JS-visible
   mutable state.
8. Тесты запрещено удалять, ослаблять, `#[ignore]`-ить, feature-disable-ить,
   заменять моками вместо реального `Context::run_jobs`, скрывать exclude
   правилами coverage или менять ожидаемый результат ради зелёного прогона.

## 3. Жёсткая граница M3-A

### Входит

* `Blob.prototype.text()`, `arrayBuffer()`, `bytes()` с дескрипторами,
  native brand checks и наследованием через `File`;
* безопасная ограниченная materialization `BlobData` для bindings;
* постановка native/promise job в очередь Boa и документированный host loop
  `context.run_jobs()`;
* memory-only read path, UTF-8 replacement decode, ArrayBuffer/Uint8Array
  упаковка и лимит materialization;
* unit, integration, guard tests, M3 trace rows, ADR, README/architecture,
  validation/audit/handoff и CI command для M3 теста.

### Не входит

Не реализовывай, не регистрируй и не добавляй заглушек для:

* `Blob.stream()` и `Blob.textStream()`; `ReadableStream`, chunks,
  backpressure или stream cancellation;
* `FileReader`, `FileReaderSync`, `EventTarget`, `Event`, `ProgressEvent`,
  `DOMException`, state machine, abort или File Reading task source;
* filesystem-backed sources, paths, snapshots, permissions, external async
  I/O, worker globals, threads/executors, URL/blob URL, structured clone,
  IndexedDB, WPT harness;
* `readAsBinaryString`, `readAsDataURL`, data URL/base64, host APIs beyond
  существующих M2 constructors.

Строка вроде `new Blob(['C:\\secret.txt'])` остаётся данными. Любой выход
за границу — запись в `QUESTIONS.md` и остановка, а не «разумное» расширение.

## 4. Фиксированная архитектура

### 4.1. Ограниченная materialization в core

В `boa_fapi_core::BlobData` добавь **единственный** M3 semantic primitive
(его внесение в M2 guard и ADR обязательно):

```rust
pub fn materialize(
    &self,
    limits: &FileApiLimits,
    cancel: &CancellationToken,
) -> Result<bytes::Bytes, FileApiError>;
```

Это разрешённое M3 исключение из прежнего запрета byte-read API: оно нужно
для нормативного чтения Blob, но не раскрывает сегменты/источники.

Алгоритм обязан:

1. Проверить `self.size() <= limits.max_materialize_bytes` **до** любой
   output allocation; при нарушении вернуть
   `ResourceLimit(MaterializeBytes)`.
2. Конвертировать размер в `usize` через `try_from`; зарезервировать output
   fallibly (`try_reserve_exact` или эквивалент). Allocation failure —
   `ResourceLimit(MaterializeBytes)`, без panic и без partial result.
3. До первого и перед каждым segment read проверить cancellation; читать
   ровно `offset..offset+len` с checked arithmetic через `ByteSource`;
   при любой ошибке вернуть `Err`, не выдавая partial bytes.
4. Вернуть immutable `Bytes` в исходном порядке. Метод не меняет Blob,
   source или лимиты. Пустой Blob возвращает empty `Bytes`, но всё равно
   вызывается из job на уровне JS.

Обнови `blob_data_public_api_is_fixed`: разрешены ровно прежние девять
методов M1/M2 плюс `materialize`; запрещены `segments`, `read_all`,
identity probes и любые другие content/source test accessors. Private
`#[cfg(test)]` child-module продолжает быть единственным местом для
проверки `Arc::ptr_eq`.

### 4.2. Job и Promise в Boa

Добавь изолированный модуль, например `promise_read.rs`; существующие
`blob.rs`, `brand.rs`, `extension.rs` используют его, но не дублируют
конверсию, packaging или scheduling.

Для каждого метода:

1. Применить `require_blob` до любых side effect. Foreign/forged/copied
   object и borrowed method дают синхронный `TypeError`, Promise не создаётся.
2. Создать Promise capability в current/relevant realm, сохранив resolver
   только в GC-safe Boa job capture. Нельзя хранить `Context`, raw pointer,
   `JsObject` или callback в core/native Blob DTO.
3. Захватить `Arc<BlobData>`, immutable limits и mode (`Text`,
   `ArrayBuffer`, `Bytes`) в `PromiseJob`/допустимой native job Boa 0.22;
   поставить job через `Context::enqueue_job`.
4. Вернуть Promise без materialize/packaging/settlement на текущем JS stack.
   `Promise.resolve` как реализация и прямой вызов resolver до enqueue
   запрещены.
5. В job вызвать `BlobData::materialize`; на успехе упаковать и вызвать
   resolve, на error вызвать reject. Реакции `.then` выполняются последующим
   обычным проходом Boa jobs. Job не должен сам вызывать `run_jobs()`.

M3-A работает только с memory sources, поэтому не добавляет собственный
event loop/task source и не притворяется FileReader. `run_jobs()` остаётся
явной обязанностью embedder; README показывает это буквально.

### 4.3. Упаковка и errors

* `text()` — decode всех materialized bytes как UTF-8 с replacement semantics
  (некорректные/усечённые последовательности → U+FFFD), верни JS string.
* `arrayBuffer()` — верни новый, недетаченный `ArrayBuffer` с копией bytes.
  Каждый вызов создаёт независимый backing store.
* `bytes()` — верни новый `Uint8Array` с `byteOffset=0`, `byteLength=size`,
  поверх нового независимого `ArrayBuffer`; не `DataView`, не shared backing
  memory.
* M3 фиксирует отображение `ResourceLimit(MaterializeBytes)` в rejected
  `RangeError`. Любая другая internal/core read ошибка reject-ится
  `Error` без path/source/body в message. Не добавляй DOMException раньше M4.
* Promise, который уже был возвращён, settle-ится ровно один раз. Ошибка
  одного чтения не меняет Blob и не затрагивает другие pending reads.

### 4.4. JS surface

На `Blob.prototype` зарегистрируй ровно:

```js
Blob.prototype.text()        // length 0
Blob.prototype.arrayBuffer() // length 0
Blob.prototype.bytes()       // length 0
```

Каждый member writable, non-enumerable, configurable и возвращает native
Promise. `File.prototype` не получает копий методов: File наследует их через
`Blob.prototype`. Обнови M2 registration/preflight атомарно, сохранив его
правило повторной регистрации и rollback: конфликт/нерасширяемый global не
оставляет частично установленной M3 поверхности.

## 5. Обязательные тесты

Создай `crates/boa_fapi/tests/m3_promise_blob_reads.rs`. Каждый integration
test использует новый реальный `boa_engine::Context`, регистрирует extension,
исполняет JS и явно вызывает `context.run_jobs()`; Rust-only проверка не
доказывает асинхронность.

Минимальные сценарии:

1. **Surface/descriptors.** Все три метода существуют только на
   `Blob.prototype`, имеют name/length/descriptors; File наследует их;
   getter/borrowed invocation/`Object.create(Blob.prototype)`/foreign/copy
   дают синхронный `TypeError`.
2. **Pending и order.** Сразу после `b.text()` Promise pending и `.then`
   не выполнен; синхронный код после вызова виден раньше handler; после
   `run_jobs()` handler выполнен. Проверить то же для empty Blob и минимум
   два одновременно созданных Promise, включая FIFO order их settlements.
3. **`text`.** ASCII, multibyte UTF-8, invalid/obtruncated UTF-8 replacement,
   empty bytes, File instance и Blob из composed/sliced segments. Проверяй
   exact JS string, не только длину.
4. **`arrayBuffer`.** Exact bytes, zero length, returned object именно
   ArrayBuffer, каждый вызов отдельный. Mutate result первого чтения,
   затем докажи неизменность второго result и исходного Blob.
5. **`bytes`.** Exact bytes, `Uint8Array`, offset 0/length, empty bytes,
   independent backing store и отсутствие влияния mutation result на
   следующий read/Blob.
6. **Limit/rejection.** Через `FileApiExtensionBuilder` с маленьким
   `max_materialize_bytes` проверить: вызов возвращает pending Promise,
   после job он rejected `RangeError`; Blob остаётся пригодным для M2
   metadata/slice и нет partial ArrayBuffer/string. Boundary `size == limit`
   succeeds, `size == limit + 1` rejects.
7. **Core.** Unit tests materialize: multi-segment order, empty, exact/over
   limit, cancellation before first and between segments, checked offset
   failure and allocation/size conversion path where платформенно достижимо.
   Используй test-only controlled `ByteSource`; не добавляй production hook.
8. **Regression/guards.** Guard фиксирует 10 допустимых public `BlobData`
   methods and forbids old probes/unbounded accessor; guard подтверждает, что
   core всё ещё Boa-free и M3 не экспортирует fs/Path/raw segments/mutable
   bytes/native data/test APIs. Production scan покрывает новый модуль.
9. **No accidental M4/M3-B.** Negative guard/JS checks подтверждают, что
   `stream`, `textStream`, `FileReader`, `FileReaderSync`, EventTarget и
   DOMException не появляются как новые globals/members в этом заказе.

Не используй таймер, sleep, порядок потоков или private internals из
integration tests как доказательство Promise behaviour. Единственное
допустимое управление очередью — documented `Context::run_jobs()`.

## 6. Документация и трассировка

1. Добавь `M3-READ-01..08` в `docs/spec-matrix.md`: правило, точный
   `file:symbol`, normal/error/boundary test, status. Минимум: registration,
   pending job order, text UTF-8, ArrayBuffer isolation, Uint8Array isolation,
   limits/rejection, File inheritance, core materialize/cancellation.
2. Создай `docs/m3-validation.md` и `docs/m3-final-audit.md`; оба содержат
   только фактические результаты последнего прогона.
3. Обнови README и `docs/architecture.md`: M3-A поддерживает только
   memory-backed Promise reads; покажи `context.run_jobs()` после вызова;
   Streams, FileReader, DOMException и fs остаются отсутствующими.
4. Добавь ADR: почему `BlobData::materialize` — единственная ограниченная
   semantic byte operation, почему M3 использует Boa job queue, и почему
   materialization-limit сейчас становится `RangeError`. Любое dependency
   решение — отдельный ADR с maintenance/license analysis.
5. Обнови CI workflow: оба OS job запускают
   `cargo test --package boa_fapi --test m3_promise_blob_reads` после M2
   integration test. Не удаляй существующие шаги.

## 7. Обязательная валидация

После последнего production change выполни именно этот набор по порядку и
запиши exact command, exit code и PASS/FAIL/BLOCKED:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test m2_blob_file_filelist
cargo test --package boa_fapi --test m3_promise_blob_reads
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo test --package boa_fapi --doc
cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85
cargo hack check --feature-powerset --depth 2
$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny fetch db
$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny check
git diff --check
```

`cargo deny` без доступной advisory DB — `BLOCKED`, не PASS. Любой
BLOCKED/N-A/UNVERIFIED/FALSE PASS означает, что M3-A не принят. CI на Windows
и Ubuntu должен быть green для commit после финального audit fix.

## 8. Приёмка

M3-A принимается только если всё ниже истинно:

* JS-visible `text`/`arrayBuffer`/`bytes` работают для Blob и File и не
  добавляют API вне scope;
* любой valid read возвращает pending Promise, который settle-ится лишь через
  Boa jobs; real JS tests доказывают порядок до/после `run_jobs()`;
* text replacement, byte order и новые независимые JS buffers верны;
* materialization limit/overflow/cancellation не дают panic, OOM-by-design,
  partial result или синхронный settlement; rejection имеет обещанный тип;
* core остаётся Boa-free, raw-segment/test/identity/unbounded-read APIs не
  возвращены; M2 API guard расширен ровно на bounded `materialize`;
* M1/M2 поведение, tests, docs и CI steps не регрессировали;
* trace matrix, ADR, validation, final audit и
  `docs/reviews/M3-handoff.md` дают реальные воспроизводимые доказательства;
* все команды §7 и оба CI jobs после последнего change завершились exit 0.

## 9. Обязательный финальный audit-pass

После первого полного зелёного прогона выполни отдельный review, не
смешивая его с реализацией:

1. Для каждого `M3-READ-01..08` и каждого пункта этого заказа запиши в
   `docs/m3-final-audit.md` actual `file:symbol`, normal/error/boundary tests
   и verdict.
2. Проверь заново public API core и bindings, capture/GC ownership jobs,
   settlement order, realm, exact byte packaging, conversions/allocation,
   limits/cancellation, promise rejection, отсутствие accidental streams/
   FileReader/DOM shim, docs and CI command order.
3. Найди дефекты самостоятельно. Исправь каждый дефект, слабый/пропущенный
   тест, ложное утверждение или scope violation; добавь regression test и
   начни audit сначала. Запрещено скрывать finding ignore/exclude/lint
   reduction/feature disablement или изменением требований/критериев.
4. После последнего исправления повтори весь §7 по порядку и обнови отчёты.
   Любой незапущенный, заблокированный или не подтверждённый шаг — не
   приёмка.

## 10. Завершение и handoff

До commit сделай retrospective bug-find pass из `AGENTS.md`; исправь
найденное. Затем один логичный commit на `task/m3` с imperative subject
не длиннее 72 символов и body с причиной. Создай
`docs/reviews/M3-handoff.md`: что реализовано, точная база/commit, команды
демонстрации, matrix/ADR, coverage, CI links or run IDs, findings/fixes и
deviations (ожидается `None`).

Остановись и передай работу на acceptance review. Не начинай M3-B, M4,
не публикуй и не меняй условия задачи или приёмки.
