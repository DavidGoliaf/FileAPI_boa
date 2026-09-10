//! `FileApiExtension`, its builder, atomic registration and the host handle.
//!
//! Registration is atomic: constructor and prototype objects are built
//! first, every preflight runs before `globalThis` changes, and a failed
//! install rolls back so that none of the globals remains installed.
//! Re-registration is identity-aware: the same opaque identity is
//! idempotent, a different identity is rejected with
//! [`RegisterError::AlreadyRegistered`].

use std::sync::Arc;

use boa_engine::context::intrinsics::StandardConstructor;
use boa_engine::object::ConstructorBuilder;
use boa_engine::object::JsObject;
use boa_engine::property::{PropertyDescriptor, PropertyKey};
use boa_engine::{Context, JsData, JsResult, js_string};
use boa_fapi_core::blob::BlobData;
use boa_fapi_core::blob_url::{BlobUrlError, BlobUrlStore, EnvironmentDescriptor, EnvironmentKey};
use boa_fapi_core::clone::{CloneError, FileApiClonePayload};
use boa_fapi_core::limits::FileApiLimits;
use boa_gc::{Finalize, Trace};
use bytes::Bytes;

use crate::blob::{self, BlobNative};
use crate::brand;
use crate::clock::{Clock, SystemClock};
use crate::error::{RegisterError, js_from_core};
use crate::file;
use crate::file_list;

/// Shared ownership of a context-local Blob URL store.
///
/// Cloned into [`RegisteredSpecs`], every URL job payload and the
/// [`FileApiHandle`]: `clear()` at shutdown releases all strong payload
/// references at once, while already-handed-out `Arc<BlobData>` reads run
/// to completion.
pub(crate) type SharedUrlStore = Arc<BlobUrlStore>;

/// Source of 16 CSPRNG bytes per Blob URL UUID.
///
/// The production default draws from the operating system through
/// `getrandom` (documented in the crate ADR); tests inject a deterministic
/// sequence. Counters, timestamps and predictable PRNGs are forbidden as
/// implementations by contract. The all-zero block is reserved as the
/// failure sentinel: `insert_url` maps it to `EntropyUnavailable` without
/// leaking platform detail, so it never escapes as a UUID (a legitimate
/// all-zero draw, probability 2^-128, is safely retried as a failure).
pub trait UrlEntropySource: Send + Sync + 'static {
    /// Fills 16 bytes of cryptographic entropy for one UUID.
    fn fill_16(&self) -> [u8; 16];
}

/// OS entropy via `getrandom`: the production [`UrlEntropySource`].
///
/// A platform failure surfaces as [`BlobUrlError::EntropyUnavailable`]
/// (the same network-error equivalent in JS); no counter/timestamp/PRNG
/// fallback exists by design.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsEntropy;

impl UrlEntropySource for OsEntropy {
    fn fill_16(&self) -> [u8; 16] {
        let mut bytes = [0_u8; 16];
        if getrandom::fill(&mut bytes).is_ok() {
            bytes
        } else {
            // The caller maps the zero sentinel to `EntropyUnavailable`
            // without leaking platform detail; `getrandom` leaves the
            // buffer untouched on failure, and all-zero never escapes as
            // a UUID because the sentinel check runs first.
            [0_u8; 16]
        }
    }
}

/// Capability/version descriptor of one structured-clone bridge.
///
/// Carries the bridge name and the encoding version it speaks; the
/// bindings compare `version` against
/// [`CLONE_ENCODING_VERSION`](boa_fapi_core::clone::CLONE_ENCODING_VERSION)
/// before touching any global or payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneBridgeDescriptor {
    /// Human-readable bridge name (e.g. `"fake-idb-bridge"` in tests).
    pub name: String,
    /// The clone encoding version the bridge speaks.
    pub version: u32,
}

/// Host bridge connecting clone payloads to an external structured-clone
/// / IndexedDB runtime.
///
/// `boa_fapi` never depends on `boa-idb`: this trait is the only coupling,
/// implemented by the host (or by the fake bridge in tests). DTOs carry
/// materialized bytes and public metadata only — never `Context`,
/// `JsObject`, paths, capabilities, OS handles or snapshot identities —
/// so ownership stays GC-safe under the accepted M2–M4 patterns.
pub trait CloneAdapter: Send + Sync + 'static {
    /// Returns the capability/version descriptor of this bridge.
    fn descriptor(&self) -> CloneBridgeDescriptor;

    /// Encodes one live payload into bytes for external storage.
    fn encode(&self, payload: &FileApiClonePayload) -> Result<Vec<u8>, CloneError>;

    /// Decodes bytes previously produced by [`CloneAdapter::encode`].
    fn decode(&self, bytes: &[u8]) -> Result<FileApiClonePayload, CloneError>;
}

/// Opaque registration identity for one built [`FileApiExtension`].
///
/// Created once per `build()` and preserved by `Clone`: a clone of an
/// extension carries the same identity, while a separately built
/// extension — even with identical visible configuration — carries a
/// different one. The value is never exposed publicly, never derived
/// from configuration contents, and never compared by value: the
/// registration compares only opaque identity tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RegistrationIdentity(pub(crate) u64);

impl RegistrationIdentity {
    /// Mints a fresh identity token.
    ///
    /// CAS-guarded and saturating: issues `1..u64::MAX-1` exactly once
    /// each and never wraps. On u64 exhaustion (practically unreachable:
    /// 2^64 built extensions) returns the `u64::MAX` sentinel instead of
    /// `0` or a reused live value; `register` rejects the sentinel with
    /// `IoIdsExhausted` before any mutation, so exhausted identities can
    /// never alias two contexts.
    fn fresh() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        loop {
            let candidate = NEXT.load(Ordering::Relaxed);
            if candidate == 0 || candidate == u64::MAX {
                NEXT.store(u64::MAX, Ordering::Relaxed);
                return Self(u64::MAX);
            }
            match NEXT.compare_exchange_weak(
                candidate,
                candidate.wrapping_add(1),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Self(candidate),
                Err(_) => continue,
            }
        }
    }
}

/// Immutable extension configuration.
#[derive(Clone)]
pub(crate) struct ExtensionConfig {
    /// Opaque registration identity: assigned once per built extension
    /// and preserved by `Clone`. Two separately built extensions never
    /// share an identity, even with visually identical fields; identity
    /// is never exposed publicly and never compared by value.
    pub(crate) identity: RegistrationIdentity,
    /// Clock for `File.lastModified` defaults.
    pub(crate) clock: Arc<dyn Clock>,
    /// M1 resource limits for blob construction and slicing.
    pub(crate) limits: FileApiLimits,
    /// Whether the M3-B streams shim is registered.
    pub(crate) streams_shim: bool,
    /// Whether the M4-A DOM shim (`EventTarget`, `Event`, `ProgressEvent`,
    /// `DOMException`, `FileReader`) is registered.
    pub(crate) dom_shim: bool,
    /// Whether the M6 URL shim (`URL.createObjectURL/revokeObjectURL`) is
    /// registered.
    pub(crate) url_shim: bool,
    /// Whether the M6 structured-clone bridge is enabled.
    pub(crate) structured_clone: bool,
    /// The host-controlled environment descriptor. Only worker descriptors
    /// install `FileReaderSync`; the service-worker kind forbids Blob URL
    /// creation.
    pub(crate) environment: FileApiEnvironment,
    /// Serialized origin embedded in `blob:` URLs of this context.
    pub(crate) origin: String,
    /// Opaque storage-partition identity for same-partition checks.
    pub(crate) partition: u64,
    /// Per-global nonce so opaque origins never share a key.
    pub(crate) nonce: u64,
    /// CSPRNG entropy for Blob URL UUIDs.
    pub(crate) entropy: Arc<dyn UrlEntropySource>,
    /// Optional host structured-clone bridge.
    pub(crate) clone_adapter: Option<Arc<dyn CloneAdapter>>,
    /// Optional host file I/O executor (`None` selects the built-in
    /// bounded pool at registration).
    pub(crate) io_executor: Option<Arc<dyn crate::io::FileIoExecutor>>,
    /// Optional host wake hook (`None` selects `NoopWake`).
    pub(crate) io_wake: Option<Arc<dyn crate::io::FileIoWake>>,
}

/// The host-controlled environment descriptor selecting which globals the
/// registration installs.
///
/// The descriptor is an explicit host choice: it is never derived from the
/// thread ID, the `Context` type, or the presence of a host callback. The
/// default is [`FileApiEnvironment::Window`], so existing M4-A users get no
/// new global without changing their configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileApiEnvironment {
    /// A window-like environment: no `FileReaderSync` global is installed.
    #[default]
    Window,
    /// A dedicated worker: the normative `FileReaderSync` is installed.
    DedicatedWorker,
    /// A shared worker: the normative `FileReaderSync` is installed.
    SharedWorker,
    /// A service worker: no `FileReaderSync` global is installed (the
    /// service-worker capability is explicitly forbidden).
    ServiceWorker,
}

impl FileApiEnvironment {
    /// Returns `true` for the two worker descriptors that install the
    /// normative `FileReaderSync`.
    ///
    /// Only meaningful with the `dom-shim` feature: without it no sync
    /// surface exists to gate.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn file_reader_sync_enabled(&self) -> bool {
        matches!(self, Self::DedicatedWorker | Self::SharedWorker)
    }

    /// Maps this kind onto the core [`EnvironmentKind`](boa_fapi_core::blob_url::EnvironmentKind).
    pub(crate) fn core_kind(&self) -> boa_fapi_core::blob_url::EnvironmentKind {
        use boa_fapi_core::blob_url::EnvironmentKind as Core;
        match self {
            Self::Window => Core::Window,
            Self::DedicatedWorker => Core::DedicatedWorker,
            Self::SharedWorker => Core::SharedWorker,
            Self::ServiceWorker => Core::ServiceWorker,
        }
    }
}

/// The registered classes and configuration of a context.
#[derive(Clone)]
pub(crate) struct RegisteredSpecs {
    /// Blob constructor and prototype.
    pub(crate) blob: StandardConstructor,
    /// File constructor and prototype.
    pub(crate) file: StandardConstructor,
    /// FileList prototype (no public constructor exists).
    pub(crate) file_list_proto: JsObject,
    /// Streams shim constructors/prototypes (present when enabled).
    #[cfg(feature = "streams-shim")]
    pub(crate) streams: Option<crate::streams::StreamSpecs>,
    /// DOM shim constructors/prototypes (present when enabled).
    #[cfg(feature = "dom-shim")]
    pub(crate) dom: Option<crate::dom::DomSpecs>,
    /// FileReader constructor/prototype (present when the DOM shim is on).
    #[cfg(feature = "dom-shim")]
    pub(crate) filereader: Option<crate::filereader::FileReaderSpecs>,
    /// `FileReaderSync` constructor/prototype (present only for worker
    /// environment descriptors with the DOM shim on).
    #[cfg(feature = "dom-shim")]
    pub(crate) sync_reader: Option<crate::filereader_sync::FileReaderSyncSpecs>,
    /// URL namespace object (present when the URL shim is on; the
    /// service-worker environment still installs the namespace for a
    /// uniform error surface while forbidding creation). Retained for
    /// atomic install/rollback ownership: the live store is `url_store`.
    #[cfg(feature = "url-shim")]
    #[allow(dead_code)]
    pub(crate) url: Option<crate::url_shim::UrlSpecs>,
    /// Context-local Blob URL store (M6). Always present: the store exists
    /// even when the JS surface is off, so host `resolve_blob_url` keeps
    /// working and shutdown can clear it.
    pub(crate) url_store: SharedUrlStore,
    /// Shared shutdown flag. Cloned into the handle; filesystem-backed
    /// reads, URL creation and pending clone work observe the same closed
    /// state (no longer `fs`-gated: shutdown exists in every configuration).
    pub(crate) shutdown: crate::lifecycle::ShutdownFlag,
    /// Context-local file I/O bridge (M9-B): quota, completion queue and
    /// executor/wake handles for promise reads.
    pub(crate) io: std::sync::Arc<crate::io::IoBridge>,
    /// Immutable blob payloads of live FileReader operations (M9-C).
    ///
    /// Keyed by I/O operation id; inserted at `readAs*` time, removed at
    /// terminal settlement, abort, or shutdown drain. The worker chunk
    /// task clones the `Arc` (no copy, no Boa), and removal makes late
    /// worker chunks stale. Holds no JS values.
    #[cfg(feature = "dom-shim")]
    pub(crate) reader_payloads: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<u64, std::sync::Arc<boa_fapi_core::blob::BlobData>>,
        >,
    >,
    /// Immutable blob payloads of live stream operations (M9-D).
    ///
    /// Keyed by I/O operation id; inserted at `stream()`/`textStream()`
    /// time, removed at terminal settlement (EOF/error), at `cancel()`, or
    /// at shutdown drain. The worker chunk task clones the `Arc` (no copy,
    /// no Boa), and removal makes late worker chunks stale. Holds no JS
    /// values.
    #[cfg(feature = "streams-shim")]
    pub(crate) stream_payloads: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<u64, std::sync::Arc<boa_fapi_core::blob::BlobData>>,
        >,
    >,
    /// Fairness budget for FileReader chunk completions per `poll_io`
    /// (M9-C): `None` drains everything queued; `Some(n)` settles at most
    /// `n` reader chunks and re-queues the rest for the next host-loop
    /// turn with a fresh wake.
    #[cfg(feature = "dom-shim")]
    pub(crate) io_poll_budget: std::sync::Arc<std::sync::Mutex<Option<usize>>>,
    /// Opaque identity of this registration's I/O context.
    pub(crate) context_id: crate::io::FileApiContextId,
    /// Extension configuration.
    pub(crate) config: ExtensionConfig,
    /// Opaque identity stored at registration time: a repeat `register`
    /// with the same identity is idempotent, a different identity is
    /// rejected without mutation.
    pub(crate) identity: RegistrationIdentity,
}

