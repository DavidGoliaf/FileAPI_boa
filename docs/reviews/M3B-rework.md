# M3-B — заказ на доработку после acceptance review

| Поле | Значение |
|---|---|
| Статус | `REWORK REQUIRED` |
| Основание | review commit `fb46cff` на `task/m3b` |
| Scope | только R1–R3, их tests, документы и handoff |

## 1. Неизменяемые правила

1. Не меняй ТЗ, `tasks/04_TASK_BLOB_STREAMS.md`, заказы M1/M2/M3-A,
   thresholds, trace IDs, CI semantics или приёмку. Не подгоняй результаты
   и не называй непрошедшую, незапущенную или заблокированную проверку PASS.
2. Не скрывай finding через `#[ignore]`, feature disablement, coverage/lint
   exclude, weakened assertions, test hook или изменение expected result.
   Существующие tests не удалять и не ослаблять.
3. Соблюдай `AGENTS.md`: safe Rust, без `unsafe`, `unsafe impl Trace`, raw
   pointers, fabricated references, production `unwrap`/`expect`/`panic` и
   blanket allow. Новые dependencies запрещены.
4. Не расширяй M3-B до full Streams, FileReader, DOM, fs, URL, clone, WPT
   или host adapter. Не меняй public core API; если это окажется необходимо,
   зафиксируй причину в `QUESTIONS.md` и остановись.

## 2. Обязательные исправления

### R1 — P0: GC-safe ownership pending read resolver-ов

**Дефект.** `PendingRead` хранит `ResolvingFunctions` в
`StreamShared.pending`, но `StreamShared` достижим через
`#[unsafe_ignore_trace] Rc<RefCell<_>>`. `ResolvingFunctions` содержит Boa
`JsFunction`; pending resolver-ы не трассируются, а PromiseJob захватывает
только `Rc`.

**Исправление.**

* `StreamShared`, `PendingRead` и любой excluded-from-Trace объект содержат
  только Rust data: reader, decoder, mode, flags, sequence ID. В них нет
  `JsValue`, `JsObject`, `JsFunction`, `ResolvingFunctions`, callback или
  wrapper над ними.
* Resolver каждого pending `read()` захватывается его `PromiseJob` либо
  хранится в корректно трассируемом Boa-owned состоянии. Job удерживает все
  GC pointers до settlement.
* Сохрани FIFO, один chunk/request, cancel/error settlement и `releaseLock`.
  Запрещены raw pointer, `unsafe impl Trace` и ложный `unsafe_ignore_trace`.
* Обнови comments и final audit: claim «shared state contains no GC pointers»
  должен быть истинным, ownership resolver-ов описана фактически.

**Regression tests.**

1. Real `Context`: stream + reader + pending `read()` сохранены в JS global;
   до `run_jobs()` выполнен штатный deterministic Boa GC path. Promise
   settle-ится exact chunk; private address/identity probe запрещён.
2. То же для двух queued reads: после GC FIFO и exact chunks.
3. После GC проверь pending read + `reader.cancel()` и terminal core error:
   все returned promises settle-ятся promised done/plain-Error paths без
   lost resolver, panic или extra source read.

Если Boa 0.22 не имеет публичного deterministic GC trigger, используй
поддерживаемый test API; если такого API нет — опиши это в `QUESTIONS.md` и
остановись. Отсутствие теста не является решением.

### R2 — P1: запретить пустой `textStream` chunk

**Дефект.** `pump_one` превращает decoder output `""` в
`{ value: "", done: false }`, если chunk оканчивается partial UTF-8
sequence. M3-B §4.4 прямо запрещает такой результат.

**Исправление.**

* Если decoder поглотил bytes в pending UTF-8 prefix без text output,
  текущий `read()` продолжает reading до первого non-empty decoded text,
  EOF flush либо error. Пустая строка не resolve-ится.
* Backpressure сохраняется: source может прочитать несколько смежных byte
  ranges только для текущего text request; следующий request не начинает
  чтение. Память O(chunk size)+<=3 decoder bytes; whole Blob не materialize.
* EOF с incomplete bytes выдаёт один non-empty replacement result
  (`done:false`), следующий read — done. Empty Blob сразу done.

**Regression tests.**

1. `default_chunk_size=16 KiB`: `16 KiB-1` ASCII bytes, затем leader в конце
   первого chunk и continuation byte(s) в следующем. Первый JS result не
   пустой, joined string exact, empty `done:false` result отсутствует.
2. Split 2/3/4-byte valid sequence на boundary и truncated EOF: valid symbol
   ровно один раз, U+FFFD только for invalid/truncated data.
3. Controlled source counter: no read before demand; one text read не читает
   дальше первого non-empty text/EOF/error; second request starts exact next range.

### R3 — P1: диапазон chunk size валидируется `FileApiLimits`

**Дефект.** `FileApiLimits::validate()` принимает 1..16383 и значения >1MiB
(если они <= materialize limit); диапазон проверяется поздно лишь в
`BlobData::reader`.

**Исправление.**

* `FileApiLimits::validate()` проверяет `16*1024 <= default_chunk_size <=
  1024*1024`; нарушение возвращает существующий typed
  `ResourceLimit(MaterializeBytes)`.
* Убери дублирование либо используй единую проверку в `BlobData::reader`.
  Invalid host limits не становятся success и не вызывают allocation/read.
* Проверь builder/register validation. Invalid limits должны fail-fast typed
  error до установки Blob/File/stream globals. При конфликте с M1 contract —
  `QUESTIONS.md` и stop, а не произвольный API change.

**Regression tests.**

1. `validate()` rejects 0, 16383, 1MiB+1; accepts 16KiB, 64KiB, 1MiB.
2. Extension с invalid limits fails до установки всех globals.
3. Valid three boundary sizes всё ещё stream exact bytes.

## 3. Документы и финальный audit

1. Обнови `docs/spec-matrix.md` rows M3-STREAM-02/03/07/08 с exact tests.
2. Обнови `docs/m3b-validation.md`, `docs/m3b-final-audit.md` и
   `docs/reviews/M3B-handoff.md` только реальными final results. Ложный claim
   о GC-safe shared state удалить.
3. В final audit отдельная секция `M3B-rework`: R1–R3, root cause, exact fix,
   regression evidence.
4. После первого green run сделай независимый final audit: GC captures,
   FIFO/cancel, UTF-8 boundary, limits validation, guards, scope и truthful
   docs. Найденный defect исправь с regression и начни audit сначала.

## 4. Валидация

После последнего production change выполни по порядку, зафиксируй exact
command, exit code и PASS/FAIL/BLOCKED:

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

No advisory DB means `BLOCKED`, never PASS. Windows/Ubuntu CI подтверждает
владелец проекта; до результата handoff содержит только
`awaiting customer verification`.

## 5. Приёмка rework

Rework готов к review, только если R1–R3 исправлены с обязательными tests;
M3-B scope не расширен; no unsafe/hidden GC references/empty partial text
chunks/late invalid limits remain; every local §4 command PASS; audit/handoff
правдивы. После handoff остановись, следующий этап не начинай.
