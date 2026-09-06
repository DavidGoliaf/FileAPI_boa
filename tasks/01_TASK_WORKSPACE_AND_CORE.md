# Заказ исполнителю: M1 — workspace и независимое ядро File API

| | |
|---|---|
| **Задача** | M1: создать Cargo workspace и реализовать `boa_fapi_core` |
| **Проект** | `D:\projects\BoaX\boa-fapi` |
| **Нормативный источник** | `TZ_boa_fapi_FileAPI.md`, разделы 2, 3, 6, 10–12 и 14 |
| **Статус** | к исполнению |
| **Исполнитель** | кодовый агент; архитектурные решения этой задачи уже зафиксированы ниже |

## 1. Результат, который нужно сдать

Создай компилируемый Cargo workspace `boa-fapi` и production-ready крейт `boa_fapi_core`, не зависящий от Boa. В нём должна быть реализована безопасная модель неизменяемых байтовых данных, достаточная для последующих `Blob`/`File` bindings:

* лимиты и доменные ошибки;
* `ByteSource`, cancellation и memory source;
* сегментированный неизменяемый `BlobData`;
* конкатенация сегментов без копирования их данных;
* алгоритм `slice()` по File API;
* нормализация `Blob.type`;
* конвертация переводов строк для `endings = "native"`;
* unit-, property- и integration-тесты перечисленных правил;
* документация и матрица трассировки требований M1.

Это **не** задача по JS bindings. На момент сдачи в JavaScript ещё не обязаны существовать `Blob`, `File`, `FileReader`, `URL`, `ReadableStream` или DOMException.

## 2. Неизменяемые условия задачи

Следующие пункты являются контрактом заказчика. Исполнитель не вправе менять, ослаблять, переносить, переименовывать или интерпретировать их в свою пользу.

1. Единственная нормативная база — `TZ_boa_fapi_FileAPI.md`; для M1 применяется File API WD от 23.08.2026, зафиксированный в ТЗ.
2. `boa_fapi_core` **не зависит** от `boa_engine`, `boa_gc`, `boa_runtime`, JavaScript-типов или DOM-типов.
3. Никакого доступа к произвольному файловому пути, чтения диска, UI выбора файла, HTTP, URL store, FileReader, Worker или event loop в M1 нет.
4. Код production-крейтов использует safe Rust: `#![deny(unsafe_code)]`.
5. В путях, достижимых из публичного API, запрещены `unwrap()`, `expect()`, `panic!`, `unimplemented!()` и `todo!()`.
6. Публичные типы и функции имеют rustdoc; warnings компилятора и rustdoc запрещены.
7. Производительность и неизменяемость — часть функционального контракта: `slice()` и добавление существующего Blob source не копируют payload.
8. Тесты и условия приёмки этого файла неизменяемы. Если они конфликтуют с кодом, исправляется код, а не тест/критерий.

### Прямой запрет на подгонку и жульничество

Запрещено:

* редактировать этот заказ, `TZ_boa_fapi_FileAPI.md`, критерии приёмки, пороги, команды или матрицу требований ради зелёного результата;
* удалять, пропускать, игнорировать, помечать `#[ignore]`, ослаблять assertion либо делать тест условно-проходящим без письменного решения заказчика;
* добавлять wildcard exclusions, фальшивые mocks или feature flags, отключающие проверяемую production-логику;
* подменять реализацию константами/заглушками, которые проходят только известные тестовые данные;
* скрывать ошибку через `Result::ok`, пустой `match`, `#[allow(...)]`, снижение уровня lint, отключение документации или изменение команды CI;
* заявлять о прохождении команды, которую реально не запускали, либо заменять упавшую команду похожей;
* менять публичный контракт или архитектурные решения из этого файла без явного разрешения заказчика.

Если выполнение невозможно, не обходи ограничение: остановись, оставь рабочее дерево в диагностируемом состоянии и сообщи точный блокер, команду, вывод и влияние на приёмку.

## 3. Начальное состояние и граница изменений

Предполагай, что каталог может быть пустым и не быть Git-репозиторием. Это не повод сокращать задачу: создай workspace-файлы и исходники, но не инициализируй remote, не публикуй crates и не создавай коммит без отдельного указания.