impl RegisteredSpecs {
    /// Returns the Blob interface prototype.
    pub(crate) fn blob_proto(&self) -> JsObject {
        self.blob.prototype()
    }

    /// Returns the File interface prototype.
    pub(crate) fn file_proto(&self) -> JsObject {
        self.file.prototype()
    }

    /// Returns the configured resource limits.
    pub(crate) fn limits(&self) -> &FileApiLimits {
        &self.config.limits
    }

    /// Reads the current time from the injected clock.
    pub(crate) fn now_unix_millis(&self) -> i64 {
        self.config.clock.now_unix_millis()
    }

    /// Reads the current time from the injected clock as an event time stamp.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn clock_millis(&self) -> f64 {
        self.config.clock.now_unix_millis() as f64
    }

    /// Returns the `EventTarget` interface prototype.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn dom_event_target_proto(&self) -> JsObject {
        self.dom
            .as_ref()
            .map(|dom| dom.event_target.prototype())
            .unwrap_or_else(|| self.blob_proto())
    }

    /// Returns the `Event` interface prototype.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn dom_event_proto(&self) -> JsObject {
        self.dom
            .as_ref()
            .map(|dom| dom.event.prototype())
            .unwrap_or_else(|| self.blob_proto())
    }

    /// Returns the `ProgressEvent` interface prototype.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn dom_progress_event_proto(&self) -> JsObject {
        self.dom
            .as_ref()
            .map(|dom| dom.progress_event.prototype())
            .unwrap_or_else(|| self.blob_proto())
    }

    /// Returns the `DOMException` interface prototype.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn dom_exception_proto(&self) -> JsObject {
        self.dom
            .as_ref()
            .map(|dom| dom.dom_exception.prototype())
            .unwrap_or_else(|| self.blob_proto())
    }

    /// Returns the `FileReader` interface prototype.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn filereader_proto(&self) -> JsObject {
        self.filereader
            .as_ref()
            .map(|spec| spec.reader.prototype())
            .unwrap_or_else(|| self.blob_proto())
    }

    /// Returns the `FileReaderSync` interface prototype.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn sync_reader_proto(&self) -> JsObject {
        self.sync_reader
            .as_ref()
            .map(|spec| spec.sync.prototype())
            .unwrap_or_else(|| self.blob_proto())
    }

    /// Returns the cloned DOM specs when the shim is registered.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn dom_specs(&self) -> Option<crate::dom::DomSpecs> {
        self.dom.clone()
    }

    /// Returns the context-local Blob URL store.
    pub(crate) fn url_store(&self) -> SharedUrlStore {
        Arc::clone(&self.url_store)
    }

    /// Builds the core environment descriptor of this context.
    ///
    /// Fails only when the configured origin is not a valid serialized
    /// origin; registration preflights this before touching `globalThis`,
    /// so this helper is infallible in every registered context.
    pub(crate) fn environment_descriptor(&self) -> Result<EnvironmentDescriptor, BlobUrlError> {
        EnvironmentDescriptor::new(
            self.config.environment.core_kind(),
            self.config.origin.clone(),
            self.config.partition,
            self.config.nonce,
        )
    }

    /// Returns the environment descriptor this context was registered with.
    pub(crate) fn environment(&self) -> FileApiEnvironment {
        self.config.environment
    }

    /// Returns the context-local I/O bridge.
    pub(crate) fn io_bridge(&self) -> std::sync::Arc<crate::io::IoBridge> {
        Arc::clone(&self.io)
    }

    /// Returns the `poll_io` reader-completion budget (`None` = unbounded).
    #[cfg(feature = "dom-shim")]
    pub(crate) fn io_poll_budget(&self) -> Option<usize> {
        self.io_poll_budget.lock().ok().and_then(|slot| *slot)
    }

    /// Returns the immutable payload of a live FileReader operation.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn reader_payload(
        &self,
        operation: u64,
    ) -> Option<std::sync::Arc<boa_fapi_core::blob::BlobData>> {
        self.reader_payloads
            .lock()
            .ok()
            .and_then(|map| map.get(&operation).cloned())
    }

    /// Stores the immutable payload of a new FileReader operation.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn store_reader_payload(
        &self,
        operation: u64,
        data: std::sync::Arc<boa_fapi_core::blob::BlobData>,
    ) {
        if let Ok(mut map) = self.reader_payloads.lock() {
            map.insert(operation, data);
        }
    }

    /// Drops the payload of a settled FileReader operation.
    #[cfg(feature = "dom-shim")]
    pub(crate) fn drop_reader_payload(&self, operation: u64) {
        if let Ok(mut map) = self.reader_payloads.lock() {
            map.remove(&operation);
        }
    }

    /// Returns the immutable payload of a live stream operation (M9-D).
    #[cfg(feature = "streams-shim")]
    pub(crate) fn stream_payload(
        &self,
        operation: u64,
    ) -> Option<std::sync::Arc<boa_fapi_core::blob::BlobData>> {
        self.stream_payloads
            .lock()
            .ok()
            .and_then(|map| map.get(&operation).cloned())
    }

    /// Stores the immutable payload of a new stream operation (M9-D).
    #[cfg(feature = "streams-shim")]
    pub(crate) fn store_stream_payload(
        &self,
        operation: u64,
        data: std::sync::Arc<boa_fapi_core::blob::BlobData>,
    ) {
        if let Ok(mut map) = self.stream_payloads.lock() {
            map.insert(operation, data);
        }
    }

    /// Drops the payload of a settled stream operation (M9-D).
    #[cfg(feature = "streams-shim")]
    pub(crate) fn drop_stream_payload(&self, operation: u64) {
        if let Ok(mut map) = self.stream_payloads.lock() {
            map.remove(&operation);
        }
    }

    /// Rebuilds the owning handle from a specs snapshot (test helper).
    ///
    /// The handle carries the same context id, bridge and shutdown flag,
    /// which is all `poll_io` needs. Production code keeps the real handle
    /// returned by `register`.
    #[cfg(test)]
    pub(crate) fn test_handle(&self) -> FileApiHandle {
        FileApiHandle {
            specs: self.clone(),
            shutdown: self.shutdown.clone(),
        }
    }
}

/// Clones the registration state out of the context so that callers never
/// hold borrows across further `Context` use.
pub(crate) fn snapshot(context: &Context) -> JsResult<RegisteredSpecs> {
    let Some(specs) = context.get_data::<RegisteredSpecs>() else {
        return Err(boa_engine::JsNativeError::typ()
            .with_message("the File API extension is not registered in this context")
            .into());
    };
    Ok(specs.clone())
}

/// Builder for a [`FileApiExtension`].
#[derive(Clone, Default)]
pub struct FileApiExtensionBuilder {
    clock: Option<Arc<dyn Clock>>,
    limits: Option<FileApiLimits>,
    streams_shim: Option<bool>,
    dom_shim: Option<bool>,
    url_shim: Option<bool>,
    structured_clone: Option<bool>,
    environment: Option<FileApiEnvironment>,
    origin: Option<String>,
    partition: Option<u64>,
    nonce: Option<u64>,
    entropy: Option<Arc<dyn UrlEntropySource>>,
    clone_adapter: Option<Arc<dyn CloneAdapter>>,
    io_executor: Option<Arc<dyn crate::io::FileIoExecutor>>,
    io_wake: Option<Arc<dyn crate::io::FileIoWake>>,
}

impl FileApiExtensionBuilder {
    /// Injects a [`Clock`] used for `File.lastModified` defaults.
    ///
    /// Tests inject a deterministic fake; the default is the system clock.
    pub fn clock(&mut self, clock: Arc<dyn Clock>) -> &mut Self {
        self.clock = Some(clock);
        self
    }

    /// Overrides the resource limits applied to blob construction.
    ///
    /// Defaults to [`FileApiLimits::default`].
    pub fn limits(&mut self, limits: FileApiLimits) -> &mut Self {
        self.limits = Some(limits);
        self
    }

    /// Enables or disables the M3-B streams shim registration.
    ///
    /// Defaults to `true`. When `false` (or the `streams-shim` Cargo
    /// feature is off), `register` fails before touching `globalThis`
    /// because no host stream adapter is implemented in this milestone.
    pub fn streams_shim(&mut self, enabled: bool) -> &mut Self {
        self.streams_shim = Some(enabled);
        self
    }

    /// Enables or disables the M4-A DOM shim registration.
    ///
    /// Defaults to `true`. When `false` (or the `dom-shim` Cargo feature
    /// is off), `register` fails before touching `globalThis` because no
    /// host DOM adapter is implemented in this milestone.
    pub fn dom_shim(&mut self, enabled: bool) -> &mut Self {
        self.dom_shim = Some(enabled);
        self
    }

    /// Enables or disables the M6 URL shim registration.
    ///
    /// Defaults to `true`. When `false` (or the `url-shim` Cargo feature
    /// is off), no `URL` global is installed and `URL.createObjectURL` is
    /// unavailable from JS; host-side `create_blob_url`/`resolve_blob_url`
    /// keep working against the context-local store.
    pub fn url_shim(&mut self, enabled: bool) -> &mut Self {
        self.url_shim = Some(enabled);
        self
    }

    /// Enables or disables the M6 structured-clone bridge.
    ///
    /// Defaults to `true`. When `false` (or the `structured-clone` Cargo
    /// feature is off), clone globals/encode entry points stay absent and
    /// M1–M5 behavior is unchanged; the host encode/decode helpers keep
    /// working as pure Rust functions.
    pub fn structured_clone(&mut self, enabled: bool) -> &mut Self {
        self.structured_clone = Some(enabled);
        self
    }

    /// Selects the host-controlled environment descriptor.
    ///
    /// Defaults to [`FileApiEnvironment::Window`]. Only
    /// [`FileApiEnvironment::DedicatedWorker`] and
    /// [`FileApiEnvironment::SharedWorker`] install the normative
    /// `FileReaderSync`; `Window` and `ServiceWorker` install no such
    /// global (not even an `undefined` shim). The descriptor is an
    /// explicit host choice and is never inferred.
    pub fn environment(&mut self, environment: FileApiEnvironment) -> &mut Self {
        self.environment = Some(environment);
        self
    }

