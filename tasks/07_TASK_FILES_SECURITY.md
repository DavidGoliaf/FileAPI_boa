# Заказ M5-FS — filesystem-backed `File`, capability policy и snapshot security

| Поле | Значение |
|---|---|
| ID | `M5-FS-FILE-SECURITY` |
| База | Принятый M4-B final commit `f5404de6a105c52dc128e686e18d92b376603bd4` |
| Ветка | `task/m5`, от `main` через принятую базу; не работать напрямую в `main` |
| Нормативная база | `TZ_boa_fapi_FileAPI.md` §2.3–§2.4, §3.1–§3.3, §4.1–§4.4, §6.3, §10–§16 |
| Зависимости | `ByteSource`, `BlobData`, `FileApiLimits`, M4-A `FileReader`, M4-B `FileReaderSync`, текущая atomic registration модель |
| Результат | Capability-based filesystem `File`, snapshot validation, безопасные host limits и lifecycle shutdown |

## 1. Цель

Добавить следующий нормативный слой после memory-only M4-B: host может явно
передать JavaScript `File` доступ к заранее авторизованному read-only
ресурсу. JS получает только содержимое и display name. Произвольный путь,
корень файловой системы, directory enumeration и механизм «открыть путь из
JS» в API не появляются.

Файловый источник должен проверять, что читается тот же ресурс, для которого
хост выдал capability. Проверка выполняется до первого чтения и перед каждой
новой range/chunk операцией. Изменение, исчезновение, truncate, потеря
доступа, lock или short read дают типизированную ошибку и единый
`NotReadableError` на JavaScript-границе, без partial result.

Заказ также добавляет минимальный lifecycle shutdown зарегистрированного File
API runtime: pending filesystem reads прекращаются, новые операции после
shutdown запрещены, host handles освобождаются, а callbacks/jobs не приходят
после уничтожения context.

## 2. Жёсткие границы

1. Не менять `TZ_boa_fapi_FileAPI.md`, принятые M1–M4-B acceptance rows и
   наблюдаемую memory-backed семантику `Blob`, `File`, `FileReader` и
   `FileReaderSync`.
2. Не добавлять `unsafe`, production `unwrap`/`expect`/`panic`, raw path API
   для JS, directory enumeration, background Tokio/runtime или JS-вызовы из
   source completion.
3. Не раскрывать абсолютный путь, canonical path, mount point, handle value,
   secret resource name или platform identity в JS, DOMException message,
   tracing output, blob URL или test artifact.
4. Не добавлять Blob URL, structured clone, полноценный Workers runtime,
   DOM/HTML, WPT harness или новые worker environments. Blob URL остаётся
   M6 scope.
5. Любая новая dependency требует ADR в `docs/DECISIONS.md` до применения:
   назначение, поддержка, лицензия и причина, почему нельзя использовать
   существующий Rust API. Предпочтительно обойтись стандартной библиотекой
   и уже принятыми crates.
6. Сохранить feature-поведение: все поддерживаемые комбинации features
   компилируются, а отключение `fs` не создаёт частично зарегистрированный
   global или доступный обход policy.
7. Один заказ — одна логическая change set. После handoff не начинать M6 и
   не принимать этот заказ самому.

## 3. Capability и host API

### 3.1 Предавторизованный ресурс

Ввести Boa-free host abstraction для уже открытого read-only ресурса. Имя и
точная форму trait/type можно выбрать при реализации, но публичный контракт
должен обеспечивать следующее:

- capability содержит только host-owned resource/handle, достаточный для
  безопасного чтения и получения metadata;
- capability не превращается в `PathBuf`, не сериализуется в JS и не имеет
  public getter, возвращающего путь;
- resource открывается хостом до создания JS `File`; произвольный путь,
  переданный JavaScript, не является допустимым входом;
- закрытие/clone capability не допускает чтение после shutdown;
- для Unix и Windows identity извлекается средствами safe Rust API и
  включает platform-appropriate file identity, размер и modification time,
  насколько их предоставляет платформа;
- если платформа не может гарантировать безопасную identity-проверку на
  открытом handle, реализация обязана выбрать `copy_on_import` либо отказать
  в импорте, а не переходить на проверку строкового prefix пути.

### 3.2 `FileAccessPolicy`

Добавить host-side policy boundary, совместимую с целевым API TZ:

```rust
pub trait FileAccessPolicy: Send + Sync {
    fn authorize_open(
        &self,
        request: &FileOpenRequest,
    ) -> Result<FileGrant, FileApiError>;

    fn authorize_read(
        &self,
        grant: &FileGrant,
        snapshot: &SnapshotState,
    ) -> Result<(), FileApiError>;
}
```

