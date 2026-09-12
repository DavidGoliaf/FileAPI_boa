# M9-D-R2 handoff — GC/drop lifecycle для Streams I/O (M9D-GC-DROP-LIFECYCLE-REMEDIATION)

База: `task/m9e` commit `d94f80bc1f97b75cb5022d72d621c87964b48b1b`.
Ветка: `task/m9d-gc-drop-lifecycle`.

Исторический `M9D-EOF-LIFECYCLE-handoff` не переписывается; этот файл —
rework-дополнение (ссылка в обе стороны). Первый вариант handoff был
отклонён приёмкой (5 замечаний: releaseLock-hook, ложный
missing-prototype блок GC-06, cap 1024 против unbounded limits,
подмена GC тестовым cleanup вопреки строке 195 заказа, недостоверные
GC-результаты при 26/26 PASS); настоящий файл описывает повторный
rework, закрывающий все пять.

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

- Стороны вместо счётчиков: `StreamShared::stream_registered` /
  `reader_registered` (plain `bool`) + lease `StreamLease { context,
  operation, generation, terminal, released }` (только opaque ids).
  Claim живёт в native data (`StreamNative::dropped` /
  `ReaderNative::dropped: Cell<bool>`, последний shared с
  `releaseLock()` через `take_lease`): ровно один из путей (finalize,
  drop, releaseLock) владеет снятием своей стороны. Повторный
  `releaseLock()` на том же объекте видит `released` первым и бросает
  `TypeError`, не трогая shared флаги, — провал "hook уменьшает обе
  стороны включая уже released reader" невозможен по построению.
  Drop одной стороны не завершает operation, пока зарегистрирована
  другая. Pending read Promise — самостоятельный живой demand
  (advisory probe `IoBridge::stream_live_demand` + проверка
  `PendingStreamReads`): endpoints могут уйти, а owed promise всё равно
  settles chunk/error первым (demand-first settlement); после settlement
  post-drain перевооружает abandonment. `Rc::strong_count` не
  используется (технические `Rc` в таблицах).
- Finalizer boundary: GC finalizer не мутирует `Context` и не исполняет
  JS — только смена своего флага и cleanup record `(context, operation,
  generation)` в клон очереди owning context, захваченный при создании
  объекта (`StreamCleanupQueue`, `Arc<Mutex<VecDeque>>` в native data).
  Очередь unbounded: не более одной записи на живую операцию
  (публикует только переход последней стороны в ноль), длина ограничена
  самим `max_concurrent_reads_per_global` — фиксированный cap удалён как
  противоречащий unbounded limits (`limits.rs:75` не ставит верхней
  границы). Процесс-широкий registry и orphan-очередь удалены:
  маршрутизация — клоном, не глобальным реестром. `poll_io` валидирует
  каждую запись (живой op root, та же generation, не terminal, ноль
  сторон, пустая очередь, не in-flight; demand — demand-first с
  табличным/bridge views как defense-in-depth) и выполняет single
  abandoned transition (token cancel, cursor clear, op root + payload
  removal, conditional release, один `stream_read` event в существующем
  классе `cancelled` — без JS event/error, без нового класса), плюс
  sweep сторон-без-demand без записи. Stale записи — strict no-op.
- Exactly-once: EOF/error/cancel/abandoned/shutdown конкурируют за одну
  transition (флаги `terminal`/`released`); только победитель релизит и
  эмитит. `IoBridge::unreserve`/`release` — условно-идемпотентны
  (`-> bool`): повторный release неизвестного id не трогает чужой slot.
  Late completion после abandoned — strict no-op.
- Failure atomicity, честный scope: при включённом `streams-shim`
  после `reserve()` в `create_stream` нет достижимого fallible шага
  (prototype infallible, вставки infallible) — ложный missing-prototype
  блок GC-06 удалён, а не перемаркирован; rollback helpers покрывают
  disabled-shim сборки. Проверяются quota-full, locked-`getReader`,
  queued-demand `releaseLock`, QueueFull submit — тройка неизменна.
