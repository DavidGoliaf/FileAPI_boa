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

## ADR-0014 (M3-B): bounded `BlobReader` вместо materialization для потоков

Контекст: стриминг обязан выдавать данные по demand без чтения всего Blob;
`materialize()` для этого непригоден (память O(size), нарушение
backpressure), переписывать его запрещено.

Решение: `BlobData::reader(limits)` snapshot-ит `default_chunk_size`
(валидация `16 KiB..=1 MiB`, без silent clamp) и возвращает `BlobReader`
с приватным курсором. `read_next()` выдаёт максимум один chunk
`min(chunk, remaining)` за O(chunk) памяти, пересекая сегменты с checked
arithmetic и exact-length проверками источника; short/long/cancel/source
ошибки делают reader terminal с replay того же класса без новых чтений.
`cancel()` идемпотентен и изолирован. Guard фиксирует 11 методов
`BlobData` + 2 метода `BlobReader`; позиции/сегменты/источники не публичны.

Последствия: bindings читают строго по demand; whole-Blob materialization
в stream-пути отсутствует по построению.

## ADR-0015 (M3-B): capability-checked shim вместо WHATWG Streams

Контекст: Boa 0.22 не содержит `ReadableStream`; полный WHATWG Streams вне
scope M3-B, но `stream()`/`textStream()` обязаны возвращать настоящие
брендированные объекты с demand-семантикой.

Решение: `streams.rs` + Cargo feature `streams-shim` (default on) +
`FileApiExtensionBuilder::streams_shim(bool)` (default true). Регистрируются
только 2 globals, 5 методов, 1 accessor, 2 toStringTag; конструкторы
неконструируемы (`TypeError`). Бренд — native data с shared
`Rc<RefCell<StreamShared>>` (только Rust-состояние); jobs захватывают
shared cell + resolvers и ставят ровно одну job на `read()`. Отключение
(feature off или `streams_shim(false)`) возвращает typed
`RegisterError::StreamsShimDisabled` до мутации `globalThis`; конфликты
глобалов и нерасширяемость — fail-fast с atomic rollback, как M2.

Последствия: отсутствие host-адаптера честно сигнализируется вместо
заглушек; M2 preflight/rollback атомарно охватывают новые globals.

## ADR-0016 (M3-B): incremental UTF-8 decoder без `encoding_rs`

Контекст: `textStream()` обязан не выдавать U+FFFD раньше EOF при split
multibyte-последовательности; разрешённая зависимость `encoding_rs`
не понадобилась.

Решение: собственный `Utf8Decoder` (~60 строк): `push()` отделяет
валидный префикс через `incomplete_tail_len` (только строгие
продолжения лидеров, max 3 байта буфера), `flush()` на EOF превращает
остаток в U+FFFD побайтово — итог равен `from_utf8_lossy` на всём входе
без его материализации. Новых dependencies нет.

Последствия: split на любой байтовой границе доказан JS-тестами;
зависимость не добавлена, ADR о ней не нужен.

## ADR-0017 (M4-A): минимальный DOM shim и capability negotiation

Контекст: `FileReader` требует `EventTarget`/`Event`/`ProgressEvent`/
`DOMException`, но Boa 0.22 не содержит DOM; полный DOM вне scope M4-A.

Решение: `dom.rs` + Cargo feature `dom-shim` (default on) +
`FileApiExtensionBuilder::dom_shim(bool)` (default true). Регистрируются
только 5 globals (`EventTarget`, `Event`, `ProgressEvent`, `DOMException`,
`FileReader`), 3 метода EventTarget, 7 атрибутов + 2 метода Event, 3
атрибута ProgressEvent, `name`/`message` DOMException, 5 методов + 3
readonly атрибута + 6 handlers + 3 константы FileReader. Бренд — нативные
типы данных (JS не может подделать); dispatch только at-target;
`capture` принимается, но не влияет на порядок. Отключение (feature off
или `dom_shim(false)`) возвращает typed `RegisterError::DomShimDisabled`
до мутации `globalThis`; конфликты и нерасширяемость — fail-fast с atomic
rollback, как M2/M3-B. Host DOM adapter отсутствует честно, вместо
заглушек.

Последствия: M2 preflight/rollback атомарно охватывают новые globals;
`FileReader.prototype` наследует `EventTarget.prototype`.

## ADR-0018 (M4-A): FileReading FIFO + generation lifetime

Контекст: File API требует отдельную FIFO File Reading task source,
generation-защиту от stale completion и доставку через обычный цикл
`Context::run_jobs()` без фоновых потоков.

Решение: jobs используют promise-job очередь Boa (`PromiseJob::with_realm`
+ `Context::enqueue_job`): `SimpleJobExecutor` дренирует всю promise
очередь за проход, поэтому successor pump, поставленный после всех уже
очередных jobs, всё равно выполняется в том же `run_jobs()` с точным FIFO
против ранних readers. `loadstart`/`progress` диспатчатся синхронно внутри
своего pump job (всё ещё внутри task source, никогда на вызывающем JS
стеке): иначе successor pump того же reader успел бы перевести state в
DONE до прибытия `loadstart` на non-terminal LOADING gate. Terminal
события (`load`/`error`/`abort` + условный `loadend`) идут через queued
dispatch jobs, чтобы reentrant handlers наблюдали settled DONE state.
Каждая операция несёт монотонную nonzero generation; любой late job с
чужой generation — строгий no-op (без чтения, мутации, слота квоты и
событий). Quota `max_concurrent_reads_per_global` живёт в per-Context
числах (`QueueHolder`: `active` + `next_generation`, без GC-указателей);
инкрементальный `BlobReader` и `encoding_rs::Decoder` путешествуют по
значению от job к job. Throttle `progress`: максимум раз в 50 мс по
инжектированному `Clock`, кроме одного на chunk при редких chunks;
финальный `progress(loaded=total)` всегда перед `load`; события несут тот
же clock tick как `timeStamp` (создание события часов не читает).