Разрешённые изменения:

```text
Cargo.toml
Cargo.lock
rust-toolchain.toml
deny.toml
README.md
crates/boa_fapi_core/**
crates/boa_fapi/**
crates/boa_fapi_fs/**
crates/boa_fapi_wpt/**
tests/**
docs/architecture.md
docs/spec-matrix.md
docs/m1-validation.md
docs/m1-final-audit.md
benches/**
fuzz/**
```

Не добавляй сетевую зависимость, базу данных, runtime, файловый backend или hidden global state. Не меняй файлы за пределами проекта.

## 4. Готовая архитектура: реализовать именно её

### 4.1. Workspace

Создай следующий workspace:

```text
boa-fapi/
├── Cargo.toml
├── rust-toolchain.toml
├── deny.toml
├── README.md
├── crates/
│   ├── boa_fapi_core/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── blob.rs
│   │       ├── cancellation.rs
│   │       ├── endings.rs
│   │       ├── error.rs
│   │       ├── limits.rs
│   │       ├── mime.rs
│   │       ├── snapshot.rs
│   │       └── source/
│   │           ├── mod.rs
│   │           └── memory.rs
│   ├── boa_fapi/          # компилируемый пустой фасад, без Boa dependency до M2
│   ├── boa_fapi_fs/       # компилируемый пустой фасад, без disk I/O до M5
│   └── boa_fapi_wpt/      # компилируемый dev facade, без WPT harness до M7
├── docs/
│   ├── architecture.md
│   ├── spec-matrix.md
│   └── m1-validation.md
└── tests/
```

Все члены workspace используют edition 2024 и MSRV 1.91.0. В root `Cargo.toml` зафиксируй общие `package` metadata, workspace dependencies и строгие lint-настройки. Мелкие placeholder-крейты не содержат API-заглушек: они должны иметь только минимальный `lib.rs` с пояснением, что функциональность появится на своём этапе.

### 4.2. Допустимые зависимости M1

В `boa_fapi_core` разрешены только:

```toml
bytes = "1"
thiserror = "2"
```

Для тестов разрешены `proptest` и стандартная библиотека. Не добавляй `async`, `tokio`, `futures`, MIME parser, URL parser, Boa и файловые зависимости: они не нужны данной задаче. Версии фиксируются lock-файлом.

### 4.3. Модули и публичные контракты

Ниже приведены обязательные имена и семантика. Допустимы приватные helpers, но не замена этих контрактов собственными альтернативами.

#### `cancellation.rs`

```rust
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(/* Arc<AtomicBool> */);

impl CancellationToken {
    pub fn new() -> Self;
    pub fn cancel(&self);
    pub fn is_cancelled(&self) -> bool;
}
```

* `cancel()` идемпотентен, clone наблюдает тот же флаг.
* Тип не блокируется, не выполняет I/O и не хранит callback.

#### `snapshot.rs`

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SnapshotState {
    Memory,
}
```

В M1 существует только `Memory`. Будущие внешние файловые варианты добавятся в M5. Не симулируй filesystem snapshot сейчас.

#### `error.rs`

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceLimitKind {
    BlobSize,
    BlobParts,
    BlobSegments,
    MaterializeBytes,
    ConcurrentReads,
    BlobUrls,
    DataUrlOutput,
    SyncReadBytes,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FileApiError {
    #[error("the resource was not found")]
    NotFound,
    #[error("the resource is unsafe")]
    UnsafeFile,
    #[error("too many concurrent reads")]
    TooManyReads,
    #[error("the source snapshot changed")]
    SnapshotChanged,
    #[error("the source cannot be read")]
    FileLocked,
    #[error("access to the source was denied")]
    PermissionDenied,
    #[error("resource limit exceeded: {0:?}")]
    ResourceLimit(ResourceLimitKind),
    #[error("read cancelled")]
    Cancelled,
    #[error("source range is invalid")]
    InvalidRange,
    #[error("internal File API error")]
    Internal,
}
```

Не включай в `Display` путь, байты, конфиденциальное имя файла или platform-specific error string.

