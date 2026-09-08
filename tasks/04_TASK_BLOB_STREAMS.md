# Заказ M3-B — потоки `Blob.stream()` и `Blob.textStream()`

| Поле | Значение |
|---|---|
| ID | `M3B-BLOB-STREAMS` |
| Исполнитель | кодовый агент; границы, архитектура, тесты и приёмка ниже фиксированы |
| Основание | `TZ_boa_fapi_FileAPI.md`: §2.4, §3.2 AD-4/AD-6/AD-7, §4.4, §5.1–5.3, §6.6, §7.1, §10.2, §11.1–11.2, §12–15; M1, M2 и M3-A приняты |
| База | принятый M3-A commit `59a8932` |
| Ветка | создай `task/m3b` от указанной базы; не работай на `main`, `task/m2` или `task/m3` |

## 1. Цель

Заверши M3 из ТЗ: реализуй `Blob.prototype.stream()` и
`Blob.prototype.textStream()` для `Blob` и наследующего от него `File`.

Boa 0.22 не содержит WHATWG `ReadableStream`. Поэтому реализуй небольшой,
но настоящий capability-checked Streams shim: byte stream и stream строк.
Данные выдаются строго по demand из `reader.read()`; запрещено строить stream
через `arrayBuffer()`/`materialize()`/массив всех chunks. Каждый вызов
`stream()`/`textStream()` создаёт независимый stream.

Это M3-B, не M4. Production+test diff не превышает ~3000 строк. Если без
изменения заказа это невозможно, зафиксируй точную причину в `QUESTIONS.md`
и остановись.

## 2. Неизменяемые условия

1. Не меняй ТЗ, этот заказ, заказы M1/M2/M3-A, thresholds, trace IDs и
   условия приёмки. Не подгоняй результаты, не жульничай и не называй
   непрошедшую/незапущенную/заблокированную проверку PASS.
2. Соблюдай `AGENTS.md`: Rust 2024, `#![deny(unsafe_code)]`, public rustdoc,
   typed errors; в production нет `unwrap`/`expect`/`panic`/`todo`/
   `unimplemented`, blanket `allow`, `unsafe`, lint/coverage исключений.
3. `boa_fapi_core` остаётся свободен от Boa, JS, DOM, streams shim, Context,
   JsValue, URL, fs, threads и runtime. JS/GC код — только в `boa_fapi`.
4. Не добавляй background thread, executor, `tokio`, blocking I/O, сеть,
   filesystem/path, host callback, FileReader, DOMException или M4 API.
   Единственная допустимая новая dependency — `encoding_rs = "0.8"` для
   incremental UTF-8 decoder; до добавления нужен ADR о назначении,
   поддержке и permissive license. Если не нужна — не добавляй.
5. Не раскрывай segments/source/path/Arc identity/mutable bytes, raw native
   data, arbitrary `read_all` или production test hook. Все новые core API
   фиксируются guard-тестом; controlled `ByteSource` только в test child module.
6. Сохрани M1/M2/M3-A API, tests, descriptors, job semantics и coverage.
   Переписывать M3-A `materialize` для имитации streaming запрещено.
7. Все conversions, ranges, remaining length и allocation checked/fallible.
   Ошибка не даёт panic/wrap/oversized chunk/partial successful chunk либо
   изменения Blob.
8. Не удаляй, не ignore/disable тесты, не скрывай файлы coverage exclude,
   не меняй expected result ради зелёного прогона и не расширяй scope.

## 3. Scope

### Входит

* bounded core reader, выдающий максимум один chunk по demand;
* встроенный branded `ReadableStream` shim и `ReadableStreamDefaultReader`;
* `stream()` с `Uint8Array` chunks и `textStream()` с string chunks;
* backpressure, FIFO read requests, EOF, cancellation и normal/error paths
  через Boa job queue;
* registration/preflight/rollback, docs, traceability, ADR, CI, unit,
  integration и guard tests.