Последствия: abort/stale races детерминированы; `force_collect()` до jobs
не теряет callbacks/state (всё traced: reader в capture, listeners в
native data); потоков/`tokio`/собственного event loop нет.

## ADR-0019 (M4-A): result/error mapping

Контекст: нужны точные упаковки четырёх представлений и центральное
отображение core failures на `DOMException` (M4-A фиксирует выбор), плюс
миграция M3 promise-read failures с plain `Error`/`RangeError`.

Решение: `readAsArrayBuffer` — точные байты в свежий `ArrayBuffer`;
`readAsBinaryString` — один code unit U+0000..U+00FF на байт (NUL
сохраняются); `readAsText` — инкрементальный `encoding_rs::Decoder`
(`new_decoder_without_bom_handling` + ручной strip одного leading U+FEFF
для UTF-8 операций; replacement для malformed; split multibyte никогда не
эмитится рано); unknown label — `EncodingError` без partial result;
`readAsDataURL` — `data:<type>;base64,<payload>` (пустой type →
`data:;base64,`), стандартный base64 без пробелов, checked arithmetic до
аллокации против `max_data_url_output` (`QuotaExceededError`).
Центральный `dom::map_core_error`: NotFound→NotFoundError,
UnsafeFile/TooManyReads/PermissionDenied→SecurityError,
SnapshotChanged/FileLocked/InvalidRange/Internal→NotReadableError,
ResourceLimit→QuotaExceededError, Cancelled→AbortError; сообщения без
path/bytes/source details. M3 `text()`/`arrayBuffer()`/`bytes()` и M3-B
stream errors мигрированы ровно один раз на тот же mapping (тесты и trace
rows обновлены; старые claims про plain `Error`/`RangeError` удалены).

Последствия: память O(chunk + final result); при любой ошибке partial JS
result нет; `error` — `null` или same-realm `DOMException`.

## ADR-0020 (M4-A): зависимость `encoding_rs` 0.8

Контекст: `readAsText(blob, label)` обязан следовать Encoding Standard
через разрешённую ТЗ §2.3 зависимость (M3-B обошёлся ручным UTF-8
декодером, но произвольные labels требуют полной таблицы кодировок).

Решение: `encoding_rs = "0.8"` (workspace dep, зафиксирована в
`Cargo.lock` как 0.8.35) — Gecko-ориентированная реализация Encoding
Standard (активно поддерживается Mozilla/w3c-совместимая), permissive
лицензия (Apache-2.0 OR MIT) AND BSD-3-Clause, MSRV 1.36 (ниже нашего
1.91), без unsafe в нашем коде. Используется только `Encoding::
for_label_no_replacement` + инкрементальный `Decoder::decode_to_string`
с ручным UTF-8 BOM strip.

Последствия: `cargo-deny` allow-list пополнена `BSD-3-Clause` (третья
дизъюнктная лицензия `encoding_rs`); дерево зависимостей расширяется на
`encoding_rs` + `cfg-if`.

## ADR-0021 (M4-A): зависимость `base64` 0.22

Контекст: `readAsDataURL` требует стандартный base64 без пробелов/переносов
(ТЗ §2.3 разрешает `base64` 0.22).

Решение: `base64 = "0.22"` (workspace dep, зафиксирована в `Cargo.lock`
как 0.22.1) — широко используемый крейт (marshal pierce/rust-base64,
десятки миллионов загрузок), permissive лицензия MIT OR Apache-2.0, MSRV
1.48 (ниже нашего 1.91), без unsafe в нашем коде. Используется только
`Engine::encode` со `general_purpose::STANDARD`.

Последствия: `cargo-deny` allow-list не меняется (MIT/Apache-2.0 уже
разрешены); дерево расширяется минимально (без транзитивных deps).

## ADR-0022 (M4-B): worker environment descriptor and sync registration

Контекст: `FileReaderSync` по ТЗ §5.6 существует только в
`DedicatedWorker`/`SharedWorker`; в `Window` имени нет, в
`ServiceWorker` capability запрещена. Хост не должен выводить режим из
потока и включать его автоматически; дефолт не должен менять M4-A
поведение существующих пользователей.

Решение: публичный `FileApiEnvironment`
(`Window` default, `DedicatedWorker`, `SharedWorker`, `ServiceWorker`) +
`FileApiExtensionBuilder::environment()`; дескриптор хранится в
`ExtensionConfig`/`RegisteredSpecs`/`FileApiHandle` (есть
`FileApiHandle::environment()`), никогда не выводится из thread ID, типа
`Context` или callback'ов. Регистрация строит sync-спеки, префлайтует имя
и устанавливает global только для worker-дескрипторов; иначе имя не
появляется даже как `undefined`-shim. Конфликт имени, нерасширяемый
global, повторная регистрация и выключенный `dom-shim` идут через
существующий fail-fast/rollback contract (`rollback_globals` принимает
флаг sync-режима). Без feature `dom-shim` sync-модуль не компилируется, и
powerset остаётся зелёным.

