//! M8 observability: optional `tracing` terminal telemetry (default off).
//!
//! Collector-specific tests; the whole file compiles only with the
//! `tracing` feature. A separate `m8_feature_off` guard covers the
//! feature-off build.

#![cfg(feature = "tracing")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use boa_engine::{Context, Source, js_string};
use boa_fapi::{
    CloneAdapter, CloneBridgeDescriptor, FileApiEnvironment, FileApiExtension, UrlEntropySource,
};
use boa_fapi_core::clone::{CLONE_ENCODING_VERSION, CloneError, FileApiClonePayload};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

// ── test-only collector (std only, no tracing-subscriber) ──

const TARGET: &str = "boa_fapi::file_api.operation";

const ALLOWED_FIELDS: [&str; 6] = [
    "operation",
    "size",
    "duration_ms",
    "chunk_count",
    "result_class",
    "environment_hash",
];

const ALLOWED_OPERATIONS: [&str; 9] = [
    "promise_read",
    "stream_read",
    "filereader_read",
    "filereader_sync",
    "fs_read",
    "blob_url_create",
    "blob_url_resolve",
    "clone_encode",
    "clone_decode",
];

const ALLOWED_CLASSES: [&str; 10] = [
    "ok",
    "cancelled",
    "quota",
    "not_found",
    "permission",
    "snapshot_changed",
    "invalid_range",
    "encoding",
    "shutdown",
    "error",
];

#[derive(Debug, Clone, Default)]
struct RecordedEvent {
    target: String,
    fields: HashMap<String, String>,
}

impl RecordedEvent {
    fn serialized(&self) -> String {
        let mut parts: Vec<String> = self
            .fields
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        parts.sort();
        format!("target={} {}", self.target, parts.join(" "))
    }
}

#[derive(Debug, Default)]
struct FieldCapture {
    fields: HashMap<String, String>,
}

impl Visit for FieldCapture {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_owned(), value.to_owned());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_owned(), format!("{value:?}"));
    }
}

#[derive(Debug, Clone, Default)]
struct TestCollector {
    events: Arc<Mutex<Vec<RecordedEvent>>>,
}

impl TestCollector {
    fn new() -> (Self, Arc<Mutex<Vec<RecordedEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                events: Arc::clone(&events),
            },
            events,
        )
    }
}

impl Subscriber for TestCollector {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == TARGET
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        if event.metadata().target() != TARGET {
            return;
        }
        let mut capture = FieldCapture::default();
        event.record(&mut capture);
        let recorded = RecordedEvent {
            target: event.metadata().target().to_owned(),
            fields: capture.fields,
        };
        if let Ok(mut events) = self.events.lock() {
            events.push(recorded);
        }
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

fn with_collector<T>(collector: TestCollector, f: impl FnOnce() -> T) -> T {
    tracing::subscriber::with_default(collector, f)
}

fn snapshot_events(handle: &Arc<Mutex<Vec<RecordedEvent>>>) -> Vec<RecordedEvent> {
    handle.lock().map(|e| e.clone()).unwrap_or_default()
}

fn assert_allowlisted(events: &[RecordedEvent]) {
    assert!(!events.is_empty(), "expected at least one telemetry event");
    let allowed: HashSet<&str> = ALLOWED_FIELDS.into_iter().collect();
    let ops: HashSet<&str> = ALLOWED_OPERATIONS.into_iter().collect();
    let classes: HashSet<&str> = ALLOWED_CLASSES.into_iter().collect();
    for event in events {
        assert_eq!(event.target, TARGET, "unexpected target");
        for key in event.fields.keys() {
            assert!(
                allowed.contains(key.as_str()),
                "non-allowlisted field: {key}"
            );
        }
        for required in ALLOWED_FIELDS {
            assert!(
                event.fields.contains_key(required),
                "missing field {required}"
            );
        }
        let op = event.fields.get("operation").expect("operation");
        assert!(ops.contains(op.as_str()), "bad operation: {op}");
        let class = event.fields.get("result_class").expect("result_class");
        assert!(
            classes.contains(class.as_str()),
            "bad result_class: {class}"
        );
        // Types/ranges only: u64 presence, never exact cross-run values.
        for numeric in ["size", "duration_ms", "chunk_count", "environment_hash"] {
            let raw = event.fields.get(numeric).expect(numeric);
            raw.parse::<u64>()
                .unwrap_or_else(|_| panic!("{numeric} must be u64, got {raw}"));
        }
    }
}

// ── fixtures ──

#[derive(Debug)]
struct FixedClock {
    millis: i64,
}

impl boa_fapi::Clock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        self.millis
    }
}