### Не входит

Полный WHATWG Streams (`pipeTo`, `pipeThrough`, `tee`, `values`, async
iterator, BYOB, controllers, strategies, Transform/WritableStream,
TextDecoder/TextDecoderStream, user-created streams), а также FileReader,
EventTarget/Event/ProgressEvent/DOMException, File Reading task source, fs,
workers, URL/blob URL, clone/IDB/WPT harness. Отсутствующий member должен
быть `undefined`, не заглушкой.

## 4. Фиксированная архитектура

### 4.1. Core: bounded incremental reader

В `boa_fapi_core::blob` добавь только этот public contract:

```rust
pub struct BlobReader { /* private fields */ }

impl BlobData {
    pub fn reader(&self, limits: &FileApiLimits) -> Result<BlobReader, FileApiError>;
}
impl BlobReader {
    pub fn read_next(&mut self) -> Result<Option<bytes::Bytes>, FileApiError>;
    pub fn cancel(&mut self);
}
```

Контракт обязателен:

1. `reader()` snapshot-ит `default_chunk_size`, валидируемый в
   `16 KiB..=1 MiB`; default остаётся 64 KiB. Никакого silent clamp;
   0/меньше/больше — typed `ResourceLimit`. Он не читает данные.
2. `read_next()` после cancel/EOF возвращает идемпотентный `Ok(None)` без
   source read. Иначе читает ровно один очередной logical range
   `min(default_chunk_size, remaining)`.
3. Chunk пересекает segments, сохраняет порядок, имеет exact size и не
   превышает chunk limit. Он не вызывает `materialize`, не читает следующий
   range заранее и использует O(chunk size) temporary memory.
4. Range arithmetic checked; source обязан вернуть exact requested length.
   short/long/invalid/cancel/source error → `Err` без partial chunk. Reader
   становится terminal errored; следующие calls не читают и возвращают тот
   же class error.
5. `cancel()` идемпотентен и изолирован: не меняет Blob/source/limits/другой
   reader. После cancel Blob/File и другой reader продолжают работать.

Обнови exact public-API guard: прежние 10 `BlobData` methods плюс `reader`;
у `BlobReader` только `read_next` и `cancel`. Запрещены position, segment,
source, identity, content or accumulation accessors.

### 4.2. Shim и capability

Создай изолированный `crates/boa_fapi/src/streams.rs`. Native state branded
Boa objects держит лишь Rust reader/mode/decoder/cancellation state: без
Context, raw pointer, source detail, callback или fabricated GC reference.
Jobs захватывают Boa resolvers и Rust state, но не вызывают `run_jobs()`.

Регистрация добавляет только:

```text
globalThis.ReadableStream                         // non-user-constructible shim constructor
globalThis.ReadableStreamDefaultReader            // non-user-constructible shim constructor
ReadableStream.prototype.getReader()              // length 0
ReadableStream.prototype.cancel(reason?)          // length 1
ReadableStream.prototype.locked                   // readonly getter
ReadableStreamDefaultReader.prototype.read()      // length 0
ReadableStreamDefaultReader.prototype.cancel(reason?) // length 1
ReadableStreamDefaultReader.prototype.releaseLock()   // length 0
```

Direct `new` обоих constructor синхронно бросает `TypeError`. Методы
writable/non-enumerable/configurable; accessors с корректными descriptors;
`Symbol.toStringTag` равен `ReadableStream`/`ReadableStreamDefaultReader`.
Других members/globals не добавляй.

`FileApiExtensionBuilder::streams_shim(bool)` default `true`; Cargo feature
`streams-shim` включён по умолчанию и отключает shim. При feature off или
`streams_shim(false)` registration без host adapter (он не реализуется)
возвращает typed `RegisterError` до mutation globalThis. Existing own
`ReadableStream`/`ReadableStreamDefaultReader` — fail-fast `NameConflict`,
без duck typing/overwrite. Existing M2 preflight и rollback должны атомарно
охватывать новые globals.

