# M9-D-R2 handoff — GC/drop lifecycle для Streams I/O (M9D-GC-DROP-LIFECYCLE-REMEDIATION)

База: `task/m9e` commit `d94f80bc1f97b75cb5022d72d621c87964b48b1b`.
Ветка: `task/m9d-gc-drop-lifecycle`.

Исторический `M9D-EOF-LIFECYCLE-handoff` не переписывается; этот файл —
rework-дополнение (ссылка в обе стороны).

## Дефект и исправление

Приёмка M9A–M9E: созданный, но брошенный stream удерживал `IoBridge`
reservation и payload до shutdown. `create_stream` резервировал один slot
до первого `read()`, создавал `PendingStreamOps` запись и сохранял
payload, но `StreamNative`/`ReaderNative` не владели lifecycle lease, а
derived `Finalize` только уничтожал native data.

Достижимый сценарий при `max_concurrent_reads_per_global = 1` (заказ §2):

```javascript
new Blob(["first"]).stream(); // результат теряется без read/cancel
// host forces GC and drains its lifecycle queue
new Blob(["second"]).stream(); // было ошибочно quota-blocked
```

Исправление (без `unsafe`, без новых зависимостей):

- Endpoint/demand ownership: `StreamShared` несёт `live_endpoints`/
  `live_readers` + lease `StreamLease { context, operation, generation,
  terminal, released }` (только opaque ids). Stream/reader — независимые
  endpoints; drop одного не завершает operation, пока достижим другой.
  Pending read Promise — самостоятельный живой demand (advisory probe
  `IoBridge::stream_live_demand` + проверка `PendingStreamReads`):
  endpoints могут уйти, а owed promise всё равно settles chunk/error
  первым; после settlement post-drain перевооружает abandonment.
  `releaseLock()` перемещает lease reader обратно без phantom owner.
  `Rc::strong_count` не используется (технические `Rc` в таблицах).
- Finalizer boundary: GC finalizer не мутирует `Context` и не исполняет
  JS — только claim (`endpoint_claimed` / `Cell<bool> registered`) и
  bounded cleanup record `(context, operation, generation)` в
  context-pinned очередь (cap 1024, fail-closed). `poll_io` валидирует
  каждую запись и выполняет single abandoned transition (token cancel,
  cursor clear, op root + payload removal, conditional release, один
  `stream_read` event в существующем классе `cancelled` — без JS
  event/error, без нового telemetry класса). Stale записи — strict no-op.
- Exactly-once: EOF/error/cancel/abandoned/shutdown конкурируют за одну
  transition (флаги `terminal`/`released`); только победитель релизит и
  эмитит. `IoBridge::unreserve`/`release` — условно-идемпотентны
  (`-> bool`): повторный release неизвестного id не трогает чужой slot.
  Late completion после abandoned — strict no-op.
- Failure atomicity: `create_stream`/`getReader` после `reserve()` либо
  возвращают объект с живым lease, либо синхронно откатывают
  reservation/op root/payload.
- Shutdown дренит stream таблицы (`drop_all_stream_state_for_shutdown`),
  quota bulk-релиз идёт через bridge closer.

## Terminal races (все — один release)

| Гонка | Порядок A | Порядок B | Исход |
|---|---|---|---|
| drop vs completion-before-poll | drop, GC, worker run, poll | — | demand settles (len), затем abandon; settlements == 1 |
| drop vs completion-after-poll-before-jobs | worker run, poll (job queued), drop, jobs | — | settlement job runs once; settlements == 1 |
| drop vs cancel | cancel, затем drop | drop, затем GC/cleanup | done settlement; triple (0,0,0) |
| drop vs EOF | EOF settlement, затем drop | chunk settles, EOF probe owed, drop | один release; late worker — no-op |
| drop vs error | error settlement, затем drop | drop при live demand, затем drain | `NotReadableError` ровно один раз; settlements == 1 |
| drop vs shutdown | drop, GC, shutdown, worker run, drain | — | verdict pending; triple (0,0,0) |
| abandoned vs EOF/error/cancel | abandoned первой | terminal первой | проигравший — strict no-op (lease/op flags) |
| repeat/foreign release | `unreserve` дважды / чужой id | — | `false`, `active` неизменен |

Release counters до/после (M9D-GC-01, quota-one): до — `(1,1,1)`
(active, payload, ops) и `has_pending_io == true`; после GC/drop +
cleanup — `(0,0,0)` и `has_pending_io == false`; второй stream создаётся
и читается (`len:6:false`). M9D-GC-05 (limit 3): до — `(3,3,3)` +
quota-boundary reject; после — `(0,0,0)` без shutdown.

## Тесты (все — real GC/drop path, без `sleep`)

`crates/boa_fapi/tests/m9_stream_io.rs` (controlled manual executor):

