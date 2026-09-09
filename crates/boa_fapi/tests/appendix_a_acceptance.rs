//! M7 appendix-A acceptance: the observable M1–M6 surface in one suite.
//!
//! Every test uses a fresh real `boa_engine::Context`, executes real
//! JavaScript, and drives settlement explicitly with `context.run_jobs()`.
//! Assertions observe JS values only (brands, prototypes, descriptors,
//! sizes, events, URLs as opaque strings, clone round-trips) — never
//! implementation internals as oracle. Realm/job ordering is asserted
//! where the normative contract pins it (promise settlement only after
//! jobs run, FileReader events only after jobs run).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use boa_engine::{Context, Source, js_string};
use boa_fapi::{Clock, FileApiEnvironment, FileApiExtension, HostFileOptions};

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

fn setup() -> (Context, boa_fapi::FileApiHandle) {
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build()
        .register(&mut context)
        .expect("registration failed");
    (context, handle)
}

fn assert_eval(context: &mut Context, source: &str) {
    let value = context
        .eval(Source::from_bytes(source))
        .unwrap_or_else(|error| panic!("eval failed for {source}: {error}"));
    assert!(
        value.to_boolean(),
        "JS assertion failed: {source} (got {value:?})"
    );
}

/// Evaluates `source` for side effects only (assignment, URL creation):
/// asserts evaluation succeeds, ignoring the completion value.
fn eval_side_effect(context: &mut Context, source: &str) {
    context
        .eval(Source::from_bytes(source))
        .unwrap_or_else(|error| panic!("eval failed for {source}: {error}"));
}

/// Evaluates an async IIFE and asserts it fulfills with `true` after jobs.
fn assert_async_body(context: &mut Context, handle: &boa_fapi::FileApiHandle, body: &str) {
    let source = format!("(async () => {{ {body} }})()");
    let value = context
        .eval(Source::from_bytes(&source))
        .unwrap_or_else(|error| panic!("eval failed for {source}: {error}"));
    let promise = value
        .as_object()
        .unwrap_or_else(|| panic!("expected a promise from {source}"));
    for _ in 0..200 {
        let settled = handle.poll_io(context).unwrap_or(0);
        context.run_jobs().expect("run_jobs failed");
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
    let state = boa_engine::object::builtins::JsPromise::from_object(promise)
        .expect("promise object")
        .state();
    assert_eq!(
        state,
        boa_engine::builtins::promise::PromiseState::Fulfilled(boa_engine::JsValue::from(true)),
        "async body did not fulfill with true: {source} (state: {state:?})"
    );
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

// ── Blob / File / FileList ──────────────────────────────────────────

#[test]
fn blob_creation_brands_prototypes_descriptors() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        "typeof Blob === 'function' && typeof File === 'function' \
         && typeof FileList === 'undefined' \
         && Blob.length === 0 && File.length === 2 \
         && Object.getPrototypeOf(File.prototype) === Blob.prototype \
         && Object.getOwnPropertyDescriptor(Blob.prototype, 'size').enumerable === true \
         && Object.getOwnPropertyDescriptor(Blob.prototype, 'slice').enumerable === false",
    );
    assert_eval(
        &mut context,
        "var b = new Blob(['abc'], { type: 'TEXT/PLAIN' }); \
         b instanceof Blob && !(b instanceof File) && b.size === 3 && b.type === 'text/plain' \
         && Object.prototype.toString.call(b) === '[object Blob]'",
    );
    assert_eval(
        &mut context,
        "var f = new File(['xy'], 'a/b.txt', { lastModified: 7 }); \
         f instanceof File && f instanceof Blob && f.name === 'a:b.txt' \
         && f.size === 2 && f.lastModified === 7 \
         && Object.prototype.toString.call(f) === '[object File]'",
    );
}

#[test]
fn file_last_modified_defaults_to_injected_clock() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        &format!("new File(['x'], 'f.txt').lastModified === {FIXED_TIME}"),
    );
}