Последствия: явный хост-контроль capability без workers runtime;
`Window`-дефолт сохраняет M4-A поверхность бит-в-бит (все M4-A тесты
зелёные без изменений).

## ADR-0023 (M4-B): shared sync/async packaging boundary

Контекст: четыре представления обязаны совпадать у async `FileReader` и
sync `FileReaderSync`; копирование алгоритмов грозит расхождением.
Новых dependencies нет (`encoding_rs`, `base64` и core primitives
переиспользуются), поэтому отдельный dependency-ADR не нужен.

Решение: приватный `package.rs` — единственное место для
`TextEncoding`/`resolve_label`/`IncrementalDecoder` (включая BOM strip и
replacement), `decode_text` (целый вход через тот же push+finish),
`package_binary_string`, `data_url_len` (checked arithmetic) и
`package_data_url` (повторная проверка перед аллокацией). Async
`filereader.rs` использует их инкрементально по чанкам, sync
`filereader_sync.rs` — целиком после bounded `materialize`. Sync
preflight фиксирован: brand → аргумент → label → `size >
max_sync_read_bytes` (`QuotaExceededError`) → длина data-URL → чтение;
чтений и аллокаций до preflight нет, async quota не затрагивается,
partial result невозможен.

Последствия: M4-A поведение не изменилось (вся M4-A сюита зелёная без
правок тестов); sync ошибки идут через тот же центральный
`DOMException` mapping.

## ADR-0024 (M5): capability representation — opaque registry slot, no path

Контекст: ТЗ §3.3/§4 требует capability-based FS: JS никогда не открывает
произвольный путь, capability не превращается в `PathBuf`, не
сериализуется в JS и не имеет getter'а пути; хост открывает read-only
ресурс до создания JS `File`.

