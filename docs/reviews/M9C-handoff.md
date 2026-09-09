# M9-C handoff — настоящий асинхронный FileReader (M9-C-FILEREADER-IO)

База: принятый M9-B head. Ветка: `task/m9c`.

## Что построено

`FileReader.readAs*` перенесён на executor/completion protocol M9-B.
Boa jobs больше не вызывают `BlobReader::read_next`,
`BlobData::materialize` или `ByteSource::read_range`.

Operation model (`crates/boa_fapi/src/filereader.rs`, `io.rs`):

- `start_read`: синхронная валидация → `(LOADING, null, null)` →
  reserve одного `IoBridge` слота → регистрация reader root
  (`PendingReaderOps`: reader `JsObject` + generation, `Trace` для GC) +
  packaging state (`PumpStates`) + payload (`RegisteredSpecs::
  reader_payloads`) → submit первого чанка → возврат до его выполнения.
  `loadstart` — только после первого worker completion, включая EOF
  пустого Blob через zero-length worker task (без source read).
- Worker (`FileReaderChunkTask::execute`, только off-thread): ровно один
  bounded `read_range` для `[offset, offset+len)` через `read_blob_range`
  (`BlobData::segments_slice` — единственный новый core accessor),
  panic containment как у whole-blob тасков. Completion несёт generation
  + chunk/EOF/typed error, никогда JS object.
- `poll_io` дренит reader completions FIFO в submission order, каждый —
  максимум один pump Boa job (`settle_reader_completion` с
  context/generation/shutdown/stale валидацией). Pump пакует
  инкрементально, шлёт `loadstart`/throttled `progress`/final progress/
  `load`/`error` (+ conditional `loadend`), сабмитит максимум один
  следующий чанк. Stale (abort/restart/shutdown) дропается без JS
  мутации, события, телеметрии и второго quota release.
- `abort()`: generation bump + token cancel + bridge release ровно один
  раз + `abort`/conditional `loadend`. Quota зеркалит мост (`active`
  counter), 65-й reader — `SecurityError` fast path.
- Fairness: `set_poll_io_budget(Some(n))` ограничивает reader completions
  за один `poll_io`, leftovers re-queue + re-wake. Очередь и active count
  ограничены `FileApiLimits`.
- `FileIoExecutor::submit_reader` (default `WorkerLost` — старые
  executor fail closed); built-in pool — общая bounded очередь
  `PoolRequest::{Whole, Chunk}`.

## Доказательства

Новый `crates/boa_fapi/tests/m9_filereader_io.rs` (24 default-feature
теста; 25-й, telemetry, включается с `tracing`; manual executor, без
`sleep` как oracle):

- FR-01: `read_returns_before_io_and_settles_only_through_poll_io`
  (LOADING до I/O, `run_jobs` alone ничего не селит, unrelated job идёт,
  settle только через `poll_io`); `blocking_first_chunk_never_runs_inside_
  boa_job` (REAL held chunk на worker thread, блокирован на gate; Boa
  usable, verdict pending; wake ровно 1); `chunked_source_call_path_
  absent_from_filereader_jobs` (статичный guard: в `filereader.rs` нет
  `.materialize(`/`read_range(`/`read_next(` вне `#[cfg(test)]`).
- FR-02: empty Blob через zero-length worker EOF; 3-chunk success (ровно 1 in-flight, ровно 3
  submits, final progress перед `load`, monotonic `loaded <= total`);
  error до первого чанка и между чанками (exact sequence, no partial,
  quota recovery).
- FR-03: abort до dispatch / во время pending I/O / после queued
  completion; restart из abort handler; stale после restart; shutdown
  (late worker дропается, post-shutdown `AbortError` fast path).
- FR-04: два reader out-of-worker-order (submission-order drain
  `a:first,b:second`, без crossover); slow reader не блокирует fast и
  jobs; `poll_io` budget (1 → re-wake, leftover pending, затем `a,b`);
  65 reads + recovery; foreign `poll_io` rejected.
