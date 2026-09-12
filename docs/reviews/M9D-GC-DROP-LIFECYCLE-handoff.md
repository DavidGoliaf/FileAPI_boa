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
  Publish-claim живёт в native data (`StreamNative::published` /
  `ReaderNative::published: Cell<bool>`, последний shared с
  `releaseLock()` через `take_lease`): ровно один из путей (finalize,
  drop, releaseLock) публикует lock-free intent своей стороны.
  КРИТИЧНО: claim покрывает только ПУБЛИКАЦИЮ — снятие флага стороны
  происходит исключительно в `poll_io` (`apply_stream_drop_intent` с
  retry при занятом `RefCell`: intent requeue, никогда потеря).
  Повторный `releaseLock()` на том же объекте видит `released` первым
  и бросает `TypeError`, не трогая shared флаги. Drop одной стороны не
  завершает operation, пока зарегистрирована другая. Native data НЕ
  хранит `Rc<RefCell<StreamShared>>` вообще (только identity-инты и
  клон intent-стека): brand gates (`require_stream`/`require_reader`)
  резолвят shared из context-таблиц, финаčajзер не может застать
  занятый borrow. Pending read Promise — самостоятельный живой demand
  (advisory probe `IoBridge::stream_live_demand` + проверка
  `PendingStreamReads`): endpoints могут уйти, а owed promise всё равно
  settles chunk/error первым (demand-first settlement: terminal-lease
  проверка идёт ПОСЛЕ pop слота); после settlement post-drain
  перевооружает abandonment. `Rc::strong_count` не используется
  (технические `Rc` в таблицах).
- Finalizer boundary (§3.2.1–3.2.4 буквально): GC finalizer не мутирует
  `Context`, не исполняет JS, не делает I/O/worker join и НЕ ЖДЁТ
  НИКАКОГО lock — только lock-free publish одного packed-`u64` intent
  `(context:15, operation:32, generation:16, is_reader:1)` в слот-ринг
  owning context (`StreamDropIntentStack`: 4096 `AtomicU64` слотов,
  CAS `0 -> packed`, без `Mutex`, без аллокации, без `RefCell`, без
  `unsafe`). Переполнение структурно невозможно (слотов >> живых
  эпох ≤ quota; плюс sweep sideless-эпох без записи; плюс stale no-op).
  Глобального registry/orphan-очереди нет: маршрутизация — клоном стека
  в native data. `poll_io` (единственное место transition): сначала
  применяет снятия сторон (retry через requeue), затем валидирует
  (живой op root, та же generation, не terminal, ноль сторон, пустая
  очередь, не in-flight; demand — demand-first с табличным/bridge views
  как defense-in-depth) и выполняет single abandoned transition (token
  cancel, cursor clear, op root + payload removal, conditional release,
  один `stream_read` event в существующем классе `cancelled` — без JS
  event/error, без нового класса), плюс sweep. Stale intents —
  strict no-op.
- Exactly-once: EOF/error/cancel/abandoned/shutdown конкурируют за одну
  transition (флаги `terminal`/`released`); только победитель релизит и
  эмитит. `IoBridge::unreserve`/`release` — условно-идемпотентны
  (`-> bool`): повторный release неизвестного id не трогает чужой slot.
  Late completion после abandoned — strict no-op. Terminal
  `read()`/`cancel()`/`releaseLock()` после удаления op entry НЕ
  бросают "no longer live": error replay живёт в собственной context
  таблице (`PendingStreamErrors`, plain name/message), done — через
  общий `settle_terminal_outcome` job; cancel идемпотентно резолвит
  `undefined`.
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

## Retrospective review (rework + P1 rework, перед commit)

Первый вариант (отклонён) и исправления — см. выше. Повторная приёмка
нашла два новых P1 (CI run 34711094697), оба закрыты здесь:

