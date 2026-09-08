# Заказ M6-URL-CLONE — Blob URL, environment isolation и structured-clone bridge

| Поле | Значение |
|---|---|
| ID | `M6-URL-CLONE` |
| База реализации | Принятый M5 code baseline `e23e0721e885561deda52c211075ed389dfd3cca` |
| База документов | Документальный head `a0aec4f` (`task/m5`) |
| Ветка | `task/m6`, от принятой базы; не работать напрямую в `main` |
| Нормативная база | `TZ_boa_fapi_FileAPI.md` §1.2–§1.6, §2.1–§2.4, §3.1–§3.3, §4.1–§4.5, §5.1–§5.4, §8, §9.2, §10, §11, §12, §14–§17 |
| Зависимости | Принятые M1–M5: `BlobData`, `File`, `FileList`, limits/materialization, Boa jobs, lifecycle shutdown и capability FS |
| Результат | Изолированный Blob URL store/resolver, `URL.createObjectURL()`/`revokeObjectURL()`, versioned clone payload для `Blob`/`File`/`FileList` и host adapter без зависимости от `boa-idb` |

## 1. Цель

Добавить следующий нормативный слой после M5: хост получает безопасный
механизм временно публиковать уже созданные `Blob`/`File` через `blob:` URL и
подключать сериализацию File API objects к внешнему structured-clone/IndexedDB
runtime.

Blob URL должны быть изолированы host-controlled environment descriptor и
storage partition. URL не содержит capability, путь, handle, секретный
partition key или другие внутренние данные. `resolve_blob_url` отдаёт только
разрешённый `Blob`/body metadata; resolver Fetch не регистрируется самовольно.

Clone payload должен переносить materialized immutable bytes и публичную
метаинформацию `Blob`/`File`/`FileList`, но никогда не переносить host path,
filesystem capability, OS handle или snapshot identity. Bridge остаётся
подключаемым: `boa-fapi` не начинает зависеть от `boa-idb`.

## 2. Жёсткие границы

1. Не менять принятую memory-backed и filesystem-backed семантику M1–M5:
   brands, prototypes, descriptors, Web IDL conversions, readers, streams,
   snapshot validation, limits, shutdown и central DOMException mapping.
2. Не добавлять полный WHATWG URL parser, Fetch, HTML DOM, Workers runtime,
   MediaSource, File System Access API или directory enumeration.
3. Не добавлять прямое открытие пути из JavaScript. `new Blob([string])` и
   clone payload трактуют строки/байты только как данные.
4. Не добавлять `unsafe`, production `unwrap`/`expect`/`panic`, отдельный
   error mapping в URL/clone слоях или callbacks после shutdown.
5. Не добавлять `boa-idb` как зависимость. Интеграция выполняется через
   public/host adapter и тестовый fake bridge в этом заказе.
6. Не включать полный WPT harness, benchmark-hardening и capability gaps M7;
   добавить только bounded tests, необходимые для M6 contracts.
7. Любая новая production dependency требует записи в
   `docs/DECISIONS.md` до использования. Для URL UUID обязательна
   криптографически непредсказуемая генерация; счётчик, timestamp и обычный
   предсказуемый PRNG запрещены.

## 3. Ожидаемая API-модель

Сохрани существующий публичный M1–M5 API и добавь минимальные типы/адаптеры,
не раскрывающие внутренние структуры `BlobData`:

1. `EnvironmentDescriptor` — host-supplied descriptor с видом global,
   serialized origin и opaque storage-partition identity. Descriptor нельзя
   выводить из thread id, адреса `Context`, случайной характеристики процесса
   или callback presence. `FileApiEnvironment::Window`,
   `DedicatedWorker`, `SharedWorker`, `ServiceWorker` сохраняются как
   существующий kind/compatibility surface.
2. `EnvironmentKey` — сравниваемый внутренний ключ same-partition checks,
   не сериализуемый в URL и не доступный JS. Opaque origin обязан быть
   непереиспользуемым для нового global.
3. `BlobUrlStore` — thread-safe add/get/revoke/clear store с
   амортизированной O(1) операцией. Entry удерживает `Arc<BlobData>`,
   environment ownership и creation metadata; creation timestamp не является
   источником UUID и не попадает в URL.
4. `ResolvedBlob` — host-side результат с `Arc<BlobData>`/body source,
   media type и checked length. В нём нет path, capability, OS handle,
   partition key или полного внутреннего URL token.
5. `BlobUrlError` — typed Rust error для malformed/unknown/foreign/revoked
   URL и limit/shutdown случаев. JS-facing URL failure выглядит как
   network-error equivalent и не различает наличие чужой записи.
6. `FileApiClonePayload` с versioned encoding для:
   `Blob(SerializedBlob)`, `File(SerializedFile)` и
   `FileList(Vec<SerializedFile>)`. Payload должен иметь явную версию,
   checked lengths/counts и fallible decode.
7. `CloneAdapter`/эквивалентный host bridge — capability/version descriptor,
   регистрация serializable payload и encode/decode entry points. Конкретный
   trait design допускается изменить, если сохранены границы §2 и ADR.

