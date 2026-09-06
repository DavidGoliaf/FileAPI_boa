# Architecture Decision Records

Обязательные решения зафиксированы на этапах M1–M2. Каждая запись: контекст,
решение, последствия.

## ADR-0001 (M1): `bytes` как единственный тип разделяемых байтов

Контекст: core нужен тип неизменяемых байтов с дешёвым `slice` и разделяемым
владением.

Решение: `bytes = "1"` — широко поддерживаемый, перmissive-лицензированный
(MIT), без unsafe в нашем коде; `Bytes::slice` даёт zero-copy диапазоны.

Последствия: MemorySource возвращает `Bytes::slice`; композиция Blob/File в
M2 разделяет allocation через `Arc<dyn ByteSource>` без копий.

## ADR-0002 (M1): `thiserror` для библиотечных ошибок

Контекст: нужен `std::error::Error` без ручного бойлерплейта.

Решение: `thiserror = "2"` в core; `anyhow` не используется (правило
workspace).

Последствия: `FileApiError`/`RegisterError` — типизированные, машиночитаемые
ошибки; `boa_fapi` отображает их в JS `TypeError`/`RangeError` (M2).

## ADR-0003 (M2): зависимости `boa_engine` и `boa_gc` 0.22.x

Контекст: M2 требует JS-поверхность Blob/File/FileList в реальном
`boa_engine::Context`.

Решение: `boa_engine = "0.22"`, `boa_gc = "0.22"` — только в `boa_fapi`;
минимальные features (default); версии зафиксированы в `Cargo.lock`. Оба
крейта поддерживаются boa-dev (активный проект), лицензии MIT (Unicode-3.0
для ICU-данных), MSRV 1.91 совпадает с нашим.

Последствия: `boa_fapi_core` остаётся engine-independent (guard-тесты
фиксируют изоляцию); дерево зависимостей расширяется (ICU и пр.), что
потребовало добавления лицензии `Zlib` в `deny.toml` allow-list.
Workspace-внутренняя path-зависимость `boa_fapi_core` получила точный
`version = "0.1.0"` рядом с `path`, поэтому `[bans] wildcards = "deny"`
остаётся строгим, а `allow-wildcard-paths = true` служит лишь backstop
для path-only dev-рёбер.

## ADR-0004 (M2): brands — нативные типы данных, а не JS-свойства

Контекст: заказ запрещает определять бренд по `constructor.name`,
`instanceof` или изменяемым публичным свойствам.

Решение: бренд — это тип нативных данных объекта (`BlobNative`, `FileNative`,
`FileListNative`), проверяемый через `JsObject::downcast_ref`. `File`
содержит собственный payload, поэтому `require_blob` принимает и `File`
(наследование брендов). JS не может ни подделать, ни скопировать нативные
данные.

Последствия: ворванные/поддельные объекты дают синхронный `TypeError`;
`slice` File возвращает объект с `BlobNative` (то есть Blob, не File).

## ADR-0005 (M2): хранение нативных данных вне GC-графа

Контекст: `boa_gc` не имеет `Trace` для `std::sync::Arc`, а вручную писать
`unsafe impl Trace` запрещено.

Решение: поля с `Arc<BlobData>`/`String` помечены `#[unsafe_ignore_trace]` —
санкционированный механизм derive `boa_gc` для данных без GC-указателей.
`BlobData` не содержит `Gc`/`JsObject`, поэтому игнорирование трассировки
фактно корректно; циклы невозможны.

Последствия: GC-safe нативные данные без `unsafe` в нашем коде; нативные DTO
не ссылаются на `JsObject`/`Context` (требование заказа).

## ADR-0006 (M2): регистрация — правило (b) «каждый второй вызов отклоняется»

Контекст: заказ предлагает выбор правила повторной регистрации.

Решение: правило (b): любой повторный `register` на том же `Context`
возвращает `RegisterError::AlreadyRegistered`. Состояние регистрации — тип
`RegisteredSpecs` в `Context::insert_data`; он же служит маркером.

Последствия: детерминированное и простое поведение; повторная регистрация с
другой конфигурацией тоже отклоняется (первая регистрация неизменяема).

## ADR-0007 (M2): атомарность регистрации — build → preflight → install → rollback

Контекст: при ошибке ни один из глобалов не должен остаться установленным.

Решение: конструкторы/прототипы строятся до изменения `globalThis`;
preflight проверяет расширяемость global и отсутствие имён `Blob`, `File`,
`FileList`; установка выполняется с откатом (`delete_property_or_throw`) при
сбое; маркер регистрации вставляется только после успешной установки.

Последствия: сбой на любом шаге оставляет globalThis без изменений; маркер
позволяет повторить регистрацию после отката.

## ADR-0008 (M2): `BlobPart` — только строки, BufferSource, Blob/File

Контекст: Web IDL union для не-объектов выполняет `ToString` (числа
превращаются в строковые части), но заказ требует: «Иной part — синхронный
TypeError».

Решение: строковые значения идут по пути USVString (UTF-8, `endings`);
BufferSource — точная копия видимого диапазона; Blob/File — разделяемые
сегменты; все остальные значения (числа, логические, `null`, объекты вне
брендов, Symbol) — синхронный `TypeError`.