### 4.3. State machine и JS members

`stream()`/`textStream()` сначала `require_blob`; forged/foreign/borrowed
receiver синхронно `TypeError`, stream не создаётся. Valid call создаёт fresh
unlocked stream. Методы зарегистрированы только на `Blob.prototype`:

```js
Blob.prototype.stream()     // length 0
Blob.prototype.textStream() // length 0
```

Они writable/non-enumerable/configurable; File наследует, но не копирует.

* `stream()` yields fresh offset-0 `Uint8Array` over fresh ArrayBuffer;
  `textStream()` yields primitive strings. Каждый `read()` создаёт pending
  Promise и ставит ровно одну job. `stream()`/`getReader()` ничего не читают.
* До `context.run_jobs()` read pending. Два reads до pump — FIFO, один chunk
  на request. EOF resolves `{ value: undefined, done: true }`; repeated EOF
  reads снова done через job без source read.
* `getReader()` on locked stream, wrong/foreign receiver → synchronous
  `TypeError`. `releaseLock()` permits next reader, but with queued read
  throws `TypeError` without changing state.
* `stream.cancel()` only unlocked; locked gives rejected Promise TypeError.
  `reader.cancel()` and successful stream cancel resolve undefined through
  Boa job, are idempotent, and make queued/future reads `{done:true}`.
* Core error errors only that stream: current/future reads reject same-realm
  plain `Error` without body/path/source; no DOMException/RangeError.

### 4.4. Incremental `textStream` decoding

Один reader owns один independent incremental UTF-8 decoder. Trailing
incomplete sequence хранится между chunks и не превращается в U+FFFD до EOF;
на EOF decoder flush даёт exact replacement semantics. Не emit empty
`done:false` string only because input ended in partial sequence. Empty Blob
даёт первый done. Two text streams never share decoder/cancel/progress state.
M3-B supports UTF-8 only; BOM/type do not select encoding.

## 5. Обязательные тесты

Создай `crates/boa_fapi/tests/m3_blob_streams.rs`. Каждый integration test:
новый real `boa_engine::Context`, extension registration, JS execution и
explicit `context.run_jobs()`. Rust-only/timers/private internals не доказывают
Promise/backpressure.

Обязательные сценарии:

1. **Surface/atomicity:** exact descriptors/name/length/prototypes/
   `instanceof`; Blob-only inheritance to File; illegal construction and
   receivers TypeError; unsupported full Streams API absent; global conflicts,
   non-extensible global and feature/config off leave no partial globals.
2. **Demand/FIFO/EOF:** no source read after `stream()`/`getReader()`;
   `read()` pending and JS ordering before/after `run_jobs()`; two queued
   reads FIFO, one chunk/request; EOF/repeated EOF exact done without read.
   Use controlled core source counter plus JS-visible order.
3. **Byte chunks:** composed/sliced multi-segment Blob, 16 KiB/64 KiB/1 MiB,
   exact concatenation, fresh offset-0 Uint8Array/independent backing mutation,
   empty Blob and File.
4. **Backpressure/isolation:** next source range not read before next demand;
   two streams independent; cancel/release/error one leaves other and Blob
   metadata/slice/M3-A read usable.
5. **Cancellation/state:** cancel before first/after chunk/queued reads,
   idempotence, locked stream cancel rejection, reader cancellation, release
   success/rejection with queued read, new reader after release.
6. **Errors/bounds:** controlled short/long response, checked-offset and
   source error: no partial JS chunk, same-realm plain Error not RangeError,
   future reads reject/no extra source reads. Config rejects 0, 16 KiB-1,
   1 MiB+1; both bounds work.
7. **Text:** ASCII; multibyte split at every byte boundary; invalid/truncated
   flush exact U+FFFD; no spurious empty chunks; empty/composed/sliced/File;
   two independent decoders; exact strings/done state.