    /// Sets the serialized origin embedded in `blob:` URLs.
    ///
    /// Defaults to `"https://localhost"`. The value is validated as a
    /// serialized origin (non-empty, bounded, printable ASCII, no
    /// whitespace); invalid values fail `register` before any `globalThis`
    /// mutation. For opaque origins pass the fixed `"null"` origin here
    /// with a fresh `nonce` per global (see [`Self::nonce`]), so opaque
    /// globals never share a key.
    pub fn origin(&mut self, origin: impl Into<String>) -> &mut Self {
        self.origin = Some(origin.into());
        self
    }

    /// Sets the opaque storage-partition identity.
    ///
    /// Defaults to `0`. Same-partition checks require origin *and*
    /// partition *and* nonce to match; the value never appears in a URL.
    pub fn partition(&mut self, partition: u64) -> &mut Self {
        self.partition = Some(partition);
        self
    }

    /// Sets the per-global nonce (opaque-origin uniqueness).
    ///
    /// Defaults to `0`. Hosts creating several globals with the same
    /// origin/partition must pass a fresh nonce per global; URL
    /// unguessability itself comes from the UUID.
    pub fn nonce(&mut self, nonce: u64) -> &mut Self {
        self.nonce = Some(nonce);
        self
    }

    /// Injects the CSPRNG entropy source for Blob URL UUIDs.
    ///
    /// Defaults to [`OsEntropy`]. Tests inject a deterministic sequence;
    /// counters, timestamps and predictable PRNGs are forbidden by
    /// contract.
    pub fn entropy(&mut self, entropy: Arc<dyn UrlEntropySource>) -> &mut Self {
        self.entropy = Some(entropy);
        self
    }

    /// Registers the host structured-clone bridge for this context.
    ///
    /// The bridge is capability/version-checked before any `globalThis`
    /// mutation: a missing bridge (when the feature is on) leaves clone
    /// entry points absent without failing; an explicitly registered
    /// bridge with an incompatible version fails registration atomically.
    pub fn clone_adapter(&mut self, adapter: Arc<dyn CloneAdapter>) -> &mut Self {
        self.clone_adapter = Some(adapter);
        self
    }

    /// Injects the host file I/O executor for promise reads.
    ///
    /// Defaults to a built-in bounded thread pool (4 workers, 128 queued
    /// tasks). The executor must be `Send + Sync + 'static`, must never
    /// touch Boa, and must have a hard queue/worker bound: thread-per-read
    /// without a limit is forbidden by contract. Tests inject a controlled
    /// manual executor that records requests without running them.
    pub fn io_executor(&mut self, executor: Arc<dyn crate::io::FileIoExecutor>) -> &mut Self {
        self.io_executor = Some(executor);
        self
    }

    /// Injects the host wake hook signalled on I/O completion.
    ///
    /// Defaults to [`crate::io::NoopWake`]. The hook only signals the host
    /// event loop and never touches Boa. Correctness never depends on the
    /// wake: the host always drives `poll_io` explicitly.
    pub fn io_wake(&mut self, wake: Arc<dyn crate::io::FileIoWake>) -> &mut Self {
        self.io_wake = Some(wake);
        self
    }

    /// Creates the extension.
    #[must_use]
    pub fn build(&self) -> FileApiExtension {
        FileApiExtension {
            config: ExtensionConfig {
                identity: RegistrationIdentity::fresh(),
                clock: self.clock.clone().unwrap_or_else(|| Arc::new(SystemClock)),
                limits: self.limits.clone().unwrap_or_default(),
                streams_shim: self.streams_shim.unwrap_or(true),
                dom_shim: self.dom_shim.unwrap_or(true),
                url_shim: self.url_shim.unwrap_or(true),
                structured_clone: self.structured_clone.unwrap_or(true),
                environment: self.environment.unwrap_or_default(),
                origin: self
                    .origin
                    .clone()
                    .unwrap_or_else(|| String::from("https://localhost")),
                partition: self.partition.unwrap_or(0),
                nonce: self.nonce.unwrap_or(0),
                entropy: self.entropy.clone().unwrap_or_else(|| Arc::new(OsEntropy)),
                clone_adapter: self.clone_adapter.clone(),
                io_executor: self.io_executor.clone(),
                io_wake: self.io_wake.clone(),
            },
        }
    }
}

/// The File API extension for Boa contexts.
#[derive(Clone)]
pub struct FileApiExtension {
    config: ExtensionConfig,
}

impl FileApiExtension {
    /// Returns a new builder.
    #[must_use]
    pub fn builder() -> FileApiExtensionBuilder {
        FileApiExtensionBuilder::default()
    }

    /// Registers `Blob`, `File` and the internal `FileList` machinery.
    ///
    /// The registration is atomic: when any preflight or installation step
    /// fails, none of the globals is left installed. Identity-aware
    /// re-registration: a repeat `register` of the *same* identity (the
    /// same built extension or its clone) on the same context is
    /// idempotent — globals are not reinstalled and the returned handle
    /// points at the already-registered state. A *different* identity,
    /// even with a visually identical configuration, is rejected with
    /// [`RegisterError::AlreadyRegistered`] without mutation. A repeat
    /// after `shutdown` never revives the runtime: the same-identity call
    /// returns the existing handle (still shut down), a different identity
    /// is rejected. Different contexts stay independent (per-context
    /// `RegisteredSpecs`).
    pub fn register(&self, context: &mut Context) -> Result<FileApiHandle, RegisterError> {
        // Exhausted identity tokens (u64 wraparound guard) never alias two
        // contexts: fail before any `globalThis` mutation.
        if self.config.identity == RegistrationIdentity(u64::MAX) {
            return Err(RegisterError::IoIdsExhausted);
        }
        // Identity-aware re-registration: the stored identity decides.
        if let Some(existing) = context.get_data::<RegisteredSpecs>() {
            if existing.identity == self.config.identity {
                return Ok(FileApiHandle {
                    specs: existing.clone(),
                    shutdown: existing.shutdown.clone(),
                });
            }
            return Err(RegisterError::AlreadyRegistered);
        }

        // Streams shim availability is checked before any globalThis
        // mutation: without the shim there is no host stream adapter.
        // The flag and the feature must both agree: the flag opts out at
        // runtime, the feature opts out at compile time. The combined
        // condition keeps both live in every feature configuration.
        let shim_available = self.config.streams_shim && cfg!(feature = "streams-shim");
        if !shim_available {
            return Err(RegisterError::StreamsShimDisabled);
        }

        // DOM shim availability is checked before any globalThis mutation:
        // without the shim there is no host DOM adapter. Same flag/feature
        // combination as the streams shim.
        let dom_available = self.config.dom_shim && cfg!(feature = "dom-shim");
        if !dom_available {
            return Err(RegisterError::DomShimDisabled);
        }

        // URL shim availability is informational, not fatal: with the flag
        // or the feature off the `URL` global simply stays absent while
        // host-side store operations keep working. What *is* fatal is a
        // conflicting pre-existing `URL` name when the shim would install.
        // The binding keeps the flag live in every feature configuration
        // (no `cfg`-gated unused-variable warning).
        let url_shim_available = self.config.url_shim && cfg!(feature = "url-shim");
        let _ = url_shim_available || !self.config.url_shim;

        // Structured-clone availability is informational as well: with the
        // flag or the feature off no clone globals exist and M1–M5
        // behavior is unchanged. An explicitly registered bridge with a
        // foreign version fails before any `globalThis` mutation.
        let clone_available = self.config.structured_clone && cfg!(feature = "structured-clone");
        if clone_available && let Some(adapter) = self.config.clone_adapter.as_ref() {
            let descriptor = adapter.descriptor();
            if descriptor.version != boa_fapi_core::clone::CLONE_ENCODING_VERSION {
                return Err(RegisterError::CloneBridgeIncompatible(descriptor.name));
            }
        }

        // Host limits are validated before any globalThis mutation: an
        // invalid configuration fails with a typed error and installs
        // nothing. This is the full `FileApiLimits::validate()` contract,
        // including the M3-B stream chunk-size range.
        if let Err(error) = self.config.limits.validate() {
            let message = format!("invalid FileApiLimits: {error}");
            return Err(match error {
                boa_fapi_core::file_api_error::FileApiError::ResourceLimit(_) => {
                    RegisterError::Js(crate::error::range_error(&message))
                }
                _ => RegisterError::Js(crate::error::type_error(&message)),
            });
        }

        // Build phase: no observable state changes yet. Constructors and
        // prototypes are ordinary objects until installed.
        let blob_spec = build_blob_class(context)?;
        let file_spec = build_file_class(context, blob_spec.prototype())?;
        let file_list_proto = build_file_list_prototype(context)?;
        #[cfg(feature = "streams-shim")]
        let stream_specs = crate::streams::build_stream_specs(context)?;
        #[cfg(feature = "dom-shim")]
        let error_prototype = context.intrinsics().constructors().error().prototype();
        #[cfg(feature = "dom-shim")]
        let dom_specs = crate::dom::build_dom_specs(context, error_prototype)?;
        #[cfg(feature = "dom-shim")]
        let filereader_specs =
            crate::filereader::build_filereader_specs(context, dom_specs.event_target.prototype())?;
        // `FileReaderSync` is built only for worker descriptors; window
        // and service-worker contexts install no such global at all.
        #[cfg(feature = "dom-shim")]
        let sync_specs = if self.config.environment.file_reader_sync_enabled() {
            Some(crate::filereader_sync::build_sync_specs(context)?)
        } else {
            None
        };
        // The `URL` namespace object is built whenever the shim is
        // available: installation decides the global name below. The
        // environment descriptor is validated here as well, so a bad
        // origin fails before any `globalThis` mutation.
        #[cfg(feature = "url-shim")]
        let url_specs = if url_shim_available {
            Some(crate::url_shim::build_url_specs(context)?)
        } else {
            None
        };
        let environment_descriptor = EnvironmentDescriptor::new(
            self.config.environment.core_kind(),
            self.config.origin.clone(),
            self.config.partition,
            self.config.nonce,
        )
        .map_err(|_| RegisterError::Js(crate::error::type_error("invalid serialized origin")))?;
        let _ = &environment_descriptor;
        // Preflight: extensibility and every own global name.
        let global = context.global_object();
        if !global.is_extensible(context).map_err(RegisterError::Js)? {
            return Err(RegisterError::GlobalNotExtensible);
        }
        let keys = global
            .own_property_keys(context)
            .map_err(RegisterError::Js)?;
        // `FileReaderSync` participates in the preflight only when the
        // worker capability installs it; otherwise the name is untouched.
        #[cfg(feature = "dom-shim")]
        let sync_enabled = self.config.environment.file_reader_sync_enabled();
        for name in [
            "Blob",
            "File",
            "FileList",
            #[cfg(feature = "streams-shim")]
            "ReadableStream",
            #[cfg(feature = "streams-shim")]
            "ReadableStreamDefaultReader",
            #[cfg(feature = "dom-shim")]
            "EventTarget",
            #[cfg(feature = "dom-shim")]
            "Event",
            #[cfg(feature = "dom-shim")]
            "ProgressEvent",
            #[cfg(feature = "dom-shim")]
            "DOMException",
            #[cfg(feature = "dom-shim")]
            "FileReader",
        ] {
            let key = PropertyKey::from(js_string!(name));
            if keys.contains(&key) {
                return Err(RegisterError::NameConflict(name.to_owned()));
            }
        }
        #[cfg(feature = "dom-shim")]
        if sync_enabled {
            let key = PropertyKey::from(js_string!("FileReaderSync"));
            if keys.contains(&key) {
                return Err(RegisterError::NameConflict("FileReaderSync".to_owned()));
            }
        }
        // The `URL` name is preflighted only when the shim would install
        // it; otherwise a host `URL` stays untouched.
        #[cfg(feature = "url-shim")]
        if url_shim_available {
            let key = PropertyKey::from(js_string!("URL"));
            if keys.contains(&key) {
                return Err(RegisterError::NameConflict("URL".to_owned()));
            }
        }

        // Install phase with rollback.
        if let Err(error) = install_globals(
            context,
            &blob_spec,
            &file_spec,
            #[cfg(feature = "streams-shim")]
            &stream_specs,
            #[cfg(feature = "dom-shim")]
            &dom_specs,
            #[cfg(feature = "dom-shim")]
            &filereader_specs,
            #[cfg(feature = "dom-shim")]
            sync_specs.as_ref(),
            #[cfg(feature = "url-shim")]
            url_specs.as_ref(),
        ) {
            rollback_globals(
                context,
                #[cfg(feature = "dom-shim")]
                sync_specs.is_some(),
                #[cfg(feature = "url-shim")]
                url_specs.is_some(),
            )?;
            return Err(RegisterError::Js(error));
        }

        // The context-local store plus shutdown wiring: a tracked closer
        // clears the store at shutdown, releasing every strong payload
        // reference. The flag is created per registration, so two contexts
        // never share a store or a shutdown state.
        let url_store: SharedUrlStore = Arc::new(BlobUrlStore::new());
        let shutdown = crate::lifecycle::ShutdownFlag::new();
        {
            let store = Arc::clone(&url_store);
            shutdown.track(move || store.clear());
        }
        // The I/O bridge (M9-B): per-registration context id, the
        // configured (or built-in bounded) executor, and the wake hook.
        // Shutdown cancels outstanding work, clears queued completions,
        // and forbids late settlement. The bridge handle is shared (not
        // duplicated) between the stored specs and the returned handle, so
        // `poll_io` on the handle observes the same queue the workers push
        // into. Context ids are never reused: id-space exhaustion (u64)
        // fails registration instead of aliasing two contexts.
        let Some(context_id) = crate::io::FileApiContextId::fresh() else {
            return Err(RegisterError::IoIdsExhausted);
        };
        let executor: Arc<dyn crate::io::FileIoExecutor> = self
            .config
            .io_executor
            .clone()
            .unwrap_or_else(|| Arc::new(crate::io::ThreadedFileIoExecutor::default()));
        let wake: Arc<dyn crate::io::FileIoWake> = self
            .config
            .io_wake
            .clone()
            .unwrap_or_else(|| Arc::new(crate::io::NoopWake));
        let concurrency = self.config.limits.max_concurrent_reads_per_global;
        let bridge = crate::io::IoBridge::new(
            context_id,
            concurrency,
            Arc::clone(&executor),
            Arc::clone(&wake),
            shutdown.clone(),
        );
        {
            let bridge = Arc::clone(&bridge);
            shutdown.track(move || bridge.shutdown());
        }
        let specs = RegisteredSpecs {
            blob: blob_spec,
            file: file_spec,
            file_list_proto,
            #[cfg(feature = "streams-shim")]
            streams: Some(stream_specs),
            #[cfg(feature = "dom-shim")]
            dom: Some(dom_specs),
            #[cfg(feature = "dom-shim")]
            filereader: Some(filereader_specs),
            #[cfg(feature = "dom-shim")]
            sync_reader: sync_specs,
            #[cfg(feature = "url-shim")]
            url: url_specs,
            url_store: Arc::clone(&url_store),
            shutdown: shutdown.clone(),
            io: Arc::clone(&bridge),
            #[cfg(feature = "dom-shim")]
            reader_payloads: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            #[cfg(feature = "streams-shim")]
            stream_payloads: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            #[cfg(feature = "dom-shim")]
            io_poll_budget: std::sync::Arc::new(std::sync::Mutex::new(None)),
            context_id,
            config: self.config.clone(),
            identity: self.config.identity,
        };
        context.insert_data::<RegisteredSpecs>(specs.clone());

        Ok(FileApiHandle {
            specs: specs.clone(),
            shutdown: specs.shutdown.clone(),
        })
    }
}

