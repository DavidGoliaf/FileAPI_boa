# Заказ M2 — Boa bindings: `Blob`, `File`, `FileList`

| Поле | Значение |
|---|---|
| ID | `M2-BOA-BINDINGS` |
| Исполнитель | кодовый агент; архитектура и условия приёмки ниже фиксированы |
| Основание | `TZ_boa_fapi_FileAPI.md`: §2.1–2.4, §3.2–3.4, §4.1–4.2, §5.1–5.4, §6.1–6.3, §12.1, §14–15 |
| Предусловие | M1 принят; `boa_fapi_core` остаётся engine-independent |

## 1. Цель

Реализуй в `boa_fapi` JavaScript bindings для Boa 0.22.x:

* атомарную регистрацию `Blob`, `File`, `FileList` в реальном `boa_engine::Context`;
* constructor/getters/`Blob.prototype.slice`, наследование `File` от `Blob`;
* internal brands, корректные prototypes, descriptors, `Symbol.toStringTag` и GC-safe native data;
* memory-only host constructors и JS integration tests.

Тесты обязаны исполнять JS (`new Blob`, `new File`, `instanceof`, borrowed getters, `slice`, `FileList.item`) в настоящем Boa Context. Rust-only unit tests не доказывают выполнение M2.

## 2. Неизменяемые условия

1. Используй только закреплённый WD и M2 scope. Не меняй ТЗ, этот заказ, пороги, команды, matrix или criteria.
2. `boa_fapi_core` не получает Boa/JS/DOM/fs/URL/async runtime dependencies и не хранит `Context`, `JsValue`, `JsObject`.
3. Boa dependencies допускаются только в `boa_fapi`. Разрешены `boa_engine` и при необходимости `boa_gc` 0.22.x; каждая новая dependency требует ADR в `docs/DECISIONS.md` (назначение, поддержка, лицензия).
4. Собственный production-код safe Rust: `#![deny(unsafe_code)]`; на JS/host-достижимых путях запрещены `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`, проглатывание ошибок и blanket `#[allow]`.
5. Brand нельзя определять по `constructor.name`, `instanceof`, duck typing или изменяемому public property. Только неподлежащее подделке native/internal brand data.
6. Все JS number/index conversions проверяются до `usize`/Rust integer; нельзя маскировать переполнение оператором `as`.
7. Тесты и критерии нельзя ослаблять, удалять, `#[ignore]`-ить, feature-disable-ить, заменять mock-ом или исключать из coverage. Нельзя заявлять PASS для незапущенной команды.

## 3. Граница M2

### Входит

* Blob/File/FileList + Web IDL conversion, brands, GC ownership, host constructors;
* Blob parts: USVString, BufferSource, Blob/File;
* injectable Clock для `File.lastModified`;
* `README.md`, `docs/architecture.md`, `docs/spec-matrix.md`, `docs/DECISIONS.md`, `docs/m2-validation.md`, `docs/m2-final-audit.md`.

### Не входит

Не реализовывай и не регистрируй: `Blob.text`, `arrayBuffer`, `bytes`, `stream`, `textStream`; FileReader/FileReaderSync; EventTarget/DOMException; Promise/job queue; filesystem/path/snapshot; URL/blob URL; structured clone/IDB; WPT runner; worker execution; network/async IO.

`new Blob(["C:\\secret.txt"])` означает bytes строки, никогда не доступ к пути. Любой необходимый выход за scope — запись в `QUESTIONS.md` и остановка.

## 4. Фиксированная архитектура

### 4.1. Crates

1. В workspace добавь `boa_engine` и `boa_gc` 0.22.x с минимальными features; фиксируй в lockfile.
2. `boa_fapi` зависит от Boa и локального `boa_fapi_core`; core остаётся Boa-free.
3. В `boa_fapi/src` создай минимум:

```text
lib.rs        public re-exports, deny unsafe
extension.rs  FileApiExtension/builder/register/FileApiHandle/preflight
brand.rs      private require_blob/require_file/require_file_list
blob.rs       native Blob data, constructor/getters/slice
file.rs       File data, constructor/getters
file_list.rs  FileList data/item/indexed properties
webidl.rs     all M2 coercions and BlobPart/options conversion
clock.rs       Clock + default system implementation
error.rs       RegisterError and internal-to-JS errors
```

Equivalent file layout is allowed only if these responsibilities remain isolated.

### 4.2. Native objects and brands

* Blob native state owns `Arc<BlobData>`; File owns the same immutable bytes plus immutable `name` and `last_modified`.
* File passes Blob brand and has `File.prototype -> Blob.prototype`.
* Native DTOs contain no `JsObject`, `JsValue`, `Context`, callbacks or globals; only Rust immutable data/Arc/string/numbers.
* If Boa needs `Trace`/`Finalize`, implement it without unsafe and without fabricated JS references.
* `require_blob`, `require_file`, `require_file_list` are the only brand gates. Wrong `this`/foreign/copied/forged object yields synchronous `TypeError`.
* Do not expose native data, raw segments, mutable bytes, brand key, Arc pointer or test accessor in a public Rust/JS API.