#[derive(Debug, Default)]
struct CounterEntropy {
    next: AtomicU64,
}

impl UrlEntropySource for CounterEntropy {
    fn fill_16(&self) -> [u8; 16] {
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        let mut out = [0xA5_u8; 16];
        out[..8].copy_from_slice(&n.to_be_bytes());
        if out == [0_u8; 16] {
            out[0] = 1;
        }
        out
    }
}

fn setup_default() -> (Context, boa_fapi::FileApiHandle) {
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock {
            millis: 1_700_000_000_000,
        }))
        .entropy(Arc::new(CounterEntropy::default()))
        .build()
        .register(&mut context)
        .expect("registration failed");
    (context, handle)
}

fn setup_worker() -> (Context, boa_fapi::FileApiHandle) {
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock {
            millis: 1_700_000_000_000,
        }))
        .entropy(Arc::new(CounterEntropy::default()))
        .environment(FileApiEnvironment::DedicatedWorker)
        .build()
        .register(&mut context)
        .expect("registration failed");
    (context, handle)
}

#[derive(Debug)]
struct TestBridge;

impl CloneAdapter for TestBridge {
    fn descriptor(&self) -> CloneBridgeDescriptor {
        CloneBridgeDescriptor {
            name: String::from("m8-test-bridge"),
            version: CLONE_ENCODING_VERSION,
        }
    }

    fn encode(&self, payload: &FileApiClonePayload) -> Result<Vec<u8>, CloneError> {
        payload.encode()
    }

    fn decode(&self, bytes: &[u8]) -> Result<FileApiClonePayload, CloneError> {
        FileApiClonePayload::decode(bytes)
    }
}

fn setup_with_bridge() -> (Context, boa_fapi::FileApiHandle) {
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock {
            millis: 1_700_000_000_000,
        }))
        .entropy(Arc::new(CounterEntropy::default()))
        .clone_adapter(Arc::new(TestBridge))
        .build()
        .register(&mut context)
        .expect("registration failed");
    (context, handle)
}

fn run_jobs(context: &mut Context, handle: &boa_fapi::FileApiHandle) {
    for _ in 0..200 {
        let settled = handle.poll_io(context).unwrap_or(0);
        context.run_jobs().expect("run_jobs");
        context.run_jobs().expect("run_jobs");
        if settled == 0 && !handle.has_pending_io() {
            break;
        }
        if handle.has_pending_io() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5);
            while handle.has_pending_io() {
                let _ = handle.poll_io(context);
                if !handle.has_pending_io() {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::yield_now();
            }
        }
    }
}

/// Host-loop drain for stream paths (M9-D worker I/O).
#[allow(dead_code)]
fn run_jobs_boa(context: &mut Context, handle: &boa_fapi::FileApiHandle) {
    run_jobs(context, handle);
}

fn eval(context: &mut Context, source: &str) -> String {
    context
        .eval(Source::from_bytes(source))
        .expect("eval failed")
        .to_string(context)
        .expect("to_string")
        .to_std_string_escaped()
}

// Fake filesystem resource for snapshot/invalid-range paths (Unix live path).
// On Windows the import itself is refused (copy-or-deny); tests handle both.
struct FakeResource {
    id: boa_fapi_core::policy::HostResourceId,
    import: boa_fapi_core::snapshot::SnapshotState,
    live: Mutex<boa_fapi_core::snapshot::SnapshotState>,
    bytes: Vec<u8>,
    short_on_read: bool,
}

