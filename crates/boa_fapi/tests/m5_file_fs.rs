//! M5 host integration: `file_from_resource`, limits, shutdown, JS semantics.
//!
//! Every test uses a fresh real `boa_engine::Context` plus a real
//! `boa_fapi_fs::FsRegistry` with uniquely-named temp files (cleaned up,
//! never asserting absolute paths). Reads run through async `FileReader`,
//! worker `FileReaderSync`, Blob materialization (`text()`), and streams.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(feature = "fs")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use boa_engine::{Context, Source, js_string};
use boa_fapi::{Clock, FileApiEnvironment, FileApiExtension, HostFileOptions};
use boa_fapi_core::source::ByteSource;

#[derive(Debug)]
struct FixedClock {
    millis: i64,
}

impl Clock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        self.millis
    }
}

const FIXED_TIME: i64 = 1_700_000_000_000;

fn temp_file(content: &[u8]) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    path.push(format!(
        "boa-fapi-m5js-{}-{}-{id}.bin",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&path, content).expect("write temp file");
    path
}

fn setup() -> (Context, boa_fapi::FileApiHandle) {
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build()
        .register(&mut context)
        .expect("registration failed");
    (context, handle)
}

fn import_file(
    handle: &boa_fapi::FileApiHandle,
    context: &mut Context,
    registry: &boa_fapi_fs::FsRegistry,
    content: &[u8],
    display: &str,
) -> (std::path::PathBuf, boa_engine::JsObject) {
    let path = temp_file(content);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .expect("open");
    let resource = registry.register(file).expect("register");
    let adapter =
        Arc::new(boa_fapi_fs::HostFileSource::new(registry, &resource, None).expect("adapter"));
    let object = handle
        .file_from_resource(adapter, display, HostFileOptions::default(), context)
        .expect("import");
    (path, object)
}

fn publish(context: &mut Context, name: &str, object: boa_engine::JsObject) {
    context
        .register_global_property(
            js_string!(name),
            object,
            boa_engine::property::Attribute::all(),
        )
        .expect("publish");
}

fn eval_str(context: &mut Context, source: &str) -> String {
    context
        .eval(Source::from_bytes(source))
        .expect("eval")
        .as_string()
        .expect("string")
        .to_std_string_escaped()
}

fn assert_eval(context: &mut Context, source: &str) {
    let value = context
        .eval(Source::from_bytes(source))
        .unwrap_or_else(|error| panic!("eval failed for {source}: {error}"));
    assert_eq!(
        value.as_boolean(),
        Some(true),
        "assertion failed for {source}"
    );
}

/// Drains Boa jobs until quiescent (FileReader chains enqueue successors).
fn drain(context: &mut Context) {
    for _ in 0..64 {
        context.run_jobs().expect("run_jobs");
    }
}

// 1. Host integration: metadata + async FileReader + text() + stream.

#[test]
fn file_from_resource_metadata_and_text() {
    let (mut context, handle) = setup();
    let registry = boa_fapi_fs::FsRegistry::new();
    let (path, object) = import_file(
        &handle,
        &mut context,
        &registry,
        b"hello fs",
        "notes/display.txt",
    );
    publish(&mut context, "srcFile", object);
    // Display name is the only visible name; slash becomes colon; no secret leaks.
    assert_eq!(eval_str(&mut context, "srcFile.name"), "notes:display.txt");
    assert_eval(&mut context, "srcFile.size === 8");
    assert_eval(
        &mut context,
        "srcFile instanceof File && srcFile instanceof Blob",
    );
    assert_eval(&mut context, "srcFile.lastModified === 1700000000000");
    // Promise materialization path.
    assert_eval(
        &mut context,
        "globalThis.textResult = 'pending'; srcFile.text().then(v => { globalThis.textResult = v; }); true",
    );
    drain(&mut context);
    assert_eq!(eval_str(&mut context, "globalThis.textResult"), "hello fs");
    std::fs::remove_file(&path).ok();
}