### 4.3. Fixed public Rust API

Implement the following (equivalent private fields and additional error types allowed):

```rust
pub struct FileApiExtension { /* private */ }
pub struct FileApiExtensionBuilder { /* private */ }
#[derive(Clone)]
pub struct FileApiHandle { /* opaque */ }

pub trait Clock: Send + Sync + 'static {
    fn now_unix_millis(&self) -> i64;
}

pub struct HostFileOptions {
    pub media_type: String,
    pub last_modified: Option<i64>,
}

impl FileApiExtension {
    pub fn builder() -> FileApiExtensionBuilder;
    pub fn register(&self, context: &mut boa_engine::Context)
        -> Result<FileApiHandle, RegisterError>;
}

impl FileApiHandle {
    pub fn blob_from_bytes(
        &self, bytes: impl Into<bytes::Bytes>, media_type: &str,
        context: &mut boa_engine::Context,
    ) -> boa_engine::JsResult<boa_engine::object::JsObject>;
    pub fn file_from_bytes(
        &self, bytes: impl Into<bytes::Bytes>, name: &str,
        options: HostFileOptions, context: &mut boa_engine::Context,
    ) -> boa_engine::JsResult<boa_engine::object::JsObject>;
    pub fn file_list(
        &self, files: impl IntoIterator<Item = boa_engine::object::JsObject>,
        context: &mut boa_engine::Context,
    ) -> boa_engine::JsResult<boa_engine::object::JsObject>;
}
```

Builder accepts a Clock; default is system clock. Tests use a deterministic fake clock. Do not add M5 config/executor/filesystem fields.

### 4.4. Registration contract

Before changing `globalThis`, create constructor/prototype objects and preflight all own names `Blob`, `File`, `FileList`, global extensibility and Boa requirements. Any preflight/registration error returns `RegisterError` and leaves **none** of the three globals installed. Never overwrite a host global.

Choose and document one re-registration rule: (a) same extension/context returns a usable equivalent handle and makes no changes, while a different config returns `AlreadyRegistered`; or (b) every second call returns `AlreadyRegistered`. Test the exact chosen rule.

Constructors have correct `name`, `length`, descriptors and prototype links. `FileList` has no public constructor/global constructor function. `file_list` validates every element as a File brand before creating any output.

## 5. Required JS semantics

### 5.1. One Web IDL conversion layer

All constructor/method conversion lives in `webidl.rs`; no duplicated coercion in binding modules.

* DOMString preserves JS code units as far as Boa represents them; USVString replaces every lone surrogate with U+FFFD.
* `[Clamp] long long` used by `Blob.slice`: ToNumber; NaN -> 0; infinities -> matching bound; fractional values use Web IDL Clamp rounding (nearest, ties-to-even); then clamp into `i64`. BigInt, Symbol and throwing conversion yield TypeError.
* Implement separate exact Web IDL converters for ordinary `long long` (`lastModified`) and `unsigned long` (FileList `item` index). Never reuse the clamp converter or a Rust cast.
* Converter tests cover `undefined`, `null`, numeric/string values, NaN, infinities, fractions, large magnitude, BigInt, Symbol and throwing coercion where applicable.
* Wrong `this` on every getter/method is TypeError.

### 5.2. Blob

Implement:

```js
new Blob(blobParts?, options?)
blob.size
blob.type
blob.slice(start?, end?, contentType?)
```

`blobParts` absent/undefined = empty sequence. Process ordinary Array parts left-to-right. Support:

1. USVString: UTF-8 encode; `endings: "native"` first calls M1 line-ending conversion with explicit platform target; `transparent` preserves endings.
2. BufferSource: ArrayBuffer, DataView and every Boa TypedArray. Copy exactly the visible view bytes at construction; later mutation cannot affect Blob.
3. Blob/File: append existing immutable sources without payload copy and ignore inner type.
4. Other part: synchronous TypeError and no observable partial Blob.

Options default to `{ type: "", endings: "transparent" }`. `type` is DOMString then M1 normalize. Only `transparent` and `native` are valid endings; any other converted value throws TypeError. Options getter exceptions propagate.

`size` is a precise JS Number within M1 limits, `type` readonly, and `Object.prototype.toString.call(blob)` is `[object Blob]`. `slice` uses M1 `BlobData::slice`; absent contentType is empty type; result is a new Blob; payload is not materialized/copied.

### 5.3. File

Implement:

```js
new File(fileBits, fileName, options?)
file.name
file.lastModified
```

File uses exact Blob parts/options algorithm. `fileName` is USVString then every `/` becomes `:`; never compute basename or accept host path. Absent/undefined `lastModified` calls injected Clock; supplied value uses ordinary Web IDL `long long`. File is both `instanceof File` and `instanceof Blob`; its `size`, `type`, `slice` follow Blob (slice returns Blob, not File); `Object.prototype.toString.call(file)` is `[object File]`. Metadata is readonly.

### 5.4. FileList

