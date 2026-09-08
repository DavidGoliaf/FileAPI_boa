# M9-A rework handoff — conformance rework (P0-1/2/3 + iterator + FileList)

| Поле | Значение |
|---|---|
| ID | `M9-A-REWORK-CONFORMANCE` (дополнение к `M9-A-WEBIDL-REGISTRATION`) |
| База | `master` at `1519c3c0edc6dcbac7330ef16ba410e3eb9ac564`, продолжение ветки `task/m9a` |
| Нормативная база | File API WD 23.08.2026; Web IDL; Encoding Standard; `tasks/18_TASK_M9A_REWORK_CONFORMANCE.md` |
| Web IDL snapshot | `boa_engine 0.22.0` + `encoding_rs 0.8.35` (vendored, `Encoding::for_label` + `new_decoder()` со sniffing); drift living standard после этой точки — новым решением в `spec-delta.md` |

## Что исправлено (только границы rework-заказа)

- **P0-1 (M9A-RW-01).** `package::resolve_text_encoding` возвращает
  `TextEncoding` (не `Option`): explicit label → MIME `charset` → UTF-8
  через точный `get an encoding` (`Encoding::for_label`);
  `replacement`-label резолвится и декодирует побайтово в U+FFFD.
  Ранний `?` после неизвестного label удалён; fail-fast
  `EncodingError`-ветки удалены из `filereader::read_as_text` и
  `filereader_sync::read_bytes_sync`; async идёт обычным
  start/read/load путём, sync возвращает строку.
- **P0-2 (M9A-RW-02).** BOM-sniffing оставлен (`new_decoder()`, без
  `without_bom_handling`, без provenance-флага, без ручного strip):
  Decode заменяет любой fallback (explicit включительно) на UTF-8 /
  UTF-16LE / UTF-16BE. Production-код не менялся; латентный пробел
  закрыт sync+async тестами, включая split-BOM через 16 KiB chunk
  границу FileReading jobs (sync/async byte-for-byte равны).
- **P0-3 (M9A-RW-03/04).** Порядок `Blob(blobParts → options)`,
  `File(fileBits → fileName → options)`; двухфазная модель
  `ConvertedBlobPart { Bytes, Shared, Text }`: фаза 1 (открытый
  итератор) — iterator walk + union conversion + conversion-time
  snapshots BufferSource/USVString/brand + счётчик частей; фаза 2
  (после всех аргументов) — `process_converted` с `endings` и итоговым
  size-accounting. Сырые `JsValue` между фазами не хранятся.
- **Iterator (M9A-RW-05).** `iterator_close_and_propagate` удалён из
  normative path; никакого `return()` при abrupt `next`/`done`/
  `value`/conversion и при quota-лимите. Negative-тесты: `return()`
  не вызывается и не подменяет исходное исключение.
- **FileList (M9A-RW-06).** `FileList.prototype[Symbol.iterator]` —
  тот же function object, что `%Array.prototype.values%`
  (`{writable:true, enumerable:false, configurable:true}`); borrowed
  call — общая Array-семантика без FileList-бренда; `entries`/`keys`/
  `values`/`forEach` не добавлены.
- **Tracing.** Класс `encoding` сохранён только для завершившихся
  `replacement`-label reads (sync+async `ok`→`encoding` на терминале);
  fail-fast `EncodingError`-маппинг удалён. M8 oracle обновлены.

## Переписанные oracle (нормативная причина каждого)

- `m4_filereader_async::read_as_text_…`: unknown-label блок →
  трёхкейсовый fallback-цикл (MIME→é, plain→A, bogus-MIME→é) с
  `readyState===1` + `loadstart|progress|load|loadend` + `error===null`.
  Причина: rework §3 — unknown label не исключение.
- `::reentrant_error_handler_starts_new_read`: триггер — quota
  `SecurityError` (64 filler + радикал), ожидание
  `error:SecurityError|loadstart|progress|load|loadend`. Причина: тот же
  fallback; `error`-реентрантность сохранена как путь.
- `::bounded_operation_sequences_match_pure_model` + pure model:
  `ReadBad`→`ReadAgain`, `TermKind::Error`/`ErrorRestart` удалены,
  корпус `9×3×6`→`9×3×5`, coverage — load/abort (error-терминалы
  покрыты quota/error suites). Причина: fail-fast пути больше нет.
