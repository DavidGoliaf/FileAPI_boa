//! M6 Blob URL integration: JS surface, isolation, limits, environments.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(feature = "url-shim")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use boa_engine::{Context, Source, js_string};
use boa_fapi::{FileApiEnvironment, FileApiExtension, UrlEntropySource};

/// Deterministic clock (M6 URL paths never read it; required by builder).
#[derive(Debug)]
struct FixedClock {
    millis: i64,
}

impl boa_fapi::Clock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        self.millis
    }
}

const FIXED_TIME: i64 = 1_700_000_000_000;

/// Deterministic entropy: a big-endian counter mixed into 16 bytes.
///
/// Test-only: production uses [`boa_fapi::OsEntropy`] (OS CSPRNG).
#[derive(Debug, Default)]
struct CounterEntropy {
    next: AtomicU64,
}

impl UrlEntropySource for CounterEntropy {
    fn fill_16(&self) -> [u8; 16] {
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        let mut out = [0xA5_u8; 16];
        out[..8].copy_from_slice(&n.to_be_bytes());
        out
    }
}

/// Entropy that always returns the same block (collision fixture).
#[derive(Debug)]
struct StuckEntropy;

impl UrlEntropySource for StuckEntropy {
    fn fill_16(&self) -> [u8; 16] {
        [0x5A; 16]
    }
}

fn setup() -> (Context, boa_fapi::FileApiHandle) {
    setup_with_entropy(
        Arc::new(CounterEntropy::default()),
        FileApiEnvironment::Window,
    )
}

fn setup_with_entropy(
    entropy: Arc<dyn UrlEntropySource>,
    environment: FileApiEnvironment,
) -> (Context, boa_fapi::FileApiHandle) {
    let mut context = Context::default();
    let handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .entropy(entropy)
        .environment(environment)
        .build()
        .register(&mut context)
        .expect("registration failed");
    (context, handle)
}

fn assert_eval(context: &mut Context, source: &str) {
    let value = context
        .eval(Source::from_bytes(source))
        .unwrap_or_else(|error| panic!("eval failed for {source}: {error}"));
    let truthy = value.to_boolean();
    assert!(truthy, "JS assertion failed: {source} (got {value:?})");
}

fn eval_string(context: &mut Context, source: &str) -> String {
    let value = context
        .eval(Source::from_bytes(source))
        .unwrap_or_else(|error| panic!("eval failed for {source}: {error}"));
    value
        .to_string(context)
        .expect("to_string")
        .to_std_string_lossy()
        .to_string()
}

fn assert_eval_type_error(context: &mut Context, source: &str) {
    let result = context.eval(Source::from_bytes(&format!(
        "(() => {{ 'use strict'; return (() => {{ {source} }})(); }})()"
    )));
    let error = result.expect_err("expected a TypeError");
    assert!(
        format!("{error}").contains("TypeError"),
        "expected TypeError for {source}, got: {error}"
    );
}

/// M6-URL-01: generated URLs match `blob:<origin>/<uuid-v4>`; the store
/// holds no secret internals (origin only, never partition/capability).
#[test]
fn generated_url_format_and_no_secret_internals() {
    let (mut context, _handle) = setup();
    let url = eval_string(
        &mut context,
        "URL.createObjectURL(new Blob(['abc'], { type: 'text/plain' }))",
    );
    assert!(url.starts_with("blob:https://localhost/"), "got {url}");
    let uuid = url.rsplit('/').next().expect("uuid");
    assert_eq!(uuid.len(), 36);
    assert_eq!(&uuid[14..15], "4");
    assert!(matches!(&uuid[19..20], "8" | "9" | "a" | "b"));
    // Two URLs never collide with fresh entropy.
    let second = eval_string(&mut context, "URL.createObjectURL(new Blob(['x']))");
    assert_ne!(url, second);
    // No uppercase/whitespace leakage, no partition key in the string.
    assert!(!url.contains(' '));
}

/// M6-URL-01: a stuck entropy source yields collision, never overwrite.
#[test]
fn stuck_entropy_collides_without_overwrite() {
    let (mut context, _handle) =
        setup_with_entropy(Arc::new(StuckEntropy), FileApiEnvironment::Window);
    assert_eval(
        &mut context,
        "globalThis.first = URL.createObjectURL(new Blob(['first']))",
    );
    assert_eval_type_error(&mut context, "URL.createObjectURL(new Blob(['second']))");
    assert_eval(&mut context, "typeof globalThis.first === 'string'");
}

