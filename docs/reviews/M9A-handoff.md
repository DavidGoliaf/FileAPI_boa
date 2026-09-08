# M9-A handoff — Web IDL constructors, text packaging, registration identity

> SUPERSEDED в части encoding/порядка/iterator/FileList документом
> `docs/reviews/M9A-rework.md` и ADR-0040. Исторический текст ниже
> сохранён без изменений, кроме этой пометки (требование rework-заказа
> §8: исторические handoff не переписываются, неверные заявления
> исправлены в rework-handoff).

| Поле | Значение |
|---|---|
| ID | `M9-A-WEBIDL-REGISTRATION` |
| База | `master` at `1519c3c0edc6dcbac7330ef16ba410e3eb9ac564` |
| Ветка | `task/m9a` |

## Что построено

- `webidl::convert_parts` — единый нормативный `sequence<BlobPart>`
  конвертер для `Blob` и `File`: `GetMethod(V, @@iterator)` один раз,
  primitive string — conversion error, boxed `String`/`TypedArray`-
  as-sequence итерируются, `next` → `done` → `value` слева направо,
  abrupt completion закрывает итератор (`return()`-precedence) без
  подмены класса/сообщения, quota (`max_parts`/`max_blob_size`)
  проверяется до накопления — бесконечный итератор закрывается
  детерминированной quota-ошибкой. Старые `blob_parts`/`collect_parts`
  (array-only) удалены; `blob.rs`/`file.rs` используют один путь.
- `process_part` — точная union-развилка: BufferSource (видимый
  диапазон), branded Blob/File (shared), всё остальное — USVString/
  `ToString`; forged brand — fallback; throwing `toString` —
  abrupt rules; Symbol бросает собственный `TypeError`, BigInt
  stringifies. Порядок наблюдаем и одинаков для Blob/File.
- `package::resolve_text_encoding(label, media_type)` — единственная
  функция выбора (explicit label → MIME `charset` через `mime_charset`
  → UTF-8; BOM-sniff через `new_decoder()` переопределяет fallback;
  malformed → U+FFFD; неизвестный пользовательский label — `None` →
  `EncodingError`). `FileReader` и `FileReaderSync` вызывают её;
  `resolve_label` удалён.
- `RegistrationIdentity(u64)` — opaque токен (`AtomicU64`, `build()`
  mint'ит, `Clone` сохраняет); `RegisteredSpecs` хранит identity первой
  регистрации. Повтор той же identity — idempotent (handle на уже
  зарегистрированное состояние); другая identity —
  `AlreadyRegistered` без мутации; после `shutdown` та же identity
  возвращает существующий закрытый handle (не живой runtime); разные
  Context независимы.
- `crates/boa_fapi/tests/m9_webidl_conformance.rs` — 12 тестов, trace
  rows `M9A-IDL-01/02/03`, `M9A-TEXT-01`, `M9A-REG-01`, `M9A-FLIST-01`;
  JS-матрица: Array/Set/generator/custom iterator/String object/
  Uint8Array-as-sequence/proxy getters/throwing next+done+value+part/
  early quota close/nested Blob+File/lone surrogates/object `toString`.
- `FileList`: нового surface нет; regression через host object
  (`length`, `item`, indexed props, descriptors, порядок, out-of-range
  `null`, brand, нет constructor/`Symbol.iterator`/`entries`/`keys`/
  `values`/`forEach`).

## Изменённые oracle (нормативная причина каждого)

- `m2_blob_file_filelist::string_parts_and_usv_replacement`: бывшие
  `TypeError` для `[123]`/`[null]`/`[{}]` переписаны на USVString-размеры
  (3/4/15) + добавлены `[undefined]` (9), `[true]` (4); Symbol оставлен
  `TypeError`. Причина: Web IDL union fallback `(BufferSource or Blob
  or USVString)` — только Symbol/BigInt-исключения `ToString` не
  stringify; простое удаление assertion не применялось.
- `m2_blob_file_filelist::hostile_values_never_panic`: `Proxy`/`Date`/
  `{length}` убраны из must-throw (теперь USVString-fallback и бросают
  только при недоступном `toString`); остались `Symbol` и
  `File(name={toString:null})` (неконвертируемый `toString`). Причина:
  та же union-развилка.
- `guards::sync_surface_is_bounded` (`package.rs must contain`):
  `resolve_label` → `resolve_text_encoding` + `mime_charset`. Причина:
  переименование единой функции выбора.

## DECISIONS.md

ADR-0036 (sequence), ADR-0037 (union), ADR-0038 (packaging+MIME),
ADR-0039 (identity+shutdown). Новых зависимостей нет.

## spec-delta.md

Добавлен change-control раздел про MIME `charset`-шаг (ссылка на W3C WD
packaging-data) + пометка о supersede array-only пункта 1. Исторические
handoff не изменены.

## Retrospective

- Conversion order: `@@iterator` читается один раз до создания
  итератора; `next`/`done`/`value` — слева направо; options (`endings`,
  `type`, `lastModified`) парсятся после `fileBits`/`fileName` в
  существующем порядке словаря; label конвертируется после
  brand/argument checks — throwing getters наблюдаются в нормативном
  порядке (тесты `*_read_once_and_left_to_right`, `*_order`).
- Exception precedence: abrupt `next`/`done`/`value`/`toString`/`return`
  не подменяются; throwing `return()` во время close выигрывает только
  над нормальным завершением, оригинальный throw — над нормальным
  `return()` (`iterator_close_and_propagate`; тест `return-boom` vs
  `conv-boom`).
- Iterator closing: quota/range-ошибки конверсии закрывают открытый
  итератор до escape; бесконечный итератор закрывается (`m9aClosed`)
  детерминированной `RangeError`.
- GC roots: конвертер не хранит `JsObject` в native state; части
  копятся в `PartsCollector` (`Bytes`/`Arc<BlobData>`), итератор живёт
  только на стеке вызова — `force_collect` не требуется, утечек корней
  нет.
- Ошибочные старые oracle: array-only `blob_parts`, `TypeError` для
  `123`/`null`/`{}`, отсутствие MIME-шага, правило (b) «каждый второй
  вызов» — все исправлены выше с тестами; `EncodingError` для
  неизвестного пользовательского label сохранён (тест sync-throw).

## Demo

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test m9_webidl_conformance -- --nocapture
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo deny check
cargo hack check --feature-powerset --depth 2
git diff --check
```

Все команды зелёные на `task/m9a` (deny: advisories/bans/licenses/
sources ok; powerset без errors). Остановлен перед M9-B: I/O модель,
Promise settlement, FileReader state machine, Streams и WPT runner не
тронуты.