#[test]
fn slice_semantics_are_js_observable() {
    let (mut context, _handle) = setup();
    // Bisect: slice type normalization follows the accepted M2 contract
    // (explicit contentType normalized; absent contentType is empty).
    // The constructor-type inheritance case is pinned by M2
    // `slice_content_type` on a typed source blob.
    eval_side_effect(&mut context, "var b = new Blob(['hello world']);");
    assert_eval(&mut context, "b.slice(6, 11).size === 5");
    assert_eval(&mut context, "b.slice(-5).size === 5");
    // Clamp: end beyond size clamps to size (11 - 3 = 8).
    assert_eval(&mut context, "b.slice(3, 100).size === 8");
    assert_eval(&mut context, "b.slice(5, 2).size === 0");
    assert_eval(&mut context, "b.slice(0, 5, 'A/B').type === 'a/b'");
    assert_eval(&mut context, "b.slice(0, 5).type === ''");
    assert_eval(&mut context, "b.size === 11");
    assert_eval(
        &mut context,
        "var f = new File(['hello'], 'f.txt'); \
         var s = f.slice(1); \
         !(s instanceof File) && (s instanceof Blob) && s.size === 4",
    );
}

#[test]
fn host_file_list_order_identity_and_access() {
    let (mut context, handle) = setup();
    let first = handle
        .file_from_bytes(
            bytes::Bytes::from_static(b"one"),
            "1.txt",
            HostFileOptions::default(),
            &mut context,
        )
        .expect("first");
    let second = handle
        .file_from_bytes(
            bytes::Bytes::from_static(b"two"),
            "2.txt",
            HostFileOptions::default(),
            &mut context,
        )
        .expect("second");
    let list = handle
        .file_list([first, second], &mut context)
        .expect("list");
    publish(&mut context, "acceptList", list);
    assert_eval(
        &mut context,
        "acceptList.length === 2 && acceptList.item(0).name === '1.txt' \
         && acceptList[1].name === '2.txt' && acceptList.item(0) === acceptList[0] \
         && acceptList.item(2) === null && acceptList[9] === undefined \
         && Object.prototype.toString.call(acceptList) === '[object FileList]'",
    );
}

// ── Promise reads and streams settle only after run_jobs ─────────────

#[test]
fn promise_reads_settle_only_after_jobs() {
    let (mut context, handle) = setup();
    assert_async_body(
        &mut context,
        &handle,
        "var seen = 'pending'; \
         var p = new Blob(['abc']).text().then(v => { seen = v; }); \
         if (seen !== 'pending') return false; \
         var t = await p.then(() => seen); \
          return t === 'abc';",
    );
    assert_async_body(
        &mut context,
        &handle,
        "var buf = await new Blob(['abc']).arrayBuffer(); \
         var view = new Uint8Array(buf); \
          return buf.byteLength === 3 && view[0] === 97 && view[2] === 99;",
    );
    assert_async_body(
        &mut context,
        &handle,
        "var bytes = await new File(['xy'], 'f.txt').bytes(); \
         return bytes instanceof Uint8Array && bytes.length === 2 && bytes.byteOffset === 0;",
    );
}

#[test]
fn streams_deliver_chunks_on_demand_after_jobs() {
    let (mut context, handle) = setup();
    assert_async_body(
        &mut context,
        &handle,
        "var reader = new Blob(['hello']).stream().getReader(); \
         var first = await reader.read(); \
         var second = await reader.read(); \
         return first.done === false && first.value.length === 5 && second.done === true;",
    );
    assert_async_body(
        &mut context,
        &handle,
        "var reader = new Blob(['a']).textStream().getReader(); \
         var first = await reader.read(); \
         return typeof first.value === 'string' && first.value === 'a';",
    );
}

// ── FileReader state/events/result/error ─────────────────────────────