Допускается адаптировать названия под текущие crates, но решение и mapping
должны быть записаны в ADR. `FileGrant` — opaque capability, а не путь.

Требования к policy:

- default policy не разрешает импорт по raw path;
- authorization выполняется до создания JS `File`;
- `authorize_read` вызывается до первого чтения и перед каждой новой
  range/chunk операцией;
- optional root policy канонизирует уже открытый handle или проверяет
  platform identity; строковая проверка `starts_with(root)` запрещена;
- `..`, symlink escape, Windows junction/reparse-point escape и path aliasing
  не должны обходить policy;
- ошибка policy не раскрывает причину, абсолютный путь или identity в JS.

### 3.3 Host-facing `File` creation

Под feature `fs` добавить host method, эквивалентный целевой форме TZ:

```rust
#[cfg(feature = "fs")]
pub fn file_from_resource(
    &self,
    resource: &dyn FileResource,
    display_name: &str,
    options: HostFileOptions,
    context: &mut Context,
) -> JsResult<JsObject>;
```

Допустима небольшая адаптация к текущему `FileApiHandle`, который уже
возвращается из `FileApiExtension::register`, но она должна быть backward
compatible с `file_from_bytes` и зафиксирована ADR. Обязательные свойства:

- `display_name` — единственное имя, видимое JS; basename из secret host path
  никогда не вычисляется;
- name normalization совпадает с текущим M1/M2 `File` behavior;
- resource проверяется до создания native object и не оставляет partial JS
  state при отказе;
- host-created File может использовать существующие `FileReader`,
  `FileReaderSync`, Blob materialization и stream paths без расхождения
  error mapping;
- без feature `fs` method и filesystem types недоступны, но memory API и
  registration продолжают работать;
- `file_list` не превращается в directory listing и принимает только
  explicit File objects.

## 4. `SnapshotState` и `FileSource`

### 4.1 Snapshot model

Расширить core snapshot model non-exhaustively, не включая path:

- `Memory` остаётся всегда валидным и не меняет M1–M4 behavior;
- filesystem snapshot содержит opaque/platform-neutral identity, size и
  modification marker; абсолютный путь и открытый handle в snapshot не
  записываются;
- comparison должен обнаруживать замену файла другим объектом, а не только
  изменение `mtime`/size;
- snapshot нельзя подделать из JS и нельзя использовать для чтения после
  shutdown.

Если metadata платформы недостаточно для строгого обнаружения replacement,
реализовать безопасный handle identity или `copy_on_import`; нельзя объявлять
проверку по одному `mtime + size` полноценной защитой. Ограничение и
выбранный fallback занести в ADR.

### 4.2 `FileSource`

В `boa_fapi_fs` реализовать filesystem-backed `ByteSource` поверх
предавторизованного read-only resource:

- `len`, `is_empty` и `snapshot` не выполняют JS-код и не меняют global state;
- `read_range` проверяет cancellation до I/O и snapshot/policy до чтения;
- range проверяется checked arithmetic до системного вызова;
- чтение ограничено текущими `FileApiLimits` (`max_blob_size`,
  `max_materialize_bytes`, `max_sync_read_bytes`, chunk/read quotas), без
  обхода лимитов через filesystem path;
- short read, unexpected EOF, changed identity/size, disappearance,
  permission failure и lock переводятся в соответствующие core errors
  (`SnapshotChanged`, `NotFound`, `FileLocked`, `PermissionDenied`,
  `InvalidRange` или `ResourceLimit`), после чего JS получает
  `NotReadableError`;
- ответ строго соответствует requested range: extra bytes запрещены, partial
  bytes наружу не возвращаются;
- source не удерживает глобальную блокировку на время медленного I/O и не
  вызывает Boa/context из worker completion;
- текущий `MemorySource` остаётся неизменным по behavior и public API.

Проверка выполняется как минимум перед первым chunk и на каждой границе
следующего chunk. Если реализация читает одну range целиком, она всё равно
должна проверять identity до read и проверять размер/short read после read;
документировать, почему внутри системного read нет более мелкой boundary.

## 5. Lifecycle и shutdown

Добавить host-controlled shutdown для зарегистрированного File API runtime в
текущей архитектуре. Поскольку текущий `register` возвращает
`FileApiHandle`, допустима реализация `FileApiHandle::shutdown(&mut Context)`;
если вводится target-shaped `FileApiExtension::shutdown`, она должна иметь
эквивалентные гарантии. Выбранную форму и error type зафиксировать ADR.