/// Creates and stores a Blob URL for the registered specs.
///
/// Single shared helper behind both `URL.createObjectURL` and the host
/// [`FileApiHandle::create_blob_url`]: brand checks happen at the call
/// boundary, everything below is identical (environment gate, shutdown,
/// quota, CSPRNG UUID, collision retry, atomic insert). Service-worker
/// contexts fail with [`BlobUrlError::Forbidden`] before touching the
/// store or the entropy source.
///
/// Without the `url-shim` feature the store insert runs inline (the shim
/// module owns only the JS namespace); with the feature it delegates to
/// the shim's retry helper so both paths share one implementation.
pub(crate) fn create_url_for_specs(
    specs: &RegisteredSpecs,
    data: &Arc<BlobData>,
) -> Result<String, BlobUrlError> {
    #[cfg(feature = "tracing")]
    let trace_start = crate::observability::now();
    #[cfg(feature = "tracing")]
    let trace_env = crate::observability::environment_hash_for_specs(specs);
    let outcome: Result<String, BlobUrlError> = (|| {
        if specs.shutdown.is_shutdown() {
            return Err(BlobUrlError::Shutdown);
        }
        let descriptor = specs
            .environment_descriptor()
            .map_err(|_| BlobUrlError::Malformed)?;
        if descriptor.creation_forbidden() {
            return Err(BlobUrlError::Forbidden);
        }
        let owner: EnvironmentKey = descriptor.key();
        let cap = specs.config.limits.max_blob_urls_per_global;
        if cap == 0 {
            return Err(BlobUrlError::LimitExceeded);
        }
        #[cfg(feature = "url-shim")]
        {
            crate::url_shim::insert_url(
                &specs.url_store,
                descriptor.serialized_origin(),
                &owner,
                data,
                specs.config.entropy.as_ref(),
                cap,
            )
        }
        #[cfg(not(feature = "url-shim"))]
        {
            use boa_fapi_core::blob_url::{format_blob_url, format_uuid_v4};
            for _ in 0..8 {
                let raw = specs.config.entropy.fill_16();
                if raw == [0_u8; 16] {
                    return Err(BlobUrlError::EntropyUnavailable);
                }
                let uuid = format_uuid_v4(raw);
                let url = format_blob_url(descriptor.serialized_origin(), &uuid);
                match specs.url_store.insert_capped(
                    url.clone(),
                    owner.clone(),
                    Arc::clone(data),
                    cap,
                ) {
                    Ok(()) => return Ok(url),
                    Err(BlobUrlError::Collision) => continue,
                    Err(other) => return Err(other),
                }
            }
            Err(BlobUrlError::Collision)
        }
    })();
    #[cfg(feature = "tracing")]
    {
        let class = match &outcome {
            Ok(_) => crate::observability::result_class_for_blob_url(None),
            Err(error) => crate::observability::result_class_for_blob_url(Some(error)),
        };
        crate::observability::emit(
            "blob_url_create",
            data.size(),
            crate::observability::elapsed_ms(trace_start),
            0,
            class,
            trace_env,
        );
    }
    outcome
}

/// Builds the `Blob` constructor/prototype pair.
fn build_blob_class(context: &mut Context) -> JsResult<StandardConstructor> {
    use boa_engine::native_function::NativeFunction;

    let mut builder =
        ConstructorBuilder::new(context, NativeFunction::from_fn_ptr(blob::constructor));
    builder.name("Blob");
    // Web IDL: both constructor arguments are optional, so `length` is the
    // shortest effective overload (0).
    builder.length(0);
    let spec = builder.build();
    blob::init_prototype(&spec.prototype(), context)?;
    Ok(spec)
}

/// Builds the `File` constructor/prototype pair.
///
/// `File.prototype` inherits from `Blob.prototype`.
fn build_file_class(
    context: &mut Context,
    blob_prototype: JsObject,
) -> JsResult<StandardConstructor> {
    use boa_engine::native_function::NativeFunction;

    let mut builder =
        ConstructorBuilder::new(context, NativeFunction::from_fn_ptr(file::constructor));
    builder.name("File");
    // Web IDL: `fileBits` and `fileName` are required, `options` optional.
    builder.length(2);
    builder.inherit(blob_prototype);
    let spec = builder.build();
    file::init_prototype(&spec.prototype(), context)?;
    Ok(spec)
}

/// Builds the FileList prototype object (no public constructor exists).
fn build_file_list_prototype(context: &mut Context) -> JsResult<JsObject> {
    let object_prototype = context.intrinsics().constructors().object().prototype();
    let prototype = JsObject::from_proto_and_data(object_prototype, OrdinaryPrototype);
    file_list::init_prototype(&prototype, context)?;
    Ok(prototype)
}

/// Marker native data for plain prototype objects.
#[derive(Debug, Trace, Finalize, JsData)]
struct OrdinaryPrototype;

/// Installs `Blob` and `File` on the global object (Web IDL attributes:
/// writable, non-enumerable, configurable). `FileList` installs nothing.
/// The streams shim installs `ReadableStream` and
/// `ReadableStreamDefaultReader` the same way; the DOM shim installs
/// `EventTarget`, `Event`, `ProgressEvent`, `DOMException` and `FileReader`;
/// worker environments additionally install `FileReaderSync`; the URL shim
/// installs the `URL` namespace object the same way.
///
/// Eight parameters (one per surface) are the atomic-install contract, not
/// accidental complexity: every global installs or none does.
#[allow(clippy::too_many_arguments)]
fn install_globals(
    context: &mut Context,
    blob_spec: &StandardConstructor,
    file_spec: &StandardConstructor,
    #[cfg(feature = "streams-shim")] stream_specs: &crate::streams::StreamSpecs,
    #[cfg(feature = "dom-shim")] dom_specs: &crate::dom::DomSpecs,
    #[cfg(feature = "dom-shim")] filereader_specs: &crate::filereader::FileReaderSpecs,
    #[cfg(feature = "dom-shim")] sync_specs: Option<&crate::filereader_sync::FileReaderSyncSpecs>,
    #[cfg(feature = "url-shim")] url_specs: Option<&crate::url_shim::UrlSpecs>,
) -> JsResult<()> {
    let global = context.global_object();
    for (name, constructor) in [
        ("Blob", blob_spec.constructor()),
        ("File", file_spec.constructor()),
    ] {
        global.define_property_or_throw(
            js_string!(name),
            PropertyDescriptor::builder()
                .value(constructor)
                .writable(true)
                .enumerable(false)
                .configurable(true),
            context,
        )?;
    }
    #[cfg(feature = "streams-shim")]
    for (name, constructor) in [
        ("ReadableStream", stream_specs.stream.constructor()),
        (
            "ReadableStreamDefaultReader",
            stream_specs.reader.constructor(),
        ),
    ] {
        global.define_property_or_throw(
            js_string!(name),
            PropertyDescriptor::builder()
                .value(constructor)
                .writable(true)
                .enumerable(false)
                .configurable(true),
            context,
        )?;
    }
    #[cfg(feature = "dom-shim")]
    for (name, constructor) in [
        ("EventTarget", dom_specs.event_target.constructor()),
        ("Event", dom_specs.event.constructor()),
        ("ProgressEvent", dom_specs.progress_event.constructor()),
        ("DOMException", dom_specs.dom_exception.constructor()),
        ("FileReader", filereader_specs.reader.constructor()),
    ] {
        global.define_property_or_throw(
            js_string!(name),
            PropertyDescriptor::builder()
                .value(constructor)
                .writable(true)
                .enumerable(false)
                .configurable(true),
            context,
        )?;
    }
    #[cfg(feature = "dom-shim")]
    if let Some(sync) = sync_specs {
        global.define_property_or_throw(
            js_string!("FileReaderSync"),
            PropertyDescriptor::builder()
                .value(sync.sync.constructor())
                .writable(true)
                .enumerable(false)
                .configurable(true),
            context,
        )?;
    }
    #[cfg(feature = "url-shim")]
    if let Some(url) = url_specs {
        global.define_property_or_throw(
            js_string!("URL"),
            PropertyDescriptor::builder()
                .value(url.url.clone())
                .writable(true)
                .enumerable(false)
                .configurable(true),
            context,
        )?;
    }
    Ok(())
}