Последствия: поведение ужесточено относительно «наивного» ToString-коэрсинга
в соответствии с фиксированным контрактом M2.

## ADR-0009 (M2): инъекция Clock для `File.lastModified`

Контекст: детерминированные тесты требуют управляемого времени.

Решение: `trait Clock: Send + Sync + 'static` с `now_unix_millis()`;
`FileApiExtensionBuilder::clock` инъектирует реализацию, по умолчанию —
`SystemClock` (системное время, без вывода из окружения процесса).

Последствия: значение используется только когда `lastModified` опущен;
переданное значение конвертируется обычным `long long` без обращения к часам.

## ADR-0010 (M2): no-copy Blob composition without public probes

Контекст: композиция Blob без копирования требует доступа к сегментам
существующего блоба, но M2-заказ запрещает раскрывать raw segments,
Arc-указатели и тестовые аксессоры в публичном Rust/JS API, а M1-контракт
фиксирует публичную поверхность `BlobData`.

Решение: публичный API core — ровно M1-контракт плюс два семантических
примитива композиции: `concat_shared(&self, other, media_type, limits)`
(перелинковка shared `Arc`-источников под теми же лимитами) и
`push_shared` (пошаговое накопление частей для `PartsCollector` с учётом
`max_parts`). Никаких публичных чтений байтов (`read_all` удалён:
материализация до `max_blob_size` обходила бы `max_materialize_bytes`) и
никаких identity-проб (`shares_sources_with` /
`first_segment_shares_source_with` удалены как тестовые аксессоры).
Доказательство no-copy живёт только в `#[cfg(test)]` child-модуле
`src/blob.rs` через прямой доступ к приватным полям (`Arc::ptr_eq`),
что явно разрешено заказом M1 §6.6. `PartsCollector` в `boa_fapi`
владеет одним `BlobData` и никогда не трогает сырые сегменты; тесты
`boa_fapi` (`src/tests.rs`) assert'ят только JS-наблюдаемое состояние и
публичные M1-метаданные (`size`, `segment_count`, `media_type`).

Последствия: контракт M1 расширен минимально и только семантическими
операциями композиции; guard `blob_data_public_api_is_fixed` фиксирует
точную поверхность из 9 методов; guard-тесты M1 продолжают проходить.

## ADR-0011 (M3-A): `BlobData::materialize` — единственная ограниченная byte-операция

Контекст: нормативное чтение Blob требует байтов, но M2 запретил
публичные чтения (`read_all` удалён: материализация до `max_blob_size`
обходила бы `max_materialize_bytes`).

Решение: `materialize(&self, limits, cancel) -> Result<Bytes, FileApiError>`
— единственный M3 semantic primitive. Проверяет
`size <= max_materialize_bytes` до allocation, конвертирует размер
fallibly в `usize`, резервирует `try_reserve_exact` (failure →
`ResourceLimit(MaterializeBytes)`), проверяет cancellation до первого и
перед каждым сегментом, читает ровно `offset..offset+len` с checked
arithmetic, при любой ошибке возвращает `Err` без partial bytes. Guard
расширен ровно на `materialize` (10 методов); сегменты, источники и
identity-пробы по-прежнему не публичны.

Последствия: bindings читают Blob только через этот метод внутри Boa job;
лимит materialization впервые становится наблюдаемым как `RangeError`
(см. ADR-0012).

## ADR-0012 (M3-A): Boa job queue как единственный механизм settlement

Контекст: заказ требует pending `Promise` немедленно и settlement только
из очереди Boa, без `Promise.resolve`-реализации, синхронного settle,
собственного event loop, потоков и `tokio`.

Решение: методы создают `JsPromise::new_pending` в current realm,
захватывают `Arc<BlobData>` + клон лимитов + mode в `PromiseJob` с realm
и ставят через `Context::enqueue_job`. Job вызывает `materialize`,
упаковывает (UTF-8 replacement / свежий `ArrayBuffer` / свежий offset-0
`Uint8Array`) и вызывает resolve/reject ровно один раз. `run_jobs()`
остаётся обязанностью embedder (показано в README); job сам его не
вызывает. `ResolvingFunctions` (`JsFunction`-пара) путешествуют в capture
job'а, а не в core/native DTO — GC-safe без `unsafe`.

Последствия: реакции `.then` выполняются следующим проходом Boa jobs;
порядок FIFO доказан JS-тестами; новых dependencies нет.

## ADR-0013 (M3-A): materialization-limit — `RangeError`, остальное — `Error`

Контекст: M3 фиксирует отображение ошибок чтения, но `DOMException`
появится только в M4.

Решение: `ResourceLimit(MaterializeBytes)` reject-ится `RangeError`;
любая другая core/read-ошибка — plain `Error` без path/source/body в
message (`js_read_error`); packaging-ошибка движка reject-ится её opaque
значением. Синхронные brand-нарушения остаются `TypeError` без создания
`Promise`.

Последствия: тип rejection предсказуем и проверен JS-тестами на границе
`size == limit` / `size == limit + 1`.