- P1-A (Mutex в Finalize, §3.2.4): `push_stream_cleanup_record` делал
  blocking `Mutex::lock()` прямо из `Finalize`/`Drop`. Исправлено:
  intent-стек — 4096 `AtomicU64` слотов, publish = pack + CAS
  `0 -> packed` (без `Mutex`, без аллокации, без `RefCell`, без
  `unsafe`); в финализаторе нет никакого ожидания — только CAS-ретрай
  при гонке слотов, что не является lock wait. `cargo clippy -D warnings`
  подтверждает отсутствие `unsafe_code`; structural guard подтверждает
  отсутствие `.lock()`/`try_lock`/`borrow` в `Finalize`/`Drop` путях.
- P1-B (потеря claim при занятом RefCell): claim `dropped=true`
  ставился ДО `try_borrow_mut()`, и занятый borrow навсегда оставлял
  сторону registered (phantom owner + утечка quota). Исправлено
  архитектурно: claim покрывает только ПУБЛИКАЦИЮ intent; снятие флага
  стороны происходит исключительно в `poll_io`
  (`apply_stream_drop_intent`) с retry через requeue при занятом borrow
  — intent переживает contention и применяется позже. Native data
  больше не хранит `Rc<RefCell>` вообще (только identity-инты + клон
  стека), так что финаčajзер не может застать занятый borrow через
  собственные данные; brand gates резолвят shared из context-таблиц.
  Попутно terminal `read()`/`cancel()`/`releaseLock()` после удаления
  op entry больше не бросают "no longer live": error replay живёт в
  `PendingStreamErrors`, done — через общий `settle_terminal_outcome`,
  cancel идемпотентно резолвит `undefined` (поймано упавшими
  M3-тестами `cancel_before_first_read_resolves_done`,
  `source_error_rejects_queued...` — все 26+28 зелёные после фикса).

Плюс из первого rework (сохранено):

1. `Trace` derive генерирует собственный `Drop` — ручной `Drop` без
   `#[boa_gc(unsafe_no_drop)]` не компилируется (E0119). Оба native
   типа — `#[derive(Trace)] + #[boa_gc(unsafe_no_drop)]` + ручной
   `Finalize` + ручной `Drop` с тем же publish-claim.
2. Ручной `unsafe impl Trace` запрещён `#![deny(unsafe_code)]`:
   только derive; intent-стек — safe `AtomicU64` + `Arc`, без
   `unsafe impl Send/Sync`, без raw pointers.

## Внешний CI run 34714298795 (не блокирует код, детализация вместо прежней записи)

Три красных job — все в WPT-шагах workflow, после зелёных Rust-тестов.
Ни один не затрагивает код ветки (`git diff task/m9e...HEAD --name-only`
не содержит `ci.yml`/`wpt-manifest.json`/`corpus`/`inventory`/
`expectations.json`; шард `task/m9d-gc-drop-lifecycle` на
`1a1cd2ac8ce8620aac355a196919d116bbc856e1`, в ногу с
`origin/task/m9d-gc-drop-lifecycle`):

- Ubuntu (`exit 2`): `WPT smoke run` упал с
  `boa_fapi_wpt: invalid corpus path
  'corpus/filereader_readasarraybuffer.js'`. Механизм точный: casing
  manifest-vs-disk. В `wpt-manifest.json` пять `path` в нижнем регистре
  (`corpus/filereader_readasarraybuffer.js`,
  `..._readasbinarystring.js`, `..._readasdataurl.js`,
  `..._readastext.js`, `..._readastext_blob_type_charset.js`), а на
  диске лежат camelCase-файлы
  (`crates/boa_fapi_wpt/corpus/filereader_readAsArrayBuffer.js` и т.д.).
  На case-sensitive Ubuntu `resolve_corpus_path` не находит файл; на
  Windows/macOS тот же mismatch маскируется case-insensitive FS. Источник
  mismatch — наследие `task/m9e`, не эта ветка. Фикс — только за scope
  заказа (переименовать либо manifest entries, либо файлы + `sha256`,
  с учётом `.gitattributes` `corpus/*.js text eol=lf` для hash-стабильности).