/// Removes partially installed globals after a failed install.
///
/// `remove_sync` mirrors the worker capability: when the failed
/// registration would have installed `FileReaderSync`, its name is rolled
/// back as well; otherwise the name is left untouched. `remove_url`
/// mirrors the URL shim the same way.
fn rollback_globals(
    context: &mut Context,
    #[cfg(feature = "dom-shim")] remove_sync: bool,
    #[cfg(feature = "url-shim")] remove_url: bool,
) -> Result<(), RegisterError> {
    let global = context.global_object();
    for name in [
        "Blob",
        "File",
        #[cfg(feature = "streams-shim")]
        "ReadableStream",
        #[cfg(feature = "streams-shim")]
        "ReadableStreamDefaultReader",
        #[cfg(feature = "dom-shim")]
        "EventTarget",
        #[cfg(feature = "dom-shim")]
        "Event",
        #[cfg(feature = "dom-shim")]
        "ProgressEvent",
        #[cfg(feature = "dom-shim")]
        "DOMException",
        #[cfg(feature = "dom-shim")]
        "FileReader",
    ] {
        // The property was just defined as configurable, so deletion succeeds
        // on ordinary globals. A hostile exotic global may still refuse; the
        // reported error then reflects the rollback failure.
        global
            .delete_property_or_throw(js_string!(name), context)
            .map_err(RegisterError::Js)?;
    }
    #[cfg(feature = "dom-shim")]
    if remove_sync {
        global
            .delete_property_or_throw(js_string!("FileReaderSync"), context)
            .map_err(RegisterError::Js)?;
    }
    #[cfg(feature = "url-shim")]
    if remove_url {
        global
            .delete_property_or_throw(js_string!("URL"), context)
            .map_err(RegisterError::Js)?;
    }
    Ok(())
}

/// Opaque handle to a registered File API extension.
///
/// The handle owns the registered constructors/prototypes and configuration,
/// allowing the host to create Blob/File/FileList objects without JS.
/// After [`FileApiHandle::shutdown`] the handle rejects every new host
/// operation; already-created JS objects keep their payload but their
/// filesystem reads fail on the next chunk boundary. M6 adds the Blob URL
/// store and the structured-clone entry points to the same handle; the
/// target `FileApiExtension::shutdown` shape of the TZ is covered by this
/// handle method (compatibility recorded in the crate ADR): `register`
/// returns the handle, and the handle owns `shutdown`.
#[derive(Clone)]
pub struct FileApiHandle {
    specs: RegisteredSpecs,
    shutdown: crate::lifecycle::ShutdownFlag,
}

impl FileApiHandle {
    /// Returns `true` after [`FileApiHandle::shutdown`].
    ///
    /// Public so host loops and tests can observe shutdown without
    /// touching JS state.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.is_shutdown()
    }

    /// Fails with a `TypeError` when the runtime is shut down.
    fn reject_if_shutdown(&self) -> JsResult<()> {
        if self.is_shutdown() {
            return Err(crate::error::type_error(
                "the File API runtime is shut down",
            ));
        }
        Ok(())
    }

    /// Returns `true` when the context-local Blob URL store holds no entry.
    ///
    /// Observable-safe replacement for direct store inspection: proves
    /// shutdown release without exposing the live store, raw insertion or
    /// environment identity.
    pub fn blob_urls_empty(&self) -> bool {
        self.specs.url_store.is_empty()
    }

    /// Returns the number of live Blob URL entries of this context.
    ///
    /// Count only — no entry, key or token is revealed. Intended for host
    /// diagnostics and tests; it cannot insert, resolve or mutate.
    pub fn blob_url_count(&self) -> usize {
        self.specs.url_store.len()
    }

    /// Creates a brand-valid `Blob` object over a host-owned immutable
    /// [`BlobData`] payload.
    ///
    /// The object is indistinguishable from any other `Blob` for brand
    /// checks and promise reads; it exists so integration tests can drive
    /// the real `Blob.prototype.text()/arrayBuffer()/bytes()` path with a
    /// controlled host source (blocking, panicking, filesystem-backed)
    /// without copying the payload through memory first. The payload stays
    /// private to the host; JavaScript receives an ordinary `Blob` only.
    pub fn blob_from_data(
        &self,
        data: std::sync::Arc<boa_fapi_core::blob::BlobData>,
    ) -> boa_engine::JsObject {
        blob::create_instance(BlobNative::new(data), self.specs.blob_proto().clone())
    }

    /// Creates a brand-valid `File` object over a host-owned immutable
    /// [`BlobData`] payload.
    ///
    /// Same purpose as [`Self::blob_from_data`]: imports a pre-built
    /// host payload without exposing its source implementation to JS.
    /// `name` is a display name (`/` becomes `:`); `last_modified`
    /// defaults to the injected clock.
    pub fn file_from_data(
        &self,
        data: std::sync::Arc<boa_fapi_core::blob::BlobData>,
        name: &str,
        last_modified: Option<i64>,
    ) -> boa_engine::JsObject {
        let native =
            file::native_from_data(data, name, last_modified, self.specs.config.clock.as_ref());
        boa_engine::JsObject::from_proto_and_data(self.specs.file_proto().clone(), native)
    }
}

impl FileApiHandle {
    /// Creates a `Blob` from host bytes.
    ///
    /// The bytes are wrapped in an immutable memory source; the media type
    /// is normalized by the M1 rules. After `shutdown` the call fails
    /// before touching JS state.
    pub fn blob_from_bytes(
        &self,
        bytes: impl Into<Bytes>,
        media_type: &str,
        _context: &mut Context,
    ) -> JsResult<JsObject> {
        self.reject_if_shutdown()?;
        let data = blob::data_from_bytes(bytes.into(), media_type, self.specs.limits())
            .map_err(js_from_core)?;
        Ok(blob::create_instance(
            BlobNative::new(data),
            self.specs.blob_proto().clone(),
        ))
    }

    /// Creates a `File` from host bytes.
    ///
    /// `name` is a display name: no basename is computed, but `/` is
    /// replaced by `:` like the JS constructor. `options.last_modified` of
    /// `None` reads the injected clock. After `shutdown` the call fails
    /// before touching JS state.
    pub fn file_from_bytes(
        &self,
        bytes: impl Into<Bytes>,
        name: &str,
        options: HostFileOptions,
        _context: &mut Context,
    ) -> JsResult<JsObject> {
        self.reject_if_shutdown()?;
        let native = file::native_from_bytes(
            bytes.into(),
            name,
            &options.media_type,
            options.last_modified,
            self.specs.config.clock.as_ref(),
            self.specs.limits(),
        )
        .map_err(js_from_core)?;
        Ok(JsObject::from_proto_and_data(
            self.specs.file_proto().clone(),
            native,
        ))
    }

    /// Creates a `File` from a pre-authorized filesystem resource.
    ///
    /// The host passes an opaque [`FileResource`](boa_fapi_core::policy::FileResource)
    /// handle (already open, read-only, capability-checked) plus the only
    /// name JS observes, `display_name` (no basename is computed from any
    /// secret host location; `/` becomes `:` like the JS constructor). The
    /// resource is validated (live snapshot matches the import snapshot,
    /// plus a preflight size check against `max_blob_size`) before any
    /// JS-visible object exists, so a denial leaves no partial state. Name
    /// normalization matches the M1/M2 `File` behavior. Without the `fs`
    /// Cargo feature this method does not exist; the memory API and
    /// registration keep working unchanged.
    ///
    /// Two enforced boundaries (recorded in the ADR):
    ///
    /// - signature adaptation: the target shape takes `&dyn FileResource`,
    ///   but `ByteSource: 'static` cannot borrow it, so this method takes
    ///   `Arc<dyn FileResource>` ownership instead. The backing
    ///   `ArcResourceSource` revalidates the live snapshot before every
    ///   read and verifies exact bytes afterwards, with no partial result
    ///   and no location/identity disclosure.
    /// - shutdown handle release: the caller additionally passes the
    ///   `FsRegistry` that owns the resource, so the import can register a
    ///   shutdown closer (`close_all`) with it. [`FileApiHandle::shutdown`]
    ///   then drops OS handles immediately instead of deferring release to
    ///   registry destruction.
    #[cfg(feature = "fs")]
    pub fn file_from_resource(
        &self,
        registry: &boa_fapi_fs::FsRegistry,
        resource: std::sync::Arc<dyn boa_fapi_core::policy::FileResource>,
        display_name: &str,
        options: HostFileOptions,
        _context: &mut Context,
    ) -> JsResult<JsObject> {
        use std::sync::Arc;
        self.reject_if_shutdown()?;
        // Authorize before any JS-visible object exists. The grant check
        // uses the live snapshot so a resource that changed between open
        // and import is denied with no partial state.
        let live = resource
            .current_snapshot()
            .map_err(crate::error::js_from_core)?;
        let grant = boa_fapi_core::policy::FileGrant::new(
            resource.resource_id(),
            resource.import_snapshot(),
        );
        // A grant whose import snapshot no longer matches the live state
        // is stale: deny without creating anything.
        if grant.snapshot != live {
            return Err(crate::error::js_from_core(
                boa_fapi_core::file_api_error::FileApiError::SnapshotChanged,
            ));
        }
        // Enforced (not advisory): live-handle imports exist only on
        // platforms with a strong open-handle identity. On Unix the
        // live-vs-import comparison detects replacement; elsewhere a
        // filesystem-backed resource is refused outright — hosts must use
        // `copy_on_import` (immutable memory bytes via `file_from_bytes`)
        // or deny the import. Memory snapshots are always valid.
        //
        // The check runs only when the platform reports a weak identity so
        // that `platform_has_strong_identity` stays mockable in unit tests
        // without changing production behavior.
        #[cfg(not(unix))]
        if matches!(
            grant.snapshot,
            boa_fapi_core::snapshot::SnapshotState::Filesystem(_)
        ) && !boa_fapi_fs::platform_has_strong_identity()
        {
            return Err(crate::error::js_from_core(
                boa_fapi_core::file_api_error::FileApiError::PermissionDenied,
            ));
        }
        #[cfg(unix)]
        {
            let _ = &grant;
        }
        // Track the registry for handle release at shutdown: `close_all`
        // is idempotent, so tracking once per import is harmless even when
        // several imports share one registry.
        let tracked = registry.clone();
        self.shutdown.track(move || tracked.close_all());
        #[cfg(feature = "tracing")]
        let trace_env = crate::observability::environment_hash_for_specs(&self.specs);
        #[cfg(feature = "tracing")]
        let adapter: Arc<dyn boa_fapi_core::source::ByteSource> = Arc::new(ArcResourceSource::new(
            resource,
            self.shutdown.clone(),
            trace_env,
        ));
        #[cfg(not(feature = "tracing"))]
        let adapter: Arc<dyn boa_fapi_core::source::ByteSource> =
            Arc::new(ArcResourceSource::new(resource, self.shutdown.clone()));
        let data = blob::data_from_fs_source(adapter, &options.media_type, self.specs.limits())
            .map_err(js_from_core)?;
        let native = file::native_from_data(
            data,
            display_name,
            options.last_modified,
            self.specs.config.clock.as_ref(),
        );
        Ok(JsObject::from_proto_and_data(
            self.specs.file_proto().clone(),
            native,
        ))
    }

    /// Creates a `FileList` from File objects.
    ///
    /// Every element is brand-validated as a File before any output object
    /// is created; a non-File element fails without partial state. Only
    /// explicit `File` objects are accepted: this never enumerates a
    /// directory. After `shutdown` the call fails before touching JS state.
    pub fn file_list(
        &self,
        files: impl IntoIterator<Item = JsObject>,
        context: &mut Context,
    ) -> JsResult<JsObject> {
        self.reject_if_shutdown()?;
        let mut validated = Vec::new();
        for file in files {
            brand::require_file_object(&file)?;
            validated.push(file);
        }
        file_list::create(validated, &self.specs.file_list_proto, context)
    }