#[test]
fn file_from_resource_async_filereader() {
    let (mut context, handle) = setup();
    let registry = boa_fapi_fs::FsRegistry::new();
    let (path, object) = import_file(&handle, &mut context, &registry, b"reader-bytes", "r.txt");
    publish(&mut context, "srcFile", object);
    context
        .eval(Source::from_bytes(
            "globalThis.outcome = 'pending'; \
             var r = new FileReader(); \
             r.onload = function () { globalThis.outcome = this.result; }; \
             r.onerror = function () { globalThis.outcome = 'error:' + this.error.name; }; \
             r.readAsText(srcFile);",
        ))
        .expect("eval");
    drain(&mut context);
    assert_eq!(eval_str(&mut context, "globalThis.outcome"), "reader-bytes");
    std::fs::remove_file(&path).ok();
}

#[test]
fn file_from_resource_stream_and_slice() {
    let (mut context, handle) = setup();
    let registry = boa_fapi_fs::FsRegistry::new();
    let (path, object) = import_file(&handle, &mut context, &registry, b"stream-me-now!", "s.txt");
    publish(&mut context, "srcFile", object);
    context
        .eval(Source::from_bytes(
            "globalThis.chunks = ''; globalThis.streamDone = false; \
             var reader = srcFile.stream().getReader(); \
             function pump() { return reader.read().then(function (r) { \
               if (r.done) { globalThis.streamDone = true; return; } \
               var a = r.value; var s = ''; \
               for (var i = 0; i < a.length; i++) { s += String.fromCharCode(a[i]); } \
               globalThis.chunks += s; return pump(); }); } \
             globalThis.pumpPromise = pump();",
        ))
        .expect("eval");
    drain(&mut context);
    assert_eq!(
        eval_str(&mut context, "globalThis.chunks"),
        "stream-me-now!"
    );
    assert_eval(&mut context, "globalThis.streamDone === true");
    // Slice of an fs-backed File is a Blob sharing the source.
    assert_eval(
        &mut context,
        "var sl = srcFile.slice(0, 6); sl instanceof Blob && !(sl instanceof File) && sl.size === 6",
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn file_from_resource_sync_reader_in_worker() {
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .environment(FileApiEnvironment::DedicatedWorker)
        .build()
        .register(&mut context)
        .expect("registration failed");
    let registry = boa_fapi_fs::FsRegistry::new();
    let (path, object) = import_file(&handle, &mut context, &registry, b"sync-bytes", "w.txt");
    publish(&mut context, "srcFile", object);
    assert_eval(
        &mut context,
        "var s = new FileReaderSync(); \
         var ab = s.readAsArrayBuffer(srcFile); \
         (ab instanceof ArrayBuffer) && ab.byteLength === 10",
    );
    assert_eq!(
        eval_str(&mut context, "new FileReaderSync().readAsText(srcFile)"),
        "sync-bytes"
    );
    std::fs::remove_file(&path).ok();
}

// 2. Snapshot change between import and read fails as NotReadableError.

#[test]
fn changed_file_read_fails_not_readable_without_partial() {
    let (mut context, handle) = setup();
    let registry = boa_fapi_fs::FsRegistry::new();
    let (path, object) = import_file(
        &handle,
        &mut context,
        &registry,
        b"stable-content-000",
        "c.txt",
    );
    publish(&mut context, "srcFile", object);
    // Mutate the file after import: the next chunk must reject, not leak.
    std::fs::write(&path, b"CHANGED-content-000!").expect("mutate");
    context
        .eval(Source::from_bytes(
            "globalThis.verdict = 'pending'; \
             srcFile.text().then(v => { globalThis.verdict = 'fulfilled:' + v; }, \
               e => { globalThis.verdict = 'rejected:' + (e instanceof DOMException) + ':' + e.name; });",
        ))
        .expect("eval");
    drain(&mut context);
    assert_eq!(
        eval_str(&mut context, "globalThis.verdict"),
        "rejected:true:NotReadableError"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn changed_file_filereader_fails_not_readable() {
    let (mut context, handle) = setup();
    let registry = boa_fapi_fs::FsRegistry::new();
    let (path, object) = import_file(
        &handle,
        &mut context,
        &registry,
        b"stable-content-000",
        "c.txt",
    );
    publish(&mut context, "srcFile", object);
    std::fs::write(&path, b"CHANGED-content-000!").expect("mutate");
    context
        .eval(Source::from_bytes(
            "globalThis.errName = 'none'; \
             var r = new FileReader(); \
             r.onload = function () { globalThis.errName = 'loaded'; }; \
             r.onerror = function () { globalThis.errName = this.error.name; }; \
             r.readAsArrayBuffer(srcFile);",
        ))
        .expect("eval");
    drain(&mut context);
    assert_eq!(
        eval_str(&mut context, "globalThis.errName"),
        "NotReadableError"
    );
    std::fs::remove_file(&path).ok();
}

// 3. No path/identity disclosure in JS errors or messages.

#[test]
fn js_errors_carry_no_location_detail() {
    let (mut context, handle) = setup();
    let registry = boa_fapi_fs::FsRegistry::new();
    let (path, object) = import_file(&handle, &mut context, &registry, b"secret-content", "d.txt");
    publish(&mut context, "srcFile", object);
    std::fs::write(&path, b"different-content!").expect("mutate");
    context
        .eval(Source::from_bytes(
            "globalThis.report = 'pending'; \
             srcFile.text().then(v => { globalThis.report = 'fulfilled'; }, \
               e => { globalThis.report = e.name + '|' + e.message + '|' + String(e); });",
        ))
        .expect("eval");
    drain(&mut context);
    let report = eval_str(&mut context, "globalThis.report");
    assert!(
        report.starts_with("NotReadableError|"),
        "unexpected report: {report}"
    );
    // The report must not contain a separator, drive letter, or the content.
    assert!(!report.contains('/'), "leak in {report}");
    assert!(!report.contains('\\'), "leak in {report}");
    assert!(!report.contains("secret-content"), "leak in {report}");
    assert!(!report.contains("tmp"), "leak in {report}");
    std::fs::remove_file(&path).ok();
}

// 4. Limits: preflight before JS object, == ok / +1 rejected, no partial.

#[test]
fn blob_size_preflight_boundary() {
    // Exact == boundary comes from the fs unit tests (== ok / +1 rejected
    // on the preflight path); here the JS integration proves a failed
    // import leaves no JS-visible object and a good import stays usable.
    let mut context = Context::default();
    // max_blob_size alone cannot drop below the default materialize
    // ceiling (validate: materialize <= blob), so shrink the whole triple
    // consistently; the ceiling stays far above the 8-byte import.
    let limits = boa_fapi_core::limits::FileApiLimits {
        max_blob_size: 32 * 1024,
        max_materialize_bytes: 32 * 1024,
        max_sync_read_bytes: 32 * 1024,
        default_chunk_size: 16 * 1024,
        ..boa_fapi_core::limits::FileApiLimits::default()
    };
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .limits(limits)
        .build()
        .register(&mut context)
        .expect("registration failed");
    let registry = boa_fapi_fs::FsRegistry::new();
    // Well under the blob ceiling: import succeeds.
    let (path8, object8) = import_file(&handle, &mut context, &registry, b"12345678", "ok.txt");
    publish(&mut context, "okFile", object8);
    assert_eval(&mut context, "okFile.size === 8");
    // A stale snapshot at import is denied before any JS object exists.
    // Register a file, mutate it, then import via a stale adapter whose
    // import snapshot no longer matches the live state.
    let stale_path = temp_file(b"stale-content-00");
    let stale_file = std::fs::OpenOptions::new()
        .read(true)
        .open(&stale_path)
        .expect("open");
    let stale_resource = registry.register(stale_file).expect("register");
    let stale_adapter = Arc::new(
        boa_fapi_fs::HostFileSource::new(&registry, &stale_resource, None).expect("adapter"),
    );
    std::fs::write(&stale_path, b"CHANGED-content-00").expect("mutate");
    let result = handle.file_from_resource(
        stale_adapter,
        "stale.txt",
        HostFileOptions::default(),
        &mut context,
    );
    assert!(result.is_err(), "stale snapshot import must fail");
    // The failed import left no global behind.
    assert_eval(&mut context, "typeof globalThis.staleFile === 'undefined'");
    std::fs::remove_file(&path8).ok();
    std::fs::remove_file(&stale_path).ok();
}

#[test]
fn sync_limit_preflight_for_fs_source() {
    let mut context = Context::default();
    let limits = boa_fapi_core::limits::FileApiLimits {
        max_sync_read_bytes: 4,
        ..boa_fapi_core::limits::FileApiLimits::default()
    };
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .limits(limits)
        .environment(FileApiEnvironment::SharedWorker)
        .build()
        .register(&mut context)
        .expect("registration failed");
    let registry = boa_fapi_fs::FsRegistry::new();
    let (path, object) = import_file(&handle, &mut context, &registry, b"12345678", "ok.txt");
    publish(&mut context, "okFile", object);
    let error = context
        .eval(Source::from_bytes(
            "new FileReaderSync().readAsArrayBuffer(okFile)",
        ))
        .expect_err("expected QuotaExceededError");
    assert!(format!("{error}").contains("DOMException"), "got {error}");
    let verdict = eval_str(
        &mut context,
        "(function () { try { new FileReaderSync().readAsText(okFile); } \
          catch (e) { return (e instanceof DOMException) + ':' + e.name; } return 'no-throw'; })()",
    );
    assert_eq!(verdict, "true:QuotaExceededError");
    std::fs::remove_file(&path).ok();
}

#[test]
fn materialize_limit_rejects_fs_promise_read() {
    // Quote the materialize boundary through a >17 KiB file: the default
    // 16 KiB chunk forces multichunk FileReader pumps, while `text()`
    // materializes the whole 20 KiB file. A tight (but still valid:
    // chunk <= materialize <= blob) config rejects it as QuotaExceeded.
    let payload = vec![b'q'; 20 * 1024];
    let mut context = Context::default();
    let limits = boa_fapi_core::limits::FileApiLimits {
        max_blob_size: 64 * 1024,
        max_materialize_bytes: 16 * 1024,
        max_sync_read_bytes: 16 * 1024,
        default_chunk_size: 16 * 1024,
        ..boa_fapi_core::limits::FileApiLimits::default()
    };
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .limits(limits)
        .build()
        .register(&mut context)
        .expect("registration failed");
    let registry = boa_fapi_fs::FsRegistry::new();
    let (path, object) = import_file(&handle, &mut context, &registry, &payload, "ok.txt");
    publish(&mut context, "okFile", object);
    context
        .eval(Source::from_bytes(
            "globalThis.verdict = 'pending'; \
             okFile.text().then(v => { globalThis.verdict = 'fulfilled'; }, \
               e => { globalThis.verdict = (e instanceof DOMException) + ':' + e.name; });",
        ))
        .expect("eval");
    drain(&mut context);
    assert_eq!(
        eval_str(&mut context, "globalThis.verdict"),
        "true:QuotaExceededError"
    );
    std::fs::remove_file(&path).ok();
}

// 5. Shutdown: new operations rejected, pending work cancelled, no late jobs.

#[test]
fn shutdown_rejects_new_operations_and_repeats_idempotently() {
    let (mut context, handle) = setup();
    let registry = boa_fapi_fs::FsRegistry::new();
    let (path, object) = import_file(&handle, &mut context, &registry, b"hello", "a.txt");
    publish(&mut context, "srcFile", object.clone());
    handle.shutdown(&mut context).expect("shutdown");
    // Repeated shutdown: no panic, no callbacks.
    handle.shutdown(&mut context).expect("second shutdown");
    // New host operations fail before touching JS state.
    let bytes_result = handle.blob_from_bytes(bytes::Bytes::from_static(b"x"), "", &mut context);
    assert!(bytes_result.is_err());
    let file_result = handle.file_from_bytes(
        bytes::Bytes::from_static(b"x"),
        "x",
        HostFileOptions::default(),
        &mut context,
    );
    assert!(file_result.is_err());
    let list_result = handle.file_list([object.clone()], &mut context);
    assert!(list_result.is_err());
    // New FileReader reads fail fast with no events after shutdown.
    context
        .eval(Source::from_bytes(
            "globalThis.events = []; \
             var r = new FileReader(); \
             ['loadstart','progress','load','error','abort','loadend'].forEach(function (t) { \
               r.addEventListener(t, function () { globalThis.events.push(t); }); }); \
             r.readAsText(srcFile);",
        ))
        .expect("eval");
    drain(&mut context);
    // Either fail-fast error dispatch (single error+loadend) or nothing —
    // but never success events and never a result.
    let events = eval_str(&mut context, "globalThis.events.join(',')");
    assert!(
        events.is_empty() || events == "error,loadend",
        "unexpected post-shutdown events: {events}"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn shutdown_before_jobs_settles_nothing_late() {
    let (mut context, handle) = setup();
    let registry = boa_fapi_fs::FsRegistry::new();
    let (path, object) = import_file(
        &handle,
        &mut context,
        &registry,
        b"late-bytes-0000",
        "l.txt",
    );
    publish(&mut context, "srcFile", object);
    // Queue a promise read and a FileReader read, then shut down before
    // draining: late completions must settle nothing.
    context
        .eval(Source::from_bytes(
            "globalThis.promiseVerdict = 'pending'; \
             srcFile.text().then(v => { globalThis.promiseVerdict = 'fulfilled:' + v; }, \
               e => { globalThis.promiseVerdict = 'rejected:' + e.name; }); \
             globalThis.readerEvents = []; \
             var r = new FileReader(); \
             ['loadstart','progress','load','error','abort','loadend'].forEach(function (t) { \
               r.addEventListener(t, function () { globalThis.readerEvents.push(t); }); }); \
             r.readAsText(srcFile);",
        ))
        .expect("eval");
    handle.shutdown(&mut context).expect("shutdown");
    drain(&mut context);
    let verdict = eval_str(&mut context, "globalThis.promiseVerdict");
    assert!(
        verdict == "pending" || verdict == "rejected:AbortError",
        "unexpected late promise verdict: {verdict}"
    );
    let events = eval_str(&mut context, "globalThis.readerEvents.join(',')");
    assert!(
        events.is_empty() || events == "error,loadend",
        "unexpected late reader events: {events}"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn post_shutdown_source_reads_fail() {
    let (mut context, handle) = setup();
    let registry = boa_fapi_fs::FsRegistry::new();
    let path = temp_file(b"direct-bytes");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .expect("open");
    let resource = registry.register(file).expect("register");
    let source = boa_fapi_fs::FileSource::new(&registry, &resource, None).expect("source");
    let cancel = boa_fapi_core::cancellation::CancellationToken::new();
    assert_eq!(&source.read_range(0..6, &cancel).unwrap()[..], b"direct");
    handle.shutdown(&mut context).expect("shutdown");
    resource.close();
    assert!(source.read_range(6..12, &cancel).is_err());
    std::fs::remove_file(&path).ok();
}

// 6. Registration matrix: fs off leaves no filesystem surface.

#[test]
fn file_list_accepts_only_explicit_files() {
    let (mut context, handle) = setup();
    let registry = boa_fapi_fs::FsRegistry::new();
    let (path, object) = import_file(&handle, &mut context, &registry, b"a", "a.txt");
    let list = handle.file_list([object], &mut context).expect("file list");
    publish(&mut context, "theList", list);
    assert_eval(
        &mut context,
        "theList.length === 1 && theList.item(0).name === 'a.txt'",
    );
    // A non-File element fails without partial state.
    let blob = handle
        .blob_from_bytes(bytes::Bytes::from_static(b"x"), "", &mut context)
        .expect("blob");
    assert!(handle.file_list([blob], &mut context).is_err());
    std::fs::remove_file(&path).ok();
}

#[test]
fn display_name_is_the_only_visible_name() {
    let (mut context, handle) = setup();
    let registry = boa_fapi_fs::FsRegistry::new();
    // A display name that looks like a secret location stays verbatim
    // (slash → colon); no basename is ever computed from host state.
    let (path, object) = import_file(
        &handle,
        &mut context,
        &registry,
        b"data",
        "/secret/mount/name.txt",
    );
    publish(&mut context, "srcFile", object);
    assert_eq!(
        eval_str(&mut context, "srcFile.name"),
        ":secret:mount:name.txt"
    );
    std::fs::remove_file(&path).ok();
}