Если выбранная форма API отличается от целевой формы ТЗ (`FileApiExtension`
против текущего `FileApiHandle`), зафиксируй совместимость и последствия в
ADR до реализации. Старые M1–M5 вызовы должны продолжить собираться и
работать.

## 4. Blob URL store и resolver

### 4.1. Создание и формат

1. `URL.createObjectURL(object)` принимает только brand-валидный `Blob` или
   `File`. `FileList`, arbitrary object, forged object и `MediaSource` дают
   синхронный `TypeError`/эквивалент существующего central mapping.
2. URL сериализуется как `blob:<serialized-origin>/<uuid>` согласно
   `TZ_boa_fapi_FileAPI.md` §8.2. Serialized origin не должен раскрывать
   storage partition, capability или host path.
3. UUID генерируется CSPRNG. Тестовый entropy source может быть injected,
   но production default не может быть счётчиком, timestamp или predictable
   PRNG. Повтор UUID должен обрабатываться без тихого overwrite.
4. Создание атомарно относительно `max_blob_urls_per_global`; отказ лимита
   не оставляет entry и не меняет JS global. `0`/невалидные limits
   отклоняются существующим `FileApiLimits::validate()`.
5. `URL.createObjectURL()` возвращает строку в том же relevant realm и не
   ставит Boa job сам по себе.

### 4.2. Resolve, partition и revoke

1. `resolve_blob_url(url, requester)` принимает только корректный `blob:`
   URL формата store. Malformed, unknown, revoked и foreign-partition URL
   имеют одинаковую externally observable failure class.
2. Same-partition check выполняется до выдачи `Arc<BlobData>`. Одного origin
   недостаточно: storage partition также обязан совпасть.
3. Не выдавай URL token, UUID, origin internals, existence bit или host
   metadata в JS exception, Rust display string, tracing payload или test
   assertion.
4. `URL.revokeObjectURL(url)` идемпотентен: повторный revoke, malformed URL
   и URL другой partition не превращаются в observable enumeration oracle.
5. Revoke удаляет entry для новых resolve, но уже начатое чтение, которое
   получило `Arc<BlobData>`, продолжает работать до завершения. Revoke не
   мутирует уже опубликованный `Blob`/`File`.
6. Resolver возвращает status class, media type, checked length и body
   stream/source для host Fetch adapter. `boa-fapi` не устанавливает
   network handler и не реализует Fetch самостоятельно.

### 4.3. JS registration и environments

1. При включённом `url-shim` зарегистрируй минимальный URL surface только
   атомарно: `URL.createObjectURL` и `URL.revokeObjectURL` со статическими
   descriptors/length и корректным illegal invocation behavior. Если host
   предоставляет совместимый `UrlAdapter`, используй его без двойной
   регистрации.
2. В `Window`, `DedicatedWorker` и `SharedWorker` URL API работает с
   переданным descriptor. В `ServiceWorker` создание Blob URL запрещено по
   ТЗ; глобал не должен частично изменяться.
3. `url-shim` off не должен ломать M1–M5. В этом режиме registration требует
   совместимый host URL adapter для URL methods либо явно оставляет URL
   surface отсутствующим; выбор и ошибка preflight документируются.
4. Повторная registration и global name conflict используют существующий
   atomic rollback contract.

## 5. Structured-clone bridge

### 5.1. Payload semantics

1. `Blob` round-trip сохраняет bytes, `size` и normalized `type`.
2. `File` round-trip сохраняет bytes, `type`, sanitized `name` и
   `lastModified`; результат получает новый immutable backing и не делит
   mutable JS buffers с исходным object.
3. `FileList` round-trip сохраняет порядок, количество, identity только
   внутри результата и все `File` metadata. Каждый элемент до упаковки
   проходит brand validation; partial list запрещён.
4. Host-backed File сначала materialize-ится через существующий checked path
   и `max_materialize_bytes`; snapshot/permission/short-read failure даёт
   typed error без partial payload. Clone не копирует live path/capability.
5. Decode выполняет checked arithmetic для version, byte lengths, string
   lengths, file count и total payload. Invalid/truncated/unknown-version
   input отклоняется без panic и без частичной JS регистрации.
6. Версия encoding стабильна. Зарезервированные SCF tags должны иметь
   documented mapping и fixture для same-version round-trip и unsupported
   future-version rejection. Не внедряй proprietary IDB format.

### 5.2. Adapter/lifetime

1. `CloneAdapter` регистрируется и проверяется до изменения глобалов;
   missing/incompatible adapter даёт Rust error и полный rollback.
2. Adapter не получает `Context`/`JsObject` в DTO, которые удерживаются
   после Boa job; GC-safe ownership должна следовать принятым M2–M4 patterns.
3. Pending clone work observes `ShutdownFlag`. После shutdown не должно быть
   новых JS callbacks, promise settlements или clone writes в уничтоженный
   `Context`; already returned detached/materialized payload remains owned
   safely by the host.
4. `structured-clone` off сохраняет M1–M5 behavior и не оставляет partial
   clone globals/brands. `boa_fapi` не зависит от `boa-idb` в любой feature
   комбинации.