8. **Guards:** exact core API, core still Boa/JS/fs/path/thread/raw-byte-free,
   production scan includes streams module, M4/full Streams absent, M3-A
   regression suite untouched.

## 6. Документация, trace и CI

1. Добавь `M3-STREAM-01..09` в `docs/spec-matrix.md`: shim registration/
   atomicity; core reader/bounds; demand/FIFO/backpressure; byte packaging;
   EOF; cancel/release/isolation; errors; decoder; File/no accidental API.
   Каждая строка содержит `file:symbol`, normal/error/boundary test/status.
2. Создай `docs/m3b-validation.md` и `docs/m3b-final-audit.md` только с
   фактическими результатами последнего прогона.
3. README и `docs/architecture.md`: bounded shim, `context.run_jobs()`,
   demand/cancel/UTF-8 boundaries and explicit absence full Streams/M4/fs.
4. ADR: bounded reader vs materialization, capability+atomic shim, incremental
   decoder; отдельный ADR dependency, если добавлен `encoding_rs`.
5. CI на обеих OS добавляет `cargo test --package boa_fapi --test m3_blob_streams`
   после M3-A; existing steps не удалять; имя/comment обновить до M3 validation.

## 7. Обязательная локальная валидация

После последнего production change выполни по порядку и запиши exact command,
exit code и PASS/FAIL/BLOCKED в `docs/m3b-validation.md`:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test m2_blob_file_filelist
cargo test --package boa_fapi --test m3_promise_blob_reads
cargo test --package boa_fapi --test m3_blob_streams
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo test --package boa_fapi --doc
cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85
cargo hack check --feature-powerset --depth 2
$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny fetch db
$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny check
git diff --check
```

`cargo deny` without advisory DB is BLOCKED, never PASS. External Windows/
Ubuntu CI runs after handoff by project owner: agent never invents URL/run ID
or a green result.

## 8. Приёмка

M3-B принимается, только если `stream`/`textStream` return new branded streams
for Blob/File with exact descriptors and no extra API; byte reader is
demand-driven/bounded/order-preserving/cancellation-safe without whole-Blob
materialization; real JS plus controlled source tests prove backpressure/FIFO;
chunks are independent; EOF/error/cancel/release isolated; text decoder proves
split UTF-8 replacement; registration failures atomic; core stays Boa-free;
M1/M2/M3-A do not regress; trace/ADR/validation/final audit/handoff contain
reproducible actual evidence; every local §7 command is PASS. CI status is
recorded only by project owner.

## 9. Обязательный финальный audit-pass

После первого полного зелёного локального прогона сделай отдельный review:

1. Для каждого `M3-STREAM-01..09` и пункта заказа в `m3b-final-audit.md`
   укажи actual `file:symbol`, normal/error/boundary test и verdict.
2. Перепроверь reader arithmetic/allocation/source exactness/O(chunk),
   GC/job captures, registration rollback, brands/descriptors, demand/FIFO,
   cancel/release/error, decoder boundaries, scope/docs/CI command order.
3. Сам найди ошибки. Исправь каждую, добавь regression test и начни audit
   сначала. Нельзя скрыть finding ignore/exclude/lint reduction/feature
   disablement или изменением задачи/приёмки.
4. После последнего исправления повтори весь §7 и обнови отчёты. Любой
   unrun/blocked/unverified local step — работа не готова к приёмке.

## 10. Завершение и handoff

До commit выполни retrospective bug-find pass из `AGENTS.md` и исправь
найденное. Один логичный commit на `task/m3b`, imperative subject <=72 chars,
body с причиной. Создай `docs/reviews/M3B-handoff.md`: база/commit,
реализованное, demo commands, matrix/ADR, coverage, audit findings/fixes,
локальные результаты, честный CI status (`awaiting customer verification`,
если результата нет), deviations (`None` ожидается).

Остановись и передай работу на acceptance review. Не начинай M4, не публикуй
и не меняй условия задачи или приёмки.