#[test]
fn filereader_state_events_result() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        "var r = new FileReader(); \
         r.readyState === FileReader.EMPTY && r.result === null && r.error === null \
         && (r instanceof EventTarget)",
    );
    context
        .eval(Source::from_bytes(
            "globalThis.acceptLog = []; \
             globalThis.acceptReader = new FileReader(); \
             globalThis.acceptReader.onloadstart = e => globalThis.acceptLog.push('start:' + e.loaded); \
             globalThis.acceptReader.onload = function() { globalThis.acceptLog.push('load:' + this.result); }; \
             globalThis.acceptReader.onloadend = () => globalThis.acceptLog.push('end'); \
             globalThis.acceptReader.readAsText(new Blob(['hi']));",
        ))
        .expect("start read");
    assert_eval(&mut context, "globalThis.acceptReader.readyState === 1");
    context.run_jobs().expect("run_jobs");
    context.run_jobs().expect("run_jobs");
    assert_eval(
        &mut context,
        "globalThis.acceptReader.readyState === 2 \
         && globalThis.acceptReader.result === 'hi' \
         && globalThis.acceptReader.error === null \
         && globalThis.acceptLog.join('|') === 'start:0|load:hi|end'",
    );
}

#[test]
fn filereader_abort_events_and_null_error() {
    let (mut context, _handle) = setup();
    context
        .eval(Source::from_bytes(
            "globalThis.abortLog = []; \
             globalThis.abortReader = new FileReader(); \
             globalThis.abortReader.onabort = () => globalThis.abortLog.push('abort'); \
             globalThis.abortReader.onloadend = () => globalThis.abortLog.push('end'); \
             globalThis.abortReader.readAsText(new Blob(['data'])); \
             globalThis.abortReader.abort();",
        ))
        .expect("abort");
    context.run_jobs().expect("run_jobs");
    context.run_jobs().expect("run_jobs");
    assert_eval(
        &mut context,
        "globalThis.abortReader.readyState === 2 && globalThis.abortReader.result === null \
         && globalThis.abortReader.error === null \
         && globalThis.abortLog.join('|') === 'abort|end'",
    );
}

// ── FileReaderSync environment gating ────────────────────────────────

#[test]
fn filereader_sync_gated_by_environment() {
    for (environment, present) in [
        (FileApiEnvironment::Window, false),
        (FileApiEnvironment::DedicatedWorker, true),
        (FileApiEnvironment::SharedWorker, true),
        (FileApiEnvironment::ServiceWorker, false),
    ] {
        let mut context = Context::default();
        FileApiExtension::builder()
            .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
            .environment(environment)
            .build()
            .register(&mut context)
            .expect("register");
        let value = context
            .eval(Source::from_bytes("typeof FileReaderSync"))
            .expect("typeof");
        let name = value.as_string().expect("string").to_std_string_escaped();
        assert_eq!(
            name.as_str(),
            if present { "function" } else { "undefined" },
            "environment {environment:?}"
        );
    }
    let mut context = Context::default();
    FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .environment(FileApiEnvironment::DedicatedWorker)
        .build()
        .register(&mut context)
        .expect("register");
    assert_eval(
        &mut context,
        "new FileReaderSync().readAsText(new Blob(['s'])) === 's'",
    );
}

// ── Capability File, unified shutdown ────────────────────────────────

#[test]
#[cfg(feature = "fs")]
fn capability_file_and_unified_shutdown() {
    let registry = boa_fapi_fs::FsRegistry::new();
    let (mut context, handle) = setup();
    let mut path = std::env::temp_dir();
    path.push(format!(
        "boa-fapi-accept-{}-{}.bin",
        std::process::id(),
        line!()
    ));
    std::fs::write(&path, b"cap-content").expect("write");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .expect("open");
    let resource = registry.register(file).expect("register");
    #[cfg(unix)]
    {
        let adapter = Arc::new(
            boa_fapi_fs::HostFileSource::new(&registry, &resource, None).expect("adapter"),
        );
        let object = handle
            .file_from_resource(
                &registry,
                adapter,
                "shown.txt",
                HostFileOptions::default(),
                &mut context,
            )
            .expect("import");
        publish(&mut context, "acceptFile", object);
    }
    #[cfg(not(unix))]
    {
        let bytes = boa_fapi_fs::open_copy_on_import(&registry, &resource, u64::MAX).expect("copy");
        let object = handle
            .file_from_bytes(bytes, "shown.txt", HostFileOptions::default(), &mut context)
            .expect("import");
        publish(&mut context, "acceptFile", object);
    }
    assert_eval(
        &mut context,
        "acceptFile instanceof File && acceptFile.name === 'shown.txt' && acceptFile.size === 11",
    );
    handle.shutdown(&mut context).expect("shutdown");
    handle.shutdown(&mut context).expect("repeated shutdown");
    assert_eval(&mut context, "acceptFile.size === 11");
    std::fs::remove_file(&path).ok();
}

