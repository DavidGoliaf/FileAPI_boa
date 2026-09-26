# Rework M9-A — encoding, порядок Web IDL и FileList iterator

| Поле | Значение |
|---|---|
| ID | `M9-A-REWORK-CONFORMANCE` |
| Ветка | продолжить работу на `task/m9a`; новую ветку не создавать |
| База проверки | текущий M9-A working tree поверх `1519c3c0edc6dcbac7330ef16ba410e3eb9ac564` |
| Нормативная база | File API WD 23.08.2026; Web IDL; Encoding Standard; `TZ_boa_fapi_FileAPI.md`; `tasks/12_TASK_M9A_WEBIDL_REGISTRATION.md` с переопределениями ниже |
| Результат | M9-A без `EncodingError` для неизвестного label, с нормативным порядком аргументов, BOM sniffing и точной FileList iterator surface |
| Предельный diff | совокупный M9-A production+tests+docs diff не более 3000 строк; при превышении остановиться и разделить rework |

## 1. Статус и цель

Текущий M9-A handoff не принимается. Реализация исправила array-only
`sequence<BlobPart>`, union fallback и registration identity, но содержит два
подтверждённых P0-расхождения и два дополнительных Web IDL defect. Один
предложенный defect — запрет BOM override для explicit label — отклоняется как
противоречащий Encoding Standard.

Задача этого rework — исправить только перечисленные ниже границы, обновить
неверные тестовые oracle и handoff, затем повторно передать M9-A на приёмку.
Не начинать M9-B и не менять async I/O architecture.

## 2. Нормативный приоритет и переопределения исходного заказа

Этот документ является change-control дополнением к
`tasks/12_TASK_M9A_WEBIDL_REGISTRATION.md`:

1. §4 исходного заказа читать так: explicit label выбирает fallback encoding;
   неизвестный label переходит к MIME `charset`, затем к UTF-8. BOM может
   переопределить любой выбранный fallback encoding на стадии Decode.
2. §2.4–2.5 исходного заказа о нормативном IteratorClose при abrupt completion
   отменяются. Web IDL sequence conversion не вызывает `iterator.return()` для
   ошибок `next`/`done`/`value`/element conversion.
3. §6 исходного заказа исправляется: pinned File API IDL не объявляет
   `iterable<File>`, но Web IDL автоматически добавляет `Symbol.iterator` для
   интерфейса с indexed property getter. `entries`/`keys`/`values`/`forEach`
   без value-iterator declaration не добавляются.
4. File API algorithm body не отменяет Web IDL argument conversion. Сначала
   все JS arguments конвертируются в IDL values слева направо, затем выполняются
   constructor steps и processing blob parts.

В `docs/DECISIONS.md` зафиксировать точную дату/commit Web IDL snapshot,
использованный для rework. Изменение living standard после этой точки требует
отдельного `docs/spec-delta.md` решения.

## 3. P0-1 — неизвестный explicit encoding label

### 3.1. Требуемое поведение

Для `readAsText(blob, encodingLabel)` реализовать W3C Packaging data / Text:

1. Начать с `encoding = failure`.
2. Если `encodingLabel` присутствует, выполнить Encoding Standard
   `get an encoding`.
3. Если результат failure, разобрать Blob MIME type и попытаться получить
   encoding из параметра `charset`.
4. Если результат всё ещё failure, выбрать UTF-8.
5. Декодировать bytes через Encoding Standard Decode с выбранным fallback.

Неизвестный explicit label не является исключением и не создаёт
`EncodingError` ни в `FileReader`, ни в `FileReaderSync`.

### 3.2. Изменения production-кода

- `package::resolve_text_encoding` должен возвращать `TextEncoding`, а не
  `Option<TextEncoding>`.
- Удалить ранний `?`, который завершает функцию после неизвестного label.
- Использовать точную семантику `get an encoding`; отдельно проверить label,
  соответствующий replacement encoding. `for_label_no_replacement` нельзя
  сохранять без нормативного обоснования и теста.
- Удалить fail-fast `EncodingError` branches из `filereader::read_as_text` и
  `filereader_sync::read_common`.
- Async reader после неизвестного label идёт по обычному start/read/load path;
  sync reader возвращает строку.
- Удалить устаревшие комментарии, утверждающие, что неизвестный label обязан
  вызвать `EncodingError`.

### 3.3. Обязательные тесты

Переписать, а не удалить, неверные oracle в:

