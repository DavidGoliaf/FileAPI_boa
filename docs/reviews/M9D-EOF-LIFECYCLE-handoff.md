# M9-D-R1 handoff — закрытие EOF lifecycle для Streams I/O (M9D-EOF-LIFECYCLE-REMEDIATION)

База: принятый M9-D head (ветка `task/m9d` поверх `task/m9c` = `b39770f`).
Ветка: `task/m9d-eof-lifecycle`.

## Дефекты и исправление

P0-1 (EOF не освобождал reservation/quota): `settle_stream_eof`
намеренно оставлял `IoBridge` reservation живой — при
`max_concurrent_reads = 1` следующий stream блокировался, пока жив первый
объект. Исправлено: оба EOF-пути (обычный EOF и text-tail EOF) выполняют
общую `transition_stream_eof` — очистка payload-курсора (`data = None`,
`in_flight = false`), удаление live operation root и payload, ровно один
`IoBridge::unreserve` — всё до постановки Promise jobs.

P0-2 (text-tail EOF не устанавливал terminal state): ранний return при
непустом `decoder.flush()` резолвил последний `{ done: false }`, но не
чистил state и reservation. Исправлено: text-tail ветвь выполняет ту же
`transition_stream_eof` перед постановкой flush job; queued demands —
`done` FIFO через `drain_queue_done`; future `read()` — `done: true` через
существующий terminal fast path без worker task/source read/reservation.

Дополнительно: `transition_stream_eof` идемпотентна (пропускает drop/unreserve,
когда operation root уже удалён — late completion не даёт двойного release);
stale-guard в `settle_stream_completion` при generation-mismatch больше не
восстанавливает slot/resolvers и не пересабмитит (demand после terminal
transition уже засеттлен; восстановление воскресило бы его и погнало worker
I/O за terminal state) — теперь strict no-op.

Contracts cancel/error/releaseLock, public descriptors и Streams capability
boundary не меняются; scheduling/backpressure за пределами terminal EOF не
меняются.

## State/release trace

Обычный EOF (byte stream, quota-one, `chunk:false|eof:true`):

```text
read() #0 → submit [0,1) gen=1, in_flight=true
completion Chunk(len=1) → settle demand #0 { done:false }, loaded=1,
  submit EOF-probe [1,1) gen=1
read() #1 → slot, no submit (in_flight)
completion Eof → transition: data=None, in_flight=false,
  ops.remove(op)=true, drop payload, unreserve(op) [active 1→0]
  → job demand #1 { done:true } + drain_queue_done(empty)
has_pending_io == false; submits == 2
read() #2 → terminal fast path (data.is_none()) → { done:true }, submits == 2
blobTwo.stream() → reserve ok (quota free, no cancel/drop of first object)
```

Text-tail EOF (`[0x41, 0xE2]`, textStream, два demands):

```text
read() #0, read() #1 → slots; submit [0,2) gen=1
completion Chunk(len=2) → demand #0 decodes "A" (0xE2 buffered),
  loaded=2, submit EOF-probe [2,2)
completion Eof → decoder.flush() = "�" (non-empty)
  → transition (same as plain EOF: data=None, unreserve [active 1→0])
  → job demand #1 { value:"�", done:false } + drain_queue_done(empty)
has_pending_io == false; submits == 2
read() #2 → terminal fast path → { done:true }, submits == 2
```

## Тесты

`crates/boa_fapi/tests/m9_stream_io.rs` (controlled manual executor, без sleep):

- `quota_recovery_after_eof_error_and_cancel` переписан: доказывает recovery
  именно после EOF (второй и третий стримы создаются и читают без
  cancel/drop первого объекта), затем после error, затем после cancel —
  каждый путь освобождает слот ровно один раз (`has_pending_io == false`
  после каждого terminal settlement; раньше доказывал только cancel-recovery).
- `text_tail_eof_terminates_state_and_frees_quota` (новый): incomplete UTF-8
  tail (`0x41, 0xE2`) через textStream — `r1:"A":false`, `r2:"�":false`,
  затем слот свободен, future reads — done, submits не растёт.
- `eof_future_read_is_done_without_new_io` (новый): после terminal EOF
  (`first:2:false,second:u:true`) subsequent `read()` даёт done без новых
  submits.
- `io::tests::late_stream_completion_after_eof_release_is_a_strict_noop`
  (новый): напрямую инъецирует в `IoBridge` completion уже освобождённого
  EOF operation при активном replacement stream; completion не попадает в
  `poll_io` и не меняет его active quota slot.
- error/cancel/shutdown regression: тот же `quota_recovery_*` покрывает
  error-путь (`NotReadableError` → слот свободен → новый стрим) и
  cancel-путь; `cancel_*`, `shutdown_*`, `no_late_stream_telemetry_*`
  без изменений зелёные (каждый путь — ровно один release, без двойного).

Доказательство отсутствия двойного release/duplicate telemetry: transition
идемпотентна по operation root; late completion после EOF упирается в
unknown-operation guard (`settle_stream_completion` → `Ok(0)`, без JS,
телеметрии и release); `no_late_stream_telemetry_after_cancel_or_shutdown`
(`--all-features`) по-прежнему фиксирует ровно один `cancelled` event.

## Дополнительное исправление при приёмке

Ошибка Boa при упаковке byte chunk в `Uint8Array` теперь считается terminal
`Internal` stream error: triggering и все queued reads завершаются через
общий error-path, payload/reservation освобождаются ровно один раз, а cursor
не продвигается до успешной упаковки. `package_bytes_chunk` создаёт
`Uint8Array` поверх уже скопированного `ArrayBuffer`, без прежней второй
аллокации и лишнего copy.

## Приёмка

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test m9_stream_io --all-features -- --nocapture
cargo test --package boa_fapi --test m3_blob_streams -- --nocapture
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo deny check
cargo hack check --feature-powerset --depth 2
git diff --check
```

Все команды зелёные на `task/m9d-eof-lifecycle` (полный workspace —
0 failed во всех suites; `m9_stream_io --all-features` — 20 passed;
`m3_blob_streams` — 28 passed). Затем остановиться для приёмки.