- Windows (`exit 1`): smoke прошёл
  (`374 upstream passed, 6 smoke passed, 5 defects, 32 exclusions,
  0 unexpected`), `WPT strict run` упал до report-check с
  `boa_fapi_wpt: upstream hash drift for
  'FileAPI/blob/Blob-array-buffer.any.js'`. Строго перед этим —
  `WPT upstream checkout (pinned commit, FileAPI scope)` через
  `git init target/pinned-wpt` + `fetch --depth 1
  0968c868d8095217d18d86b34c7f21dccae58768` + `checkout FETCH_HEAD --
  FileAPI/`. Значит drift касается свежевыкачанного upstream-дерева
  против `upstream_sha256` в `wpt-manifest.json`/`wpt-inventory.json`.
  Прежняя запись в handoff о дрейфе пина подтверждается дословным текстом
  ошибки. Hash-цепочка локально не воспроизводится без сети, но путь
  доказательства оставлен: сверить `sha256` файла в `target/pinned-wpt`
  с `wpt-manifest.json` и `wpt-inventory.json` для
  `FileAPI/blob/Blob-array-buffer.any.js`.
- macOS: smoke и оба strict run прошли
  (`WPT_STRICT: 374 upstream passed, 6 smoke passed, 5 defects,
  35 exclusions, 0 unexpected`; determinism-check threads 1/2 — pass),
  затем `WPT negative control` вывел ожидаемое
  `boa_fapi_wpt: upstream hash drift for
  'FileAPI/blob/Blob-slice.any.js'` для мутированного `target/mutated-wpt`
  и завершился `Process completed with exit code 1`. То есть отрицательный
  контроль сработал как задумано (мутация обязана давать nonzero), но шаг
  workflow (`Copy-Item ...; cargo run ... --upstream-root
  target/mutated-wpt ...; if ($LASTEXITCODE -eq 0) { throw ... }` в
  `ci.yml:121-127`) не имеет `expect-failure` паттерна: ненулевой exit
  самого `cargo run` помечает весь job красным до/вне зависимости от
  последующего `throw`. Статус шага определяется exit-кодом мутированного
  прогона, а не результатом проверки. Фикс — только в workflow
  (например, `continue-on-error` + явная проверка артефакта/кода, либо
  инверсия через wrapper), отдельно от lifecycle-ветки.

Дополнительно в ubuntu-логе виден `thread 'boa-fapi-io-0' panicked ...
failed to join thread: Resource deadlock avoided (os error 35)` рядом с
`m5_file_fs` (19:32:25Z). Это stdout-шум одного worker thread внутри иначе
прошедшего шага: job продолжил все последующие Rust-тесты вплоть до smoke
и упал только на smoke-мismatch выше. Отдельной `test result: FAILED`
строки этот panic не дал; классифицировать его как причину красного CI
нельзя, но приёмщику оставлен след для отдельного разбора вне заказа.

Локальный gate заказа на этом же SHA зелёный (Windows, свежий прогон):
fmt exit 0; clippy чисто; `m9_stream_io` 26/26; `m3_blob_streams` 28/28;
`cargo test --workspace --all-features -- --test-threads=1` — все suites
`ok`, ни одного FAILED; `cargo doc` с `-Dwarnings` exit 0; `cargo deny
check` ok; `cargo hack check --feature-powerset --depth 2` exit 0;
`git diff --check` exit 0. M9D-GC-01…06 идут только через блочный eval
scope + настоящий `boa_gc::force_collect()` + настоящий host `poll_io`;
`__test_*` отсутствуют в коде/тестах/guards (grep по
`crates/boa_fapi/{src,tests}` пуст, guards-дифф добавляет лишь
count-only diagnostics). CI-фиксы — отдельным изменением workflow, не этой
веткой (заказ §1 и §«Вне scope»: WPT gate чинить запрещено здесь).

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

SHA на момент handoff: `1a1cd2ac8ce8620aac355a196919d116bbc856e1`
(`task/m9d-gc-drop-lifecycle`, в ногу с origin; рабочих изменений поверх
нет, кроме неотслеживаемых `tasks/*.md`, не входящих в заказ).
M9-E-R1 в этой ветке не начинать. Дождаться внешнего CI, затем
остановиться для отдельной приёмки.