- `crates/boa_fapi/tests/m4_filereader_async.rs` — unknown-label success,
  restart/model branches и event sequence;
- `crates/boa_fapi/tests/m4_filereader_sync.rs` — строковый результат вместо
  `EncodingError`;
- `crates/boa_fapi/tests/m9_webidl_conformance.rs` — заменить test в районе
  текущих строк 514–524.

Минимальная матрица:

```text
unknown label + text/plain;charset=windows-1252 + [E9] => "é"
unknown label + отсутствующий charset + UTF-8 bytes       => UTF-8 result
unknown label + неизвестный MIME charset + UTF-8 bytes    => UTF-8 result
отсутствующий label + валидный MIME charset               => MIME encoding
валидный explicit label + конфликтующий MIME charset      => explicit encoding
```

Для async ветки проверить `loadstart -> progress -> load -> loadend`,
`error === null`, отсутствие `error` event и отсутствие partial result.

## 4. P0-2 — BOM override оставить и доказать тестами

Предложение добавить `sniff_bom = false` для explicit label отклоняется.
File API передаёт выбранную кодировку в Encoding Standard как fallback;
Encoding Standard Decode выполняет BOM sniffing до запуска decoder и заменяет
fallback на UTF-8, UTF-16LE или UTF-16BE.

Требования:

- сохранить `encoding.new_decoder()` для incremental decode;
- не использовать `new_decoder_without_bom_handling()`;
- не добавлять provenance-флаг `explicit/fallback` для управления BOM;
- не реализовывать ручной UTF-8-only BOM strip;
- сохранить корректную обработку BOM, разделённого между chunks.

Добавить sync и async тесты:

```text
UTF-8 BOM + explicit windows-1252    => BOM удалён, payload декодирован UTF-8
UTF-16LE BOM + explicit windows-1252 => payload декодирован UTF-16LE
UTF-16BE BOM + explicit windows-1252 => payload декодирован UTF-16BE
без BOM + explicit windows-1252      => payload декодирован windows-1252
каждый BOM split после byte 1/2      => тот же итоговый результат
```

Эти тесты закрывают латентный пробел; изменение production-кода для данного
пункта не требуется, если существующий decoder проходит матрицу.

## 5. P0-3 — порядок Web IDL argument conversion

### 5.1. Нормативный порядок

```text
Blob(blobParts, options):
  blobParts sequence conversion -> options dictionary conversion -> body

File(fileBits, fileName, options):
  fileBits sequence conversion -> fileName USVString ->
  options dictionary conversion -> body
```

Текущие options-first/name-first paths запрещены. Нельзя оправдывать их тем,
что processing blob parts использует `options.endings`: processing начинается
после завершения Web IDL conversion всех arguments.

### 5.2. Двухфазная модель

Разделить Web IDL conversion и processing blob parts. Рекомендуемый внутренний
тип, не являющийся обязательным публичным API:

```rust
enum ConvertedBlobPart {
    Bytes(bytes::Bytes),
    Shared(std::sync::Arc<BlobData>),
    Text(String),
}
```

Sequence converter обязан во время открытой итерации:

- получить и пройти iterator;
- конвертировать каждый элемент в union `(BufferSource or Blob or USVString)`;
- немедленно скопировать видимый диапазон BufferSource;
- немедленно выполнить USVString/ToString conversion;
- сохранить immutable Blob/File backing;
- ограничить число частей до неограниченного накопления.

Он не применяет `endings`, MIME normalization и итоговый blob-size accounting,
поскольку options ещё не конвертированы. После sequence conversion constructor
конвертирует следующие аргументы, а затем отдельный processing step применяет
`endings` и собирает `BlobData` с проверкой итоговых лимитов.

Нельзя сохранять сырые `JsValue` до обработки `fileName`/options: их getters
могут изменить BufferSource или `toString`, нарушив conversion-time snapshot и
наблюдаемый порядок.

### 5.3. Обязательные order/precedence tests

- throwing `fileBits[Symbol.iterator]` наблюдается раньше throwing `fileName`;
- throwing element `toString` не читает `fileName` и options;
- при успешном sequence throwing `fileName` наблюдается до любого options
  getter;
- Blob полностью конвертирует parts до `options.endings`/`options.type`;
- BufferSource mutation из `fileName.toString` или options getter не меняет
  уже скопированные bytes;
- element `toString` side effects завершаются до `fileName.toString`;
- исключение более раннего аргумента остаётся итоговым и не подменяется
  исключением более позднего аргумента.