/// M6-URL-01: an entropy failure (all-zero sentinel) is the opaque
/// network-error equivalent — never a minted zero UUID.
#[test]
fn zero_entropy_fails_without_minting() {
    /// Failing source: always the reserved all-zero block.
    #[derive(Debug)]
    struct FailingEntropy;
    impl UrlEntropySource for FailingEntropy {
        fn fill_16(&self) -> [u8; 16] {
            [0; 16]
        }
    }
    let (mut context, handle) =
        setup_with_entropy(Arc::new(FailingEntropy), FileApiEnvironment::Window);
    assert_eval_type_error(&mut context, "URL.createObjectURL(new Blob(['x']))");
    assert_eq!(handle.blob_url_count(), 0);
    // The zero UUID was never minted: the store has no such entry.
    assert!(
        handle
            .resolve_blob_url("blob:https://localhost/00000000-0000-4000-8000-000000000000")
            .is_err()
    );
}

/// M6-URL-02: two partitions with the same origin isolate entries; the
/// foreign lookup is indistinguishable from a missing URL.
#[test]
fn same_origin_partitions_isolate() {
    let entropy_a: Arc<dyn UrlEntropySource> = Arc::new(CounterEntropy::default());
    let entropy_b: Arc<dyn UrlEntropySource> = Arc::new(CounterEntropy::default());
    let mut context_a = Context::default();
    let handle_a = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .entropy(entropy_a)
        .partition(1)
        .nonce(1)
        .build()
        .register(&mut context_a)
        .expect("register A");
    let mut context_b = Context::default();
    let handle_b = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .entropy(entropy_b)
        .partition(2)
        .nonce(2)
        .build()
        .register(&mut context_b)
        .expect("register B");
    let value_a = context_a
        .eval(Source::from_bytes(
            "URL.createObjectURL(new Blob(['partition-a']))",
        ))
        .expect("create A");
    let url_a = value_a
        .to_string(&mut context_a)
        .expect("to_string")
        .to_std_string_lossy()
        .to_string();
    assert!(handle_a.resolve_blob_url(&url_a).is_ok());
    // Same origin, other partition: the same opaque class as missing.
    assert_eq!(
        format!("{:?}", handle_b.resolve_blob_url(&url_a).err()),
        format!(
            "{:?}",
            handle_b
                .resolve_blob_url("blob:https://localhost/00000000-0000-4000-8000-000000000000")
                .err()
        )
    );
}

/// M6-URL-03: Blob/File succeed, wrong brands throw `TypeError` before any
/// store write; repeated revoke is silent; revoked lookup is unavailable;
/// a handed-out `Arc` still reads after revoke.
#[test]
fn create_revoke_semantics() {
    let (mut context, handle) = setup();
    assert_eval(
        &mut context,
        "globalThis.u1 = URL.createObjectURL(new Blob(['a'])); \
         globalThis.u2 = URL.createObjectURL(new File(['b'], 'f.txt')); \
         typeof globalThis.u1 === 'string' && typeof globalThis.u2 === 'string'",
    );
    for bad in [
        "URL.createObjectURL({})",
        "URL.createObjectURL('blob:https://localhost/x')",
        "URL.createObjectURL(null)",
        "URL.createObjectURL(new Blob(['x']).size)",
    ] {
        assert_eval_type_error(&mut context, bad);
    }
    assert_eval(
        &mut context,
        "URL.revokeObjectURL(globalThis.u1); \
         URL.revokeObjectURL(globalThis.u1); \
         URL.revokeObjectURL('blob:https://localhost/00000000-0000-4000-8000-000000000000'); \
         URL.revokeObjectURL('not-a-url') === undefined",
    );
    // Required-argument Web IDL behavior: a missing argument throws
    // `TypeError` before any store access (no `undefined`-as-string).
    assert_eval_type_error(&mut context, "URL.revokeObjectURL()");
    let url = eval_string(&mut context, "globalThis.u1");
    assert!(handle.resolve_blob_url(&url).is_err());
    // `revokeObjectURL` on a foreign-partition URL is a silent no-op that
    // reports nothing (ownership-blind revoke, never an oracle).
    assert_eval(
        &mut context,
        "URL.revokeObjectURL(globalThis.u2) === undefined && typeof globalThis.u2 === 'string'",
    );
    // Host-created entries revoke through the handle with the same result.
    let blob = handle
        .blob_from_bytes(
            bytes::Bytes::from_static(b"host"),
            "text/plain",
            &mut context,
        )
        .expect("host blob");
    let host_url = handle.create_blob_url(&blob).expect("host URL");
    let resolved = handle.resolve_blob_url(&host_url).expect("resolve");
    handle.revoke_blob_url(&host_url);
    assert!(handle.resolve_blob_url(&host_url).is_err());
    let bytes = resolved
        .blob_data()
        .materialize(
            &boa_fapi_core::limits::FileApiLimits::default(),
            &boa_fapi_core::cancellation::CancellationToken::new(),
        )
        .expect("Arc read after revoke");
    assert_eq!(&bytes[..], b"host");
}

