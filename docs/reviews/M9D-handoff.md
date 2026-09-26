# M9-D handoff — off-thread I/O для ReadableStream shim (M9-D-STREAM-IO)

База: принятый M9-C head (`task/m9c`, `b39770f`). Ветка: `task/m9d`.

## Что построено

`Blob.stream()`/`textStream()`/`ReadableStreamDefaultReader.read()` перенесены
на executor/completion protocol M9-B. Ни Promise read, ни FileReader, ни stream
read не выполняют filesystem `read_range` внутри Boa job.

Operation model (`crates/boa_fapi/src/streams.rs`, `io.rs`, `extension.rs`):

- `create_stream`: синхронная валидация → chunk-ceiling контекста
  (`16 KiB..=1 MiB`) → reserve одного `IoBridge` слота на стрим → регистрация
  stream root (`PendingStreamOps`: shared + generation) + payload
  (`RegisteredSpecs::stream_payloads`) → возврат до любого I/O. Сабмита нет:
  первый запрос уходит только по первому `read()`.
- `read()`: pending Promise + FIFO demand slot (резолверы — в GC-traced
  `PendingStreamReads` до `poll_io`) + максимум один bounded запрос, если
  ничего не in-flight (никакого read-ahead: N `read()` — максимум один
  in-flight). Terminal fast paths (cancelled/EOF-consumed → done, errored →
  stored class) — всё равно через один Boa job, никогда синхронно.
- Worker (`StreamChunkTask::execute`, только off-thread): ровно один bounded
  `read_blob_range` для `[offset, offset+len)`; EOF выводится на воркере
  (`offset == total`, без source read); panic containment как у остальных
  тасков. Completion несёт generation + chunk/EOF/typed error, никогда JS.
- `poll_io` дренит stream completions FIFO на стрим, каждый — максимум один
  settlement Boa job (`settle_stream_completion` с
  operation/generation/shutdown/stale валидацией + восстановлением
  slot+resolvers при generation-mismatch, чтобы demand не исчезал).
  `settle_stream_chunk` пакует (свежий `Uint8Array`/инкрементальный UTF-8 без
  пустых `done:false`; чанк без текста re-queue'ит тот же demand с тем же
  sequence key), двигает курсор, сабмитит следующий запрос при спросе.
  `settle_stream_eof` отдаёт decoder flush как финальный value chunk и
  резолвит все queued/future reads done. `fail_stream_with` сохраняет mapped
  класс (центральный `dom::map_core_error`) и реджектит все queued + replay
  для future без новых source reads.
- `cancel()` (stream/reader): выигрывает синхронно на вызывающем стеке —
  сеттлит queued reads done через их jobs, бампает generation, канцеллит
  токен, релизит слот ровно один раз + один `stream_read/cancelled` telemetry
  event. Поздний воркер дропается как stale. `releaseLock` отказывает при
  queued/in-flight demand. Shutdown дропает pending roots/resolvers без JS
  и телеметрии.
- Quota: один слот на живой стрим; релиз ровно один раз — cancel/error/
  shutdown (EOF слот держит, пока жив объект стрима; future reads — done без
  worker contact). `IoBridge::chunk_reservations` маркирует chunk-резервации,
  чтобы `take_completions` пропускал их при сканировании whole-blob префикса:
  живые стримы/ридеры не блокируют promise FIFO (включая состояние после drain
  очереди, пока резервация жива). До M9-D `take_completions` останавливался на
  первом chunk id без whole-blob completion — это и блокировало
  `blob.text()` после stream reads (нашлось отладкой `two_streams_are_independent`).
- `FileIoExecutor::submit_stream` (default `WorkerLost`, как `submit_reader`
  в M9-C); built-in pool — общая bounded очередь (`PoolRequest::Stream`).
  `poll_io` дренит stream chunks после FileReader chunks, до whole-blob.
- M3-B сюита мигрирована с `run_jobs`-only на host loop
  (`poll_io` + `run_jobs`, `drive_host_loop` с double-drain для interleaved
  continuations) без ослабления assertions, кроме одного нормативного
  изменения (ниже). M5-FS, appendix-A и M8-stream пути мигрированы аналогично
  (M8: `run_jobs_boa` теперь host loop).

## Поведенческое отличие от M3-B (нормативное, зафиксировано)