- Shutdown дренит stream таблицы (`drop_all_stream_state_for_shutdown`),
  quota bulk-релиз идёт через bridge closer.

Запрет заказа §6 (строка 195) соблюдён буквально: test-only cleanup
вызовов в M9D-GC-01…06 нет вообще. Каждый тест создаёт endpoints в
блочном eval scope (ссылки умирают с возвратом eval), выполняет
поддерживаемый deterministic `boa_gc::force_collect()` (запускает
настоящие native `Finalize`), затем дренит настоящий host `poll_io`
loop. `__test_*` helpers удалены полностью из кода, guards и
документации.

## Terminal races (все — один release)

| Гонка | Порядок A | Порядок B | Исход |
|---|---|---|---|
| drop vs completion-before-poll | scope exit, GC, worker run, poll | — | demand settles (len), затем abandon в том же `poll_io`; settlements == 1 |
| drop vs completion-after-poll-before-jobs | IIFE scope exit, worker run, poll (job queued), GC, jobs | — | settlement job runs once; settlements == 1 |
| drop vs cancel | cancel в scope, затем GC/cleanup | scope exit + возврат reader, GC, cancel, scope exit | done settlement; triple (0,0,0) |
| drop vs EOF | EOF settlement, затем drop мёртвой эпохи | scope exit, GC, worker run, drain | один release; late worker — no-op |
| drop vs error | error settlement, затем drop | scope exit при живом demand, GC, drain | `NotReadableError` ровно один раз; settlements == 1 |
| drop vs shutdown | scope exit, GC, shutdown, worker run, drain | — | verdict pending; triple (0,0,0) |
| abandoned vs EOF/error/cancel | abandoned первой | terminal первой | проигравший — strict no-op (lease/op flags) |
| repeat/foreign release | `unreserve` дважды / чужой id | — | `false`, `active` неизменен |

Release counters до/после (M9D-GC-01, quota-one): до — `(1,1,1)`
(active, payload, ops) и `has_pending_io == true`; после GC/finalizer +
`poll_io` — `(0,0,0)` и `has_pending_io == false`; второй stream
создаётся и читается (`len:6:false`). M9D-GC-05 (limit 3): до — `(3,3,3)`
+ quota-boundary reject; после — `(0,0,0)` без shutdown. M9D-GC-03:
после settlement triple уже `(0,0,0)` (abandon в том же `poll_io`),
снятие promise root — идемпотентный no-op.

## Тесты (все — только real force_collect → finalizer → poll_io, без `sleep`)

`crates/boa_fapi/tests/m9_stream_io.rs` (controlled manual executor):

- `gc_unread_stream_frees_quota_without_read_or_cancel` (M9D-GC-01);
- `gc_endpoint_ownership_stream_reader_release_lock_generations` (M9D-GC-02,
  вкл. второй `releaseLock()` → `TypeError` без движения shared флагов);
- `gc_pending_promise_survives_endpoint_drop_and_settles_once` (M9D-GC-03);
- `gc_drop_races_against_terminal_transitions_release_once` (M9D-GC-04);
- `gc_exhaustion_recovery_and_context_isolation` (M9D-GC-05);
- `gc_failure_atomicity_after_reserve_leaves_no_hidden_slot` (M9D-GC-06,
  честный scope без missing-prototype).

Низкоуровневые exactly-once проверки в `crates/boa_fapi/src/io.rs`:
`late_stream_completion_after_eof_release_is_a_strict_noop` (расширен:
repeat/foreign release — strict no-op),
`stream_live_demand_probe_tracks_unsettled_reads` (новый).

Guards: `__test_*` исключения удалены из `guards.rs` (helpers больше не
существуют); `public_api_exposes_no_paths_or_mutable_bytes` требует
count-only diagnostics (`io_active_count`, `stream_payload_count`,
`stream_operation_count`).

## Документация

- `docs/spec-matrix.md`: строки `M9D-GC-01…06` с production symbols и
  точными тестами (честный GC-06 scope зафиксирован).