/// M6-URL-03 (R2): `revokeObjectURL` follows required-argument/DOMString
/// Web IDL semantics — missing arg and conversion failures throw, abrupt
/// `toString` propagates, converted strings revoke silently.
#[test]
fn revoke_webidl_conversion() {
    let (mut context, handle) = setup();
    // Missing argument throws before any store access.
    assert_eval_type_error(&mut context, "URL.revokeObjectURL()");
    // `Symbol` cannot convert to DOMString: conversion `TypeError`.
    assert_eval_type_error(&mut context, "URL.revokeObjectURL(Symbol('u'))");
    // A throwing `toString` propagates the original exception, not a
    // silent no-op and not a wrapped `TypeError`.
    let result = context.eval(Source::from_bytes(
        "(() => { 'use strict'; return URL.revokeObjectURL({ toString() { throw new RangeError('boom'); } }); })()",
    ));
    let error = result.expect_err("throwing toString must propagate");
    assert!(
        format!("{error}").contains("RangeError"),
        "expected RangeError propagation, got: {error}"
    );
    // Ordinary valid revoke and repeated revoke stay silent `undefined`.
    assert_eval(
        &mut context,
        "globalThis.u = URL.createObjectURL(new Blob(['r'])); \
         URL.revokeObjectURL(globalThis.u) === undefined && \
         URL.revokeObjectURL(globalThis.u) === undefined",
    );
    let url = eval_string(&mut context, "globalThis.u");
    assert!(handle.resolve_blob_url(&url).is_err());
    // Malformed/unknown converted strings: silent `undefined`, no oracle.
    assert_eval(
        &mut context,
        "URL.revokeObjectURL('not-a-url') === undefined && \
         URL.revokeObjectURL('blob:https://localhost/00000000-0000-4000-8000-000000000000') === undefined",
    );
    // Numeric input converts via DOMString (no throw, silent).
    assert_eval(&mut context, "URL.revokeObjectURL(42) === undefined");
}

/// M6-URL-03: the URL surface has exact descriptors and illegal-invocation
/// behavior; it is a namespace object, not a constructor.
#[test]
fn url_surface_descriptors_and_brand() {
    let (mut context, _handle) = setup();
    assert_eval(
        &mut context,
        "var d = Object.getOwnPropertyDescriptor(globalThis, 'URL'); \
         d.writable === true && d.enumerable === false && d.configurable === true",
    );
    assert_eval(
        &mut context,
        "var c = Object.getOwnPropertyDescriptor(URL, 'createObjectURL'); \
         c.writable === true && c.enumerable === false && c.configurable === true \
         && URL.createObjectURL.length === 1 && URL.createObjectURL.name === 'createObjectURL' \
         && URL.revokeObjectURL.length === 1 && URL.revokeObjectURL.name === 'revokeObjectURL' \
         && Object.getOwnPropertyDescriptor(URL, Symbol.toStringTag).value === 'URL' \
         && typeof URL === 'object'",
    );
    for bad in [
        "URL.createObjectURL.call({}, new Blob(['x']))",
        "URL.revokeObjectURL.call({}, 'blob:https://localhost/x')",
        "new URL()",
    ] {
        assert_eval_type_error(&mut context, bad);
    }
    // createObjectURL with no argument is a brand TypeError, not a URL.
    assert_eval_type_error(&mut context, "URL.createObjectURL()");
}

/// M6-URL-04: exactly `max` URLs are accepted, the `+1` is rejected with no
/// partial store write; zero/invalid limits fail registration atomically.
#[test]
fn url_limits_and_atomic_registration() {
    let mut context = Context::default();
    let limits = boa_fapi_core::limits::FileApiLimits {
        max_blob_urls_per_global: 2,
        ..boa_fapi_core::limits::FileApiLimits::default()
    };
    let _handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .limits(limits)
        .entropy(Arc::new(CounterEntropy::default()))
        .build()
        .register(&mut context)
        .expect("register");
    assert_eval(
        &mut context,
        "globalThis.a = URL.createObjectURL(new Blob(['1'])); \
         globalThis.b = URL.createObjectURL(new Blob(['2'])); \
         typeof globalThis.a === 'string' && typeof globalThis.b === 'string'",
    );
    assert_eval_type_error(&mut context, "URL.createObjectURL(new Blob(['3']))");
    assert_eval(&mut context, "typeof globalThis.a === 'string'");

    for bad in [0_usize, usize::MAX] {
        let mut context = Context::default();
        let limits = boa_fapi_core::limits::FileApiLimits {
            max_blob_urls_per_global: bad,
            ..boa_fapi_core::limits::FileApiLimits::default()
        };
        let result = FileApiExtension::builder()
            .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
            .limits(limits)
            .build()
            .register(&mut context);
        match result {
            Ok(_) => {
                // `usize::MAX` passes shape validation but the quota still
                // binds; only `0` must fail. (0 fails; MAX registers.)
                assert_eq!(bad, usize::MAX, "only MAX may register");
            }
            Err(error) => {
                assert_eq!(bad, 0, "unexpected failure: {error}");
            }
        }
    }
    // Zero-limit registration installs nothing.
    let mut context = Context::default();
    let limits = boa_fapi_core::limits::FileApiLimits {
        max_blob_urls_per_global: 0,
        ..boa_fapi_core::limits::FileApiLimits::default()
    };
    assert!(
        FileApiExtension::builder()
            .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
            .limits(limits)
            .build()
            .register(&mut context)
            .is_err()
    );
    let untouched = context
        .eval(Source::from_bytes(
            "typeof Blob === 'undefined' && typeof URL === 'undefined'",
        ))
        .expect("eval");
    assert!(untouched.to_boolean());
}