Shutdown обязан быть:

- idempotent: повторный shutdown не вызывает panic и не запускает callbacks;
- атомарным относительно новых host-created File/resource operations;
- запрещающим новые reads, materialize, stream pulls и FileReader jobs после
  перехода в closed state;
- прекращающим pending filesystem work через существующий cancellation
  protocol, без принудительного abort unsafe-кодом;
- освобождающим file handles/capabilities и не оставляющим path/identity в
  очереди, error или JS object;
- защищённым от late completion: после shutdown нет Boa job, Promise
  resolution/rejection, stream callback или FileReader event, обращённого к
  уничтоженному context;
- совместимым с уже принятым fail-fast/rollback registration contract.

Shutdown Blob URL store, structured clone lifetime и полный runtime ownership
не входят в этот заказ; для них оставить отдельные extension points без
реализации.

## 6. Error, limits и JS semantics

1. Все public filesystem errors проходят единый mapping через существующий
   `js_from_core`/DOMException boundary. Нельзя добавлять отдельный mapping в
   `FileReader`, stream и sync reader.
2. `NotReadableError` не содержит path, OS error string с path, handle number,
   canonical location или policy internals.
3. Preflight limit failure выполняется до открытия JS-visible object/read и до
   source I/O; `==` boundary сохраняет текущий accepted behavior, `+1`
   отклоняется документированным лимитом.
4. Filesystem `Blob`/`File` использует те же checked/fallible materialization,
   Data URL и text decoding paths, что и memory source. Нельзя отдавать
   partial ArrayBuffer/string/Data URL после source failure.
5. Existing M4-A async concurrency quota и M4-B sync quota сохраняются;
   filesystem source не получает скрытого обхода через отдельную executor
   ветку.
6. `File` length/lastModified/type/name должны быть стабильны для принятого
   snapshot; subsequent source change не мутирует уже опубликованные JS
   metadata и приводит к read failure.

## 7. Required tests and traceability

Добавить unit/integration tests без новой production dependency. Для
временных файлов использовать безопасный test helper с уникальным именем,
cleanup и отсутствием записи абсолютных путей в assertions/logs. Если для
portable temporary resources всё же нужна новая dev dependency, добавить ADR.

Минимальный набор:

1. `boa_fapi_fs` constructs a source from an already authorized read-only
   resource; empty file, exact ranges, boundary ranges, cancellation, short
   read and oversized range.
2. Snapshot identity: unchanged read succeeds; truncate, replacement,
   delete, rename, metadata change and changed file identity are detected
   before a new chunk; no old bytes leak after failure.
3. Unix/Windows capability tests are explicitly `cfg`-scoped: permissions,
   sharing/lock behavior, symlink escape, Windows junction/reparse-point
   escape and platform identity limitations. Unsupported OS behavior must be
   labelled, not reported as PASS by skipping silently.
4. Policy tests prove default raw-path import denial, host-approved resource
   acceptance, no directory enumeration and no path/identity disclosure in
   JS errors or tracing.
5. Host integration with real JavaScript: `file_from_resource` creates a
   `File`, reads it through async `FileReader`, worker `FileReaderSync` where
   configured, Blob materialization and stream paths; all mappings remain
   same-realm and no partial result is observable.
6. Limit tests cover `max_blob_size`, `max_materialize_bytes`,
   `max_sync_read_bytes`, chunk boundaries and read/concurrency quotas for
   host-backed sources.
7. Registration/feature matrix: `fs` on/off, `dom-shim` on/off,
   `streams-shim` on/off and existing feature powerset; no partial globals or
   filesystem surface when preflight fails.
8. Shutdown tests cover pending FileReader/stream/materialize operations,
   cancellation, late completion, new-operation rejection, handle release and
   repeated shutdown. Assert no callbacks/jobs after shutdown.
9. Regression tests rerun all accepted M2/M3/M4-A/M4-B integration suites and
   prove memory-backed behavior is unchanged.

Добавить в `docs/spec-matrix.md` rows `M5-FS-01..M5-FS-10` (или больше при
необходимости), каждый с exact `file:symbol`, test name и command. Минимальная
трассировка:

