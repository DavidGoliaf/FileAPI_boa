//! M6 structured-clone integration: payload round-trips, filesystem
//! safety, versioned encoding, and the host bridge lifecycle.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(feature = "structured-clone")]

use std::sync::Arc;

use boa_engine::{Context, Source};
use boa_fapi::{CloneAdapter, FileApiExtension};
use boa_fapi_core::clone::{CLONE_ENCODING_VERSION, CloneError, FileApiClonePayload};

/// Deterministic clock for `File` timestamps.
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

fn setup() -> (Context, boa_fapi::FileApiHandle) {
    setup_with_bridge(None)
}

fn setup_with_bridge(bridge: Option<Arc<dyn CloneAdapter>>) -> (Context, boa_fapi::FileApiHandle) {
    let mut context = Context::default();
    let mut builder = FileApiExtension::builder();
    builder.clock(Arc::new(FixedClock { millis: FIXED_TIME }));
    if let Some(bridge) = bridge {
        builder.clone_adapter(bridge);
    }
    let handle = builder.build().register(&mut context).expect("register");
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

fn publish(context: &mut Context, name: &str, object: boa_engine::JsObject) {
    context
        .register_global_property(
            boa_engine::js_string!(name),
            object,
            boa_engine::property::Attribute::all(),
        )
        .expect("publish");
}

/// Test bridge: version-checked core codec (mirrors the crate fake).
#[derive(Debug)]
struct TestBridge {
    version: u32,
}

impl CloneAdapter for TestBridge {
    fn descriptor(&self) -> boa_fapi::CloneBridgeDescriptor {
        boa_fapi::CloneBridgeDescriptor {
            name: String::from("test-bridge"),
            version: self.version,
        }
    }

    fn encode(&self, payload: &FileApiClonePayload) -> Result<Vec<u8>, CloneError> {
        if self.version != CLONE_ENCODING_VERSION {
            return Err(CloneError::UnsupportedVersion);
        }
        payload.encode()
    }

    fn decode(&self, bytes: &[u8]) -> Result<FileApiClonePayload, CloneError> {
        if self.version != CLONE_ENCODING_VERSION {
            return Err(CloneError::UnsupportedVersion);
        }
        FileApiClonePayload::decode(bytes)
    }
}

/// M6-CLONE-01: Blob/File round-trips preserve bytes, size, normalized
/// type, sanitized name and lastModified with mutation isolation.
#[test]
fn clone_blob_file_round_trip() {
    let (mut context, handle) = setup();
    let blob = handle
        .blob_from_bytes(
            bytes::Bytes::from_static(b"payload-bytes"),
            "TEXT/PLAIN",
            &mut context,
        )
        .expect("host blob");
    let file = handle
        .file_from_bytes(
            bytes::Bytes::from_static(b"file-bytes"),
            "a/b.txt",
            boa_fapi::HostFileOptions {
                media_type: String::from("Text/Plain"),
                last_modified: Some(12345),
            },
            &mut context,
        )
        .expect("host file");
    publish(&mut context, "srcBlob", blob.clone());
    publish(&mut context, "srcFile", file.clone());

    let blob_payload = handle.clone_blob(&blob).expect("clone blob");
    let FileApiClonePayload::Blob(blob_body) = &blob_payload else {
        panic!("expected blob payload");
    };
    assert_eq!(&blob_body.bytes[..], b"payload-bytes");
    assert_eq!(blob_body.media_type, "text/plain");

    let file_payload = handle.clone_file(&file).expect("clone file");
    let FileApiClonePayload::File(file_body) = &file_payload else {
        panic!("expected file payload");
    };
    assert_eq!(&file_body.bytes[..], b"file-bytes");
    assert_eq!(file_body.media_type, "text/plain");
    assert_eq!(file_body.name, "a:b.txt");
    assert_eq!(file_body.last_modified, 12345);

    // Decode into new live objects with a fresh immutable backing.
    let blob_back = handle
        .blob_from_clone(&blob_payload, &mut context)
        .expect("decode blob");
    let file_back = handle
        .file_from_clone(&file_payload, &mut context)
        .expect("decode file");
    publish(&mut context, "backBlob", blob_back);
    publish(&mut context, "backFile", file_back);
    assert_eval(
        &mut context,
        "backBlob.size === 13 && backBlob.type === 'text/plain' \
         && backFile.size === 10 && backFile.name === 'a:b.txt' \
         && backFile.lastModified === 12345 && backFile.type === 'text/plain'",
    );
    // Mutation isolation: encoding snapshots bytes at clone time.
    let re_payload = handle.clone_blob(&blob).expect("re-clone");
    assert_eq!(re_payload, blob_payload);
    // Kind mismatch fails before touching JS state.
    assert_eq!(
        handle.blob_from_clone(&file_payload, &mut context),
        Err(CloneError::UnexpectedKind)
    );
    assert_eq!(
        handle.file_from_clone(&blob_payload, &mut context),
        Err(CloneError::UnexpectedKind)
    );
}

/// M6-CLONE-02: FileList round-trip preserves order, count, brands and
/// metadata; non-File input is rejected before any output.
#[test]
fn clone_file_list_round_trip() {
    let (mut context, handle) = setup();
    let first = handle
        .file_from_bytes(
            bytes::Bytes::from_static(b"one"),
            "1.txt",
            boa_fapi::HostFileOptions::default(),
            &mut context,
        )
        .expect("first");
    let second = handle
        .file_from_bytes(
            bytes::Bytes::from_static(b"two"),
            "2.txt",
            boa_fapi::HostFileOptions {
                last_modified: Some(7),
                ..Default::default()
            },
            &mut context,
        )
        .expect("second");
    let first_clone = first.clone();
    let list = handle
        .file_list([first_clone, second], &mut context)
        .expect("list");
    let payload = handle
        .clone_file_list(&list, &mut context)
        .expect("clone list");
    let FileApiClonePayload::FileList(files) = &payload else {
        panic!("expected list payload");
    };
    assert_eq!(files.len(), 2);
    assert_eq!(&files[0].bytes[..], b"one");
    assert_eq!(&files[1].bytes[..], b"two");
    assert_eq!(files[0].name, "1.txt");
    assert_eq!(files[1].last_modified, 7);

    let back = handle
        .file_list_from_clone(&payload, &mut context)
        .expect("decode list");
    publish(&mut context, "backList", back);
    assert_eval(
        &mut context,
        "backList.length === 2 && backList.item(0).name === '1.txt' \
         && backList.item(1).name === '2.txt' && backList.item(0) !== backList.item(1) \
         && backList[0] === backList.item(0)",
    );

    // A non-File payload is rejected by the Blob decode entry point
    // before any JS state is touched.
    let not_file = handle.clone_file(&first).expect("file payload");
    assert_eq!(
        handle.blob_from_clone(&not_file, &mut context),
        Err(CloneError::UnexpectedKind)
    );
    assert_eq!(
        handle.file_list_from_clone(&not_file, &mut context),
        Err(CloneError::UnexpectedKind)
    );
    // A non-File object is rejected by the clone entry points directly.
    let plain_blob = handle
        .blob_from_bytes(bytes::Bytes::from_static(b"x"), "", &mut context)
        .expect("blob");
    assert_eq!(
        handle.clone_file(&plain_blob).map(|_| ()),
        Err(CloneError::InvalidObject)
    );
}

/// M6-CLONE-03: filesystem-backed clone materializes through the checked
/// path; changed sources fail typed with no partial payload and no
/// path/capability in the error.
#[test]
#[cfg(unix)]
fn clone_filesystem_safety_unix() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let registry = boa_fapi_fs::FsRegistry::new();
    let (mut context, handle) = setup();
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut path = std::env::temp_dir();
    path.push(format!("boa-fapi-m6clone-{}.bin", id));
    std::fs::write(&path, b"stable-content").expect("write");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .expect("open");
    let resource = registry.register(file).expect("register");
    let adapter =
        Arc::new(boa_fapi_fs::HostFileSource::new(&registry, &resource, None).expect("adapter"));
    let object = handle
        .file_from_resource(
            &registry,
            adapter,
            "stable.txt",
            boa_fapi::HostFileOptions::default(),
            &mut context,
        )
        .expect("import");
    let payload = handle.clone_file(&object).expect("clone fs file");
    let FileApiClonePayload::File(body) = &payload else {
        panic!("expected file payload");
    };
    assert_eq!(&body.bytes[..], b"stable-content");

    // Mutate the file: the next clone must fail typed, with no payload.
    std::fs::write(&path, b"completely-different-and-longer").expect("rewrite");
    let error = handle
        .clone_file(&object)
        .expect_err("changed source must fail");
    assert_eq!(error, CloneError::SourceFailed);
    let text = format!("{error:?} {error}");
    assert!(!text.contains(path.to_string_lossy().as_ref()));
    for secret in ["capability", "handle", "partition", "identity"] {
        assert!(!text.contains(secret), "leak in {text}");
    }
    std::fs::remove_file(&path).ok();
}