    /// Shuts down the registered File API runtime.
    ///
    /// Idempotent: repeated calls neither panic nor enqueue callbacks.
    /// Atomic with respect to new host-created `File`/resource operations
    /// (they are rejected once closed). Pending filesystem reads observe
    /// the shared cancellation; every tracked registry runs `close_all`,
    /// so OS handles are dropped immediately (not deferred to registry
    /// destruction); the context-local Blob URL store is cleared, releasing
    /// every strong payload reference; new reads, materializations, stream
    /// pulls, FileReader jobs, URL creations and clone writes after
    /// shutdown settle nothing against a destroyed context. No locations
    /// or identities leak into queues, errors, or JS objects.
    pub fn shutdown(&self, context: &mut Context) -> Result<(), RegisterError> {
        crate::lifecycle::shutdown_runtime(&self.shutdown, context)
    }

    /// Drains queued I/O completions into Boa settlement jobs.
    ///
    /// Called only by the owner of `context` as part of the host loop:
    ///
    /// ```text
    /// wait for FileIoWake or other host event
    /// handle.poll_io(&mut context)
    /// context.run_jobs()
    /// repeat until host and File API queues are quiescent
    /// ```
    ///
    /// Validates that `context` carries this handle's registration and
    /// rejects a foreign context without touching state. Each completion
    /// passes context/shutdown validation first; only then is the
    /// operation quota released exactly once and a Boa settlement job
    /// enqueued. FileReader chunk completions become at most one pump job
    /// each, stream chunk completions become at most one settlement job
    /// each (still through `poll_io`, never synchronously from a worker).
    /// Never calls user JS directly and never holds the bridge
    /// mutex across Boa calls. Returns the number of completions turned
    /// into jobs.
    pub fn poll_io(&self, context: &mut Context) -> Result<usize, crate::io::PollIoError> {
        use crate::io::PollIoError;
        // Identity-aware check first: a foreign context is rejected without
        // touching any bridge state. The snapshot clone ends its borrow
        // before any Boa call below.
        let stored = context.get_data::<RegisteredSpecs>().cloned();
        let Some(stored) = stored else {
            return Err(PollIoError::NotRegistered);
        };
        if stored.context_id != self.specs.context_id || stored.identity != self.specs.identity {
            return Err(PollIoError::ForeignContext);
        }
        let bridge = stored.io_bridge();
        // Non-blocking by contract: `poll_io` only turns already-queued
        // DTOs into Boa jobs. Slow workers stay invisible until their
        // completion lands; the host loop repeats `poll_io` after the next
        // wake (or its own event) until quiescent.
        //
        // M9-C FileReader chunk completions share the same bridge: they
        // are drained first (FIFO within one reader) and each becomes at
        // most one pump Boa job; a stale completion (abort/restart/
        // shutdown) settles nothing. The host may bound completions per
        // `poll_io` call (`poll_io_budget`) for fairness; leftover chunks
        // stay queued and re-wake the host loop.
        //
        // M9-D stream chunk completions share the same bridge as well: they
        // drain after FileReader chunks (submission-order within the
        // shared queue is preserved per operation kind) and each becomes at
        // most one settlement Boa job; a stale completion (cancel/error/
        // shutdown) settles nothing.
        #[cfg(feature = "dom-shim")]
        let budget = stored.io_poll_budget();
        let reader_completions = bridge.take_reader_completions();
        let mut settled = 0_usize;
        let mut reader_left = 0_usize;
        for completion in reader_completions {
            #[cfg(feature = "dom-shim")]
            if budget.is_some_and(|limit| settled >= limit) {
                reader_left += 1;
                bridge.requeue_reader_completion(completion);
                continue;
            }
            if self.is_shutdown() || bridge.is_shutdown() {
                // Shutdown: drop the late chunk and release its quota
                // exactly once; no job, no telemetry, no JS.
                bridge.unreserve(completion.operation_id());
                #[cfg(feature = "dom-shim")]
                {
                    stored.drop_reader_payload(completion.operation_id().get());
                    crate::filereader::drop_pending_for_shutdown(
                        context,
                        completion.operation_id().get(),
                    );
                }
                continue;
            }
            #[cfg(feature = "dom-shim")]
            {
                settled = settled.saturating_add(
                    crate::filereader::settle_reader_completion(&stored, completion, context)
                        .unwrap_or(0),
                );
            }
            #[cfg(not(feature = "dom-shim"))]
            {
                let _ = completion;
            }
        }
        // Re-queued leftovers (budget) keep the queue pending and re-wake
        // the host so the next `poll_io` continues without starvation.
        if reader_left > 0 {
            bridge.wake_host();
        }
        // M9-D stream chunks drain next, in submission order per stream.
        // Shutdown drops the late chunk and releases its quota exactly
        // once; a stale completion (unknown operation/generation) settles
        // nothing inside `settle_stream_completion`.
        #[cfg(feature = "streams-shim")]
        {
            let stream_completions = bridge.take_stream_completions();
            for completion in stream_completions {
                if self.is_shutdown() || bridge.is_shutdown() {
                    bridge.unreserve(completion.operation_id());
                    stored.drop_stream_payload(completion.operation_id().get());
                    crate::streams::drop_pending_for_shutdown(
                        context,
                        completion.operation_id().get(),
                    );
                    continue;
                }
                settled = settled.saturating_add(
                    crate::streams::settle_stream_completion(&stored, completion, context)
                        .unwrap_or(0),
                );
            }
        }
        #[cfg(not(feature = "streams-shim"))]
        {
            // Without the shim no stream task can exist; any queued entry
            // would be foreign — drain defensively without settling.
            for completion in bridge.take_stream_completions() {
                bridge.unreserve(completion.operation_id());
            }
        }
        let completions = bridge.take_completions();
        for completion in completions {
            if self.is_shutdown() || bridge.is_shutdown() {
                // Shutdown: drop the late completion and release its quota
                // exactly once; no job, no telemetry, no JS.
                bridge.release(completion.operation_id());
                continue;
            }
            let operation = completion.operation_id();
            let byte_len = completion.byte_len();
            let result = completion.into_result();
            bridge.release(operation);
            // `settle_completion` drops stale operations (unknown id) as a
            // strict no-op and otherwise enqueues exactly one Boa job.
            let size = byte_len.map_or(0_u64, |len| len as u64);
            if crate::promise_read::settle_completion(operation, result, size, context).is_ok() {
                settled = settled.saturating_add(1);
            }
        }
        Ok(settled)
    }

    /// Bounds the FileReader chunk completions settled per `poll_io`.
    ///
    /// `None` (default) drains everything queued; `Some(n)` settles at
    /// most `n` reader chunks per call and re-queues the rest for the next
    /// host-loop turn, so one busy reader cannot starve promise reads or
    /// other readers. Leftovers re-wake the host (`FileIoWake`).
    pub fn set_poll_io_budget(&self, budget: Option<usize>) {
        #[cfg(feature = "dom-shim")]
        {
            if let Ok(mut slot) = self.specs.io_poll_budget.lock() {
                *slot = budget;
            }
        }
        #[cfg(not(feature = "dom-shim"))]
        {
            let _ = budget;
        }
    }

    /// Returns `true` while I/O work is outstanding or completions wait.
    ///
    /// Context-local: only meaningful for the owning registration. The
    /// host loop uses it together with its own quiescence check.
    pub fn has_pending_io(&self) -> bool {
        self.specs.io.has_pending()
    }

    /// Creates and stores a Blob URL for a brand-validated `Blob`/`File`.
    ///
    /// Accepts only objects carrying the Blob brand (`File` passes through
    /// the same gate; `FileList`, forged and foreign objects fail with a
    /// synchronous `TypeError` before touching the store, the quota or the
    /// entropy source). Serialization, quota, UUID and collision semantics
    /// are identical to `URL.createObjectURL` (shared helper): the URL is
    /// `blob:<serialized-origin>/<uuid-v4>`, the insert is atomic against
    /// `max_blob_urls_per_global`, collisions retry with fresh entropy and
    /// never overwrite. After `shutdown` the call fails before touching
    /// any state.
    pub fn create_blob_url(&self, object: &JsObject) -> JsResult<String> {
        #[cfg(feature = "tracing")]
        let trace_start = crate::observability::now();
        #[cfg(feature = "tracing")]
        let trace_env = crate::observability::environment_hash_for_specs(&self.specs);
        if self.is_shutdown() {
            #[cfg(feature = "tracing")]
            crate::observability::emit(
                "blob_url_create",
                0,
                crate::observability::elapsed_ms(trace_start),
                0,
                "shutdown",
                trace_env,
            );
            return Err(crate::error::type_error(
                "the File API runtime is shut down",
            ));
        }
        let data = brand::require_blob(&boa_engine::JsValue::from(object.clone()))
            .map_err(|_| crate::error::type_error("URL.createObjectURL requires a Blob"))?;
        create_url_for_specs(&self.specs, &data).map_err(|error| match error {
            BlobUrlError::Forbidden => {
                crate::error::type_error("URL creation is not allowed in this context")
            }
            BlobUrlError::LimitExceeded => crate::error::type_error("blob URL quota exceeded"),
            BlobUrlError::Shutdown => crate::error::type_error("the File API runtime is shut down"),
            _ => crate::error::type_error("blob URL is not available"),
        })
    }

    /// Resolves a Blob URL for this context's environment.
    ///
    /// The host-side Fetch boundary: same-partition checks run before the
    /// shared payload is handed out. Malformed, unknown, revoked and
    /// foreign-partition URLs share one failure class
    /// ([`BlobUrlError::Malformed`]/[`BlobUrlError::Unavailable`] with the
    /// identical display string); the error carries no token, UUID, origin
    /// internals, existence bit or host metadata. `boa-fapi` registers no
    /// network handler: the host drives Fetch from this result.
    pub fn resolve_blob_url(
        &self,
        url: &str,
    ) -> Result<boa_fapi_core::blob_url::ResolvedBlob, BlobUrlError> {
        #[cfg(feature = "tracing")]
        let trace_start = crate::observability::now();
        #[cfg(feature = "tracing")]
        let trace_env = crate::observability::environment_hash_for_specs(&self.specs);
        let outcome = (|| {
            let key = self
                .specs
                .environment_descriptor()
                .map(|d| d.key())
                .map_err(|_| BlobUrlError::Malformed)?;
            self.specs.url_store.resolve(url, &key)
        })();
        #[cfg(feature = "tracing")]
        {
            let (size, class) = match &outcome {
                Ok(resolved) => (
                    resolved.size(),
                    crate::observability::result_class_for_blob_url(None),
                ),
                Err(error) => (
                    0,
                    crate::observability::result_class_for_blob_url(Some(error)),
                ),
            };
            crate::observability::emit(
                "blob_url_resolve",
                size,
                crate::observability::elapsed_ms(trace_start),
                0,
                class,
                trace_env,
            );
        }
        outcome
    }

    /// Revokes a Blob URL idempotently.
    ///
    /// Ownership-blind by specified `revokeObjectURL` semantics: any
    /// well-formed URL removes its entry regardless of who asks (revoke is
    /// not a gated read), malformed input is a no-op. Either way nothing
    /// is reported, so revoke can never serve as an enumeration oracle.
    /// Revoke stops new resolutions; reads that already hold the
    /// `Arc<BlobData>` run to completion.
    pub fn revoke_blob_url(&self, url: &str) {
        self.specs.url_store.revoke(url);
    }