#### `limits.rs`

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileApiLimits {
    pub max_blob_size: u64,
    pub max_materialize_bytes: u64,
    pub max_sync_read_bytes: u64,
    pub max_parts: usize,
    pub max_segments_after_normalize: usize,
    pub max_concurrent_reads_per_global: usize,
    pub max_blob_urls_per_global: usize,
    pub default_chunk_size: usize,
    pub max_data_url_output: u64,
}
```

`Default` обязан использовать ровно эти значения:

| Поле | Значение |
|---|---:|
| `max_blob_size` | 2 GiB |
| `max_materialize_bytes` | 256 MiB |
| `max_sync_read_bytes` | 32 MiB |
| `max_parts` | 1_000_000 |
| `max_segments_after_normalize` | 65_536 |
| `max_concurrent_reads_per_global` | 64 |
| `max_blob_urls_per_global` | 10_000 |
| `default_chunk_size` | 64 KiB |
| `max_data_url_output` | 256 MiB |

Реализуй `validate(&self) -> Result<(), FileApiError>`. Нулевое значение любого количественного лимита, `max_sync_read_bytes > max_materialize_bytes`, `max_materialize_bytes > max_blob_size`, а также chunk больше materialize limit — ошибка `ResourceLimit` с наиболее точным `ResourceLimitKind`.

#### `mime.rs`

```rust
pub fn normalize_blob_type(input: &str) -> String;
```

Алгоритм фиксирован:

1. Если хотя бы один Unicode scalar input вне диапазона U+0020..U+007E, вернуть пустую строку.
2. Иначе вернуть строку с ASCII lowercase (`A..Z` → `a..z`), оставляя прочие допустимые ASCII-символы без изменений.
3. Не trim-ить, не парсить как MIME, не валидировать slash/parameters и не sniff-ить содержимое.

`" TEXT/PLAIN "` нормализуется в `" text/plain "`; `"text/\u{00E9}"` и `"text/\nplain"` дают `""`.

#### `endings.rs`

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeLineEnding { Lf, Crlf }

pub fn convert_line_endings_to_native(input: &str, target: NativeLineEnding) -> String;
```

Нормализуй каждый одиночный CR, одиночный LF и CRLF как ровно один target ending. Все остальные code points, включая non-ASCII и непричастные CR/LF суррогатные replacement characters в Rust string, сохраняются. Функция не принимает путь и не читает platform config; выбор `Lf/Crlf` явный и тестируемый.

#### `source/mod.rs` и `source/memory.rs`

```rust
pub trait ByteSource: Send + Sync + 'static {
    fn len(&self) -> u64;
    fn snapshot(&self) -> SnapshotState;
    fn read_range(
        &self,
        range: std::ops::Range<u64>,
        cancel: &CancellationToken,
    ) -> Result<bytes::Bytes, FileApiError>;
}

#[derive(Clone, Debug)]
pub struct MemorySource { /* Bytes */ }

impl MemorySource {
    pub fn new(bytes: bytes::Bytes) -> Self;
}
```

Обязательная семантика `MemorySource::read_range`:

* сначала проверяет cancellation и возвращает `Cancelled`;
* диапазон с `start > end` либо `end > len()` возвращает `InvalidRange`;
* валидный диапазон, включая пустой и `0..len()`, возвращает ровно эти байты;
* результат — `Bytes::slice`, а не копия payload;
* snapshot всегда `SnapshotState::Memory`;
* источник не изменяется после `new()`.

#### `blob.rs`

```rust
pub struct BlobSegment {
    pub source: std::sync::Arc<dyn ByteSource>,
    pub offset: u64,
    pub len: u64,
}

pub struct BlobData {
    // private fields
}

impl BlobData {
    pub fn empty(media_type: impl AsRef<str>) -> Self;
    pub fn from_segments(
        segments: Vec<BlobSegment>,
        media_type: impl AsRef<str>,
        limits: &FileApiLimits,
    ) -> Result<Self, FileApiError>;

    pub fn size(&self) -> u64;
    pub fn media_type(&self) -> &str;
    pub fn snapshot(&self) -> &SnapshotState;
    pub fn segment_count(&self) -> usize;
    pub fn slice(
        &self,
        start: Option<i64>,
        end: Option<i64>,
        content_type: Option<&str>,
        limits: &FileApiLimits,
    ) -> Result<Self, FileApiError>;
}
```

