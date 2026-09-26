# Заказ M9-A — Web IDL constructors, text packaging и registration identity

| Поле | Значение |
|---|---|
| ID | `M9-A-WEBIDL-REGISTRATION` |
| База | `master` at `1519c3c0edc6dcbac7330ef16ba410e3eb9ac564` |
| Ветка | `task/m9a` |
| Нормативная база | ТЗ §4.1, §5.1–5.5, §6.1, §6.4, §15; W3C File API WD 23.08.2026; Web IDL conversion algorithms |
| Закрывает | A-02, A-03, A-04, A-10; уточнение FileList surface |
| Предельный diff | 2500 строк production+tests+docs; generated reports не учитываются |

## 1. Цель

Исправить синхронную JS-границу до начала архитектурного I/O рефакторинга:
получать `sequence<BlobPart>` из любого допустимого iterable, выполнять точную
union conversion каждого элемента, привести text packaging к File API и
реализовать identity-aware повторную регистрацию.

В этом заказе запрещено менять модель выполнения I/O, Promise settlement,
FileReader state machine, Streams и WPT runner. Это границы M9-B…M9-E.

## 2. `sequence<BlobPart>`

Заменить array-only реализацию `webidl::blob_parts`/`collect_parts` общей
Web IDL sequence conversion для Blob и File constructors.

Обязательное поведение:

1. Получить `%Symbol.iterator%` один раз в нормативном порядке; отсутствие или
   non-callable iterator даёт синхронный `TypeError`.
2. Поддержать Array, custom iterable, boxed String и TypedArray как внешний
   `sequence`. Обычная primitive string в позиции `blobParts` остаётся
   non-object и бросает согласно Web IDL argument conversion.
3. Итерировать до `done`, конвертируя элементы слева направо; не читать
   `length` и numeric properties как замену iterator protocol.
4. Любое исключение из `@@iterator`, `next`, `done`, `value`, `toString` или
   options getter распространяется без подмены класса/сообщения.
5. При abrupt completion после создания iterator выполнить нормативный
   IteratorClose. Исключение `return()` обрабатывается в Web IDL/ECMAScript
   порядке, а не проглатывается.
6. Проверять `max_blob_parts`, `max_blob_size` и арифметику до неограниченного
   накопления. Бесконечный iterator детерминированно завершается quota/range
   ошибкой и закрывается.
7. `Blob` и `File` используют один converter; расхождение двух constructors
   запрещено.

## 3. `BlobPart` union conversion

Для union `(BufferSource or Blob or USVString)` реализовать точную
развилку Web IDL:

- настоящий BufferSource копирует только видимый диапазон;
- branded Blob/File добавляет immutable byte sequence без лишней копии;
- все остальные допустимые значения проходят USVString/ToString conversion;
- `123`, `true`, `false`, `null`, `undefined` и обычный object не являются
  автоматическим `TypeError`;
- forged Blob/File и объект с throwing `toString` не обходят brand и abrupt
  completion rules;
- conversion order наблюдаем через getters/proxy log и совпадает для Blob/File.

Удалить или переписать старые тесты, утверждающие `TypeError` для element
values `[123]`, `[null]` и `[{}]`. В handoff перечислить каждый изменённый
oracle и нормативную причину; простое удаление assertion запрещено.

## 4. Text packaging

Исправить `package::resolve_label` и общую sync/async packaging ветку.
Зафиксированный алгоритм:

1. Валидный explicit label выбирает encoding.
2. Если label отсутствует или не распознан, попытаться извлечь `charset` из
   Blob MIME type согласно W3C packaging-data steps.
3. Если MIME type/charset отсутствует или не распознан, выбрать UTF-8.
4. BOM переопределяет fallback encoding согласно Encoding Standard.
5. Malformed byte sequences дают U+FFFD; `EncodingError` для неизвестного
   пользовательского label запрещён.

Поскольку краткая формулировка ТЗ §6.4 не упоминает MIME `charset`, добавить
в `docs/spec-delta.md` точное change-control пояснение со ссылкой на W3C WD;
не изменять исторические handoff. `FileReader` и `FileReaderSync` должны
использовать одну функцию packaging и выдавать одинаковые строки.

## 5. Registration identity

Реализовать ТЗ §4.1 без сравнения trait objects по значениям:

- builder создаёт opaque registration identity;
- clone одного `FileApiExtension` сохраняет ту же identity;
- первый `register(context)` сохраняет identity в `RegisteredSpecs`;
- повторный `register` той же identity на том же Context не переустанавливает
  globals и возвращает handle к уже зарегистрированному состоянию;
- другая identity, даже с визуально одинаковой конфигурацией, возвращает
  `RegisterError::AlreadyRegistered` без mutation;
- повтор после `shutdown` не оживляет закрытый runtime; точный результат
  (`AlreadyRegistered` или отдельный typed error) фиксируется ADR и тестом;
- разные Context остаются независимыми.

Не вводить публичное сравнение config, не раскрывать identity и не ослаблять
атомарный preflight/rollback.

## 6. FileList boundary

Не добавлять `Symbol.iterator`/`entries`/`keys`/`values`/`forEach`: pinned W3C
IDL их не объявляет. Добавить regression test, который создаёт FileList через
`FileApiHandle::file_list` и проверяет только нормативную поверхность:
`length`, `item(index)`, indexed own properties, descriptors, порядок,
out-of-range `null`/`undefined`, brand и отсутствие публичного constructor.

## 7. Tests и traceability

Создать `crates/boa_fapi/tests/m9_webidl_conformance.rs` и добавить строки:

| Trace ID | Требование |
|---|---|
| `M9A-IDL-01` | iterable sequence и отсутствие array-only path |
| `M9A-IDL-02` | abrupt completion, IteratorClose и quota boundary |
| `M9A-IDL-03` | BlobPart union fallback для primitive/object values |
| `M9A-TEXT-01` | label → MIME charset → UTF-8 → BOM алгоритм |
| `M9A-REG-01` | same identity idempotent, different identity rejected |
| `M9A-FLIST-01` | точная pinned FileList surface через настоящий host object |

Минимальная JS-матрица включает Array, Set, generator, custom iterator,
String object, Uint8Array-as-sequence, proxy getters, throwing next/value,
early quota close, nested Blob/File, lone surrogates и object `toString`.

## 8. Приёмка

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

До handoff выполнить retrospective по conversion order, exception precedence,
iterator closing, GC roots и ошибочным старым oracle. Результат оформить в
`docs/reviews/M9A-handoff.md`; затем остановиться.