    /// Encodes a live `Blob` object into its clone payload.
    ///
    /// Materializes through the existing checked path
    /// (`max_materialize_bytes`); snapshot/permission/short-read failures
    /// yield a typed [`CloneError`] with no partial payload. The payload
    /// carries bytes and public metadata only — never a path, capability,
    /// OS handle or snapshot identity. After `shutdown` the call fails
    /// before touching any state.
    pub fn clone_blob(&self, object: &JsObject) -> Result<FileApiClonePayload, CloneError> {
        #[cfg(feature = "tracing")]
        let trace_start = crate::observability::now();
        #[cfg(feature = "tracing")]
        let trace_env = crate::observability::environment_hash_for_specs(&self.specs);
        let outcome: Result<FileApiClonePayload, CloneError> = (|| {
            if self.is_shutdown() {
                return Err(CloneError::Shutdown);
            }
            let data = brand::require_blob(&boa_engine::JsValue::from(object.clone()))
                .map_err(|_| CloneError::InvalidObject)?;
            let bytes = data
                .materialize(
                    self.specs.limits(),
                    &boa_fapi_core::cancellation::CancellationToken::new(),
                )
                .map_err(clone_error_from_core)?;
            boa_fapi_core::clone::serialized_blob(bytes, data.media_type())
                .map(FileApiClonePayload::Blob)
        })();
        #[cfg(feature = "tracing")]
        {
            let (size, chunks, class) = match &outcome {
                Ok(FileApiClonePayload::Blob(blob)) => {
                    let size = clone_payload_size(&FileApiClonePayload::Blob(blob.clone()));
                    (
                        size,
                        if size == 0 { 0 } else { 1 },
                        crate::observability::result_class_for_clone(None),
                    )
                }
                Ok(_) => (0, 1, crate::observability::result_class_for_clone(None)),
                Err(error) => (
                    0,
                    0,
                    crate::observability::result_class_for_clone(Some(error)),
                ),
            };
            crate::observability::emit(
                "clone_encode",
                size,
                crate::observability::elapsed_ms(trace_start),
                chunks,
                class,
                trace_env,
            );
        }
        outcome
    }

    /// Encodes a live `File` object into its clone payload.
    ///
    /// Same materialization and failure semantics as [`Self::clone_blob`];
    /// `name` is the already-sanitized display name and `lastModified` the
    /// stored timestamp — no clock is read here.
    pub fn clone_file(&self, object: &JsObject) -> Result<FileApiClonePayload, CloneError> {
        #[cfg(feature = "tracing")]
        let trace_start = crate::observability::now();
        #[cfg(feature = "tracing")]
        let trace_env = crate::observability::environment_hash_for_specs(&self.specs);
        let outcome: Result<FileApiClonePayload, CloneError> = (|| {
            if self.is_shutdown() {
                return Err(CloneError::Shutdown);
            }
            let (data, name, last_modified) =
                brand::require_file(&boa_engine::JsValue::from(object.clone()))
                    .map_err(|_| CloneError::InvalidObject)?;
            let bytes = data
                .materialize(
                    self.specs.limits(),
                    &boa_fapi_core::cancellation::CancellationToken::new(),
                )
                .map_err(clone_error_from_core)?;
            boa_fapi_core::clone::serialized_file(bytes, data.media_type(), &name, last_modified)
                .map(FileApiClonePayload::File)
        })();
        #[cfg(feature = "tracing")]
        {
            let (size, chunks, class) = match &outcome {
                Ok(FileApiClonePayload::File(file)) => {
                    let size = clone_payload_size(&FileApiClonePayload::File(file.clone()));
                    (
                        size,
                        if size == 0 { 0 } else { 1 },
                        crate::observability::result_class_for_clone(None),
                    )
                }
                Ok(_) => (0, 1, crate::observability::result_class_for_clone(None)),
                Err(error) => (
                    0,
                    0,
                    crate::observability::result_class_for_clone(Some(error)),
                ),
            };
            crate::observability::emit(
                "clone_encode",
                size,
                crate::observability::elapsed_ms(trace_start),
                chunks,
                class,
                trace_env,
            );
        }
        outcome
    }

    /// Encodes a live `FileList` object into its clone payload.
    ///
    /// Every element is brand-validated before any output exists; a
    /// non-`File` element fails with no partial list. Order, count and all
    /// `File` metadata survive the round-trip; identity holds only within
    /// the decoded result. The entry count is bounded before materializing
    /// (a forged `length` larger than the u32 index space or the encode
    /// ceiling fails without per-element work).
    pub fn clone_file_list(
        &self,
        object: &JsObject,
        context: &mut Context,
    ) -> Result<FileApiClonePayload, CloneError> {
        use boa_engine::property::PropertyKey;
        #[cfg(feature = "tracing")]
        let trace_start = crate::observability::now();
        #[cfg(feature = "tracing")]
        let trace_env = crate::observability::environment_hash_for_specs(&self.specs);
        let outcome: Result<FileApiClonePayload, CloneError> = (|| {
            if self.is_shutdown() {
                return Err(CloneError::Shutdown);
            }
            let value = boa_engine::JsValue::from(object.clone());
            let len = brand::require_file_list(&value).map_err(|_| CloneError::InvalidObject)?;
            if len > boa_fapi_core::clone::MAX_ENCODE_FILES {
                return Err(CloneError::LimitExceeded);
            }
            // The indexed slots are non-configurable own properties (see
            // `file_list::create`), so a brand-valid list always yields exactly
            // `len` elements; a missing slot is a corrupted list, not a short
            // one — fail rather than emit a partial payload.
            let mut files = Vec::new();
            for index in 0..len {
                let index_u32 = u32::try_from(index).map_err(|_| CloneError::LimitExceeded)?;
                let element = object
                    .get(PropertyKey::from(index_u32), context)
                    .map_err(|_| CloneError::InvalidObject)?;
                let Some(element) = element.as_object() else {
                    return Err(CloneError::InvalidObject);
                };
                let (data, name, last_modified) =
                    brand::require_file(&boa_engine::JsValue::from(element.clone()))
                        .map_err(|_| CloneError::InvalidObject)?;
                let bytes = data
                    .materialize(
                        self.specs.limits(),
                        &boa_fapi_core::cancellation::CancellationToken::new(),
                    )
                    .map_err(clone_error_from_core)?;
                files.push(boa_fapi_core::clone::serialized_file(
                    bytes,
                    data.media_type(),
                    &name,
                    last_modified,
                )?);
            }
            Ok(FileApiClonePayload::FileList(files))
        })();
        #[cfg(feature = "tracing")]
        {
            let (size, chunks, class) = match &outcome {
                Ok(FileApiClonePayload::FileList(files)) => {
                    let size = clone_payload_size(&FileApiClonePayload::FileList(files.clone()));
                    (
                        size,
                        if size == 0 { 0 } else { 1 },
                        crate::observability::result_class_for_clone(None),
                    )
                }
                Ok(_) => (0, 1, crate::observability::result_class_for_clone(None)),
                Err(error) => (
                    0,
                    0,
                    crate::observability::result_class_for_clone(Some(error)),
                ),
            };
            crate::observability::emit(
                "clone_encode",
                size,
                crate::observability::elapsed_ms(trace_start),
                chunks,
                class,
                trace_env,
            );
        }
        outcome
    }

    /// Decodes a clone payload into a live `Blob` object.
    ///
    /// Only `Blob` payloads are accepted here; `File`/`FileList` payloads
    /// fail with [`CloneError::UnexpectedKind`] before touching JS state.
    /// The result gets a new immutable backing and never shares mutable JS
    /// buffers with the source. After `shutdown` the call fails before
    /// touching JS state.
    pub fn blob_from_clone(
        &self,
        payload: &FileApiClonePayload,
        context: &mut Context,
    ) -> Result<JsObject, CloneError> {
        #[cfg(feature = "tracing")]
        let trace_start = crate::observability::now();
        #[cfg(feature = "tracing")]
        let trace_env = crate::observability::environment_hash_for_specs(&self.specs);
        let outcome: Result<JsObject, CloneError> = (|| {
            self.reject_clone_if_shutdown()?;
            let FileApiClonePayload::Blob(blob) = payload else {
                return Err(CloneError::UnexpectedKind);
            };
            let data =
                blob::data_from_bytes(blob.bytes.clone(), &blob.media_type, self.specs.limits())
                    .map_err(clone_error_from_core)?;
            let _ = context;
            Ok(blob::create_instance(
                BlobNative::new(data),
                self.specs.blob_proto().clone(),
            ))
        })();
        #[cfg(feature = "tracing")]
        {
            let (size, chunks, class) = match &outcome {
                Ok(_) => {
                    let size = clone_payload_size(payload);
                    (
                        size,
                        if size == 0 { 0 } else { 1 },
                        crate::observability::result_class_for_clone(None),
                    )
                }
                Err(error) => (
                    0,
                    0,
                    crate::observability::result_class_for_clone(Some(error)),
                ),
            };
            crate::observability::emit(
                "clone_decode",
                size,
                crate::observability::elapsed_ms(trace_start),
                chunks,
                class,
                trace_env,
            );
        }
        outcome
    }

    /// Decodes a clone payload into a live `File` object.
    ///
    /// Only `File` payloads are accepted; the stored `name`/`lastModified`
    /// are reused verbatim (no clock read, no re-sanitization beyond the
    /// constructor-equivalent slash replacement, which is idempotent).
    pub fn file_from_clone(
        &self,
        payload: &FileApiClonePayload,
        context: &mut Context,
    ) -> Result<JsObject, CloneError> {
        #[cfg(feature = "tracing")]
        let trace_start = crate::observability::now();
        #[cfg(feature = "tracing")]
        let trace_env = crate::observability::environment_hash_for_specs(&self.specs);
        let outcome: Result<JsObject, CloneError> = (|| {
            self.reject_clone_if_shutdown()?;
            let FileApiClonePayload::File(file) = payload else {
                return Err(CloneError::UnexpectedKind);
            };
            let data =
                blob::data_from_bytes(file.bytes.clone(), &file.media_type, self.specs.limits())
                    .map_err(clone_error_from_core)?;
            let _ = context;
            Ok(JsObject::from_proto_and_data(
                self.specs.file_proto().clone(),
                file::FileNative::new(
                    data,
                    file::normalize_file_name(&file.name),
                    file.last_modified,
                ),
            ))
        })();
        #[cfg(feature = "tracing")]
        {
            let (size, chunks, class) = match &outcome {
                Ok(_) => {
                    let size = clone_payload_size(payload);
                    (
                        size,
                        if size == 0 { 0 } else { 1 },
                        crate::observability::result_class_for_clone(None),
                    )
                }
                Err(error) => (
                    0,
                    0,
                    crate::observability::result_class_for_clone(Some(error)),
                ),
            };
            crate::observability::emit(
                "clone_decode",
                size,
                crate::observability::elapsed_ms(trace_start),
                chunks,
                class,
                trace_env,
            );
        }
        outcome
    }

    /// Decodes a clone payload into a live `FileList` object.
    ///
    /// Only `FileList` payloads are accepted; every decoded `File` is
    /// created through the same checked path as [`Self::file_from_clone`]
    /// before the list object exists, so a failure leaves no partial list.
    /// A forged oversized `Vec` fails on the entry-count bound before any
    /// per-file allocation.
    pub fn file_list_from_clone(
        &self,
        payload: &FileApiClonePayload,
        context: &mut Context,
    ) -> Result<JsObject, CloneError> {
        #[cfg(feature = "tracing")]
        let trace_start = crate::observability::now();
        #[cfg(feature = "tracing")]
        let trace_env = crate::observability::environment_hash_for_specs(&self.specs);
        let outcome: Result<JsObject, CloneError> = (|| {
            self.reject_clone_if_shutdown()?;
            let FileApiClonePayload::FileList(files) = payload else {
                return Err(CloneError::UnexpectedKind);
            };
            if files.len() > boa_fapi_core::clone::MAX_ENCODE_FILES {
                return Err(CloneError::LimitExceeded);
            }
            let mut objects = Vec::new();
            for file in files {
                let data = blob::data_from_bytes(
                    file.bytes.clone(),
                    &file.media_type,
                    self.specs.limits(),
                )
                .map_err(clone_error_from_core)?;
                objects.push(JsObject::from_proto_and_data(
                    self.specs.file_proto().clone(),
                    file::FileNative::new(
                        data,
                        file::normalize_file_name(&file.name),
                        file.last_modified,
                    ),
                ));
            }
            file_list::create(objects, &self.specs.file_list_proto, context)
                .map_err(|_| CloneError::Internal)
        })();
        #[cfg(feature = "tracing")]
        {
            let (size, chunks, class) = match &outcome {
                Ok(_) => {
                    let size = clone_payload_size(payload);
                    (
                        size,
                        if size == 0 { 0 } else { 1 },
                        crate::observability::result_class_for_clone(None),
                    )
                }
                Err(error) => (
                    0,
                    0,
                    crate::observability::result_class_for_clone(Some(error)),
                ),
            };
            crate::observability::emit(
                "clone_decode",
                size,
                crate::observability::elapsed_ms(trace_start),
                chunks,
                class,
                trace_env,
            );
        }
        outcome
    }