Требования к `from_segments`:

1. `media_type` пропускается через `normalize_blob_type`.
2. Нулевая длина сегмента допустима, но не должна приводить к неверному размеру/переполнению. Можно нормализовать такие сегменты, если это не меняет семантику.
3. Для каждого сегмента проверяется `offset <= source.len()` и `offset + len <= source.len()` через checked arithmetic. Нарушение даёт `InvalidRange`; overflow никогда не wrap-ится.
4. Сумма длин считается через `checked_add`. Overflow или превышение `max_blob_size` — `ResourceLimit(BlobSize)`.
5. Число сегментов после допустимой нормализации не превышает `max_segments_after_normalize`; иначе `ResourceLimit(BlobSegments)`.
6. Сегменты сохраняют `Arc<dyn ByteSource>`: bytes существующего source не копируются.
7. M1 создаёт Blob только из memory sources, поэтому `snapshot()` результата — `Memory`. Код не должен делать предположений о будущем filesystem source.

### 4.4. Алгоритм `BlobData::slice`

Это ядро будущего `Blob.prototype.slice()`. Web IDL-конвертация значений JavaScript в `Option<i64>` появится в M2; M1 реализует только описанную целочисленную семантику.

Пусть `original_size = self.size()`:

1. `start = None` → `relative_start = 0`.
2. `start < 0` → `relative_start = max(original_size + start, 0)` с арифметикой без overflow.
3. `start >= 0` → `relative_start = min(start as u64, original_size)`.
4. `end = None` → `relative_end = original_size`.
5. `end < 0` → `relative_end = max(original_size + end, 0)`.
6. `end >= 0` → `relative_end = min(end as u64, original_size)`.
7. `span = max(relative_end - relative_start, 0)`.
8. `content_type = None` означает пустую строку; иначе применяется `normalize_blob_type`.
9. Результат содержит сегменты, пересекающие абсолютный диапазон `[relative_start, relative_start + span)`, со скорректированными offset/len.
10. Payload не копируется. Ссылки `Arc` на исходные `ByteSource` сохраняются.
11. Для empty span вернуть корректный пустой Blob с нормализованным type. Он не обязан сохранять пустые исходные сегменты.
12. Результат обязан проходить те же лимиты `max_blob_size` и `max_segments_after_normalize`.

Сложность: O(k), где k — число сегментов, пересекающих slice; допустим fast path O(1) для одного сегмента. Реализация с materialize всего Blob, сканированием каждого байта или копированием payload не принимается.

## 5. Обязательная документация

### `README.md`

Коротко объясни назначение workspace, список будущих крейтов и текущую границу M1. Не заявляй реализацию JavaScript File API, пока bindings отсутствуют.

### `docs/architecture.md`

Опиши ровно три слоя:

```text
M1 core (данные и алгоритмы, без Boa)
  → будущий boa_fapi (Web IDL / GC / jobs)
  → будущие host adapters (FS / DOM / Streams / URL)
```

Объясни, что `ByteSource` — граница, через которую будущий File source будет проверять snapshot и cancellation. Не добавляй фактическую FS-реализацию.

### `docs/spec-matrix.md`

Создай таблицу как минимум с этими идентификаторами:

| ID | Нормативное правило | Код | Тест |
|---|---|---|---|
| M1-CORE-01 | core не зависит от Boa | Cargo.toml | dependency guard |
| M1-CORE-02 | ByteSource range/cancel contract | source | unit/property |
| M1-CORE-03 | неизменяемый memory source | source/memory | mutation/no-copy |
| M1-CORE-04 | Blob type normalization | mime | table/property |
| M1-CORE-05 | native line endings | endings | exhaustive table/property |
| M1-CORE-06 | segment validation и checked arithmetic | blob | boundary tests |
| M1-CORE-07 | File API slice semantics | blob | unit/property |
| M1-CORE-08 | limits validation | limits | unit |
| M1-CORE-09 | safe Rust/no panics in public paths | workspace | lint/review |