- FR-05: split UTF-8 BOM + multibyte/emoji через 16 KiB границу;
  split UTF-16LE BOM (sniff once, strip once, no U+FFFD).
- FR-06: submit failures (QueueFull→SecurityError, WorkerLost→
  NotReadableError, panic→contained); listener exception (order kept,
  single terminal, job error); fs parity byte-for-byte (live `HostFileSource`
  на unix); mutation snapshot error без partial; no late telemetry
  (`tracing`: ровно 1 `cancelled`, shutdown — 0).

Сохранены и прогнаны: `abort_races`, `abort_races_fs`, M4-A (33),
M4-B, M8 observability (5), M9-A conformance (23), appendix-A, guards
(14), WPT strict (38 passed, 0 unexpected).

## State/event trace для каждого race

- abort-before-dispatch: `abort|loadend` (stale chunk: `poll_io` = 0).
- abort-during-pending: `abort|loadend` (late completion stale).
- abort-after-queued: `abort|loadend` (queued completion stale).
- restart-from-abort: `loadstart|progress:16384|abort|loadstart|
  progress:5|load|loadend` (старый `loadend` suppressed).
- stale-after-restart: `""` до drain второго read, затем `result ===
  'second'`.
- shutdown: `""` (late worker дропается, `poll_io` = 0).

## Отсутствие blocking source calls в Boa jobs

- Поведенческое: blocking `ByteSource` (gate) + manual executor держит
  чанк; `run_jobs` alone оставляет reader `LOADING`/`pending`, unrelated
  job идёт; чанк выполняется на worker thread (`task.execute()` вне
  `Context`); settle только через `poll_io` + `run_jobs`.
- Статическое: `chunked_source_call_path_absent_from_filereader_jobs`
  сканирует `filereader.rs` без `#[cfg(test)]` на `.materialize(`/`
  `read_range(`/`read_next(` (0 hits) и требует worker entry в `io.rs`.
- `guards.rs::promise_read_has_no_sync_filesystem_fallback` и
  `no_out_of_scope_surface` (без `std::thread` вне `io.rs`) зелёные.

## Приёмка

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test m9_filereader_io -- --test-threads 1 --nocapture
cargo test --package boa_fapi --test abort_races -- --nocapture
cargo test --package boa_fapi --test abort_races_fs -- --nocapture
cargo test --package boa_fapi --test m8_observability --all-features -- --nocapture
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo deny check
cargo hack check --feature-powerset --depth 2
git diff --check
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict
```

Примечание: `m9_filereader_io` содержит два 16 KiB multichunk
encoding-теста; полный прогон `--test-threads 1` занимает ~120 с
(последовательные worker drains). Остальные suites — секунды.

## Отклонения

Функциональных отклонений нет. Однако исходный diff M9-C относительно
`f380abf` составляет 3 842 добавления и 296 удалений: он превышает
лимит заказа в 3 000 строк. До формального принятия требуется либо
разделить work order на state/event migration, либо получить явный
waiver этого лимита. Waiver получен. Внешний CI для итоговой реализации
`2082ce29ab0132e196e68f5692cc1c67e832cf8e` зелёный на Ubuntu, macOS и
Windows: https://github.com/DavidGoliaf/FileAPI_boa/actions/runs/34394832867.
`FileReaderSync` не изменён. Исторические M4/M7/M8 validation/
handoff не переписаны; обновлены только `docs/spec-matrix.md` (M9C-FR
строки), `docs/architecture.md` (Layer 2b/2d), `docs/host-integration.md`
(M9-B/M9-C loop + budget), `crates/boa_fapi/README.md`, `docs/DECISIONS.md`
(ADR-0043). `docs/spec-matrix.md` M4-FR-08 строка сохранена (M4 suite
зелёный через тот же host loop).

Затем остановиться.