Порядок dictionary members проверять отдельно согласно Web IDL dictionary
algorithm; не выводить его из порядка полей Rust struct.

## 6. Дополнительный defect — IteratorClose

Актуальный Web IDL алгоритм создания `sequence<T>` выполняет
`IteratorStepValue`, затем конвертирует полученное значение в `T`. В нём нет
шага `IteratorClose`/`iterator.return()` при abrupt completion.

Требуется:

- удалить `iterator_close_and_propagate` из normative sequence path;
- не вызывать `return()` при ошибке `next`, `done`, `value` или BlobPart
  conversion;
- переписать M9-A tests, которые сейчас требуют `closed = true` или проверяют
  `return-boom` precedence;
- отдельно решить поведение implementation-generated `max_blob_parts` limit.
  Если оно вызывает `return()`, это observable extension и требует ADR;
  предпочтительно следовать единому Web IDL path без закрытия iterator;
- не называть IteratorClose доказательством Web IDL conformance в handoff.

Добавить negative tests, где `return()` меняет счётчик или бросает: при
ошибке element conversion `return()` не вызывается и не подменяет исходное
исключение.

## 7. Дополнительный defect — `FileList[Symbol.iterator]`

File API IDL объявляет indexed getter `item(unsigned long)`. При создании
interface prototype Web IDL вызывает `define the iteration methods`; интерфейс
с indexed property getter получает:

```text
FileList.prototype[Symbol.iterator] === Array.prototype.values
```

Требуется:

- определить `Symbol.iterator` на FileList prototype в relevant realm с
  нормативным descriptor;
- итерация возвращает File objects в indexed order и завершается по `length`;
- borrowed call соблюдает общую Array iterator semantics для array-like
  receiver; не добавлять лишний FileList-specific brand check, если его нет у
  `%Array.prototype.values%`;
- не добавлять `entries`, `keys`, `values`, `forEach`: FileList не объявляет
  value-iterator;
- удалить assertions и handoff-текст, требующие отсутствия Symbol.iterator.

Тестировать настоящий FileList, созданный через `FileApiHandle::file_list`, а
не обычный Array.

## 8. Handoff и документация

Обновить:

- `docs/reviews/M9A-handoff.md` — убрать заявления о `EncodingError`,
  options-first compatibility, нормативном IteratorClose и отсутствии
  FileList iterator;
- `docs/DECISIONS.md` — исправить/пометить superseded соответствующие M9-A ADR;
- `docs/spec-delta.md` — зафиксировать label → MIME → UTF-8 и BOM authority;
- `docs/spec-matrix.md` — указать точные source/test anchors для rework;
- актуальные crate docs/comments, содержащие старые правила.

Добавить trace rows:

| Trace ID | Требование |
|---|---|
| `M9A-RW-01` | unknown label falls through MIME charset to UTF-8 |
| `M9A-RW-02` | BOM overrides explicit and other fallback encodings |
| `M9A-RW-03` | Blob/File arguments converted left to right |
| `M9A-RW-04` | typed sequence conversion precedes options-dependent processing |
| `M9A-RW-05` | no IteratorClose on Web IDL sequence abrupt completion |
| `M9A-RW-06` | FileList indexed getter supplies only required Symbol.iterator |

## 9. Обязательная проверка

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

Дополнительно выполнить targeted source search: production/comments/tests не
должны утверждать, что unknown label даёт `EncodingError`, что FileList не
имеет `Symbol.iterator`, либо что IteratorClose является Web IDL requirement.

## 10. Критерии повторной приёмки

Rework принят только если одновременно:

1. Неизвестный explicit label успешно проходит MIME/UTF-8 fallback в sync и
   async API.
2. BOM override доказан для explicit label и split chunk boundaries.
3. Observable conversion order совпадает с Web IDL для Blob и File.
4. BufferSource копируется при element conversion, до side effects следующих
   arguments.
5. Web IDL abrupt completion не вызывает `iterator.return()`.
6. Настоящий FileList итерируется через `Array.prototype.values`, но не
   получает лишние iterator helper methods.
7. Все старые ошибочные oracle переписаны с нормативным объяснением.
8. Handoff описывает фактический код и новые результаты на текущем SHA.
9. Совокупный diff остаётся в пределах 3000 строк либо работа заранее
   разделена отдельным согласованным заказом.

После retrospective создать/обновить `docs/reviews/M9A-rework.md`, передать
ветку на независимую проверку и остановиться. M9-B до принятия rework не
начинать.