// ── URL create/revoke/resolve isolation ──────────────────────────────

#[test]
fn url_create_revoke_resolve_isolation() {
    let (mut context, handle) = setup();
    eval_side_effect(
        &mut context,
        "globalThis.acceptUrl = URL.createObjectURL(new Blob(['u']));",
    );
    assert_eval(
        &mut context,
        "typeof globalThis.acceptUrl === 'string' && globalThis.acceptUrl.indexOf('blob:') === 0",
    );
    let url = context
        .eval(Source::from_bytes("globalThis.acceptUrl"))
        .expect("url")
        .as_string()
        .expect("string")
        .to_std_string_escaped();
    assert!(handle.resolve_blob_url(&url).is_ok());
    assert_eval(
        &mut context,
        "URL.revokeObjectURL(globalThis.acceptUrl) === undefined",
    );
    assert!(handle.resolve_blob_url(&url).is_err());
    // Foreign partition sees the same opaque class as missing.
    let mut other = Context::default();
    let other_handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .partition(4242)
        .nonce(1)
        .build()
        .register(&mut other)
        .expect("register other");
    eval_side_effect(
        &mut context,
        "globalThis.acceptUrl2 = URL.createObjectURL(new Blob(['v']));",
    );
    assert_eval(&mut context, "typeof globalThis.acceptUrl2 === 'string'");
    let url2 = context
        .eval(Source::from_bytes("globalThis.acceptUrl2"))
        .expect("url2")
        .as_string()
        .expect("string")
        .to_std_string_escaped();
    assert_eq!(
        format!("{:?}", other_handle.resolve_blob_url(&url2).err()),
        format!(
            "{:?}",
            other_handle
                .resolve_blob_url("blob:https://localhost/00000000-0000-4000-8000-000000000000")
                .err()
        )
    );
}

// ── Structured clone round-trips without leakage ─────────────────────

#[test]
fn structured_clone_round_trips_without_leakage() {
    let (mut context, handle) = setup();
    let blob = handle
        .blob_from_bytes(
            bytes::Bytes::from_static(b"clone-bytes"),
            "text/plain",
            &mut context,
        )
        .expect("blob");
    let payload = handle.clone_blob(&blob).expect("clone");
    let back = handle
        .blob_from_clone(&payload, &mut context)
        .expect("decode");
    publish(&mut context, "acceptClone", back);
    assert_eval(
        &mut context,
        "acceptClone instanceof Blob && acceptClone.size === 11 && acceptClone.type === 'text/plain'",
    );
    let file = handle
        .file_from_bytes(
            bytes::Bytes::from_static(b"f"),
            "n.txt",
            HostFileOptions {
                last_modified: Some(99),
                ..Default::default()
            },
            &mut context,
        )
        .expect("file");
    let file_payload = handle.clone_file(&file).expect("clone file");
    let file_back = handle
        .file_from_clone(&file_payload, &mut context)
        .expect("decode file");
    publish(&mut context, "acceptCloneFile", file_back);
    assert_eval(
        &mut context,
        "acceptCloneFile.name === 'n.txt' && acceptCloneFile.lastModified === 99",
    );
    // Encoded bytes carry no path/capability text.
    let encoded = payload.encode().expect("encode");
    for needle in [
        b"acceptClone".as_slice(),
        b"/tmp".as_slice(),
        b"capability".as_slice(),
    ] {
        assert!(
            encoded.windows(needle.len()).all(|w| w != needle),
            "encoded payload leaks observable text"
        );
    }
}