M3-B `reader_cancel_makes_queued_and_future_reads_done` ожидал `q:false`
(queued read сеттлится чанком, затем cancel). M9-D контракт требует:
`cancel()` выигрывает синхронно на вызывающем стеке, queued read сеттлится
done (`q:true`). Тест обновлён под нормативный M9-D контракт (§2 п.8 заказа:
cancel отменяет pending I/O и не допускает late completion mutation).
ADR-0044 фиксирует отличие.

## Доказательства

Новый `crates/boa_fapi/tests/m9_stream_io.rs` (17 default-feature тестов;
18-й, telemetry, включается с `tracing`; manual executor, без `sleep`):

- STR-01: `read_returns_pending_before_io_and_settles_only_through_poll_io`
  (pending до I/O, `run_jobs` alone ничего не селит/читает, unrelated job
  идёт, completion до `poll_io` не исполняет JS, settle только через
  `poll_io`); `blocking_source_never_runs_inside_boa_job` (REAL held chunk
  на worker thread, блокирован на gate; Boa usable, verdict pending; wake
  ровно 1).
- STR-02: 3 queued reads → `16384,16384,8192` FIFO (ровно 1 in-flight);
  16 KiB boundary + post-EOF read без нового сабмита; error до первого чанка
  (оба queued reject `NotReadableError`, ровно 1 source read, future replay
  без новых reads); error между чанками (first chunk + `second-err`, 2 reads).
- STR-03: cancel до I/O (queued done + `cancelled`, 0 pending, future done);
  cancel после worker completion до `poll_io` (late chunk stale);
  cancel после `poll_io` до jobs (ровно один settlement);
  releaseLock с pending demand (`TypeError` без смены состояния, demand жив);
  shutdown с pending I/O (late worker дропается, `poll_io` = 0, verdict pending).
- STR-04: 5 demands → ровно 1 in-flight, ровно 4 сабмита (3 чанка + EOF probe);
  два стрима с reverse worker completion (оба сеттлятся независимо);
  quota-one: второй стрим quota-blocked до cancel первого, затем recovery;
  QueueFull→`SecurityError`, WorkerLost→`NotReadableError`, panic→contained;
  foreign `poll_io` rejected без смены состояния.
- STR-05: `stream_has_no_sync_filesystem_call_path` (статичный guard) +
  `guards.rs::stream_has_no_sync_read_fallback` (та же проверка как
  архитектурный guard) + `no_late_stream_telemetry_after_cancel_or_shutdown`
  (`tracing`: ровно 1 `cancelled`, shutdown — 0, allow-list полей, JS error
  без bytes/paths).

Сохранены и прогнаны: M3-B (28), M3-A (16), M4-A (33), M4-B, M5-FS (15),
M9-B (17), M9-C (24), M8 observability (5), appendix-A (12), guards (15),
WPT strict (38 passed, 0 unexpected).

## Controlled-source evidence и structural guard

- Поведенческое: blocking `ByteSource` (gate) + manual executor держит чанк;
  `run_jobs` alone оставляет promise pending, unrelated job идёт; чанк
  выполняется на worker thread (`task.execute()` вне `Context`); settle
  только через `poll_io` + `run_jobs`; wake ровно 1.
- Статическое: `guards.rs::stream_has_no_sync_read_fallback` сканирует
  `streams.rs` без `#[cfg(test)]` на `.materialize(`/`read_range(`/`read_next(`/`.execute(`
  (0 hits) и требует worker entry в `io.rs`
  (`StreamChunkTask`/`StreamChunkCompletion`/`submit_stream`/`push/take_stream_completions`/`stream_task_for`/`submit_stream_guarded`/`read_blob_range`/`StreamChunkKind::Chunk`)
  плюс `settle_stream_completion` в `poll_io` (`extension.rs`).

## Приёмка

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test m9_stream_io -- --nocapture
cargo test --package boa_fapi --test m9_stream_io --all-features -- --nocapture
cargo test --package boa_fapi --test m3_blob_streams -- --nocapture
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo deny check
cargo hack check --feature-powerset --depth 2
git diff --check
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict
```

Все команды зелёные на `task/m9d` (см. вывод выше; полный workspace — 0 failed
во всех suites; WPT strict 38 passed). Затем остановиться.