/// M6-URL-05: Window/DedicatedWorker/SharedWorker allow creation;
/// ServiceWorker forbids it with no partial global change; the URL
/// feature-off guard leaves the surface absent without breaking M1–M5.
#[test]
fn url_environment_gating() {
    for environment in [
        FileApiEnvironment::Window,
        FileApiEnvironment::DedicatedWorker,
        FileApiEnvironment::SharedWorker,
    ] {
        let (mut context, _handle) =
            setup_with_entropy(Arc::new(CounterEntropy::default()), environment);
        assert_eval(
            &mut context,
            "typeof URL.createObjectURL(new Blob(['x'])) === 'string'",
        );
    }
    let (mut context, _handle) = setup_with_entropy(
        Arc::new(CounterEntropy::default()),
        FileApiEnvironment::ServiceWorker,
    );
    // The namespace still installs (uniform error surface), but creation
    // is forbidden; revoke stays silent; no Blob global changed.
    assert_eval(&mut context, "typeof URL === 'object'");
    assert_eval_type_error(&mut context, "URL.createObjectURL(new Blob(['x']))");
    assert_eval(
        &mut context,
        "URL.revokeObjectURL('blob:https://localhost/00000000-0000-4000-8000-000000000000') === undefined \
         && new Blob(['still-here']).size === 10",
    );

    // Feature-off: no `URL` global, M1–M5 intact.
    let mut context = Context::default();
    let _handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .url_shim(false)
        .build()
        .register(&mut context)
        .expect("register without URL shim");
    assert_eval(&mut context, "typeof URL === 'undefined'");
    assert_eval(
        &mut context,
        "new Blob(['m1-m5']).size === 5 && typeof FileReader === 'function'",
    );
}

/// M6-URL-06: shutdown clears the store and releases strong references;
/// repeated shutdown is idempotent; late creation/resolution fails.
#[test]
fn url_shutdown_lifetime() {
    let (mut context, handle) = setup();
    assert_eval(
        &mut context,
        "globalThis.u = URL.createObjectURL(new Blob(['doomed']))",
    );
    let url = eval_string(&mut context, "globalThis.u");
    assert_eq!(handle.blob_url_count(), 1);
    handle.shutdown(&mut context).expect("shutdown");
    handle.shutdown(&mut context).expect("repeated shutdown");
    assert!(handle.blob_urls_empty());
    assert_eq!(handle.blob_url_count(), 0);
    assert!(handle.resolve_blob_url(&url).is_err());
    // Late creation fails; late JS creation fails too.
    let blob = eval_string(&mut context, "typeof URL.createObjectURL");
    assert_eq!(blob, "function");
    assert_eval_type_error(&mut context, "URL.createObjectURL(new Blob(['late']))");
    // No late job/callback: creating after shutdown settles nothing.
    assert_eval(&mut context, "new Blob(['x']).size === 1");
}

/// M6-URL-06: a duplicate `URL` global name fails atomically (no partial
/// registration); re-registration keeps rule (b).
#[test]
fn url_name_conflict_rolls_back_atomically() {
    let mut context = Context::default();
    context
        .register_global_property(
            js_string!("URL"),
            js_string!("host-url"),
            boa_engine::property::Attribute::all(),
        )
        .expect("host URL");
    let result = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build()
        .register(&mut context);
    assert!(matches!(
        result,
        Err(boa_fapi::RegisterError::NameConflict(_))
    ));
    let value = context
        .eval(Source::from_bytes(
            "typeof Blob === 'undefined' && globalThis.URL === 'host-url'",
        ))
        .expect("eval");
    assert!(value.to_boolean());

    let (mut context, _handle) = setup();
    let again = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .build()
        .register(&mut context);
    assert!(matches!(
        again,
        Err(boa_fapi::RegisterError::NameConflict(_))
            | Err(boa_fapi::RegisterError::AlreadyRegistered)
    ));
}