/// M6-CLONE-04: versioned checked encoding — malformed, truncated,
/// overflowing and future-version inputs are rejected; the bridge
/// round-trips same-version bytes.
#[test]
fn clone_versioned_checked_encoding() {
    let (mut context, handle) = setup_with_bridge(Some(Arc::new(TestBridge {
        version: CLONE_ENCODING_VERSION,
    })));
    let blob = handle
        .blob_from_bytes(
            bytes::Bytes::from_static(b"v-bytes"),
            "text/plain",
            &mut context,
        )
        .expect("blob");
    let payload = handle.clone_blob(&blob).expect("payload");
    let bytes = handle
        .clone_encode_via_bridge(&payload)
        .expect("bridge encode");
    let back = handle
        .clone_decode_via_bridge(&bytes)
        .expect("bridge decode");
    assert_eq!(back, payload);

    assert_eq!(
        handle.clone_decode_via_bridge(b""),
        Err(CloneError::Malformed)
    );
    assert_eq!(
        handle.clone_decode_via_bridge(&bytes[..bytes.len() - 1]),
        Err(CloneError::Malformed)
    );
    let mut future = bytes.clone();
    future[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        handle.clone_decode_via_bridge(&future),
        Err(CloneError::UnsupportedVersion)
    );
    // Unknown SCF tag is malformed, not misread.
    let mut bad_tag = bytes.clone();
    let tag_pos = 8;
    bad_tag[tag_pos..tag_pos + 4].copy_from_slice(&0xDEAD_BEEF_u32.to_le_bytes());
    assert_eq!(
        handle.clone_decode_via_bridge(&bad_tag),
        Err(CloneError::Malformed)
    );
}