| ID | Требование | Evidence |
|---|---|---|
| `M5-FS-01` | capability-only host import, no raw JS path | `boa_fapi_fs` policy tests + host integration |
| `M5-FS-02` | opaque cross-platform resource identity | `FileSource` unit tests, Unix/Windows cfg tests |
| `M5-FS-03` | snapshot validation before first/new chunk | replacement/truncate/delete/rename tests |
| `M5-FS-04` | permissions/lock/short-read mapping | platform filesystem tests + DOMException assertions |
| `M5-FS-05` | no path/identity disclosure | JS error/tracing negative tests |
| `M5-FS-06` | limits and no partial result | host FileReader/stream/sync tests |
| `M5-FS-07` | feature-safe atomic registration | feature matrix and `cargo hack` evidence |
| `M5-FS-08` | shutdown cancellation and no late callbacks | lifecycle integration tests |
| `M5-FS-09` | memory-backed M2–M4 regression | existing integration suites |
| `M5-FS-10` | CI parity and reproducible handoff | Linux/Windows CI artifacts and handoff |

## 8. Documentation, ADR, CI и handoff

Обновить только документацию, которая становится истинной после реализации:

- `README.md`: M5 filesystem capability, explicit host resource, snapshot
  checks, shutdown и оставшиеся M6+ omissions;
- `docs/architecture.md`: `boa_fapi_fs`, resource/policy boundary,
  snapshot/lifecycle flow и отсутствие JS path API;
- `docs/security.md`: threat model для path traversal, symlink/junction/
  reparse escape, replacement race, metadata disclosure, lock/permission
  failures и shutdown race;
- `docs/host-integration.md`: how to open/authorize a resource, pass an
  explicit display name, configure limits and invoke shutdown; никаких
  examples с secret absolute path в output;
- `docs/DECISIONS.md`: ADR для resource/capability representation, snapshot
  identity/fallback и lifecycle API; отдельный ADR на каждую новую dependency;
- `docs/m5-validation.md`, `docs/m5-final-audit.md` и
  `docs/reviews/M5-handoff.md`: exact commands/results, CI links, coverage,
  trace rows, deviations and retrospective bug-find findings;
- `.github/workflows/ci.yml`: `boa_fapi_fs` tests и M5 host integration на
  Ubuntu и Windows, с сохранением existing M2–M4 jobs и deny-fetch semantics.

Не переписывать M4 validation как будто M5 был частью прошлой приемки.
Различать local `cargo deny` result и CI result: network-blocked advisory DB
локально — `BLOCKED`, не `PASS`; зелёный CI не скрывает local blocker.

## 9. Required validation

Запустить из чистого checkout/worktree на финальной M5 ветке:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi_fs --all-features -- --nocapture
cargo test --package boa_fapi --test m2_blob_file_filelist -- --nocapture
cargo test --package boa_fapi --test m3_promise_blob_reads -- --nocapture
cargo test --package boa_fapi --test m3_blob_streams -- --nocapture
cargo test --package boa_fapi --test m4_filereader_async -- --nocapture
cargo test --package boa_fapi --test m4_filereader_sync -- --nocapture
cargo test --package boa_fapi --test m5_file_fs -- --nocapture
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo test --package boa_fapi --doc
cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85
cargo hack check --feature-powerset --depth 2
cargo deny check
git diff --check
```

Дополнительно проверить clean `git status`, отсутствие path/secret leaks в
artifacts и что CI действительно запущен на SHA финального code commit, а не
на старом rerun. `cargo deny check` документировать с фактическим exit code.
Coverage `boa_fapi` не ниже 85%; M4 regression tests должны остаться зелёными.

## 10. Completion и stop condition

Перед commit выполнить обязательный retrospective bug-find из `AGENTS.md`:

- scope boundaries и отсутствие M6 surface;
- `unsafe`/panic/unwrap/expect и fallible allocation/I/O;
- capability lifetime, snapshot replacement race и short-read handling;
- symlink/junction/reparse policy и отсутствие string-prefix security;
- path/identity leakage в messages, logs, artifacts и JS metadata;
- atomic registration, feature guards, rollback и shutdown idempotency;
- cancellation/late completion и отсутствие callbacks после context shutdown;
- exact limits, no partial result, platform-specific test truthfulness;
- traceability completeness и соответствие документов фактическим CI/local
  результатам.

Исправить все findings и повторить затронутую валидацию до отсутствия
unresolved item. Создать один imperative commit на `task/m5`, subject не
длиннее 72 символов, затем написать `docs/reviews/M5-handoff.md` с base/final
commit, implemented/omitted surface, commands, trace/ADR links, coverage,
audit findings и честным CI status.

После handoff остановиться. Не начинать Blob URL, structured clone, WPT или
полный Workers runtime и не изменять этот заказ в ходе его выполнения.