- `::dom_exception_names…`: `EncodingError` убран из конструкторного
  списка (имя остаётся валидным для DOMException вообще). Причина:
  ридеры его больше не производят.
- `m4_filereader_sync::sync_text_matches…`: `assert_throws_dom` →
  три fallback-assert'а. Причина: sync возвращает строку.
- `m4_filereader_sync::throwing_label…`: комментарий «no EncodingError
  exists anymore». Причина: инвариант rework.
- `m8_observability` (2 теста): `encoding`-триггер — `csiso2022kr`
  (replacement), `bogus-label-xyz` из секретов убран. Причина: класс
  `encoding` теперь только для replacement-reads.
- `m9_webidl_conformance`: IDL-02 переписан на `!closed`; quota-тест —
  `!m9aClosed` + parts-first precedence; IDL-03 дубликат `{}` →
  `toString:'\uD800'` (3 байта); TEXT — fallback + replacement asserts;
  новые `m9a_rw_02` (×2), `m9a_rw_03`, `m9a_rw_04`; FLIST — value
  iterator + `list[oob]===undefined` + дескриптор алиаса.

## Retrospective (rework)

- Conversion order: аргументы слева направо до body; `endings`/MIME
  только в processing; dictionary `endings`→`type` в Web IDL порядке;
  BufferSource/USVString снапшотятся до side effects следующих
  аргументов (тесты RW-03/04). Счётчик частей инкрементируется только
  за принятую часть (throwing accessors счётчик не трогают — вся
  конверсия всё равно падает).
- Exception precedence: ранний аргумент финализирует ошибку; поздние
  геттеры не читаются; `return()` не участвует ни в каком precedence.
- Iterator closing: отсутствует по построению; quota — единый path.
- GC roots: между фазами только `ConvertedBlobPart`
  (`Bytes`/`Arc<BlobData>`/`String`) — без `JsObject`; итератор живёт
  на стеке вызова.
- Ошибочные старые oracle: `EncodingError`-fail-fast (M4×3, M8×2,
  M9×1), `closed===true` (M9×6 asserts), «нет Symbol.iterator»
  (M9 FLIST), options-first порядок (код) — все переписаны выше.

## Demo (обязательная проверка rework §9)

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test m9_webidl_conformance -- --nocapture
cargo test --package boa_fapi --test m2_blob_file_filelist -- --nocapture
cargo test --package boa_fapi --test m4_filereader_async -- --nocapture
cargo test --package boa_fapi --test m4_filereader_sync -- --nocapture
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo deny check
cargo hack check --feature-powerset --depth 2
git diff --check
```

Результаты — следующим прогоном перед передачей; targeted search
(`EncodingError`-fail-fast, «FileList без Symbol.iterator»,
«IteratorClose как requirement») — там же. M9-B не начат.

## Фактические результаты проверки (SHA рабочей ветки `task/m9a`)

- `cargo fmt --all -- --check` — чисто.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — чисто.
- `cargo test --workspace --all-features` — все сюиты зелёные
  (m9: 16 passed; m4a: 33; m4s: 21; m2: 51; m8 tracing: 5).
- `m9_webidl_conformance -- --nocapture` / m2 / m4a / m4s — зелёные
  по отдельности.
- `cargo doc --workspace --no-deps` (`RUSTDOCFLAGS=-Dwarnings`) — чисто.
- `cargo deny check` — advisories/bans/licenses/sources ok (только
  pre-existing duplicate-version warnings).
- `cargo hack check --feature-powerset --depth 2` — без errors
  (только pre-existing dead_code warnings в урезанных комбинациях).
- `git diff --check` — чисто (только CRLF-предупреждения Git).
- Targeted search: `resolve_label` / `for_label_no_replacement` /
  fail-fast-`EncodingError` / `iterator_close` / `IteratorClose`-как-
  requirement / «FileList без Symbol.iterator» — отсутствуют
  (остались только намеренные `!closed`-negative asserts и
  исторические тексты в `tasks/` + superseded-пометки в ADR/spec-delta).
- Совокупный M9-A diff: ~970 строк production+tests+docs (лимит 3000,
  generated reports не учитываются).