/// M6-CLONE-04 (R3): the host `clone_blob`/bridge path enforces the same
/// symmetric string bounds — a Blob whose media type exceeds the ceiling
/// fails as `LimitExceeded` with no partial payload.
#[test]
fn clone_host_path_enforces_blob_media_type_bound() {
    use boa_fapi_core::clone::MAX_CLONE_STRING_BYTES;
    let (mut context, handle) = setup_with_bridge(Some(Arc::new(TestBridge {
        version: CLONE_ENCODING_VERSION,
    })));
    let over = "t".repeat(MAX_CLONE_STRING_BYTES + 1);
    let blob = handle
        .blob_from_bytes(bytes::Bytes::from_static(b"x"), &over, &mut context)
        .expect("host blob keeps over-long type as empty");
    // The M1 MIME rule normalizes over-long non-ASCII to empty, but a
    // printable-ASCII over-long type survives normalization — and must
    // then fail the symmetric clone bound, not produce undecodable bytes.
    let result = handle.clone_blob(&blob);
    assert_eq!(result, Err(CloneError::LimitExceeded));
    let direct = boa_fapi_core::clone::serialized_blob(bytes::Bytes::from_static(b"x"), &over);
    assert_eq!(direct, Err(CloneError::LimitExceeded));
}

/// M6-CLONE-05: missing bridge and incompatible versions fail atomically;
/// shutdown cancels pending clone work with no late writes.
#[test]
fn clone_adapter_lifecycle() {
    // Missing bridge: encode/decode fail before touching any state.
    let (mut context, handle) = setup();
    let blob = handle
        .blob_from_bytes(bytes::Bytes::from_static(b"q"), "", &mut context)
        .expect("blob");
    let payload = handle.clone_blob(&blob).expect("payload");
    assert_eq!(
        handle.clone_encode_via_bridge(&payload),
        Err(CloneError::NoBridge)
    );
    assert_eq!(
        handle.clone_decode_via_bridge(&payload.encode().expect("encode")),
        Err(CloneError::NoBridge)
    );

    // Incompatible bridge: registration itself fails atomically.
    let mut foreign = Context::default();
    let result = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .clone_adapter(Arc::new(TestBridge { version: 999 }))
        .build()
        .register(&mut foreign);
    assert!(matches!(
        result,
        Err(boa_fapi::RegisterError::CloneBridgeIncompatible(_))
    ));
    let untouched = foreign
        .eval(Source::from_bytes("typeof Blob === 'undefined'"))
        .expect("eval");
    assert!(untouched.to_boolean());

    // Shutdown: clone entry points fail; no late JS writes happen.
    let (mut context, handle) = setup_with_bridge(Some(Arc::new(TestBridge {
        version: CLONE_ENCODING_VERSION,
    })));
    let blob = handle
        .blob_from_bytes(bytes::Bytes::from_static(b"z"), "", &mut context)
        .expect("blob");
    handle.shutdown(&mut context).expect("shutdown");
    assert_eq!(handle.clone_blob(&blob), Err(CloneError::Shutdown));
    assert_eq!(
        handle.clone_encode_via_bridge(&payload),
        Err(CloneError::Shutdown)
    );
    assert_eval(&mut context, "new Blob(['x']).size === 1");
}

/// M6-CLONE-05: the `structured-clone` feature-off guard keeps M1–M5
/// behavior and leaves no partial clone surface.
#[test]
fn clone_feature_off_keeps_m1_m5() {
    let mut context = Context::default();
    let _handle = FileApiExtension::builder()
        .clock(Arc::new(FixedClock { millis: FIXED_TIME }))
        .structured_clone(false)
        .build()
        .register(&mut context)
        .expect("register without clone");
    assert_eval(
        &mut context,
        "new Blob(['m1-m5']).size === 5 && typeof structuredClone === 'undefined'",
    );
}