Заполни реальные путь теста и функцию/тип после реализации, а не placeholder-текстом.

### `docs/m1-validation.md`

После запуска проверок запиши дату, toolchain, ОС, команды, exit code и краткий фактический итог. Нельзя писать «PASS», если команда не выполнялась. Внешне заблокированная команда должна быть отмечена `BLOCKED` с точной причиной; это не является приёмкой.

## 6. Тесты, которые обязательно написать

Тесты должны жить либо рядом с модулем, либо в `crates/boa_fapi_core/tests/`. Их имена должны отражать сценарий. Минимальный набор:

### 6.1. `mime`

* empty type;
* ASCII lowercase;
* mixed-case type;
* пробелы сохраняются;
* DEL, LF, CR, NUL, non-ASCII и emoji дают empty type;
* property: результат либо пустой, либо содержит только U+0020..U+007E и не содержит ASCII uppercase.

### 6.2. `endings`

Для **обоих** target (`Lf`, `Crlf`) проверить пустую строку, нет переводов, CR, LF, CRLF, CRCR, LFLF, CRLFCR, начало/конец строки, смешанный Unicode-текст. Property: в результате не остаётся CR, не входящий в CRLF, и количество логических переводов равно входному.

### 6.3. `limits`

* exact defaults из раздела 4.3;
* каждый нулевой лимит отдельно;
* каждое недопустимое отношение `sync > materialize`, `materialize > blob`, `chunk > materialize`;
* корректная конфигурация около границ.

### 6.4. `MemorySource` и cancellation

* full/empty/prefix/middle/suffix range;
* `start > end`, `end > len`, край `u64::MAX`;
* cancel до чтения;
* clone token видит cancel;
* `Bytes` result разделяет allocation с input, если API `Bytes` позволяет это доказать без доступа к внутренностям; как минимум тестируй, что его содержимое корректно и source не materialize-ится в `Vec`.

### 6.5. `BlobData::from_segments`

* empty Blob;
* один и несколько источников;
* смещения и длины на обеих границах;
* нулевой сегмент;
* invalid offset/end и overflow;
* total size ровно на лимите и на один байт выше;
* число сегментов ровно на лимите и на один выше;
* normalised MIME type;
* `MemorySource` и Blob остаются неизменяемыми: повторные read и slices возвращают исходное содержимое, а публичный API не предоставляет mutable access к payload.

### 6.6. `BlobData::slice`

Проверить содержимое через чтение сегментов, а не через несуществующий JS API:

* `slice(None, None)`;
* start/end равны 0 и size;
* положительные границы за size;
* отрицательные границы `-1`, `-size`, меньше `-size`, `i64::MIN`;
* `i64::MAX`;
* end < start;
* пустой Blob;
* slice, пересекающий один, два и все сегменты;
* override type: normal, invalid Unicode, отсутствующий;
* исходный Blob не изменяется;
* result разделяет те же source `Arc` (докажи pointer equality через `Arc::ptr_eq` в тестовом accessor либо другим наблюдаемым тестовым hook, не раскрывая mutable internals в production API);
* property test: для произвольного массива байтов и допустимых start/end materialized результат равен эталонной безопасной реализации алгоритма на `Vec<u8>`; эталон не используется production-кодом.

### 6.7. Dependency и quality guards

* Проверка, что `boa_fapi_core/Cargo.toml` не содержит `boa_`, `tokio`, `futures`, `url`, filesystem backend или MIME parser dependency.
* Проверка, что public API не содержит `Path`, `PathBuf`, raw file descriptor и `JsValue`.
* Поиск production source на `unwrap(`, `expect(`, `panic!`, `todo!`, `unimplemented!` с allowlist только для тестов. Если guard проверяется script-ом, он сам должен быть протестирован/прост и не исключать production directories.

## 7. Что не делать

Не делать раньше срока:

* `Blob`/`File` JavaScript constructors, Web IDL coercions, `ArrayBuffer`, `TypedArray`;
* FileReader, promise jobs, EventTarget, DOMException, ProgressEvent;
* stream/text/data URL/encoding;
* FileList, structured clone, IndexedDB bridge;
* filesystem-backed source, path policy, snapshot checking beyond `Memory`;
* URL.createObjectURL, Fetch integration, WPT harness;
* оптимизацию ценой нарушения ограничений этого задания.