- `docs/DECISIONS.md`: ADR-0046 переписан под rework (стороны/claim,
  queue-clone без cap/registry/orphans, demand-first + sweep, честный
  failure scope, буквальный §6-запрет).
- `docs/architecture.md`: Layer 2c переписан под rework.
- Исторический `M9D-EOF-LIFECYCLE-handoff` не переписан.

## Retrospective review (rework, перед commit)

Первый вариант (отклонён) и исправления:

1. `Trace` derive генерирует собственный `Drop` — ручной `Drop` без
   `#[boa_gc(unsafe_no_drop)]` не компилируется (E0119). Сохранено:
   оба native типа — `#[derive(Trace)] + #[boa_gc(unsafe_no_drop)]` +
   ручной `Finalize` + ручной `Drop` с тем же claim.
2. Ручной `unsafe impl Trace` запрещён `#![deny(unsafe_code)]`.
   Сохранено: только derive.
3. ЗАМЕЧАНИЕ 1 (releaseLock-hook): общий hook декрементировал обе
   стороны включая released reader. Исправлено: стороны — `bool` флаги,
   claim — в native data (`take_lease`), `released` проверяется до
   shared флагов; добавлен регрессионный assert (второй `releaseLock()`
   → `TypeError`, triple неизменна).
4. ЗАМЕЧАНИЕ 2 (GC-06): блок "missing prototype" выполнял успешное
   создание. Исправлено: блок удалён; в scope честно зафиксировано
   отсутствие достижимого post-`reserve()` отказа при включённом шиме;
   rollback helpers — для disabled-shim сборок.
5. ЗАМЕЧАНИЕ 3 (cap 1024): противоречил unbounded
   `max_concurrent_reads_per_global`. Исправлено: cap, registry и
   orphans удалены; очередь — unbounded клон в native data, длина ≤
   числу живых операций ≤ quota.
6. ЗАМЕЧАНИЯ 4–5 (подмена GC + недостоверность): `__test_*` hook и
   `delete globalThis.*` давали ложные PASS. Исправлено: hook удалён
   полностью; тесты — только block-scope evals + настоящий
   `force_collect()` (native `Finalize`) + настоящий `poll_io`;
   demand-first settlement + post-settlement re-arm + sweep закрывают
   GC-03/GC-04 формы, где demand откладывал публикацию, а settlement
   уже всё дренировал.
7. `poll_io` shutdown path оставлял op root/payload (тройка
   `(0,1,1)`). Сохранено: `shutdown_runtime` дренит таблицы,
   quota — через bridge closer.
8. Clippy `-D warnings`: `collapsible_if`,
   `unnecessary_lazy_evaluations` (`.then` → `.then_some`),
   `manual_inspect` (`map_err` → `inspect_err`),
   `doc_lazy_continuation` — исправлено.
9. M3-B `reads_are_pending_until_run_jobs_with_fifo_order` флапал при
   полном параллельном прогоне; при `--test-threads=1` стабилен —
   приёмка требует `--test-threads=1` для stream suites.

## Внешний CI run 34708491031 (не блокирует код)

Три падения — все на workflow-уровне, вне кода ветки:

- Ubuntu: неправильный corpus path (WPT checkout кладёт `FileAPI/`
  не туда, куда смотрит `--upstream-root target/pinned-wpt`);
- Windows: upstream hash drift (пин `0968c868…` больше не совпадает с
  upstream содержимым — дрейф вне репозитория);
- macOS: negative-control ожидаемо вернул nonzero (мутированный
  upstream обязан падать), но workflow трактовал nonzero шага как
  падение job вместо `expect-failure` паттерна.

Ни одно не связано с GC/drop lifecycle: локальный gate
(fmt/clippy/m9_stream_io/m3_blob_streams/workspace/doc/deny/hack/diff)
зелёный (см. ниже). CI-фиксы — отдельным изменением workflow, не этой
веткой (заказ прямо запрещает расширять scope за lifecycle
stream/reader).

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