Решение: `boa_fapi_core::policy` (Boa-free) владеет типами
`HostResourceId(u64)` (opaque слот), `FileOpenRequest { resource,
display_name, max_bytes }` (без локации), `FileGrant { resource,
snapshot }` (opaque capability), `trait FileResource` (positional
`read_at` + `current_snapshot`/`import_snapshot`, без локации), `trait
FileResourceOpener` (host callback, возвращающий только id),
`trait FileAccessPolicy` целевой формы ТЗ (`authorize_open`/
`authorize_read`) и `DenyAllPolicy` (default deny). `boa_fapi_fs`
владеет `FsRegistry` (map id → открытый `std::fs::File` + import
snapshot + pending `on_shutdown` closers; Mutex охраняет только карту и
никогда не удерживается во время I/O — операции клонируют handle через
`try_clone` под короткой блокировкой и читают метаданные/байты после
снятия блокировки) и `RegisteredResource` (только id + клон registry).
`FsRegistry::close` удаляет слот (OS handle дропается немедленно, не
откладывается до уничтожения registry); `close_all` дропает все слоты;
`on_shutdown`/`run_closers` выполняют one-shot closers ровно один раз
вне блокировки. `file_from_resource` принимает `(registry, Arc<dyn
FileResource>)` вместо целевого `&dyn` (иначе `ByteSource: 'static`
нельзя построить без lifetime в публичном типе, а registry нужен для
трекинга shutdown closer'а); backing `ArcResourceSource` хранит только
id/снапшот/shutdown и делегирует каждое чтение ресурсу с pre/post
snapshot-проверкой.

Последствия: локация не пересекает границу ни в одном публичном типе,
методе или ошибке (guards `public_api_no_path_types`,
`public_api_exposes_no_paths_or_mutable_bytes` расширены); закрытие
удаляет слот немедленно и идемпотентно; чтения после закрытия —
`NotFound`.

## ADR-0025 (M5): snapshot identity и enforced `copy_on_import`-or-deny

Контекст: ТЗ §4.1 требует opaque identity + size + modification marker,
детект replacement (не только mtime/size), без `mtime + size` как
«полноценной защиты»; если платформа не гарантирует identity —
`copy_on_import` либо отказ, никогда string-prefix локации. Заказ §3.1
прямо: «если платформа не может гарантировать безопасную
identity-проверку на открытом handle, реализация обязана выбрать
`copy_on_import` либо отказать в импорте».

Решение: `FileSnapshot { identity: u64, size, mtime_secs, mtime_nanos }`
(FNV-1a hash; Unix: `dev`+`ino` через `MetadataExt` — единственная
сильная identity в safe Rust 1.91). `platform_has_strong_identity() ==
cfg!(unix)`; на всех остальных платформах (Windows включительно)
прямые live-handle импорты **принудительно запрещены**, а не покрыты
слабым fallback'ом: `FileSource::new` возвращает `PermissionDenied`,
`RegistryPolicy::authorize_open` возвращает `PermissionDenied`, а
`file_from_resource` (non-Unix) отказывает `Filesystem` grants с
`PermissionDenied` до создания JS-объекта. NTFS `file_index`/`volume`
не используются: они требуют нестабильный `windows_by_handle` и потому
не являются safe-гарантией на зафиксированном тулчейне.
`open_copy_on_import` — обязательный fallback: доступен везде (через
`new_for_copy` мимо gate), материализует точечный снимок под
`max_bytes`, закрывает live handle **на каждом выходе** (успех, отказ по
лимиту, ошибка аллокации, ошибка чтения — внешняя обёртка безусловно
`close`ит потреблённую регистрацию поверх `open_copy_inner`);
одна копия потребляет одну регистрацию (для следующей — перерегистрация). `BlobData` вычисляет blob-level
snapshot через `snapshot_for_segments` (первый `Filesystem` в порядке
сегментов; информативен — границей является per-source проверка в
`read_range`). `FileSource::read_range` и `ArcResourceSource::read_range`:
cancel → shutdown → checked arithmetic → live snapshot == import →
policy hook → positional read → exact-length check → post-read snapshot
confirm. Несовпадение — `SnapshotChanged` (JS: `NotReadableError`), без
partial bytes. `RootConfinedPolicy` сверяет только opaque identity
открытого handle; `starts_with(root)` запрещён по построению (локации
нет вообще).

Последствия: на Unix truncate/replacement/delete/rename детектятся до
нового chunk; short read — `InvalidRange`; mtime+size одни никогда не
объявляются достаточными. На Windows/прочих — только
`copy_on_import`-or-deny (тесты `weak_platform_direct_import_is_refused`,
`weak_platform_copy_reports_no_location_detail`); живых handle-чтений и
мутационных race-тестов там нет по построению, а не silent-skip.

## ADR-0026 (M5): lifecycle API — `FileApiHandle::shutdown`

Контекст: ТЗ §4.1 требует host-controlled shutdown; текущий `register`
возвращает `FileApiHandle`, целевая форма — `FileApiExtension::shutdown`.

Решение: выбрана форма `FileApiHandle::shutdown(&self, context) ->
Result<(), RegisterError>` (mapping зафиксирован здесь): эквивалентные
гарантии целевой форме без разрыва существующего `register → handle`
контракта. `ShutdownFlag` (closed bit + shared `CancellationToken` +
`Mutex<Vec<ShutdownCloser>>`, за `Arc`) живёт в `RegisteredSpecs` и
клонируется в handle и в каждый fs-import. Каждый `file_from_resource`
трекает свой registry (`track(move || registry.close_all())`); shutdown
идемпотентен (closers выполняются ровно один раз, повторный shutdown —
no-op), атомарен относительно новых host-операций (reject до мутации
`globalThis`), выполняет все трекнутые closers (каждый `close_all`
удаляет слоты и дропает OS handles **немедленно**, не откладывая до
уничтожения registry), затем отменяет pending fs-work через
существующий cancellation protocol (без abort unsafe-кодом). Late Boa
jobs (promise reads, FileReader pump/dispatch, stream `pump_one`)
находят closed state и не посылают jobs/callbacks в уничтоженный
context. Blob URL store и structured-clone lifetime — M6 scope:
extension points зарезервированы в `lifecycle.rs` без реализации.

Последствия: повторный shutdown не паникует и не ставит callbacks; OS
handles освобождаются в момент shutdown (доказано `live_slot_count`
тестами, а не уничтожением registry); новые reads/materialize/stream/
FileReader после shutdown запрещены; M2–M4 regression зелёная.

## ADR-0027 (M5): новая Cargo feature `fs` без новых dependencies

Контекст: заказ требует feature-поведение (все комбинации собираются,
`fs` off не оставляет partial global) и ADR на каждую новую dependency.

Решение: новая feature `fs` (default on) в `boa_fapi`
(`fs = ["dep:boa_fapi_fs"]`); без неё `file_from_resource`, fs-типы и
shutdown не компилируются, memory API и регистрация работают
бит-в-бит. Новых dependencies нет: только std (`fs`, `collections`,
`sync`) + существующие `bytes`/`thiserror`/`boa_fapi_core`; отдельный
dependency-ADR не нужен. `cargo hack check --feature-powerset --depth 2`
зелёный.

## ADR-0028 (M6): environment identity — origin + partition + nonce, never inferred

Контекст: ТЗ §1.2/§3.2/§8 требует изоляции Blob URL по origin и storage
partition через host-controlled environment descriptor; заказ M6 §3
запрещает выводить descriptor из thread id, адреса `Context`, случайной
характеристики процесса или callback presence, а opaque origin обязан
быть непереиспользуемым для нового global.

Решение: core `EnvironmentDescriptor { kind, serialized_origin,
partition, nonce }` (Boa-free, `boa_fapi_core::blob_url`) + сравнимый
`EnvironmentKey { origin, partition, nonce }`. Биндинги строят его только
из явных builder-полей (`environment`/`origin`/`partition`/`nonce`,
дефолты `Window`/`"https://localhost"`/`0`/`0`); никакого вывода из
потока/контекста нет. `serialized_origin` — единственное, что попадает в
URL (`blob:<origin>/<uuid>`); partition и nonce не сериализуются и
отредактированы из обоих `Debug` (`EnvironmentDescriptor` и
`EnvironmentKey` — ручные redacted-impl; регрессия
`url_key_debug_redacts_partition_and_nonce`). Opaque origin — фиксированная строка
`"null"`, но ключ несёт host-supplied `nonce`, поэтому два opaque global
никогда не делят ключ при общем `blob:null/`-префиксе. Resolve требует
равенства полного ключа (одного origin недостаточно). Маппинг целевой
формы ТЗ (`EnvironmentDescriptor`/`EnvironmentKey`) — 1:1, без
адаптации; `FileApiEnvironment::Window/DedicatedWorker/SharedWorker/
ServiceWorker` сохранён как kind/compatibility surface (ServiceWorker
запрещает создание URL).

Последствия: same-partition check до выдачи `Arc<BlobData>`; foreign URL
неотличим от missing (один display string у `Malformed`/`Unavailable`);
тесты `url_partition_and_nonce_isolation`,
`url_failures_share_one_opaque_class`,
`same_origin_partitions_isolate` фиксируют изоляцию.

## ADR-0029 (M6): зависимость `getrandom` 0.3 для CSPRNG UUID

Контекст: заказ M6 §2.7 требует криптографически непредсказуемый UUID
для Blob URL (счётчик, timestamp, обычный PRNG запрещены) и ADR на
каждую новую production dependency (maintenance, license, `cargo-deny`,
отсутствие более узкого решения).

Решение: `getrandom = "0.3"` (workspace dep; в `boa_fapi` — единственное
место генерации UUID). Обоснование: узкая single-purpose библиотека
Rust Random WG (активно поддерживается, десятки миллионов загрузок),
пермиссивная лицензия MIT OR Apache-2.0 (allow-list `deny.toml` не
меняется), MSRV 1.85 (ниже нашего 1.91), `no_std`-совместима,
без `unsafe` в нашем коде (вызов — safe wrapper `getrandom::fill`).
Более узкого решения нет: `rand`/`uuid` тянут лишнее (генераторы,
парсеры, serde-фасады); std не даёт CSPRNG (`HashMap` RandomState —
не криптографический и запрещён контрактом); `boa_engine` CSPRNG не
экспортирует. `uuid`-крейт не взят сознательно: нужен ровно v4-формат
одной функцией `format_uuid_v4` (~15 строк, биты версии/варианта
ставятся у нас), а парсинг — строгий shape-check `parse_blob_url`
(36 символов, дефисы, nibble `4`, вариант `8/9/a/b`). Production
default — `OsEntropy` (`getrandom::fill` напрямую, без перебора
блоков); тестовый entropy source injectable через builder
(`UrlEntropySource`: `CounterEntropy`/`StuckEntropy` в M6-сюитах).
Платформенный отказ `getrandom` — `BlobUrlError::EntropyUnavailable`
(тот же network-error equivalent в JS), без fallback на счётчик/PRNG
по построению. `cargo-deny` чист (advisories/bans/licenses/sources).

Последствия: повтор UUID — `Collision` с bounded retry (8 попыток со
свежей энтропией), никогда тихий overwrite; creation timestamp не
является источником UUID и не попадает в URL (только монотонный `seq`
в entry как creation metadata); `cargo hack --feature-powerset`
зелёный (entropy signage живёт в `ExtensionConfig` независимо от
`url-shim` feature).

## ADR-0030 (M6): URL store lifetime — context-local store + shutdown clear

Контекст: ТЗ §8.4 требует удаления всех URL global при shutdown/
уничтожении runtime; заказ M6 §4.2/§4.3 требует revoke-семантику (новые
resolve — network-error, начатые чтения — до завершения) и отсутствие
Fetch-регистрации в `boa-fapi`.

Решение: `BlobUrlStore` (core, Boa-free: `Mutex<HashMap<String, Entry>>`
+ `AtomicU64 seq`, амортизированный O(1); мьютекс держится только на
map-операцию, никогда через I/O/JS) живёт per-context
(`Arc<BlobUrlStore>` в `RegisteredSpecs`; хэндл его не возвращает —
только count-only `blob_url_count()`/`blob_urls_empty()`; guard запрещает
`pub fn url_store`/`environment_key` и store/key в любой `pub fn`
сигнатуре/`pub use`): два контекста никогда не делят store/shutdown. Entry — `Arc<BlobData>` +
`EnvironmentKey` owner + `seq`. Shutdown трекает closer
`store.clear()` во флаге (как fs-closers): все сильные ссылки падают в
момент shutdown, повторный shutdown — no-op, новых JS callbacks нет.
`revoke` — после M6-rework R2: required-arg + центральный `webidl::dom_string`
(missing arg — `TypeError` до store, abrupt конверсия propagates), затем
idempotentный silent no-op для malformed/unknown/revoked/foreign (не oracle);
уже выданный `Arc` читается до конца (`materialize` после revoke доказан
тестом). `ResolvedBlob` — только `Arc<BlobData>` + `media_type`/`size`
(нет path/capability/handle/partition key/URL token). Resolver —
`FileApiHandle::resolve_blob_url` (host Fetch boundary, без network
handler); JS-facing failure — один `TypeError("blob URL is not
available")` для malformed/unknown/foreign/revoked/collision (не
различает наличие чужой записи, не выдаёт UUID/origin/existence bit).

Последствия: `url_shutdown_lifetime`,
`url_revoke_keeps_live_reads_and_clear_releases` фиксируют lifetime;
`resolve_blob_url` отдаёт только разрешённый body metadata.

## ADR-0031 (M6): feature/API compatibility — `FileApiHandle`, не `FileApiExtension`

Контекст: целевая форма ТЗ §4.1 (`FileApiExtension::register →
()/shutdown`) отличается от принятой M2–M5 формы (`register →
FileApiHandle`, `handle::shutdown`); заказ M6 §3 требует зафиксировать
совместимость до реализации и не ломать старые вызовы.

Решение: сохранена форма `register → FileApiHandle`; эквивалентность
зафиксирована здесь: `FileApiHandle::shutdown` даёт те же гарантии
(идемпотентность, атомарность к новым операциям, отмена pending work,
никаких callbacks после уничтожения `Context`), плюс M6 — `clear()` URL
store. M5 `shutdown` расширен с `#[cfg(feature = "fs")]` на
безусловный (флаг живёт в каждой регистрации): M1–M5 вызовы продолжают
собираться и работать бит-в-бит (все M2–M5 сюиты зелёные без правок,
кроме двух M4 negative-guard строк про `URL`, обновлённых под
нормативный M6 surface). Новые builder-флаги `url_shim`/
`structured_clone` (default true) + `origin`/`partition`/`nonce`/
`entropy`/`clone_adapter`: `url-shim`/`structured-clone` off оставляют
JS-surface отсутствующим, host-операции и M1–M5 — рабочими
(документированный выбор заказа §4.3/§5.2: отсутствующий surface вместо
`UrlAdapter`-ошибки, т.к. host URL adapter вне scope M6). `CloneAdapter`
— единственный мост к `boa-idb` (зависимости нет в любой комбинации,
guards фиксируют). `URL` — namespace-object (не конструктор, не WHATWG
URL): `createObjectURL.length === 1`, `revokeObjectURL.length === 1`,
`typeof URL === "object"`, `[Symbol.toStringTag] === "URL"`.

Последствия: diff scoped только M6; `M6-REG-01` — powerset зелёный;
`UrlAdapter`/`TaskAdapter`/`DomAdapter`/`StreamAdapter` остаются вне
scope (штрафов за их отсутствие нет — shim'ы покрывают).

## ADR-0032 (M6): versioned clone encoding `FCL1`/v1 и SCF tags

Контекст: ТЗ §4.5/§6.7 требует serializable `Blob`/`File`/`FileList`
через подключаемый bridge без зависимости от `boa-idb`: payload несёт
bytes + публичную метаинформацию, но никогда path/capability/handle/
snapshot identity; нужны стабильная версия, checked lengths/counts,
fallible decode и mapping зарезервированных SCF tags.

Решение: core `boa_fapi_core::clone` (Boa-free): layout `b"FCL1" | u32
LE version (= 1, `CLONE_ENCODING_VERSION`) | u32 LE tag | body`.
Теги: `SCF_BLOB_TAG = 0x424C_4F42` (`"BLOB"`), `SCF_FILE_TAG =
0x4649_4C45` (`"FILE"`), `SCF_FILE_LIST_TAG = 0x464C_5354`
(`"FLST"`) — стабильны с версией, задокументированы здесь; proprietary
IDB-формат не внедряется. Body — le-длины + байты + UTF-8 строки +
`i64 lastModified`; decode — `Cursor` с checked arithmetic против
`MAX_CLONE_BYTES` (256 MiB) / `MAX_CLONE_STRING_BYTES` (1 MiB) /
`MAX_CLONE_FILES` (100k): malformed/truncated/overflow/unknown-version/
unknown-tag/трейлинг — `CloneError::{Malformed, UnsupportedVersion,
LimitExceeded}` без panic и без partial output (same-version fixture в
core-тестах). После M6-rework R3 границы симметричны: `serialized_blob`
тоже проверяет string ceiling, а `encode()` прогоняет публичные поля
через `validate_payload` (bytes/strings/count/total, checked) до
первого байта — прямые конструкции без хелперов видят те же границы,
что и decode; принятое всегда декодируется текущим декодером. Encode — из уже материализованных bytes через
существующий checked path (`max_materialize_bytes`;
snapshot/permission/short-read — `SourceFailed` без partial payload);
`File` хранит sanitized `name` (`/` → `:` идемпотентно при decode) и
stored `lastModified` (часы не читаются); результат — новый immutable
backing (mutable JS buffers не делятся). `FileList` — порядок,
количество, brand каждого элемента до упаковки (partial list запрещён;
identity только внутри результата). `CloneAdapter { descriptor,
encode, decode }` + `CloneBridgeDescriptor { name, version }` —
capability/version check до изменения глобалов
(`RegisterError::CloneBridgeIncompatible` + полный rollback);
`NoBridge`/`Shutdown` — до payload. JS-глобалов у bridge нет
сознательно (host-side capability, не `structuredClone`).

Последствия: `M6-CLONE-01..05` — round-trip, fs-safety (unix live),
versioned encoding, adapter/lifecycle; `boa-idb` не зависит ни в одной
комбинации; M7 exclusions (WPT, benchmark-hardening) — вне scope.

## ADR-0033 (M7): ноль новых production-зависимостей для WPT harness

Контекст: M7 требует CLI harness с manifest-парсингом, SHA-256,
детерминированными JSON/JUnit отчётами и реальным Boa Context; заказ
§2.4 требует ADR на каждую новую dependency, SPEC §4 предпочитает уже
принятые крейты.

Решение: новых зависимостей нет. `boa_engine`/`boa_fapi`/`thiserror` уже
в дереве (транзитивно через `boa_fapi`); JSON-парсинг и сериализация —
свой маленький проверенный модуль (`manifest.rs`/`report.rs`, только
std); SHA-256 — собственная FIPS 180-4 реализация в CLI (~60 строк,
только для проверки corpus-хэшей, не для security boundary — UUID
по-прежнему из `getrandom` через M6 `OsEntropy`); CLI-парсинг —
ручной `Args::parse` (6 флагов, без `clap`/`lexopt`); параллелизм —
std threads в порядке manifest (без `rayon`); время — `std::time`
(дата для `review_by` — civil-from-days, без `chrono`/`time`).
`cargo-deny` не меняется (allow-list тот же); `cargo tree` новых узлов
не показывает.

Последствия: `boa_fapi_wpt` зависит только от уже принятых крейтов;
SBOM-артефакт CI подтверждает отсутствие новых лицензий; отдельный
dependency-ADR не нужен сверх этой записи.

## ADR-0034 (M8): единственная optional `tracing`-зависимость для terminal telemetry

Контекст: M8 требует terminal-only наблюдаемость девяти операций с
фиксированным allow-list из шести полей (§11.3), default-off, без
изменения JS API, порядка jobs, ошибок и lifetime. Нужен зрелый
инструментарий событий вместо ручного логгера; `tracing-subscriber`,
`tokio`, `futures`, `serde` и любые другие зависимости запрещены заказом.

Решение: `tracing = "0.1"` — workspace dependency, в `boa_fapi` только
optional (`tracing = ["dep:tracing"]`, не в `default`). Мотивация:
tokio-ecosystem стандарт де-факто для structured events (активно
поддерживается tokio-rs, десятки миллионов загрузок), пермиссивная
лицензия MIT (allow-list `deny.toml` не меняется), MSRV 1.63 (ниже нашего
1.91), `no_std`-совместима в нужном профиле, без `unsafe` в нашем коде
(только `info!`/`event!` макросы с шестью типизированными полями).
Транзитивно тянет только `tracing-core` + `tracing-attributes` (обе MIT).
Более узкого решения нет: `log`/`env_logger` не дают типизированных
полей и target-идентичности события; ручной subscriber-фасад дублировал
бы `tracing::Subscriber` без выигрыша в secrecy-контроле. Схема события
фиксирована: target `boa_fapi::file_api.operation` — единственный
идентификатор (отдельного поля `event` нет); `operation` (9 имён),
`size`/`duration_ms`/`chunk_count`/`environment_hash` (`u64`),
`result_class` (10 классов). Тестовый collector — только
`tracing::Subscriber` + std (без `tracing-subscriber`).

Последствия: feature-off сборки не содержат dependency в графе
(`cargo hack --feature-powerset` зелёный, `cargo tree` без tracing без
фичи); `cargo-deny` чист (только pre-existing duplicate-version
warnings); telemetry-слой — внутренний instrumentation без публичных
адаптеров и без вызовов JS.

## ADR-0035 (M8): wasm backend для уже существующей Boa entropy-зависимости

Контекст: обязательный memory-only gate M8 собирает `boa_fapi` на
`wasm32-unknown-unknown` с отключёнными File API features. `boa_engine` и
`boa_fapi` используют уже существующие `getrandom` 0.4 и 0.3; без web
backend upstream crates намеренно завершаются `compile_error!`. Добавлять
новый runtime, JS API или новый crate для этого gate нельзя.

Решение: для wasm target включать существующую feature `boa_engine::js`,
которая подключает его штатный `getrandom/wasm_js` backend, и
`getrandom/wasm_js` для прямой зависимости `boa_fapi`. В корневом
`.cargo/config.toml` зафиксировать требуемый для `getrandom` 0.3 cfg
`getrandom_backend="wasm_js"`. На native targets dependency features и
rustflags не меняются; новых зависимостей и public API нет.

Последствия: оба обязательных wasm `cargo check` проходят воспроизводимо,
а web entropy implementation остаётся штатной реализацией upstream. Runtime
использование File API на wasm по-прежнему не расширяется: M8 проверяет
только memory-only compilation gate.

## ADR-0036 (M9-A, rework-superseded in part): `sequence<BlobPart>` conversion

Контекст: M2 фиксировал array-only контракт (`JsArray`-проверка); ТЗ
M9-A §2 требует общую Web IDL sequence conversion для Blob и File:
`@@iterator` один раз, поддержка Array/custom iterable/boxed String/
TypedArray-as-sequence, primitive string — conversion error, quota до
накопления, один converter на оба конструктора.

Решение: единый конвертер; `GetMethod(V, @@iterator)` один раз;
`Call`/`next`/`done`/`value` слева направо; quota `max_parts`
проверяется до накопления — бесконечный итератор завершается
детерминированной quota-ошибкой. Rework-поправка (§6 заказа-rework):
никакого `return()` при abrupt completion — первоначальный
`iterator_close_and_propagate` удалён как несоответствующий актуальным
`sequence<T>` creation steps; quota-ошибка тоже не закрывает итератор
(observable extension отклонён, см. ADR-0040).

Последствия: M9A-IDL-01/02 и M9A-RW-05 фиксируют поведение;
расхождение Blob/File запрещено по построению (один путь).

## ADR-0037 (M9-A): exact `BlobPart` union conversion

Контекст: M2 уже́сточал union до `TypeError` для не-объектов (ADR-0008);
ТЗ M9-A §3 требует точную Web IDL развилку: BufferSource → видимый
диапазон; branded Blob/File → shared bytes; всё остальное — USVString/
ToString; forged brand — fallback; throwing `toString` — abrupt rules;
порядок наблюдаем и одинаков для Blob/File.

Решение: `process_part` — BufferSource, затем бренд, затем USVString
fallback для любого другого значения (Symbol бросает собственный
`TypeError` из `ToString`); старые oracle `TypeError` для `[123]`,
`[null]`, `[{}]` переписаны на размеры USVString (3/4/15), а не удалены.
BigInt stringifies (`10n` → 2 байта).

Последствия: M9A-IDL-03 фиксирует fallback; ADR-0008 superseded в части
union fallback, бренд-проверки сохранены.

## ADR-0038 (M9-A, rework-superseded): shared text packaging

Контекст: ТЗ §6.4 краток (label → UTF-8 → BOM → U+FFFD) и не упоминает
MIME `charset`; W3C File API WD 23.08.2026 packaging-data steps требуют
промежуточный `charset`-шаг. Первоначально сохранялся `EncodingError`
для неизвестного пользовательского label.

Решение (rework, см. ADR-0040): `package::resolve_text_encoding(label,
media_type)` возвращает `TextEncoding` (не `Option`): explicit label →
MIME `charset` → UTF-8 через точный `get an encoding`
(`Encoding::for_label`, не `for_label_no_replacement`); `replacement`
резолвится и декодирует в U+FFFD побайтово. Оба ридера идут обычным
путем (async — start/read/load, sync — строка). Change control —
`docs/spec-delta.md`.

Последствия: M9A-TEXT-01/M9A-RW-01/M9A-RW-02 фиксируют алгоритм и
sync/async-паритет; новых зависимостей нет.

## ADR-0039 (M9-A): opaque registration identity, shutdown never revives

Контекст: ТЗ §4.1 требует identity-aware повторную регистрацию без
сравнения trait objects по значениям, без публичного сравнения config и
без раскрытия identity; атомарный preflight/rollback не ослабляется;
повтор после `shutdown` не оживляет runtime.

Решение: `RegistrationIdentity(u64)` — opaque токен (`AtomicU64`,
`build()` mint'ит, `Clone` сохраняет); `RegisteredSpecs` хранит identity
первой регистрации. Повтор той же identity — idempotent (handle на уже
зарегистрированное состояние, globals не переустанавливаются); другая
identity — `RegisterError::AlreadyRegistered` без мутации; после
`shutdown` та же identity возвращает существующий (закрытый) handle, а
не живой runtime; разные Context независимы. Отдельного typed error для
post-shutdown не введено: существующий закрытый handle уже несёт
shutdown-состояние, что фиксирует тест.

Последствия: M9A-REG-01 фиксирует все четыре ветви; публичная
поверхность не расширена (guard `lib_rs_denies_unsafe` зелёный).

## ADR-0040 (M9-A rework): encoding fallback, argument order, iterator surface

Контекст: заказ-rework `tasks/18_TASK_M9A_REWORK_CONFORMANCE.md`
переопределяет три M9-A границы и фиксирует два дополнительных defect.
Web IDL snapshot для rework: `boa_engine 0.22.0` (sequence/iterator
семантика vendored-крейта; `get an encoding`/`Decode` — `encoding_rs
0.8.35`, `Encoding::for_label` + `new_decoder()` со sniffing).

Решение:

1. Неизвестный explicit label — failure с fallback (MIME charset →
   UTF-8), не `EncodingError`; `for_label_no_replacement` заменён
   точным `for_label`; fail-fast `EncodingError`-ветки удалены из
   обоих ридеров; M4/M8 oracle переписаны (pure-model `ReadBad` →
   `ReadAgain`, `TermKind::Error`/`ErrorRestart` удалены,
   `reentrant_error_handler` — через quota-`SecurityError`,
   tracing-класс `encoding` — только для `replacement`-label reads).
2. Порядок аргументов: `Blob(blobParts → options)`,
   `File(fileBits → fileName → options)`; двухфазная модель
   (`ConvertedBlobPart`: conversion-time snapshots BufferSource/
   USVString/brand → `process_converted` с `endings` и итоговым
   size-accounting); сырые `JsValue` между фазами не хранятся.
3. Никакого `return()` при abrupt completion, включая quota-лимит
   (единый Web IDL path без закрытия; observable extension отклонён).
4. `FileList.prototype[Symbol.iterator] === Array.prototype.values`
   (тот же function object, `{writable:true, enumerable:false,
   configurable:true}`); `entries`/`keys`/`values`/`forEach` не
   добавляются. BOM-sniffing оставлен как есть (`new_decoder()`),
   provenance-флаг отклонён: Decode заменяет любой fallback.
5. `docs/spec-delta.md` фиксирует label → MIME → UTF-8 и BOM
   authority со ссылками; `docs/spec-matrix.md` — M9A/M9A-RW строки с
   source/test anchors.

Последствия: trace rows M9A-RW-01…06; совокупный M9-A diff — см.
rework-handoff.

## ADR-0041 (M9-A acceptance remediation): local MIME parser

Контекст: MIME `charset` нельзя извлекать ad hoc-разделением строки по
`;`: сначала требуется parse type/subtype, после чего параметры обрабатываются
по permissive WHATWG algorithm — malformed parameter пропускается, quoted
value может содержать `;`, незакрытая кавычка завершается на EOF, а suffix
после закрывающей кавычки игнорируется.
Новая внешняя зависимость не нужна для ограниченного packaging surface.

Решение: использовать небольшой локальный parser в
`crates/boa_fapi/src/package.rs`. Он проверяет ASCII grammar type/subtype,
пропускает malformed individual parameters, поддерживает token/quoted
parameter values включая EOF-terminated quote, выбирает первый duplicate
parameter и возвращает charset из сформированного MIME record. `mime`/прочие
crate не добавляются: они были бы шире текущего surface, не дают выигрыша
для этой фиксированной операции и потребовали бы license/maintenance gate.

Последствия: `Blob.type` по-прежнему нормализуется существующим M1 helper,
а packaging отдельно валидирует MIME syntax перед charset fallback; Cargo
граф и `cargo-deny` остаются без новых зависимостей.