Если у тебя есть идея расширить M1, опиши её в финальном отчёте как предложение для M2+, но не реализуй.

## 8. Обязательные команды валидации

Выполни из корня workspace и приложи полный компактный итог каждой команды:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo test --package boa_fapi_core --doc
cargo llvm-cov --package boa_fapi_core --all-features --fail-under-lines 85
cargo hack check --feature-powerset --depth 2
$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny fetch db
$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny check
git diff --check
```

Если `cargo hack` или `cargo deny` не установлены, установи их только при разрешённой среде; если сеть/установка недоступны, явно зафиксируй `BLOCKED`. Не удаляй эти команды и не заменяй их на `cargo check`.

## 9. Условия приёмки

M1 принимается **только**, когда одновременно выполнены все пункты:

- [ ] Workspace имеет все четыре заданных члена и собирается с `--workspace --all-features`.
- [ ] `boa_fapi_core` не имеет зависимости от Boa, DOM, async runtime, URL, MIME parser или файловой системы.
- [ ] Все публичные контракты раздела 4 существуют с описанной семантикой.
- [ ] `BlobData` валидирует диапазоны и все арифметические операции без wrap/overflow.
- [ ] `BlobData::slice` проходит все boundary и property-тесты, не копируя исходный payload.
- [ ] MIME и newline алгоритмы соответствуют разделам 4.3 и 6.
- [ ] Лимиты совпадают с ТЗ и не допускают некорректных конфигураций.
- [ ] Public API не раскрывает путь/handle/JS типы и не создаёт filesystem access.
- [ ] Есть заполненная трассировка M1-CORE-01…M1-CORE-09 с реальными ссылками на код и тесты.
- [ ] Строковое покрытие `boa_fapi_core` не ниже 85%; coverage не исключает production-модули и не заменяет обязательные сценарии.
- [ ] Все команды раздела 8 прошли с exit code 0; ни одна не замаскирована и не имеет unexpected skip.
- [ ] В `docs/m1-validation.md` записаны реальные результаты, а не ожидаемые.
- [ ] Выполнен и задокументирован финальный audit-pass из раздела 10; в нём нет открытых ошибок, недоработок или непроверенных обязательных требований.
- [ ] После всех исправлений весь набор команд раздела 8 повторно выполнен в финальном audit-pass и прошёл с exit code 0.
- [ ] Рабочее дерево проверено `git diff --check`; если Git отсутствует, это явно указано в отчёте, а все прочие проверки всё равно выполнены.

Никакой процент покрытия не заменяет обязательные сценарии. Если coverage tool доступен, добавь отчёт, но низкое покрытие или непройденный mandatory test не может быть закрыт декларацией «достаточно».

## 10. Обязательный финальный audit-pass

Этот проход выполняется **после** того, как реализация и первоначальный прогон команд раздела 8 уже завершены. Его цель — не повторить формально тесты, а самостоятельно найти расхождения между кодом и этим заказом, устранить их и доказать, что все условия реально выполнены.

Не разрешается завершать M1 без этого прохода, даже если первый запуск тестов был зелёным.

### 10.1. Шаг A — сверка требований с реализацией

Пройди требования M1-CORE-01…M1-CORE-09, каждый обязательный API-контракт раздела 4, каждый граничный случай раздела 6 и каждый пункт раздела 9.

Для **каждого** пункта самостоятельно проверь:

1. точный файл и символ, реализующий требование;
2. конкретный тест, доказывающий нормальный и ошибочный/граничный путь;
3. отсутствие противоречащего кода или обхода условия;
4. совпадение фактической семантики с формулировкой заказа, а не только совпадение имени функции.

Заполни `docs/m1-final-audit.md` таблицей: `ID`, `требование`, `код (файл:символ)`, `тест`, `вердикт`, `найденная проблема и исправление`. Значения `не проверено`, `предположительно`, `позже`, `N/A` и пустая ссылка недопустимы для обязательного пункта.

### 10.2. Шаг B — самостоятельный поиск дефектов

Отдельно проаудируй исходники, не ограничиваясь уже существующими тестами:

* Cargo dependency graph: нет Boa, DOM, async runtime, URL, MIME parser, FS I/O и запрещённых скрытых зависимостей в `boa_fapi_core`;
* public API: нет `Path`, `PathBuf`, file descriptor, `JsValue`, mutable payload access или неоговорённой файловой возможности;
* арифметика: нет `as`-преобразования, wraparound или unchecked `+/-` там, где работают `u64` offsets, длины, сумма сегментов и границы `i64::MIN`/`i64::MAX`;
* `MemorySource`: cancellation проверяется до range access, invalid range не выдаёт частичный результат, payload не копируется;
* `BlobData`: все сегменты валидируются, размер/лимиты проверяются после нормализации, `slice` корректно работает на пустых и многосегментных данных и не копирует bytes;
* `mime` и `endings`: правила не «улучшены» MIME-парсером, trim-ом, locale-case conversion или зависимостью от текущей ОС;
* production source: нет `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`, blanket `allow`, проглатывания ошибок или утечки чувствительных данных в `Display`/docs;
* границы M1: нет преждевременной реализации JS, DOM, filesystem, stream, URL, WPT или скрытого global state;
* документы и матрица: ссылки ведут на существующий код и тесты, а текст не заявляет несуществующую функциональность.

Используй целевой поиск по исходникам (`rg`), чтение кода, запуск тестов и проверку manifest/dependency tree. Не полагайся на имя файла, комментарий или предыдущий статус как на доказательство реализации.

### 10.3. Шаг C — устранение найденного

Если audit нашёл ошибку, недоработку, отсутствие теста, слабый тест, несоответствие документу или нарушение границы M1, исполнитель обязан:

1. исправить production-код, тест или документацию в пределах задачи;
2. добавить регрессионный тест, когда дефект мог бы повториться;
3. повторить относящиеся к дефекту тесты;
4. обновить точную строку `docs/m1-final-audit.md` с описанием причины и исправления;
5. начать audit-pass заново с шага A, чтобы исправление не сломало другой контракт.

Запрещено считать дефект «допустимым», если он не отменён заказчиком письменно. Его нельзя закрыть `#[ignore]`, ослаблением assertion, исключением path из coverage, сокращением теста или изменением этой задачи/ТЗ.