impl FakeResource {
    fn new_filesystem(size: u64, short_on_read: bool) -> Self {
        let snapshot = boa_fapi_core::snapshot::SnapshotState::Filesystem(
            boa_fapi_core::snapshot::FileSnapshot::new(
                0xC0FFEE,
                size,
                Some(1_700_000_000),
                Some(0),
            ),
        );
        Self {
            id: boa_fapi_core::policy::HostResourceId::new(7),
            import: snapshot.clone(),
            live: Mutex::new(snapshot),
            bytes: vec![9_u8; size as usize],
            short_on_read,
        }
    }

    fn mutate_snapshot(&self) {
        let mut live = self.live.lock().expect("live lock");
        *live = boa_fapi_core::snapshot::SnapshotState::Filesystem(
            boa_fapi_core::snapshot::FileSnapshot::new(0xDEAD, 1, Some(1), Some(0)),
        );
    }
}

impl boa_fapi_core::policy::FileResource for FakeResource {
    fn read_at(
        &self,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>, boa_fapi_core::file_api_error::FileApiError> {
        if self.short_on_read {
            return Ok(vec![0_u8; len.saturating_sub(1)]);
        }
        let start = usize::try_from(offset)
            .map_err(|_| boa_fapi_core::file_api_error::FileApiError::InvalidRange)?;
        let end = start
            .checked_add(len)
            .ok_or(boa_fapi_core::file_api_error::FileApiError::InvalidRange)?;
        self.bytes
            .get(start..end)
            .map(|s| s.to_vec())
            .ok_or(boa_fapi_core::file_api_error::FileApiError::InvalidRange)
    }

    fn current_snapshot(
        &self,
    ) -> Result<boa_fapi_core::snapshot::SnapshotState, boa_fapi_core::file_api_error::FileApiError>
    {
        Ok(self.live.lock().expect("live lock").clone())
    }

    fn import_snapshot(&self) -> boa_fapi_core::snapshot::SnapshotState {
        self.import.clone()
    }

    fn resource_id(&self) -> boa_fapi_core::policy::HostResourceId {
        self.id
    }

    fn close(&self) {}
}

// ── 1. allow-listed fields across every operation ──

#[test]
fn tracing_emits_only_allowlisted_fields() {
    let (collector, events) = TestCollector::new();
    with_collector(collector, || {
        // Memory promise reads.
        let (mut context, handle) = setup_default();
        context
            .eval(Source::from_bytes(
                "globalThis.out = 'pending'; \
                 new Blob(['hello']).text().then(v => { globalThis.out = v; });",
            ))
            .expect("eval");
        run_jobs(&mut context, &handle);
        assert_eq!(eval(&mut context, "globalThis.out"), "hello");
        context
            .eval(Source::from_bytes(
                "globalThis.n = -1; \
                 new Blob(['abc']).bytes().then(a => { globalThis.n = a.length; });",
            ))
            .expect("eval");
        run_jobs(&mut context, &handle);
        assert_eq!(eval(&mut context, "globalThis.n"), "3");

        // Stream read (one demand chunk + EOF): M9-D host loop (`poll_io`
        // turns the worker completion into a Boa job).
        context
            .eval(Source::from_bytes(
                "globalThis.chunks = 0; globalThis.done = false; \
                 globalThis.reader = new Blob(['stream-me']).stream().getReader(); \
                 globalThis.reader.read().then(r => { \
                     globalThis.chunks = r.value.length; globalThis.done = r.done; });",
            ))
            .expect("eval");
        run_jobs(&mut context, &handle);
        assert_eq!(eval(&mut context, "globalThis.chunks"), "9");
        assert_eq!(eval(&mut context, "globalThis.done"), "false");

        // Async FileReader (M9-C host loop: `poll_io` drains the worker
        // chunk into a pump job).
        context
            .eval(Source::from_bytes(
                "globalThis.text = null; \
                 var reader = new FileReader(); \
                 reader.onload = function () { globalThis.text = this.result; }; \
                 reader.readAsText(new Blob(['async-ok']));",
            ))
            .expect("eval");
        run_jobs(&mut context, &handle);
        assert_eq!(eval(&mut context, "globalThis.text"), "async-ok");

        // Sync FileReaderSync (worker env).
        let (mut worker, _) = setup_worker();
        worker
            .eval(Source::from_bytes(
                "globalThis.syncText = (new FileReaderSync()).readAsText(new Blob(['sync-ok']));",
            ))
            .expect("eval");
        assert_eq!(eval(&mut worker, "globalThis.syncText"), "sync-ok");

        // Blob URL create / resolve / revoke (host API: no JS fetch).
        let (mut url_context, url_handle) = setup_default();
        let blob = url_handle
            .blob_from_bytes(
                bytes::Bytes::from_static(b"url-bytes"),
                "text/plain",
                &mut url_context,
            )
            .expect("host blob");
        url_context
            .register_global_property(
                js_string!("srcBlob"),
                blob,
                boa_engine::property::Attribute::all(),
            )
            .expect("publish");
        let url = url_context
            .eval(Source::from_bytes("URL.createObjectURL(srcBlob)"))
            .expect("create")
            .as_string()
            .expect("url string")
            .to_std_string_escaped();
        assert!(url.starts_with("blob:"), "unexpected url {url}");
        let resolved = url_handle.resolve_blob_url(&url).expect("resolve");
        assert_eq!(resolved.size(), 9);
        url_context
            .eval(Source::from_bytes("URL.revokeObjectURL('blob:https://localhost/00000000-0000-4000-8000-000000000000')"))
            .expect("revoke");

        // Structured clone encode / decode (host API).
        let (mut clone_context, clone_handle) = setup_default();
        let live = clone_handle
            .blob_from_bytes(
                bytes::Bytes::from_static(b"clone-me"),
                "text/plain",
                &mut clone_context,
            )
            .expect("host blob");
        let payload = clone_handle.clone_blob(&live).expect("encode");
        let _ = clone_handle
            .blob_from_clone(&payload, &mut clone_context)
            .expect("decode");

        // Bridge encode/decode must report logical payload size, not wire size.
        let (mut bridge_context, bridge_handle) = setup_with_bridge();
        let bridge_blob = bridge_handle
            .blob_from_bytes(
                bytes::Bytes::from_static(b"bridge-bytes"),
                "text/plain",
                &mut bridge_context,
            )
            .expect("bridge blob");
        let bridge_payload = bridge_handle
            .clone_blob(&bridge_blob)
            .expect("bridge payload");
        let bridge_bytes = bridge_handle
            .clone_encode_via_bridge(&bridge_payload)
            .expect("bridge encode");
        let _ = bridge_handle
            .clone_decode_via_bridge(&bridge_bytes)
            .expect("bridge decode");
    });
    let events = snapshot_events(&events);
    assert_allowlisted(&events);
    // Every operation family emitted at least once.
    let ops: HashSet<String> = events
        .iter()
        .map(|e| e.fields.get("operation").expect("op").clone())
        .collect();
    for required in [
        "promise_read",
        "stream_read",
        "filereader_read",
        "filereader_sync",
        "blob_url_create",
        "blob_url_resolve",
        "clone_encode",
        "clone_decode",
    ] {
        assert!(
            ops.contains(required),
            "missing operation {required}: {ops:?}"
        );
    }
    let url_create = events
        .iter()
        .find(|event| event.fields.get("operation") == Some(&String::from("blob_url_create")))
        .expect("blob_url_create event");
    assert_eq!(url_create.fields.get("size"), Some(&String::from("9")));
    let bridge_encode = events
        .iter()
        .filter(|event| event.fields.get("operation") == Some(&String::from("clone_encode")))
        .find(|event| event.fields.get("size") == Some(&String::from("12")))
        .expect("clone_encode must report the 12-byte logical payload");
    assert_eq!(
        bridge_encode.fields.get("result_class"),
        Some(&String::from("ok"))
    );
}

// ── 2. terminal result classes ──

#[test]
fn tracing_emits_terminal_result_classes() {
    let (collector, events) = TestCollector::new();
    with_collector(collector, || {
        // success (ok).
        let (mut context, handle) = setup_default();
        context
            .eval(Source::from_bytes(
                "new Blob(['ok']).text().then(() => {});",
            ))
            .expect("eval");
        run_jobs(&mut context, &handle);

        // quota: sync ceiling rejects a 5-byte read (valid config: sync <= materialize).
        let mut tight_context = Context::default();
        let tight_limits = boa_fapi_core::limits::FileApiLimits {
            max_sync_read_bytes: 4,
            ..Default::default()
        };
        FileApiExtension::builder()
            .clock(Arc::new(FixedClock {
                millis: 1_700_000_000_000,
            }))
            .limits(tight_limits)
            .entropy(Arc::new(CounterEntropy::default()))
            .environment(FileApiEnvironment::DedicatedWorker)
            .build()
            .register(&mut tight_context)
            .expect("register");
        tight_context
            .eval(Source::from_bytes(
                "try { (new FileReaderSync()).readAsText(new Blob(['hello'])); } catch (e) {}",
            ))
            .expect("eval");

        // cancel: abort a LOADING FileReader.
        let (mut cancel_context, cancel_handle) = setup_default();
        cancel_context
            .eval(Source::from_bytes(
                "globalThis.reader = new FileReader(); \
                 reader.readAsArrayBuffer(new Blob(['cancel-me'])); \
                 reader.abort();",
            ))
            .expect("eval");
        run_jobs(&mut cancel_context, &cancel_handle);

        // encoding: a label resolving to the replacement encoding still
        // succeeds (every byte decodes to U+FFFD) with class "encoding".
        let (mut enc_context, enc_handle) = setup_default();
        enc_context
            .eval(Source::from_bytes(
                "globalThis.reader2 = new FileReader(); \
                 reader2.readAsText(new Blob(['x']), 'csiso2022kr');",
            ))
            .expect("eval");
        run_jobs(&mut enc_context, &enc_handle);

        // invalid range + snapshot-changed via fake resources (Unix live).
        // On Windows the import is refused (permission); both are allow-listed.
        {
            use boa_fapi_fs::FsRegistry;
            let registry = FsRegistry::new();
            let (mut fs_context, fs_handle) = setup_default();
            let short = Arc::new(FakeResource::new_filesystem(8, true));
            match fs_handle.file_from_resource(
                &registry,
                short,
                "short.bin",
                Default::default(),
                &mut fs_context,
            ) {
                Ok(object) => {
                    fs_context
                        .register_global_property(
                            js_string!("fsBlob"),
                            object,
                            boa_engine::property::Attribute::all(),
                        )
                        .expect("publish");
                    fs_context
                        .eval(Source::from_bytes("fsBlob.text().then(()=>{},()=>{});"))
                        .expect("eval");
                    run_jobs(&mut fs_context, &fs_handle);
                }
                Err(_) => {
                    // Windows copy-or-deny: permission path exercised instead.
                }
            }
        }
        {
            use boa_fapi_fs::FsRegistry;
            let registry = FsRegistry::new();
            let (mut fs_context, fs_handle) = setup_default();
            let resource = Arc::new(FakeResource::new_filesystem(4, false));
            if let Ok(object) = fs_handle.file_from_resource(
                &registry,
                Arc::clone(&resource) as Arc<dyn boa_fapi_core::policy::FileResource>,
                "mut.bin",
                Default::default(),
                &mut fs_context,
            ) {
                resource.mutate_snapshot();
                fs_context
                    .register_global_property(
                        js_string!("mutBlob"),
                        object,
                        boa_engine::property::Attribute::all(),
                    )
                    .expect("publish");
                fs_context
                    .eval(Source::from_bytes("mutBlob.text().then(()=>{},()=>{});"))
                    .expect("eval");
                run_jobs(&mut fs_context, &fs_handle);
            }
        }

        // foreign/missing URL (opaque failure).
        let (_, url_handle) = setup_default();
        let _ = url_handle
            .resolve_blob_url("blob:https://localhost/00000000-0000-4000-8000-000000000000");

        // clone decode failure (kind mismatch).
        let (mut clone_context, clone_handle) = setup_default();
        let file = clone_handle
            .file_from_bytes(
                bytes::Bytes::from_static(b"f"),
                "a.txt",
                Default::default(),
                &mut clone_context,
            )
            .expect("host file");
        let file_payload = clone_handle.clone_file(&file).expect("encode file");
        let _ = clone_handle.blob_from_clone(&file_payload, &mut clone_context);

        // shutdown: encode after shutdown.
        let (mut shut_context, shut_handle) = setup_default();
        let live = shut_handle
            .blob_from_bytes(
                bytes::Bytes::from_static(b"s"),
                "text/plain",
                &mut shut_context,
            )
            .expect("host blob");
        shut_handle.shutdown(&mut shut_context).expect("shutdown");
        let _ = shut_handle.clone_blob(&live);
        let _ = shut_handle.create_blob_url(&live);
        let _ = shut_context;
    });
    let events = snapshot_events(&events);
    assert_allowlisted(&events);
    let classes: HashSet<String> = events
        .iter()
        .map(|e| e.fields.get("result_class").expect("class").clone())
        .collect();
    for required in ["ok", "quota", "cancelled", "shutdown", "error"] {
        assert!(
            classes.contains(required),
            "missing result_class {required}: {classes:?}"
        );
    }
    assert!(events.iter().any(|event| {
        event.fields.get("operation") == Some(&String::from("blob_url_create"))
            && event.fields.get("result_class") == Some(&String::from("shutdown"))
    }));
    // not_found (foreign URL) is platform-independent; snapshot/invalid are
    // Unix-live (Windows yields permission at import instead — also allowed).
    assert!(
        classes.contains("not_found") || classes.contains("error"),
        "foreign URL must map to not_found or error: {classes:?}"
    );
    #[cfg(unix)]
    {
        assert!(
            classes.contains("snapshot_changed") || classes.contains("invalid_range"),
            "unix fs paths must surface snapshot/invalid classes: {classes:?}"
        );
    }
}

// ── 3. secrecy ──

#[test]
fn tracing_never_leaks_sensitive_values() {
    let sensitive_name = "..\\..\\secret\\C:\\Windows\\..\\passwd\u{2603}.txt";
    let sensitive_origin = "https://sensitive-origin.example";
    let sensitive_error = "NotReadableError: top-secret-marker-xyz";
    let (collector, events) = TestCollector::new();
    with_collector(collector, || {
        let mut context = Context::default();
        let handle = FileApiExtension::builder()
            .clock(Arc::new(FixedClock {
                millis: 1_700_000_000_000,
            }))
            .entropy(Arc::new(CounterEntropy::default()))
            .origin(sensitive_origin)
            .partition(0x5EED_1234)
            .nonce(0x99AA)
            .build()
            .register(&mut context)
            .expect("register");
        let _ = handle.blob_from_bytes(
            bytes::Bytes::from_static(b"secret-bytes-marker-xyz"),
            "text/plain",
            &mut context,
        );
        let file = handle
            .file_from_bytes(
                bytes::Bytes::from_static(b"payload"),
                sensitive_name,
                Default::default(),
                &mut context,
            )
            .expect("host file");
        context
            .register_global_property(
                js_string!("sensitiveFile"),
                file,
                boa_engine::property::Attribute::all(),
            )
            .expect("publish");
        context
            .eval(Source::from_bytes(
                "sensitiveFile.text().then(()=>{},()=>{}); \
                 var r = new FileReader(); \
                 r.readAsText(sensitiveFile, 'csiso2022kr'); \
                 r.abort(); \
                 r.readAsText(sensitiveFile);",
            ))
            .expect("eval");
        run_jobs(&mut context, &handle);
        // Fake URL/UUID + snapshot marker + error-like text through resolves.
        let _ =
            handle.resolve_blob_url("blob:https://localhost/123e4567-e89b-42d3-a456-426614174000");
        let _ = sensitive_error;
    });
    let events = snapshot_events(&events);
    assert_allowlisted(&events);
    let serialized: Vec<String> = events.iter().map(|e| e.serialized()).collect();
    let joined = serialized.join("\n");
    for secret in [
        sensitive_name,
        sensitive_origin,
        "secret-bytes-marker-xyz",
        "123e4567-e89b-42d3-a456-426614174000",
        "top-secret-marker-xyz",
    ] {
        assert!(
            !joined.contains(secret),
            "sensitive value leaked: {secret}\n{joined}"
        );
    }
    // Opaque hash present, never compared to a fixed number.
    for event in &events {
        event
            .fields
            .get("environment_hash")
            .expect("env hash")
            .parse::<u64>()
            .expect("opaque u64");
    }
}

// ── 4. ordering + stale suppression ──

#[test]
fn tracing_preserves_async_order_and_stale_suppression() {
    let (collector, events) = TestCollector::new();
    let js_log = with_collector(collector, || {
        let (mut context, handle) = setup_default();
        context
            .eval(Source::from_bytes(
                "globalThis.log = []; \
                 globalThis.good = new Blob(['new']); \
                 globalThis.reader = new FileReader(); \
                 reader.addEventListener('loadstart', function () { \
                     globalThis.log.push('loadstart'); \
                     if (globalThis.armed !== false) { \
                         globalThis.armed = false; \
                         this.abort(); \
                         this.readAsText(globalThis.good); \
                     } \
                 }); \
                 for (var t of ['progress','load','error','abort','loadend']) \
                     reader.addEventListener(t, (function (tt) { \
                         return function () { globalThis.log.push(tt); }; \
                     })(t)); \
                 globalThis.armed = true; \
                 reader.readAsArrayBuffer(new Blob(['old']));",
            ))
            .expect("start read");
        run_jobs(&mut context, &handle);
        run_jobs(&mut context, &handle);
        eval(&mut context, "globalThis.log.join('|')")
    });
    assert_eq!(
        js_log, "loadstart|loadstart|progress|load|loadend",
        "M7 ordering must be preserved"
    );
    let events = snapshot_events(&events);
    assert_allowlisted(&events);
    let filereader_events: Vec<&RecordedEvent> = events
        .iter()
        .filter(|e| {
            e.fields
                .get("operation")
                .is_some_and(|op| op == "filereader_read")
        })
        .collect();
    // One abort (cancelled) + one restarted success (ok); the stale
    // completion emits nothing.
    assert_eq!(
        filereader_events.len(),
        2,
        "stale completion must not emit: {filereader_events:?}"
    );
    let mut classes: Vec<&str> = filereader_events
        .iter()
        .map(|e| e.fields.get("result_class").expect("class").as_str())
        .collect();
    classes.sort_unstable();
    assert_eq!(classes, vec!["cancelled", "ok"]);
}

// ── 5. feature-on surface guard (manifest + no JS change) ──

#[test]
fn tracing_feature_off_has_no_trace_surface() {
    // With `tracing` compiled in, the default build still exposes no new
    // JS surface and no collector export: the feature is instrumentation
    // only. The complementary `m8_feature_off` test covers the
    // `--no-default-features`-style build without the dependency.
    assert!(cfg!(feature = "tracing"));
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
    .expect("read manifest");
    assert!(
        manifest.contains("tracing = [\"dep:tracing\"]"),
        "tracing feature must be dep-only"
    );
    assert!(
        !manifest.contains("default = [\"streams-shim\", \"dom-shim\", \"fs\", \"url-shim\", \"structured-clone\", \"tracing\"]"),
        "tracing must stay off by default"
    );
    let (mut context, _) = setup_default();
    assert_eq!(eval(&mut context, "typeof Blob"), "function");
    assert_eq!(eval(&mut context, "typeof FileReader"), "function");
    // No trace-specific global leaks into JS.
    assert_eq!(eval(&mut context, "typeof observability"), "undefined");
}