## 6. Required tests and traceability

Добавь unit/integration tests с реальным `boa_engine::Context`, не ослабляя
существующие M1–M5 fixtures. Рекомендуемые файлы —
`crates/boa_fapi_core/tests/blob_url.rs`,
`crates/boa_fapi/tests/m6_blob_url.rs` и
`crates/boa_fapi/tests/m6_structured_clone.rs`; допускается иная раскладка
при сохранении traceability.

Минимальное покрытие:

| ID | Требование | Минимальное доказательство |
|---|---|---|
| M6-URL-01 | CSPRNG URL format, no secret internals, no overwrite | deterministic entropy test + generated URL assertions + collision handling |
| M6-URL-02 | same-origin + same-partition isolation | two descriptors/contexts; foreign lookup indistinguishable from missing |
| M6-URL-03 | create/revoke semantics | Blob/File success, wrong brands, repeated revoke, revoked lookup, active `Arc` read after revoke |
| M6-URL-04 | limits and atomic registration | exact max accepted, +1 rejected, zero/invalid config rejected, no partial globals |
| M6-URL-05 | environment gating | Window/DedicatedWorker/SharedWorker allowed; ServiceWorker denied; URL feature-off guard |
| M6-URL-06 | shutdown lifetime | store is cleared and strong references released at shutdown; repeated shutdown idempotent; no late job/callback |
| M6-CLONE-01 | Blob/File payload round-trip | bytes/type/name/lastModified/size with mutation isolation |
| M6-CLONE-02 | FileList round-trip | order, count, brands, metadata; non-File input rejected before output |
| M6-CLONE-03 | filesystem safety | changed/deleted/permission/short-read source gives typed failure and no partial payload; no path/capability in payload/errors |
| M6-CLONE-04 | versioned checked encoding | malformed/truncated/overflow/unknown-version input rejected; same-version fixture passes |
| M6-CLONE-05 | adapter/lifecycle | missing adapter and incompatible version fail atomically; shutdown cancels pending work |
| M6-REG-01 | memory and M5 regression | existing M2/M3/M4/M5 tests unchanged and green; feature powerset green |

Также добавь negative guards на отсутствие `boa-idb` dependency, raw paths,
`unsafe`/production panic APIs, accidental full URL/clone surface when
features are disabled и duplicate URL registration.

## 7. Documentation and decisions

1. Обнови `docs/spec-matrix.md` строками `M6-URL-01..06`,
   `M6-CLONE-01..05`, `M6-REG-01` с точными source/test anchors.
2. Обнови `README.md`, `docs/architecture.md`, `docs/security.md` и
   feature documentation: URL isolation, resolver boundary, clone payload
   excludes paths/capabilities, M6 feature-off behavior и M7 exclusions.
3. Добавь ADR в `docs/DECISIONS.md` для environment identity, UUID entropy,
   URL store lifetime, feature/API compatibility и versioned clone encoding.
   Если добавляется dependency (`uuid`, `url`, `serde` или иная), ADR должен
   отдельно обосновать maintenance, license, `cargo-deny` и отсутствие
   более узкого уже принятого решения.
4. Напиши `docs/m6-validation.md` с командами, feature matrix, coverage,
   URL/clone fixtures, leak/shutdown evidence и честным статусом внешнего CI.
5. По завершении создай `docs/reviews/M6-handoff.md`: реализовано,
   deviations, ADR, demo commands и exact commit/CI links. После handoff
   следующий M7 не начинать.

## 8. Definition of Done

Работа считается готовой только при выполнении всех условий:

1. Все `M6-*` rows имеют source/test traceability; нет hand-waved
   acceptance claims.
2. `cargo fmt --all -- --check` проходит.
3. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
   проходит.
4. `cargo test --workspace --all-features` проходит, включая M6 tests.
5. `cargo test --workspace --no-default-features` и необходимая feature
   powerset проверяют URL/clone feature-off atomicity.
6. С установленными инструментами проходят rustdoc, coverage и
   `cargo deny check`; недоступность advisory DB фиксируется как BLOCKED,
   а не как PASS.
7. Нет `unsafe` и production `unwrap`/`expect`/`panic`; public items
   документированы, README crate roles актуальны.
8. CI проходит на Ubuntu и Windows. Windows не получает слабый identity или
   иной security fallback, который обходил бы M5 contract.
9. Handoff и validation документы обновлены, diff scoped только M6 и
   подготовлены к отдельному acceptance review.

## 9. Demo commands for handoff

Команды запускаются из свежего clone на ветке заказа после установки Rust
toolchain. Exact test names должны быть заменены на фактически добавленные
тесты, если agent выбрал другую раскладку:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --workspace --no-default-features
cargo test --package boa_fapi --test m6_blob_url -- --nocapture
cargo test --package boa_fapi --test m6_structured_clone -- --nocapture
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo llvm-cov --workspace --all-features --fail-under-lines 80
cargo deny check
```

Не объявляй M6 принятым по локальному статусу без traceability, handoff и
зелёного CI на том же implementation commit.
