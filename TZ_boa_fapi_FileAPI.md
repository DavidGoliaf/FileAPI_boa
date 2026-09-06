# Техническое задание

## Разработка крейта `boa-fapi` — реализация File API для JS-движка Boa

| | |
|---|---|
| **Документ** | ТЗ на разработку программного компонента |
| **Продукт** | `boa-fapi` — реализация W3C File API для встраиваемого JS-движка Boa |
| **Версия ТЗ** | 1.0 |
| **Дата** | 2026-09-05 |
| **Целевая платформа** | Rust 1.91.0 (edition 2024), `boa_engine` 0.22.x |
| **Нормативная спецификация** | [File API, W3C Working Draft, 23 August 2026](https://www.w3.org/TR/2026/WD-FileAPI-20260823/) |
| **Статус** | к исполнению |

---

## Оглавление

1. [Общие сведения](#1-общие-сведения)
2. [Целевая платформа, зависимости и структура проекта](#2-целевая-платформа-зависимости-и-структура-проекта)
3. [Архитектура](#3-архитектура)
4. [Публичный Rust API](#4-публичный-rust-api)
5. [Поверхность JavaScript и требования Web IDL](#5-поверхность-javascript-и-требования-web-idl)
6. [Модель данных Blob, File и FileList](#6-модель-данных-blob-file-и-filelist)
7. [Чтение данных и интеграция с event loop Boa](#7-чтение-данных-и-интеграция-с-event-loop-boa)
8. [Blob URL](#8-blob-url)
9. [Ошибки и исключения](#9-ошибки-и-исключения)
10. [Безопасность, приватность и лимиты](#10-безопасность-приватность-и-лимиты)
11. [Нефункциональные требования](#11-нефункциональные-требования)
12. [Тестирование и WPT](#12-тестирование-и-wpt)
13. [Документация, сборка и поставка](#13-документация-сборка-и-поставка)
14. [Этапы работ](#14-этапы-работ)
15. [Критерии приёмки](#15-критерии-приёмки)
16. [Риски и открытые вопросы](#16-риски-и-открытые-вопросы)
17. [Приложения](#17-приложения)

---

## 1. Общие сведения

### 1.1. Назначение и цель

Разработать самостоятельный Cargo workspace `boa-fapi`, который добавляет в среду исполнения на базе **Boa** реализацию **File API** в объёме нормативной спецификации W3C File API (далее — «Спека»).

Компонент предназначен для хост-приложений на Rust, которые исполняют JavaScript вне браузера и должны предоставить совместимые с Web Platform объекты бинарных данных, файлов и асинхронного чтения без запуска браузера или Node.js.

**Ключевое требование:** JavaScript-код, использующий `Blob`, `File`, `FileList`, `FileReader`, `FileReaderSync` и `URL.createObjectURL()`/`URL.revokeObjectURL()`, должен работать без модификаций в пределах явно поддерживаемого типа global environment и документированных границ интеграции.

### 1.2. Термины и сокращения

| Термин | Определение |
|---|---|
| **Спека** | W3C File API, WD от 23.08.2026 |
| **Boa** | `boa_engine` 0.22.x и совместимые `boa_gc`, `boa_runtime` |
| **Хост** | Rust-приложение, встраивающее Boa и `boa-fapi` |
| **Blob** | Неизменяемая последовательность байтов с MIME-типом |
| **File** | `Blob` с именем, временем последнего изменения и snapshot state |
| **Byte source** | Внутренний источник байтов: память, файл или диапазон другого источника |
| **Snapshot state** | Зафиксированное состояние внешнего файла, проверяемое перед/во время чтения |
| **Global** | Логическая среда `Window`, `DedicatedWorker` или `SharedWorker`, заданная хостом |
| **Environment key** | Идентификатор origin/storage partition/global для изоляции blob URL |
| **Task source** | Очередь задач File Reading Task Source |
| **WPT** | `web-platform-tests`, каталог `FileAPI/**` |

### 1.3. Область работ версии 1.0

Реализуются:

1. `Blob`, `BlobPropertyBag`, `BlobPart`, `EndingType`.
2. `File`, `FilePropertyBag`.
3. `FileList`, включая indexed getter, `item()` и `length`.
4. `FileReader` со всеми состояниями, методами, обработчиками и событиями.
5. `FileReaderSync` для worker-подобных global environments.
6. `Blob.slice()`, `stream()`, `text()`, `arrayBuffer()`, `bytes()`, `textStream()`.
7. Сериализация и десериализация `Blob`, `File`, `FileList` через подключаемый structured-clone bridge.
8. Blob URL store, создание, отзыв, разрешение URL и очистка при уничтожении global.
9. Минимальный DOM-слой (`EventTarget`, `Event`, `ProgressEvent`, `DOMException`), если хост не предоставляет совместимый слой.
10. Интеграция с promise jobs, task queues и `JobExecutor` Boa.
11. Безопасный host-side API создания `File` из памяти и из разрешённого файлового ресурса.
12. WPT-раннер и фиксируемый манифест поддерживаемых тестов.

### 1.4. Вне области работ версии 1.0

| Возможность | Решение |
|---|---|
| UI выбора файлов, `<input type="file">`, drag-and-drop | Не реализуются; файлы передаёт хост через Rust API |
| Полные HTML DOM, Window и Workers | Не реализуются; тип global задаётся конфигурацией |
| File System Access API (`FileSystemFileHandle`, запись файлов) | Отдельная спецификация; File API предоставляет только неизменяемые снимки |
| Полная реализация WHATWG Fetch | Не входит; предоставляется host-side resolver blob URL |
| Полная реализация WHATWG URL | Не входит; методы Blob URL добавляются к URL хоста либо к минимальному совместимому shim |
| `MediaSource` в `URL.createObjectURL()` | Не входит до появления Media Source API для Boa; передача иного бренда даёт `TypeError` |
| Service Worker | `FileReaderSync` не выставляется; blob URL не создаются в service-worker global |
| Прямое открытие пути из JavaScript | Запрещено; `new File()` принимает данные, но никогда не трактует строку как путь |
| Запись или изменение исходного файла | Не поддерживается |
| `File.webkitRelativePath` | Ненормативное расширение, не реализуется |

### 1.5. Нормативные и справочные источники

1. [W3C File API](https://www.w3.org/TR/FileAPI/) — основной источник требований.
2. [WHATWG Encoding](https://encoding.spec.whatwg.org/) — UTF-8 decode, BOM sniffing, label resolution, `TextDecoderStream`.
3. [WHATWG Streams](https://streams.spec.whatwg.org/) — readable byte streams и чтение всех байтов.
4. [WHATWG DOM](https://dom.spec.whatwg.org/) — `EventTarget`, события и dispatch.
5. [WHATWG HTML](https://html.spec.whatwg.org/) — tasks, globals, serializable objects, environment settings.
6. [WHATWG URL](https://url.spec.whatwg.org/) — разбор и сериализация blob URL.
7. [WHATWG MIME Sniffing](https://mimesniff.spec.whatwg.org/) — parsable MIME type.
8. [Web IDL](https://webidl.spec.whatwg.org/) — преобразования типов, brand checks и дескрипторы.
9. ECMA-262 — `Promise`, `ArrayBuffer`, TypedArray, строки и время Unix Epoch.
10. [web-platform-tests](https://github.com/web-platform-tests/wpt/tree/master/FileAPI) — приёмочный набор conformance-тестов.

### 1.6. Правила трактовки Спеки

* Нормативные MUST/SHOULD/MAY из Спеки имеют приоритет над примерами этого ТЗ.
* Фиксированной базой разработки является версия WD от 23.08.2026. Изменения latest draft после этой даты принимаются только отдельным решением и фиксируются в `docs/spec-delta.md`.
* Известные открытые вопросы самой Спеки (в частности размер chunk, момент `loadstart`, детализация Data URL и security hooks) реализуются согласно разделу 16 и закрепляются тестами проекта.
* Если браузерное понятие отсутствует в Boa, оно не подменяется глобальным singleton: хост обязан передать эквивалент через конфигурацию среды.

---

## 2. Целевая платформа, зависимости и структура проекта

### 2.1. Toolchain

| Параметр | Значение |
|---|---|
| MSRV | 1.91.0 |
| Rust edition | 2024 |
| Boa | `boa_engine`, `boa_gc` 0.22.x |
| Основные CI-платформы | Linux x86_64, macOS aarch64, Windows x86_64 MSVC |
| `wasm32-unknown-unknown` | `boa_fapi_core` и memory-only конфигурация должны собираться |

Обязательные правила:

* `#![deny(unsafe_code)]` во всех production-крейтах.
* `#![warn(missing_docs)]`; strict clippy без предупреждений.
* Запрещены `unwrap`, `expect`, `panic!` на путях, достижимых из JS или host API.
* Состояние Boa (`JsValue`, `JsObject`, `Context`) не передаётся в IO-потоки.
* Все размеры из Web IDL проверяются до преобразования в `usize`; 32-разрядные цели не должны переполняться.

### 2.2. Состав workspace

```text
boa-fapi/
├── Cargo.toml
├── rust-toolchain.toml
├── deny.toml
├── crates/
│   ├── boa_fapi/          # фасад, JS bindings, DOM/Streams shim, регистрация
│   ├── boa_fapi_core/     # engine-independent модель Blob/File и алгоритмы
│   ├── boa_fapi_fs/       # безопасный read-only источник локальных файлов
│   └── boa_fapi_wpt/      # dev-only WPT runner
├── tests/
├── benches/
├── fuzz/
├── examples/
└── docs/
```

`boa_fapi_core` не зависит от `boa_engine`. Он содержит byte sources, диапазоны, нормализацию MIME-типа, snapshot state, декодирование текста, упаковку Data URL, URL store, лимиты и типы ошибок.

### 2.3. Базовые зависимости

| Крейт | Назначение |
|---|---|
| `boa_engine`, `boa_gc` 0.22 | JS bindings и GC |
| `thiserror` 2 | Внутренние типы ошибок |
| `bytes` 1 | Разделяемые неизменяемые диапазоны байтов |
| `encoding_rs` 0.8 | Реализация Encoding Standard |
| `base64` 0.22 | Data URL |
| `uuid` 1 | Непредсказуемая часть blob URL (`v4`) |
| `url` 2 | Валидация и разбор URL на Rust-границе |
| `futures-channel` 0.3 | Канал результата фонового чтения |
| `parking_lot` 0.12 | Короткие внутренние блокировки URL store/source registry |
| `mime` либо собственный валидатор | Проверка MIME без эвристического sniffing |
| `tracing` 0.1 | Опциональная наблюдаемость |

Версии фиксируются в `Cargo.lock`. Зависимости с `unsafe` внутри допустимы только после `cargo deny`, аудита назначения и документирования; собственный код остаётся safe Rust.

### 2.4. Cargo features

| Feature | По умолчанию | Назначение |
|---|---:|---|
| `fs` | да | Host-side импорт разрешённых локальных файлов |
| `dom-shim` | да | Минимальные `EventTarget`, `Event`, `ProgressEvent`, `DOMException` |
| `streams-shim` | да | Byte `ReadableStream` и поддержка `textStream()` в необходимом объёме |
| `url-shim` | да | Минимальный объект `URL` при отсутствии URL у хоста |
| `structured-clone` | да | Payload и bridge для сериализации `Blob`/`File`/`FileList` |
| `runtime-interop` | нет | Переиспользование совместимых объектов `boa_runtime` |
| `tracing` | нет | Span/event без содержимого файлов |

Любая поддерживаемая комбинация features должна собираться. Отключение shim требует передачи соответствующего host adapter; регистрация без обязательного adapter возвращает Rust-ошибку и не оставляет частично зарегистрированные globals.

---

## 3. Архитектура

### 3.1. Слои

```text
┌─────────────────────────────────────────────────────────────────┐
│ JavaScript: Blob / File / FileList / FileReader / URL methods   │
└──────────────────────────────┬──────────────────────────────────┘
                               │ Web IDL conversion, brand checks
┌──────────────────────────────▼──────────────────────────────────┐
│ boa_fapi: bindings, DOM/Streams adapters, Promise/task bridge   │
└──────────────────────────────┬──────────────────────────────────┘
                               │ Rust values only
┌──────────────────────────────▼──────────────────────────────────┐
│ boa_fapi_core: BlobData, FileData, ranges, readers, URL store   │
└──────────────────────────────┬──────────────────────────────────┘
                  ┌────────────┴─────────────┐
┌─────────────────▼────────────────┐ ┌───────▼────────────────────┐
│ MemorySource: Arc<[u8]>/chunks   │ │ FileSource: capability +  │
│ and zero-copy Blob ranges        │ │ snapshot validation       │
└──────────────────────────────────┘ └────────────────────────────┘
```

### 3.2. Обязательные архитектурные решения

**AD-1. Неизменяемость.** После создания `Blob` его логическая последовательность байтов и `type` не меняются. `File` также фиксирует `name`, `lastModified` и snapshot state.

**AD-2. Композиция без копирования.** Вложенный `Blob` и `slice()` разделяют byte sources через `Arc`; BufferSource копируется в момент конструктора, как требует Спека. Изменения исходного ArrayBuffer после `new Blob([buffer])` не видны.

**AD-3. Capability-based FS.** JS никогда не получает API открытия произвольного пути. Файловый `File` создаётся хостом из предварительно разрешённого ресурса или через policy-проверку.

**AD-4. Один JS-поток.** Объекты Boa живут только на потоке `Context`. IO выполняется отдельно и возвращает Rust DTO; материализация `ArrayBuffer`, строки и событий происходит в job Boa.

**AD-5. Явная среда.** Origin, storage partition, тип global, clock и task scheduler задаются `FileApiConfig`; значения не выводятся из process cwd, имени пользователя или глобальных переменных.

**AD-6. Атомарная регистрация.** До изменения `globalThis` проверяются конфликты имён и наличие adapters. При ошибке globals остаются без изменений.

**AD-7. Совместимость с другими Boa Web API.** DOMException/Event/Streams/URL переиспользуются только после capability/brand negotiation. Одновременная регистрация несовместимых shim запрещена понятной ошибкой.

### 3.3. Внутренняя модель byte source

```rust
pub trait ByteSource: Send + Sync + 'static {
    fn len(&self) -> u64;
    fn snapshot(&self) -> SnapshotState;
    fn read_range(
        &self,
        range: std::ops::Range<u64>,
        cancel: &CancellationToken,
    ) -> Result<Bytes, FileApiError>;
}

pub struct BlobData {
    pub(crate) segments: Arc<[BlobSegment]>,
    pub(crate) size: u64,
    pub(crate) media_type: Arc<str>,
    pub(crate) snapshot: SnapshotState,
}

pub struct BlobSegment {
    pub(crate) source: Arc<dyn ByteSource>,
    pub(crate) offset: u64,
    pub(crate) len: u64,
}
```

Требования:

* Суммирование длин выполняется через `checked_add`; превышение лимита или `u64::MAX` даёт синхронный `RangeError` на JS-границе.
* Пустой Blob не обязан выделять буфер.
* Количество сегментов ограничивается и периодически нормализуется, чтобы цепочки `slice()` не создавали квадратичную сложность.
* `read_range()` может вернуть меньше запрошенного только до EOF; неожиданный short read до snapshot EOF является `NotReadableError`.

### 3.4. GC и владение

* Native data классов Boa содержат `Arc<BlobData>`/идентификаторы, но не циклические Rust-ссылки на `JsObject`.
* Event handlers и pending promises трассируются через `boa_gc`.
* Незавершённая операция чтения удерживает Blob до доставки terminal event/result.
* Blob URL store удерживает сильную ссылку до revoke или уничтожения global.
* Drop JS-объекта `FileReader` не обязан синхронно блокировать IO; cancellation должна освободить ресурсы асинхронно.

---

## 4. Публичный Rust API

### 4.1. Регистрация

```rust
pub struct FileApiExtension {
    config: FileApiConfig,
}

impl FileApiExtension {
    pub fn builder() -> FileApiExtensionBuilder;
    pub fn register(&self, context: &mut boa_engine::Context) -> Result<(), RegisterError>;
    pub fn shutdown(&self, context: &mut boa_engine::Context) -> Result<(), ShutdownError>;
}

pub struct FileApiConfig {
    pub environment: EnvironmentDescriptor,
    pub limits: FileApiLimits,
    pub io_executor: Arc<dyn FileIoExecutor>,
    pub clock: Arc<dyn Clock>,
    pub entropy: Arc<dyn EntropySource>,
    pub file_policy: Arc<dyn FileAccessPolicy>,
    pub adapters: WebPlatformAdapters,
}
```

`register()` идемпотентен только для того же runtime/config identity. Повторная регистрация с другой конфигурацией возвращает `AlreadyRegistered`.

### 4.2. Создание объектов хостом

```rust
pub struct FileApiHandle { /* opaque */ }

impl FileApiHandle {
    pub fn blob_from_bytes(
        &self,
        bytes: impl Into<Bytes>,
        media_type: &str,
        context: &mut Context,
    ) -> JsResult<JsObject>;

    pub fn file_from_bytes(
        &self,
        bytes: impl Into<Bytes>,
        name: &str,
        options: HostFileOptions,
        context: &mut Context,
    ) -> JsResult<JsObject>;

    #[cfg(feature = "fs")]
    pub fn file_from_resource(
        &self,
        resource: &dyn FileResource,
        display_name: &str,
        options: HostFileOptions,
        context: &mut Context,
    ) -> JsResult<JsObject>;

    pub fn file_list(
        &self,
        files: impl IntoIterator<Item = JsObject>,
        context: &mut Context,
    ) -> JsResult<JsObject>;

    pub fn resolve_blob_url(&self, url: &str, requester: &EnvironmentKey)
        -> Result<ResolvedBlob, BlobUrlError>;
}
```

Host API обязан проверять бренд `File` для каждого элемента `file_list`. `display_name` очищается от пути: basename не вычисляется автоматически из секретного host path.

### 4.3. Политика файлового доступа

```rust
pub trait FileAccessPolicy: Send + Sync {
    fn authorize_open(&self, request: &FileOpenRequest)
        -> Result<FileGrant, FileApiError>;
    fn authorize_read(&self, grant: &FileGrant, snapshot: &SnapshotState)
        -> Result<(), FileApiError>;
}
```

`FileGrant` является непрозрачной capability. Реализация по умолчанию запрещает импорт путей. Опциональная root policy обязана:

* canonicalize уже открытый handle, а не доверять строковому префиксу;
* предотвращать выход через `..`, symlink/junction/reparse point;
* не раскрывать абсолютный путь в JS, исключениях, логах и blob URL;
* повторно проверять snapshot перед каждым новым чтением file-backed Blob.

### 4.4. Adapters

| Adapter | Обязанность |
|---|---|
| `DomAdapter` | Создание `DOMException`, Event/ProgressEvent и dispatch |
| `StreamAdapter` | Byte `ReadableStream`, cancellation, backpressure, text decoding |
| `UrlAdapter` | Добавление статических методов к `URL` и разбор URL |
| `TaskAdapter` | File Reading Task Source и enqueue в Boa |
| `CloneAdapter` | Регистрация serializable payload для Blob/File/FileList |

Каждый adapter имеет version/capability descriptor. Проверка совместимости выполняется до регистрации.

### 4.5. Интеграция с `boa-idb`

`boa-fapi` не зависит от `boa-idb`. Вместо этого feature `structured-clone` предоставляет:

```rust
pub enum FileApiClonePayload {
    Blob(SerializedBlob),
    File(SerializedFile),
    FileList(Vec<SerializedFile>),
}
```

Payload содержит байты или ссылки только на управляемое immutable blob storage, но не host path/capability. Для персистентного IndexedDB bridge обязан материализовать байты либо использовать согласованное content-addressed storage с атомарным lifetime protocol. Зарезервированные теги SCF `boa-idb` должны получать стабильное versioned encoding и тесты round-trip/cross-version.

---

## 5. Поверхность JavaScript и требования Web IDL

### 5.1. Общие требования

Для всех интерфейсов обязательны:

* корректные `constructor`, `prototype`, `name`, `length`, property descriptors и `Symbol.toStringTag`;
* non-forgeable internal brand; запрещено определять тип по `constructor.name` или duck typing;
* illegal invocation на неверном `this` даёт `TypeError`;
* порядок Web IDL-конвертаций и проверок совпадает со Спекой;
* наследование `File.prototype -> Blob.prototype`, `FileReader.prototype -> EventTarget.prototype`;
* Web IDL `DOMString`, `USVString`, `[Clamp] long long`, `unsigned long long` реализуются без потери суррогатов там, где требуется `DOMString`;
* обещания создаются в relevant realm и всегда settle через очередь Boa, а не синхронно внутри метода;
* `Blob`, `File`, `FileList` помечаются как serializable через bridge.

### 5.2. `Blob`

```webidl
[Exposed=(Window,Worker), Serializable]
interface Blob {
  constructor(optional sequence<BlobPart> blobParts,
              optional BlobPropertyBag options = {});
  readonly attribute unsigned long long size;
  readonly attribute DOMString type;
  Blob slice(optional [Clamp] long long start,
             optional [Clamp] long long end,
             optional DOMString contentType);
  [NewObject] ReadableStream stream();
  [NewObject] Promise<USVString> text();
  [NewObject] Promise<ArrayBuffer> arrayBuffer();
  [NewObject] ReadableStream textStream();
  [NewObject] Promise<Uint8Array> bytes();
};
```

`BlobPart` принимает BufferSource, Blob или USVString. Для каждого BufferSource копируется ровно видимый диапазон байтов; detached buffer обрабатывается по Web IDL/ECMAScript правилам. MIME нормализуется в ASCII lowercase; наличие символа вне U+0020..U+007E обнуляет type.

### 5.3. `File`

```webidl
[Exposed=(Window,Worker), Serializable]
interface File : Blob {
  constructor(sequence<BlobPart> fileBits,
              USVString fileName,
              optional FilePropertyBag options = {});
  readonly attribute DOMString name;
  readonly attribute long long lastModified;
};
```

`fileName` преобразуется в USVString. Символ `/` заменяется на `:`. Если `lastModified` отсутствует, используется `Clock::now_unix_millis()`; clock инъецируется для детерминированных тестов. Значение `type` обрабатывается как у Blob.

Для host-backed File `name` содержит только предоставленное отображаемое имя. MIME определяется только policy/host metadata либо расширением по явно включённой таблице; эвристическое определение содержимого и добавление charset запрещены.

### 5.4. `FileList`

* Не имеет публичного JS-конструктора.
* `length` readonly.
* `item(index)` и доступ `list[index]` возвращают тот же объект `File`; вне диапазона — `null` для `item()` и `undefined` для indexed property access согласно Web IDL binding semantics.
* Indexed properties enumerable, собственные, readonly и не допускают подмены элемента.
* Порядок файлов сохраняется при clone round-trip.

### 5.5. `FileReader`

Обязательная поверхность:

* методы `readAsArrayBuffer`, `readAsBinaryString`, `readAsText`, `readAsDataURL`, `abort`;
* константы `EMPTY = 0`, `LOADING = 1`, `DONE = 2` на constructor и prototype с корректными descriptors;
* readonly `readyState`, `result`, `error`;
* `onloadstart`, `onprogress`, `onabort`, `onerror`, `onload`, `onloadend`;
* наследование от `EventTarget`.

`result` принимает только `null`, DOMString либо ArrayBuffer. `error` — `null` либо `DOMException`.

### 5.6. `FileReaderSync`

Содержит синхронные `readAsArrayBuffer`, `readAsBinaryString`, `readAsText`, `readAsDataURL`. Конструктор выставляется только для `DedicatedWorker` и `SharedWorker` environment descriptors. В `Window` глобальное имя отсутствует. В service-worker режиме подсистема его не регистрирует.

Хост не должен ложно помечать главный JS-поток как worker ради доступа к sync API. Максимальный размер синхронного чтения задаётся отдельным меньшим лимитом.

### 5.7. `URL` partial interface

* `URL.createObjectURL(blob)` принимает только бренд Blob/File версии 1.0.
* `URL.revokeObjectURL(url)` молча возвращает `undefined` для не-blob, неизвестного, уже отозванного или неавторизованного URL.
* Если URL хоста отсутствует и включён `url-shim`, регистрируется минимальный `URL` с данными статическими методами; он явно не заявляется полной реализацией WHATWG URL.

---

## 6. Модель данных Blob, File и FileList

### 6.1. Обработка Blob parts

Алгоритм выполняется слева направо:

1. USVString: при `endings = "native"` нормализовать CR/LF/CRLF в native newline, затем UTF-8 encode.
2. BufferSource: получить копию видимых байтов в момент конструктора.
3. Blob/File: добавить сегменты его byte sequence; его `type` игнорируется.
4. Иные значения проходят точную Web IDL union conversion; ошибка является синхронным `TypeError`.

`endings = "transparent"` не меняет переводы строк. На Windows native newline — CRLF, на остальных поддерживаемых платформах — LF; тесты не должны зависеть от host git autocrlf.

### 6.2. `slice()`

Нормализация границ следует §2 Спеки:

* отсутствующий start = 0, отсутствующий end = size;
* отрицательная граница отсчитывается от конца и ограничивается нулём;
* положительная граница ограничивается size;
* span = `max(relativeEnd - relativeStart, 0)`;
* contentType нормализуется как MIME type Blob;
* результат является новым Blob и делит неизменяемые sources с исходным.

Сложность `slice()` — O(k), где k — число пересечённых сегментов, с fast path O(1) для одного сегмента. Копирование содержимого запрещено.

### 6.3. `File` snapshot state

Для memory File snapshot всегда валиден. Для host-backed File минимум snapshot включает:

* стабильный идентификатор ресурса/handle identity;
* размер;
* время изменения с максимальной доступной точностью;
* платформенный file identity, если доступен.

Проверка выполняется перед первым chunk и на границах chunk. Изменение snapshot, неожиданное исчезновение, lock/permission failure или short read преобразуются в `NotReadableError`. Режим `copy_on_import` материализует безопасный неизменяемый снимок и является рекомендуемым для недоверенного JS.

### 6.4. Текст

`Blob.text()` всегда декодирует UTF-8 с replacement semantics. `readAsText(blob, label)`:

1. получает encoding по переданному label;
2. при нераспознанном/отсутствующем label использует UTF-8;
3. BOM может переопределить encoding по Encoding Standard;
4. ошибки последовательностей заменяются U+FFFD, а не превращаются в Rust/JS exception.

Результат преобразуется в JS USVString/DOMString согласно сигнатуре конкретного метода.

### 6.5. Binary string и Data URL

* `readAsBinaryString`: каждый байт `0x00..0xFF` становится code unit U+0000..U+00FF; метод сохраняется, несмотря на legacy-статус.
* `readAsDataURL`: результат `data:<media-type>;base64,<payload>`; при пустом type media type опускается (`data:;base64,<payload>`).
* Base64 не содержит пробелов или переносов строки.
* Упаковка проверяет итоговый лимит до аллокации.

### 6.6. `arrayBuffer()`, `bytes()`, `stream()`, `textStream()`

* `arrayBuffer()` возвращает новый недетачированный ArrayBuffer.
* `bytes()` возвращает новый Uint8Array поверх нового ArrayBuffer.
* Повторные вызовы не разделяют изменяемую JS-память.
* `stream()` возвращает новый readable byte stream при каждом вызове.
* Chunk является Uint8Array; EOF закрывает stream; ошибка source переводит stream в errored.
* Cancel stream отменяет только данный reader, но не разрушает Blob или другие readers.
* Backpressure соблюдается: следующий IO chunk не читается, пока stream не запросил данные.
* `textStream()` создаёт независимый byte stream и UTF-8 decoder stream с replacement semantics; границы многобайтных символов между chunks сохраняются.

### 6.7. Сериализация

Сериализуются snapshot state и byte sequence; для File дополнительно `name` и `lastModified`, для FileList — упорядоченные sub-serializations файлов.

Запрещено сериализовать:

* абсолютные пути;
* OS handles/descriptors;
* policy tokens, credentials или environment keys;
* ссылки на mutable host storage без переносимого lifetime contract.

Сериализация в другой thread/realm должна давать отдельный JS wrapper над той же immutable logical data либо над эквивалентной копией.

---

## 7. Чтение данных и интеграция с event loop Boa

### 7.1. File Reading Task Source

Runtime содержит отдельную FIFO-очередь задач `FileReading`. Задачи одного `FileReader` сохраняют порядок. IO completion не вызывает JS напрямую: оно только планирует job в relevant realm.

Хостовый `JobExecutor` обязан выполнять promise jobs и File Reading tasks до quiescence либо предоставить явный `run_jobs()` contract. Документация должна показывать этот цикл.

### 7.2. State machine `FileReader`

Начальное состояние:

```text
readyState = EMPTY
result = null
error = null
```

Старт чтения:

1. Если state = LOADING, синхронно бросить `InvalidStateError` без изменения текущей операции.
2. Установить LOADING, `result = null`, `error = null`.
3. Создать operation id/cancellation token и stream reader.
4. После первого успешного завершения stream read (включая немедленный EOF пустого Blob) поставить `loadstart`.
5. С ограничением частоты ставить `progress`.

Успешное окончание:

1. Установить DONE.
2. Упаковать результат.
3. Поставить `load`, затем `loadend`, если обработчик `load` не начал новую операцию чтения.

Ошибка:

1. Установить DONE, `result = null`, `error = DOMException`.
2. Поставить `error`, затем `loadend`, если обработчик `error` не начал новую операцию чтения.

### 7.3. `abort()` и защита от stale completion

Если state = EMPTY или DONE, `abort()` устанавливает `result = null` и возвращает `undefined` без событий. Если state = LOADING:

1. увеличить generation/operation id;
2. отменить reader;
3. установить DONE, `result = null`, `error = null`;
4. поставить `abort`, затем `loadend`, если обработчик `abort` не начал новую операцию чтения.

Любой IO completion со старым operation id игнорируется. Он не может изменить `result/error/state` или отправить события новой операции. После terminal event (`abort`, `load`, `error`) события `progress` для завершённой generation запрещены; для каждой generation допускается не более одного terminal event. Успешное полное чтение обязано поставить итоговый `progress` до `load`.

### 7.4. ProgressEvent

Все события чтения — `ProgressEvent` с `bubbles = false`, `cancelable = false`. Для известного размера `lengthComputable = true`, `total = blob.size`, `loaded` монотонно возрастает и не превышает total. На terminal event успешного полного чтения `loaded = total`.

События `progress` не должны отправляться чаще одного раза в 50 мс либо одного раза на chunk, если chunk приходит реже. `loadstart`, terminal event и `loadend` не подавляются throttling.

### 7.5. Promise-based reads

`Blob.text()`, `arrayBuffer()` и `bytes()`:

* возвращают Promise до начала IO;
* resolve/reject только через Boa job queue;
* отклоняются подходящим `DOMException` при read failure;
* не создают `FileReader` и не отправляют ProgressEvent;
* продолжают удерживать source до settlement.

### 7.6. Синхронное чтение

`FileReaderSync` использует тот же core packaging, но выполняет чтение на текущем worker-потоке. Оно обязано:

* проверять `max_sync_read_bytes` до materialization;
* не pump-ить event loop и не отправлять события;
* бросать `SecurityError`/`NotReadableError` синхронно;
* не создавать Promise.

---

## 8. Blob URL

### 8.1. Store

Store логически является map:

```text
valid blob URL string -> { Arc<BlobData>, EnvironmentDescriptor, created_at }
```

Операции add/get/remove — thread-safe и амортизированно O(1). UUID генерируется CSPRNG; счётчик, timestamp и предсказуемый PRNG запрещены.

### 8.2. Формат и origin

URL сериализуется как `blob:<serialized-origin>/<uuid>`. Для opaque origin используется непереиспользуемый opaque serialization согласно host environment contract. Environment key и storage partition не включаются открытым текстом.

### 8.3. Доступ

`resolve_blob_url` проверяет same-partition usage. Несовпадение среды выглядит для вызывающего как отсутствие записи/network error и не раскрывает существование URL.

После revoke:

* новые resolve завершаются network-error equivalent;
* уже начатое чтение, получившее `Arc<BlobData>`, успешно продолжается;
* повторный revoke не является ошибкой.

### 8.4. Lifetime

Все URL, созданные global, автоматически удаляются при `shutdown`, уничтожении runtime или global. Хост обязан вызвать shutdown либо предоставить lifecycle hook. Тесты проверяют отсутствие сильных ссылок после очистки.

### 8.5. Интеграция с Fetch/ресурсами

Крейт предоставляет resolver, возвращающий status, media type, length и body stream. Он не регистрирует сетевой handler самовольно. Интеграция с Fetch выполняется хостом и обязана поддерживать как минимум GET и диапазоны, если это требует потребитель; нормативные ограничения доступа проверяются до выдачи тела.

---

## 9. Ошибки и исключения

### 9.1. Внутренний тип

```rust
pub enum FileApiError {
    NotFound,
    UnsafeFile,
    TooManyReads,
    SnapshotChanged,
    FileLocked,
    PermissionDenied,
    ResourceLimit(ResourceLimitKind),
    Cancelled,
    Io(std::io::ErrorKind),
    Internal,
}
```

Внутренние пути, OS error strings и содержимое данных не входят в JS message.

### 9.2. Отображение

| Условие | JS-результат |
|---|---|
| Повторный read при LOADING | синхронный `InvalidStateError` |
| Ресурс исчез до обработки чтения | `NotFoundError` |
| Unsafe file / too many reads / policy deny | `SecurityError` |
| Snapshot changed / lock / поздняя permission error / short read | `NotReadableError` |
| Неверный бренд/тип аргумента/enum | `TypeError` |
| Переполнение или превышение JS/host materialization limit | `RangeError` либо документированный `QuotaExceededError`; выбор фиксируется до M1 |
| `FileReader.abort()` | события `abort`, `loadend`; `error = null` |
| Ошибка blob URL dereference | network-error equivalent через resolver |
| Внутренняя непредвиденная ошибка | `UnknownError`/Rust `Internal`, без panic |

Асинхронный FileReader устанавливает `error`, затем отправляет события. FileReaderSync бросает DOMException. Promise-based методы reject тем же типом DOMException.

---

## 10. Безопасность, приватность и лимиты

### 10.1. Базовые требования

* По умолчанию JS может читать только байты, явно переданные в `Blob`/`File` или предоставленные capability хоста.
* Строка в `new Blob(["C:\\secret.txt"])` является содержимым, не путём.
* Никакое API не выполняет directory enumeration.
* System-sensitive файлы блокируются policy до создания JS File либо при каждом чтении.
* Blob URL изолированы минимум по storage partition и origin; угадывание UUID не обходит авторизацию.
* MIME не sniff-ится по содержимому.
* Логи не содержат body, file name по умолчанию, path, URL UUID целиком или decoded text.

### 10.2. Лимиты по умолчанию

| Лимит | Значение |
|---|---:|
| `max_blob_size` | 2 GiB |
| `max_materialize_bytes` | 256 MiB |
| `max_sync_read_bytes` | 32 MiB |
| `max_parts` | 1 000 000 |
| `max_segments_after_normalize` | 65 536 |
| `max_concurrent_reads_per_global` | 64 |
| `max_blob_urls_per_global` | 10 000 |
| `default_chunk_size` | 64 KiB |
| `max_data_url_output` | 256 MiB |

Все лимиты конфигурируемы хостом, но нулевые/небезопасные значения валидируются. Отказ по лимиту происходит до крупной аллокации. Счётчики конкурентности освобождаются при success, error, abort и drop runtime.

### 10.3. TOCTOU и внешние файлы

Предпочтителен открытый read-only handle с identity, а не повторное открытие по path. При невозможности гарантировать snapshot host обязан выбрать `copy_on_import` или отказать. Изменившийся файл никогда не выдаётся частично как успешный старый snapshot.

### 10.4. Отказоустойчивость

* Ошибка IO-пула или panic стороннего executor перехватывается на границе и превращается в terminal error.
* Отравленная блокировка не должна завершать процесс.
* Shutdown отменяет операции и гарантированно не вызывает JS после уничтожения Context.

---

## 11. Нефункциональные требования

### 11.1. Производительность

| Операция | Требование |
|---|---|
| `new Blob()` без частей | O(1), без heap buffer для данных |
| Добавление Blob part | Без копирования его bytes |
| `slice()` одного сегмента | O(1), без копирования bytes |
| Blob URL add/revoke/lookup | Амортизированно O(1) |
| Stream read | Память O(chunk size) поверх удерживаемого source |
| Полное чтение | Не более 1.5× итогового payload сверх source, исключая неизбежную JS-копию |

Benchmark baseline фиксирует CPU, ОС, Rust/Boa version, commit, features и размеры данных. Регрессия более 10% по медиане либо более 15% по p95 относительно утверждённого baseline блокирует релиз без объяснения.

### 11.2. Конкурентность

* Несколько readers одного Blob независимы.
* Один FileReader допускает ровно одну активную операцию.
* URL store безопасен для нескольких Context, но каждый entry сохраняет environment ownership.
* Порядок events одного FileReader детерминирован; порядок разных readers не специфицируется.

### 11.3. Наблюдаемость

Feature `tracing` публикует operation kind, размер, длительность, chunk count, result class и opaque environment hash. Содержимое, полные имена файлов, paths и blob URLs не логируются. Метрики не должны менять event ordering.

### 11.4. Качество

* Покрытие строк: не менее 85% для `boa_fapi_core`, 80% для `boa_fapi`.
* Mutation/property tests обязательны для slice boundaries, newline conversion, MIME normalization, encoding labels и event state machine.
* Fuzz targets не должны panic/OOM при ограниченном input.

---

## 12. Тестирование и WPT

### 12.1. Уровни тестирования

1. Unit: core algorithms и Web IDL converters.
2. Property: произвольные segment layouts/slices, UTF-8 chunk boundaries, abort races.
3. Integration: JS в реальном Boa Context с job draining.
4. Filesystem: snapshot changes, delete, rename, truncate, permission/lock failures, symlink policy.
5. Conformance: адаптированный upstream `FileAPI/**` WPT.
6. Differential: выбранные сценарии против Chromium/Firefox либо Node для общей Blob-поверхности, без объявления browser quirks нормативными.
7. Leak/race: URL lifetime, cancellation, shutdown и повторные Context.

### 12.2. Обязательные группы WPT

Раннер должен классифицировать и запускать применимые тесты:

```text
FileAPI/blob/**
FileAPI/file/**
FileAPI/filelist-section/**
FileAPI/reading-data-section/**
FileAPI/FileReader/**
FileAPI/BlobURL/**
```

Тесты, требующие HTML input UI, navigation, Fetch, MediaSource, real Window/Worker orchestration или сетевого WPT server, помечаются `not-applicable` только с машиночитаемой причиной и ссылкой на отсутствующую capability.

### 12.3. WPT harness

`boa_fapi_wpt` обязан:

* фиксировать upstream commit SHA и SHA-256 выбранных файлов;
* поддерживать `.any.js` и JS-only части HTML-тестов;
* предоставлять testharness assertions, async_test/promise_test, таймаут и event-loop pump;
* выводить PASS/FAIL/TIMEOUT/NOTRUN и JUnit/JSON отчёт;
* по умолчанию запускаться детерминированно в один поток, `--threads N` — явный opt-in;
* иметь `expectations.json` с точным test/subtest, причиной, owner и сроком пересмотра;
* считать новый неожиданный FAIL/TIMEOUT/NOTRUN ошибкой CI.

Запрещено скрывать дефекты расширением wildcard expectations, переводом FAIL в NOTRUN или исключением файла без documented capability gap.

### 12.4. Обязательные race/error тесты

* `abort()` до первого chunk, между progress events и одновременно с EOF/error.
* Старый completion после запуска новой операции на том же FileReader.
* Revoke до resolve, после resolve и во время чтения.
* Уничтожение Context с pending Promise/FileReader/stream.
* Изменение/truncate/replace файла после создания File.
* 65 concurrent reads и восстановление quota после каждого terminal path.
* `slice()` на `i64::MIN`, `i64::MAX`, пустом Blob и перевёрнутом диапазоне.
* Multi-byte UTF-8 code point/BOM, разделённые на каждом возможном chunk boundary.

### 12.5. CI matrix

| Job | Требование |
|---|---|
| fmt | `cargo fmt --all -- --check` |
| clippy | workspace/all-targets/all-features, `-D warnings` |
| test | все платформы, default features |
| minimal | `--no-default-features` с mock adapters |
| powerset | `cargo hack --feature-powerset --depth 2` |
| docs | `RUSTDOCFLAGS=-Dwarnings cargo doc --workspace --no-deps` |
| deny | advisories, bans, sources, licenses |
| WPT | строгий manifest/expectations gate |
| coverage | установленные пороги |
| miri/fuzz | nightly scheduled, bounded time |

---

## 13. Документация, сборка и поставка

Поставка включает:

* rustdoc всех публичных типов и методов;
* `README.md` с быстрым стартом и явным job-executor loop;
* `docs/architecture.md`, `docs/security.md`, `docs/host-integration.md`;
* `docs/spec-matrix.md`: каждый Web IDL member/algorithm → код → тест → статус;
* `docs/spec-delta.md`: отличия зафиксированного WD от latest draft;
* `docs/wpt.md`: upstream SHA, адаптация, команды и правила expectations;
* примеры memory Blob, host File, FileReader, stream, blob URL и shutdown;
* `CHANGELOG.md`, `LICENSE-*`, `deny.toml`, SBOM в CI artifact.

SemVer: изменение JS-visible поведения или host API требует записи в changelog; обновление нормативной версии Спеки не выполняется как patch без анализа совместимости.

---

## 14. Этапы работ

| Этап | Результат | Оценка |
|---|---|---:|
| M1. Workspace и core | toolchain, limits/errors, ByteSource, MIME/newlines, Blob composition/slice | 2 недели |
| M2. Boa bindings | Blob/File/FileList, Web IDL, brands, GC, host constructors | 2 недели |
| M3. Promise и Streams | text/arrayBuffer/bytes/stream/textStream, adapters | 2 недели |
| M4. FileReader | DOM shim, state machine, events, abort/races, sync worker API | 2–3 недели |
| M5. Files и безопасность | capability FS, snapshot validation, limits, shutdown | 2 недели |
| M6. Blob URL/clone | environment isolation, resolver, URL methods, clone bridge/IDB interop | 2 недели |
| M7. WPT и hardening | harness, WPT matrix, fuzz/bench/leak tests, docs | 2–3 недели |

После каждого этапа обязательны fmt, clippy, tests, rustdoc и обновление spec matrix. M2 не принимается без JS integration tests; M4 — без детерминированных abort race tests; M7 — без полного отчёта WPT.

---

## 15. Критерии приёмки

Работа принята только при выполнении всех условий:

1. Реализована вся JS-поверхность раздела 5 с корректными brands, prototypes и descriptors.
2. Алгоритмы Blob parts, MIME, newline, slice, packaging и text decoding соответствуют зафиксированной Спеке.
3. FileReader проходит state/event/abort tests, включая stale completion races.
4. FileReaderSync отсутствует в Window и работает в worker environment.
5. Host-backed File не раскрывает путь и обнаруживает изменение snapshot.
6. Blob URL изолированы по environment, переживают revoke для уже начатого чтения и очищаются при shutdown.
7. Promise и events доставляются только через Boa jobs/task source.
8. Все поддерживаемые WPT проходят; exclusions исчерпывающе обоснованы capability gaps.
9. Нет panic/unsafe в собственном production-коде; clippy/rustdoc без warnings.
10. Пороги coverage и benchmarks выполнены на зафиксированном baseline.
11. Документация позволяет встроить крейт без чтения его исходников.
12. Все команды ниже завершаются с exit code 0:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test appendix_a_acceptance -- --nocapture
cargo test --package boa_fapi --test abort_races -- --nocapture
cargo run --package boa_fapi_wpt -- --manifest wpt-manifest.json --strict
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo llvm-cov --workspace --all-features --fail-under-lines 80
cargo deny check
cargo hack check --feature-powerset --depth 2
git diff --check
```

---

## 16. Риски и открытые вопросы

| Риск/вопрос | Решение до начала этапа |
|---|---|
| Спека имеет статус Working Draft | Зафиксировать WD 23.08.2026; вести `spec-delta.md` |
| В Спеке не закреплён точный chunk size | 64 KiB default, конфиг 16 KiB..1 MiB; JS не должен зависеть от границы |
| Обсуждается синхронность `loadstart` | Для v1.0 ставить task после первого stream read completion, включая EOF, как в зафиксированном WD |
| Data URL алгоритм в Спеке отмечен как недостаточно детальный | Закрепить формат раздела 6.5 и WPT; не добавлять charset |
| У Boa/хоста может не быть Streams/DOM/URL | Capability adapters + shim; регистрация fail-fast |
| `FileReaderSync` может блокировать главный поток embedder | Только worker descriptor + отдельный строгий лимит |
| Snapshot по mtime/size может пропустить замену | Использовать handle/file identity; иначе copy-on-import |
| Большие Blob и JS ArrayBuffer вызывают OOM | Checked limits до аллокации, streaming/backpressure, fallible allocation где доступно |
| Интеграция с `boa-idb` может создать дублирование байтов | Versioned clone bridge; оптимизация только после корректного materialized baseline |
| Минимальный URL shim ошибочно примут за полный URL | Явное имя capability и документация; использовать URL host при наличии |
| WPT содержит браузерные зависимости | Машиночитаемая capability classification; не считать их File API pass |

До M1 владелец продукта должен утвердить только один семантический вопрос: отображать превышение host materialization limit в `RangeError` или `QuotaExceededError`. Рекомендуется `QuotaExceededError` для host quota и `RangeError` только для арифметического/представимого диапазона.

---

## 17. Приложения

### Приложение A. Приёмочный JavaScript-сценарий

```javascript
const source = new Uint8Array([0x41, 0x0d, 0x0a, 0xd0, 0x91]);
const blob = new Blob([source, "\nC"], {
  type: "TEXT/PLAIN",
  endings: "transparent",
});

console.assert(blob.size === 7);
console.assert(blob.type === "text/plain");
source[0] = 0x5a; // Blob обязан сохранить исходную копию.

const part = blob.slice(0, 1, "TEXT/CUSTOM");
console.assert(part.size === 1);
console.assert(part.type === "text/custom");

const bytes = await part.bytes();
console.assert(bytes instanceof Uint8Array);
console.assert(bytes[0] === 0x41);
console.assert((await part.text()) === "A");

const file = new File([blob], "dir/name.txt", {
  type: blob.type,
  lastModified: 1234,
});
console.assert(file instanceof Blob);
console.assert(file.name === "dir:name.txt");
console.assert(file.lastModified === 1234);

const reader = new FileReader();
const events = [];
for (const type of ["loadstart", "progress", "load", "loadend"]) {
  reader.addEventListener(type, e => {
    events.push(type);
    console.assert(e.target === reader);
  });
}

const readDone = new Promise((resolve, reject) => {
  reader.onloadend = () => reader.error ? reject(reader.error) : resolve();
});
reader.readAsArrayBuffer(file);
await readDone;
console.assert(reader.readyState === FileReader.DONE);
console.assert(reader.result instanceof ArrayBuffer);
console.assert(events[0] === "loadstart");
console.assert(events.at(-1) === "loadend");

const url = URL.createObjectURL(file);
console.assert(url.startsWith("blob:"));
const resolved = await __hostTestResolveBlob(url);
console.assert(resolved.byteLength === file.size);
URL.revokeObjectURL(url);
await promise_rejects_dom("NetworkError", __hostTestResolveBlob(url));
```

`__hostTestResolveBlob` существует только в integration-test harness и не входит в публичный JS API.

### Приложение B. Минимальное дерево исходников

```text
crates/boa_fapi_core/src/
├── blob.rs
├── file.rs
├── file_list.rs
├── source/{memory.rs,composite.rs}.rs
├── slice.rs
├── mime.rs
├── endings.rs
├── encoding.rs
├── data_url.rs
├── blob_url.rs
├── snapshot.rs
├── limits.rs
└── error.rs

crates/boa_fapi/src/
├── extension.rs
├── runtime.rs
├── config.rs
├── api/{blob.rs,file.rs,file_list.rs,file_reader.rs,file_reader_sync.rs}.rs
├── convert/{webidl.rs,blob_part.rs}.rs
├── dom/{event.rs,event_target.rs,progress_event.rs,exception.rs}.rs
├── streams/{adapter.rs,shim.rs}.rs
├── url/{adapter.rs,shim.rs}.rs
├── clone.rs
├── task.rs
└── driver.rs
```

### Приложение C. Матрица требований

| ID | Требование | Минимальное доказательство |
|---|---|---|
| R-BLOB-01 | BufferSource копируется при конструировании | unit + JS mutation test |
| R-BLOB-02 | Blob part разделяется без копии | allocation/identity test |
| R-BLOB-03 | Slice boundaries соответствуют §2 | property + WPT |
| R-BLOB-04 | MIME ASCII/lowercase rules | table/property + WPT |
| R-READ-01 | Promise reads асинхронны | JS job-order test |
| R-READ-02 | Streams соблюдают backpressure/cancel | integration test |
| R-FR-01 | Только одна активная операция FileReader | WPT + JS test |
| R-FR-02 | Event order и state корректны | model/state-machine test |
| R-FR-03 | Abort защищён от stale completion | deterministic race test |
| R-FILE-01 | Имя не раскрывает path | security test |
| R-FILE-02 | Snapshot change даёт NotReadableError | FS integration test |
| R-URL-01 | Same-partition enforcement | multi-environment test |
| R-URL-02 | Revoke semantics и lifetime | race + leak test |
| R-CLONE-01 | Blob/File/FileList round-trip | clone + IDB bridge test |
| R-WIDL-01 | Brands/prototypes/descriptors | idlharness/WPT |
| R-SEC-01 | Нет произвольного path access из JS | negative security test |
| R-WPT-01 | Нет неожиданных результатов | strict WPT report |

### Приложение D. Definition of Done для каждого JS member

Member считается реализованным только если одновременно существуют:

1. binding с точной Web IDL-конверсией;
2. core algorithm либо обоснованный adapter call;
3. unit или integration test нормального пути;
4. тест исключения/границы;
5. запись в `docs/spec-matrix.md`;
6. применимый WPT status без необоснованного expectation.

### Приложение E. Нормативная Web IDL поверхность

Ниже зафиксирована поверхность WD от 23.08.2026. В union `URL.createObjectURL` ветка `MediaSource` сохраняется как нормативная ссылка, но в версии 1.0 продукта не активна и не принимается binding-слоем.

```webidl
[Exposed=(Window,Worker), Serializable]
interface Blob {
  constructor(optional sequence<BlobPart> blobParts,
              optional BlobPropertyBag options = {});
  readonly attribute unsigned long long size;
  readonly attribute DOMString type;
  Blob slice(optional [Clamp] long long start,
             optional [Clamp] long long end,
             optional DOMString contentType);
  [NewObject] ReadableStream stream();
  [NewObject] Promise<USVString> text();
  [NewObject] Promise<ArrayBuffer> arrayBuffer();
  [NewObject] ReadableStream textStream();
  [NewObject] Promise<Uint8Array> bytes();
};

enum EndingType { "transparent", "native" };

dictionary BlobPropertyBag {
  DOMString type = "";
  EndingType endings = "transparent";
};

typedef (BufferSource or Blob or USVString) BlobPart;

[Exposed=(Window,Worker), Serializable]
interface File : Blob {
  constructor(sequence<BlobPart> fileBits,
              USVString fileName,
              optional FilePropertyBag options = {});
  readonly attribute DOMString name;
  readonly attribute long long lastModified;
};

dictionary FilePropertyBag : BlobPropertyBag {
  long long lastModified;
};

[Exposed=(Window,Worker), Serializable]
interface FileList {
  getter File? item(unsigned long index);
  readonly attribute unsigned long length;
};

[Exposed=(Window,Worker)]
interface FileReader : EventTarget {
  constructor();
  undefined readAsArrayBuffer(Blob blob);
  undefined readAsBinaryString(Blob blob);
  undefined readAsText(Blob blob, optional DOMString encoding);
  undefined readAsDataURL(Blob blob);
  undefined abort();

  const unsigned short EMPTY = 0;
  const unsigned short LOADING = 1;
  const unsigned short DONE = 2;
  readonly attribute unsigned short readyState;
  readonly attribute (DOMString or ArrayBuffer)? result;
  readonly attribute DOMException? error;

  attribute EventHandler onloadstart;
  attribute EventHandler onprogress;
  attribute EventHandler onabort;
  attribute EventHandler onerror;
  attribute EventHandler onload;
  attribute EventHandler onloadend;
};

[Exposed=(DedicatedWorker,SharedWorker)]
interface FileReaderSync {
  constructor();
  ArrayBuffer readAsArrayBuffer(Blob blob);
  DOMString readAsBinaryString(Blob blob);
  DOMString readAsText(Blob blob, optional DOMString encoding);
  DOMString readAsDataURL(Blob blob);
};

[Exposed=(Window,DedicatedWorker,SharedWorker)]
partial interface URL {
  static DOMString createObjectURL((Blob or MediaSource) obj);
  static undefined revokeObjectURL(DOMString url);
};
```