Only `FileApiHandle::file_list` creates it. It provides readonly `length`, `item(index)` (same File object or `null`) and indexed access (same File object or `undefined`). Indexed own properties are enumerable, readonly, non-replaceable and preserve host input order. `Object.prototype.toString.call(list)` is `[object FileList]`.

## 6. Mandatory tests

Create `crates/boa_fapi/tests/m2_blob_file_filelist.rs` (split only if coverage remains obvious). Every integration test creates a clean Context, registers M2 and executes JS.

Required scenarios:

1. Registration: globals, descriptors/name/length/prototypes/toStringTag; FileList not constructible; atomic conflict rollback; chosen repeat-registration contract.
2. Brands: `Object.create(Blob.prototype)`, copied public props, borrowed getters/methods, foreign this and forged constructor all TypeError; real File passes Blob brand.
3. Blob: empty/defaults, type normalization/readonly, invalid endings, throwing options getter, strings/native endings, ArrayBuffer/TypedArray/DataView visible ranges, post-construction mutation snapshot, nested Blob/File no-copy composition.
4. Slice: omitted/null/negative/reversed/empty, i64::MIN/MAX, NaN/infinities/fraction, invalid contentType, source immutable, File slice returns Blob, Arc sharing proven only from `#[cfg(test)]` child-module inspection.
5. File: lone-surrogate USVString replacement, slash replacement, deterministic clock default, supplied lastModified conversion, type/endings, readonly metadata, inheritance/toStringTag.
6. FileList: order/identity, item/index in/out range, property descriptors, assignment/delete/redefine rejection, rejected non-File host input.
7. Limits/errors: max parts/segments/blob size fail synchronously without partial object; hostile values never panic.
8. Unit tests for every converter and registration preflight/error path.
9. Guard tests: core remains Boa-free; `boa_fapi` public API exposes no Path/fs/mutable bytes/raw brand; production scan covers forbidden panic macros.

Tests requiring internal byte/Arc assertions may use a `#[cfg(test)]` child module only; no production test hooks.

## 7. Documentation and traceability

* Append M2 rows to `docs/spec-matrix.md`; keep M1 rows. Required IDs: `M2-REG-01..04`, `M2-WIDL-01..04`, `M2-BLOB-01..06`, `M2-FILE-01..05`, `M2-FLIST-01..04`, `M2-GC-01..02`. Each gives rule, `file:symbol`, real test and status.
* README/architecture state exactly that M2 supports Blob/File/FileList only; M3–M7 APIs remain absent.
* Create `docs/m2-validation.md` and `docs/m2-final-audit.md` with factual commands/results.
* Update `docs/DECISIONS.md` for dependencies, brand storage, registration rule and Clock injection.

## 8. Mandatory validation

Run after implementation and record exact exit codes:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --package boa_fapi --test m2_blob_file_filelist
$env:RUSTDOCFLAGS='-Dwarnings'; cargo doc --workspace --no-deps
cargo test --package boa_fapi --doc
cargo llvm-cov --package boa_fapi --all-features --fail-under-lines 85
cargo hack check --feature-powerset --depth 2
$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny fetch db
$env:CARGO_DENY_DB_PATH='target/cargo-deny-advisories'; cargo deny check
git diff --check
```

Network-blocked `cargo deny` is `BLOCKED`, not PASS, and M2 is not accepted until both commands later exit 0.

## 9. Acceptance

M2 is accepted only when all are true:

- core remains Boa-free and only `boa_fapi` owns M2 Boa bindings;
- registration is atomic and re-registration behavior is documented/tested;
- all specified JS members/brands/prototypes/descriptors work in real Boa Context;
- Blob composition is no-copy for Blob/File and snapshot-copy for BufferSource;
- conversions and limits are correct without cast overflow/panic;
- File and FileList meet sections 5.3–5.4;
- all JS integration, unit, guard and regression tests pass;
- matrix, ADR, docs and reports provide real evidence;
- every section-8 command exits 0 after the last production change;
- final audit is clean.

## 10. Mandatory final audit-pass

After the first green run, do a separate review:

1. For every M2 trace ID and every contract above, record actual `file:symbol`, normal/error/boundary tests and verdict in `docs/m2-final-audit.md`.
2. Independently audit dependency graph; core isolation; public APIs; native/GC ownership; conversion arithmetic; constructor rollback; brands; descriptors; exceptions; BufferSource copy/no-copy composition; docs claims.
3. Fix every defect, weak/missing test, false claim or scope violation; add regression test; restart this audit from step 1. Do not hide a finding with ignore/exclusion/lint reduction/feature disablement.
4. After the final fix, rerun the full section-8 list in order. Record exact command, exit code and PASS/FAIL/BLOCKED. Any BLOCKED/N-A/unverified result means M2 is not accepted.

## 11. Final response of executor

Report: changed files; implemented M2 contracts and JS tests; every validation exit code; audit findings/fixes; exact remaining blockers; confirmation that the TЗ/order/criteria were not changed. Do not begin M3, commit, publish or contact external services without a separate customer instruction.