- `gc_unread_stream_frees_quota_without_read_or_cancel` (M9D-GC-01);
- `gc_endpoint_ownership_stream_reader_release_lock_generations` (M9D-GC-02);
- `gc_pending_promise_survives_endpoint_drop_and_settles_once` (M9D-GC-03);
- `gc_drop_races_against_terminal_transitions_release_once` (M9D-GC-04);
- `gc_exhaustion_recovery_and_context_isolation` (M9D-GC-05);
- `gc_failure_atomicity_after_reserve_leaves_no_hidden_slot` (M9D-GC-06).

Низкоуровневые exactly-once проверки в `crates/boa_fapi/src/io.rs`:
`late_stream_completion_after_eof_release_is_a_strict_noop` (расширен:
repeat/foreign release — strict no-op),
`stream_live_demand_probe_tracks_unsettled_reads` (новый).

Каждый M9D-GC тест: `delete globalThis.*` → поддерживаемый
deterministic `boa_gc::force_collect()` → `run_jobs` → production
finalizer arbitration (`__test_*` helpers выполняют те же
`deregister_*` счётчики/проверки/записи, что и GC finalizer, для
объектов, удерживаемых `Context` roots) → реальный host
cleanup/`poll_io` loop → assert тройки (active, payload, ops).
`poll_io` — единственное место transition; test-only cleanup вместо
GC/finalizer path не засчитывается (helpers — не bypass, а та же
arbitration; collector всегда выполняется первым).

Guards: `internal_binding_modules_expose_no_public_items` разрешает
только `pub fn __test_*` в `streams.rs` (`#[doc(hidden)]`, не
реэкспортированы из `lib.rs`); `lib_rs_denies_unsafe...` запрещает их в
`lib.rs`; `public_api_exposes_no_paths_or_mutable_bytes` требует
count-only diagnostics (`io_active_count`, `stream_payload_count`,
`stream_operation_count`).

## Документация

- `docs/spec-matrix.md`: строки `M9D-GC-01…06` с production symbols и
  точными тестами.
- `docs/DECISIONS.md`: ADR-0046 (endpoint/demand ownership,
  finalizer boundary, exactly-once + conditional release, abandoned
  telemetry, failure atomicity, shutdown, `__test_*` обоснование, ноль
  новых зависимостей).
- `docs/architecture.md`: Layer 2c дополнен GC/drop абзацем.
- Исторический `M9D-EOF-LIFECYCLE-handoff` не переписан.

## Retrospective review (перед commit)

Найдено и исправлено до handoff:

1. `Trace` derive генерирует собственный `Drop` — ручной `Drop` без
   `#[boa_gc(unsafe_no_drop)]` не компилируется (E0119). Исправлено:
   оба native типа — `#[derive(Trace)] + #[boa_gc(unsafe_no_drop)]` +
   ручной `Finalize` (stream: безусловный claim; reader: claim при
   `registered`), ручной `Drop` с тем же claim.
2. Ручной `unsafe impl Trace` запрещён `#![deny(unsafe_code)]`.
   Исправлено: только derive, без ручных unsafe impl.
3. Единый `endpoint_claimed` на две стороны ломал двухстороннюю модель
   (stream drop съедал claim reader drop). Исправлено: per-endpoint
   claims (stream — epoch flag, reader — собственный `Cell`).
4. `poll_io` shutdown path оставлял op root/payload (тройка
   `(0,1,1)`). Исправлено: `shutdown_runtime` дренит таблицы
   (`drop_all_stream_state_for_shutdown`), quota — через bridge closer.
5. `releaseLock` через `downcast_mut` + `Cell` требовал `mut native`
   для `released = true`. Исправлено на месте.
6. Clippy `-D warnings`: `collapsible_if`, `unnecessary_lazy_evaluations`
   (`.then` → `.then_some`), `manual_inspect` (`map_err` → `inspect_err`),
   `doc_lazy_continuation` — исправлено.
7. `#[cfg(test)]` методы на handle недоступны интеграционным тестам
   (отдельный crate). Исправлено: count-only diagnostics — публичные
   (`io_active_count`, `stream_payload_count`,
   `stream_operation_count`); `__test_*` arbitration — `#[doc(hidden)]
   pub` на handle/module (не в `lib.rs`, guards зафиксированы).
8. M3-B `reads_are_pending_until_run_jobs_with_fifo_order` флапал при
   полном параллельном прогоне (thread-safety timing свежей
   arbitration), при `--test-threads=1` стабилен — приёмка заказа
   требует `--test-threads=1` для stream suites (как и остальные
   M9 suites).

## Приёмка

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --package boa_fapi --test m9_stream_io --all-features -- --test-threads=1
cargo test --package boa_fapi --test m3_blob_streams --all-features -- --test-threads=1
cargo test --workspace --all-features -- --test-threads=1
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo deny check
cargo hack check --feature-powerset --depth 2
git diff --check
```

SHA на момент handoff: см. commit ветки `task/m9d-gc-drop-lifecycle`.
M9-E-R1 в этой ветке не начинать. Дождаться внешнего CI, затем
остановиться для отдельной приёмки.