### 10.4. Шаг D — финальная независимая валидация

Только после чистых шагов A и B повторно запусти **весь** раздел 8 в исходном порядке. Это второй, финальный запуск; результаты первоначального запуска не заменяют его.

В `docs/m1-final-audit.md` зафиксируй для каждой команды точную команду, exit code, `PASS`/`FAIL`/`BLOCKED`, а также факт, что она выполнена после последнего изменения production-кода. `BLOCKED` означает, что M1 не принят, пока блокер не устранён или заказчик явно не изменит условия приёмки.

### 10.5. Минимальный итог audit-pass

Финальный audit считается чистым, только если одновременно:

- каждая строка M1-CORE-01…09 имеет реальное доказательство в коде и тестах;
- нет найденных, но не устранённых дефектов или непроверенных boundary cases;
- весь mandatory test suite и coverage выполнены после последней правки;
- отсутствуют замаскированные failures/skips и изменение условий задачи;
- `docs/m1-final-audit.md` позволяет другому ревьюеру воспроизвести вывод без догадок.

## 11. Формат финального отчёта исполнителя

Финальный ответ должен быть коротким и фактологичным:

1. список созданных/изменённых файлов;
2. выполненные контракты M1-CORE-01…09;
3. результаты каждой команды из раздела 8: `PASS`, `FAIL` или `BLOCKED`;
4. точные известные ограничения/блокеры;
5. отсутствие изменений в ТЗ, этом заказе и условиях приёмки;
6. итог финального audit-pass: число требований, найденные и устранённые дефекты, результаты повторного набора команд;
7. если пользователь запросил commit — только тогда статус, diff и hash коммита.

Не сообщай «готово», пока все условия раздела 9 и чистый audit-pass раздела 10 не выполнены. Не прячь частичную готовность за общими формулировками.