    /// Encodes a payload through the registered host bridge.
    ///
    /// Fails with [`CloneError::NoBridge`] when no bridge is registered
    /// (or the feature is off) and with [`CloneError::UnsupportedVersion`]
    /// when the bridge speaks a foreign version — both before touching any
    /// global or payload. After `shutdown` the call fails as well.
    pub fn clone_encode_via_bridge(
        &self,
        payload: &FileApiClonePayload,
    ) -> Result<Vec<u8>, CloneError> {
        #[cfg(feature = "tracing")]
        let trace_start = crate::observability::now();
        #[cfg(feature = "tracing")]
        let trace_env = crate::observability::environment_hash_for_specs(&self.specs);
        let outcome: Result<Vec<u8>, CloneError> = (|| {
            if self.is_shutdown() {
                return Err(CloneError::Shutdown);
            }
            let Some(adapter) = self.specs.config.clone_adapter.as_ref() else {
                return Err(CloneError::NoBridge);
            };
            if adapter.descriptor().version != boa_fapi_core::clone::CLONE_ENCODING_VERSION {
                return Err(CloneError::UnsupportedVersion);
            }
            adapter.encode(payload)
        })();
        #[cfg(feature = "tracing")]
        {
            let (size, chunks, class) = match &outcome {
                Ok(_) => {
                    let size = clone_payload_size(payload);
                    (
                        size,
                        if size == 0 { 0 } else { 1 },
                        crate::observability::result_class_for_clone(None),
                    )
                }
                Err(error) => (
                    0,
                    0,
                    crate::observability::result_class_for_clone(Some(error)),
                ),
            };
            crate::observability::emit(
                "clone_encode",
                size,
                crate::observability::elapsed_ms(trace_start),
                chunks,
                class,
                trace_env,
            );
        }
        outcome
    }

    /// Decodes bridge bytes into a payload.
    ///
    /// Same bridge/version/shutdown preflights as
    /// [`Self::clone_encode_via_bridge`]; decoding itself enforces the
    /// version and the checked bounds.
    pub fn clone_decode_via_bridge(&self, bytes: &[u8]) -> Result<FileApiClonePayload, CloneError> {
        #[cfg(feature = "tracing")]
        let trace_start = crate::observability::now();
        #[cfg(feature = "tracing")]
        let trace_env = crate::observability::environment_hash_for_specs(&self.specs);
        let outcome: Result<FileApiClonePayload, CloneError> = (|| {
            if self.is_shutdown() {
                return Err(CloneError::Shutdown);
            }
            let Some(adapter) = self.specs.config.clone_adapter.as_ref() else {
                return Err(CloneError::NoBridge);
            };
            if adapter.descriptor().version != boa_fapi_core::clone::CLONE_ENCODING_VERSION {
                return Err(CloneError::UnsupportedVersion);
            }
            adapter.decode(bytes)
        })();
        #[cfg(feature = "tracing")]
        {
            let (size, chunks, class) = match &outcome {
                Ok(payload) => {
                    let size = clone_payload_size(payload);
                    (
                        size,
                        if size == 0 { 0 } else { 1 },
                        crate::observability::result_class_for_clone(None),
                    )
                }
                Err(error) => (
                    0,
                    0,
                    crate::observability::result_class_for_clone(Some(error)),
                ),
            };
            crate::observability::emit(
                "clone_decode",
                size,
                crate::observability::elapsed_ms(trace_start),
                chunks,
                class,
                trace_env,
            );
        }
        outcome
    }

    /// Fails with [`CloneError::Shutdown`] when the runtime is shut down.
    fn reject_clone_if_shutdown(&self) -> Result<(), CloneError> {
        if self.is_shutdown() {
            return Err(CloneError::Shutdown);
        }
        Ok(())
    }

    /// Returns the environment descriptor this handle was registered with.
    ///
    /// Worker descriptors installed the normative `FileReaderSync`;
    /// `Window` and `ServiceWorker` did not.
    pub fn environment(&self) -> FileApiEnvironment {
        self.specs.environment()
    }
}

/// Maps a core materialization failure onto the clone error.
///
/// No separate error mapping is introduced: resource limits become
/// [`CloneError::LimitExceeded`], cancellation into [`CloneError::Shutdown`],
/// everything else into [`CloneError::SourceFailed`]. Messages stay generic
/// (no path, bytes, or source detail).
fn clone_error_from_core(error: boa_fapi_core::file_api_error::FileApiError) -> CloneError {
    use boa_fapi_core::file_api_error::FileApiError;
    match error {
        FileApiError::ResourceLimit(_) => CloneError::LimitExceeded,
        FileApiError::Cancelled => CloneError::Shutdown,
        FileApiError::NotFound
        | FileApiError::UnsafeFile
        | FileApiError::TooManyReads
        | FileApiError::SnapshotChanged
        | FileApiError::FileLocked
        | FileApiError::PermissionDenied
        | FileApiError::InvalidRange
        | FileApiError::Internal => CloneError::SourceFailed,
        _ => CloneError::SourceFailed,
    }
}

#[cfg(feature = "tracing")]
fn clone_payload_size(payload: &FileApiClonePayload) -> u64 {
    match payload {
        FileApiClonePayload::Blob(blob) => blob.bytes.len() as u64,
        FileApiClonePayload::File(file) => file.bytes.len() as u64,
        FileApiClonePayload::FileList(files) => files.iter().fold(0_u64, |total, file| {
            total.saturating_add(file.bytes.len() as u64)
        }),
    }
}

/// Shutdown-aware [`ByteSource`](boa_fapi_core::source::ByteSource) over an
/// owned host [`FileResource`](boa_fapi_core::policy::FileResource).
///
/// Captures the import snapshot and length once; every `read_range`
/// validates, in order: caller cancellation, the runtime shutdown flag,
/// checked range arithmetic, the live snapshot against the import snapshot
/// (before reading), then exact byte-count verification (after reading).
/// Never returns partial bytes; never touches Boa from completion; never
/// exposes paths or identities.
#[cfg(feature = "fs")]
struct ArcResourceSource {
    resource: std::sync::Arc<dyn boa_fapi_core::policy::FileResource>,
    import_snapshot: boa_fapi_core::snapshot::SnapshotState,
    len: u64,
    shutdown: crate::lifecycle::ShutdownFlag,
    #[cfg(feature = "tracing")]
    trace_env: u64,
}

#[cfg(all(feature = "fs", not(feature = "tracing")))]
impl ArcResourceSource {
    /// Captures the import snapshot and length without creating JS state.
    fn new(
        resource: std::sync::Arc<dyn boa_fapi_core::policy::FileResource>,
        shutdown: crate::lifecycle::ShutdownFlag,
    ) -> Self {
        let import_snapshot = resource.import_snapshot();
        let len = match &import_snapshot {
            boa_fapi_core::snapshot::SnapshotState::Filesystem(state) => state.size(),
            _ => 0,
        };
        Self {
            resource,
            import_snapshot,
            len,
            shutdown,
        }
    }
}

#[cfg(all(feature = "fs", feature = "tracing"))]
impl ArcResourceSource {
    /// Captures the import snapshot and length without creating JS state.
    fn new(
        resource: std::sync::Arc<dyn boa_fapi_core::policy::FileResource>,
        shutdown: crate::lifecycle::ShutdownFlag,
        trace_env: u64,
    ) -> Self {
        let import_snapshot = resource.import_snapshot();
        let len = match &import_snapshot {
            boa_fapi_core::snapshot::SnapshotState::Filesystem(state) => state.size(),
            _ => 0,
        };
        Self {
            resource,
            import_snapshot,
            len,
            shutdown,
            trace_env,
        }
    }
}

#[cfg(feature = "fs")]
impl boa_fapi_core::source::ByteSource for ArcResourceSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn snapshot(&self) -> boa_fapi_core::snapshot::SnapshotState {
        self.import_snapshot.clone()
    }

    fn read_range(
        &self,
        range: std::ops::Range<u64>,
        cancel: &boa_fapi_core::cancellation::CancellationToken,
    ) -> Result<bytes::Bytes, boa_fapi_core::file_api_error::FileApiError> {
        use boa_fapi_core::file_api_error::FileApiError;
        #[cfg(feature = "tracing")]
        let trace_start = crate::observability::now();
        #[cfg(feature = "tracing")]
        let trace_len = range.end.saturating_sub(range.start);
        let outcome: Result<bytes::Bytes, FileApiError> = (|| {
            if cancel.is_cancelled() || self.shutdown.cancel_token().is_cancelled() {
                return Err(FileApiError::Cancelled);
            }
            if self.shutdown.is_shutdown() {
                return Err(FileApiError::Cancelled);
            }
            if range.start > range.end {
                return Err(FileApiError::InvalidRange);
            }
            let len_u64 = range
                .end
                .checked_sub(range.start)
                .ok_or(FileApiError::InvalidRange)?;
            if range.end > self.len {
                return Err(FileApiError::InvalidRange);
            }
            let len = usize::try_from(len_u64).map_err(|_| {
                FileApiError::ResourceLimit(
                    boa_fapi_core::error::ResourceLimitKind::MaterializeBytes,
                )
            })?;
            if len == 0 {
                return Ok(bytes::Bytes::new());
            }
            // Snapshot validation before the read: replacement, truncation,
            // deletion, or permission change fails here with no bytes out.
            let live = self.resource.current_snapshot()?;
            if live != self.import_snapshot {
                return Err(FileApiError::SnapshotChanged);
            }
            if cancel.is_cancelled() || self.shutdown.cancel_token().is_cancelled() {
                return Err(FileApiError::Cancelled);
            }
            let bytes = self.resource.read_at(range.start, len)?;
            if bytes.len() != len {
                return Err(FileApiError::InvalidRange);
            }
            // Post-read identity confirmation: a replacement racing the read
            // surfaces here (or on the next chunk), never as partial old bytes.
            let after = self.resource.current_snapshot()?;
            if after != self.import_snapshot {
                return Err(FileApiError::SnapshotChanged);
            }
            Ok(bytes::Bytes::from(bytes))
        })();
        #[cfg(feature = "tracing")]
        {
            let (chunks, class) = match &outcome {
                Ok(bytes) => {
                    let chunks = if bytes.is_empty() { 0 } else { 1 };
                    (chunks, crate::observability::result_class_for_core(None))
                }
                Err(error) => {
                    let class = if self.shutdown.is_shutdown()
                        && matches!(error, FileApiError::Cancelled)
                    {
                        "shutdown"
                    } else {
                        crate::observability::result_class_for_core(Some(error))
                    };
                    (0, class)
                }
            };
            crate::observability::emit(
                "fs_read",
                trace_len,
                crate::observability::elapsed_ms(trace_start),
                chunks,
                class,
                self.trace_env,
            );
        }
        outcome
    }
}

/// Options for [`FileApiHandle::file_from_bytes`].
#[derive(Debug, Clone, Default)]
pub struct HostFileOptions {
    /// Normalized by the M1 MIME rules.
    pub media_type: String,
    /// `None` reads the injected clock.
    pub last_modified: Option<i64>,
}
