//! M6 core unit tests: Blob URL store, environment isolation, URL format,
//! and the versioned structured-clone encoding.
//!
//! Boa-free: these tests cover the format, partition, quota, and encoding
//! contracts without a JavaScript engine. JS registration, brand checks,
//! and the host adapters are covered by the `boa_fapi` M6 integration
//! suites.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use boa_fapi_core::blob::{BlobData, BlobSegment};
use boa_fapi_core::blob_url::{
    BlobUrlError, BlobUrlStore, EnvironmentDescriptor, EnvironmentKind, format_blob_url,
    format_uuid_v4, parse_blob_url,
};
use boa_fapi_core::clone::{
    CLONE_ENCODING_VERSION, CloneError, FileApiClonePayload, MAX_CLONE_BYTES, SerializedBlob,
    SerializedFile,
};
use boa_fapi_core::source::memory::MemorySource;
use bytes::Bytes;

fn memory_blob(content: &[u8], media_type: &str) -> Arc<BlobData> {
    let source: Arc<dyn boa_fapi_core::source::ByteSource> =
        Arc::new(MemorySource::new(Bytes::copy_from_slice(content)));
    let len = source.len();
    Arc::new(
        BlobData::from_segments(
            vec![BlobSegment {
                source,
                offset: 0,
                len,
            }],
            media_type,
            &boa_fapi_core::limits::FileApiLimits::default(),
        )
        .expect("segments"),
    )
}

fn descriptor(
    kind: EnvironmentKind,
    origin: &str,
    partition: u64,
    nonce: u64,
) -> EnvironmentDescriptor {
    EnvironmentDescriptor::new(kind, origin, partition, nonce).expect("descriptor")
}

fn resolve_err(
    store: &BlobUrlStore,
    url: &str,
    key: &boa_fapi_core::blob_url::EnvironmentKey,
) -> Result<(), BlobUrlError> {
    store.resolve(url, key).map(|_| ())
}

/// M6-URL-01: the UUID formatter emits the strict v4 shape and sets the
/// version/variant bits deterministically.
#[test]
fn uuid_v4_format_and_bits() {
    let uuid = format_uuid_v4([0xAB; 16]);
    assert_eq!(uuid.len(), 36);
    assert!(parse_blob_url(&format!("blob:https://host/{uuid}")).is_ok());
    assert_eq!(&uuid[14..15], "4");
    assert!(matches!(&uuid[19..20], "8" | "9" | "a" | "b"));
    assert_eq!(
        uuid, "abababab-abab-4bab-abab-abababababab",
        "deterministic fixture"
    );
}

/// M6-URL-01: store URLs serialize as `blob:<origin>/<uuid>` and parse
/// back; non-store shapes are malformed.
#[test]
fn url_format_and_parse_round_trip() {
    let uuid = format_uuid_v4([1; 16]);
    let url = format_blob_url("https://example.com", &uuid);
    assert_eq!(url, format!("blob:https://example.com/{uuid}"));
    let parsed = parse_blob_url(&url).expect("parse");
    assert_eq!(parsed.origin, "https://example.com");
    assert_eq!(parsed.uuid, uuid);
    for bad in [
        "https://example.com/uuid",
        "blob:",
        "blob:/uuid",
        "blob:origin/not-a-uuid",
        "blob:origin/00000000-0000-0000-0000-000000000000",
        "blob:origin/00000000-0000-1000-0000-000000000000",
    ] {
        assert_eq!(
            parse_blob_url(bad),
            Err(BlobUrlError::Malformed),
            "must reject {bad}"
        );
    }
}

/// M6-URL-01: malformed, unknown, revoked and foreign URLs share one
/// externally observable class: identical display, no token/UUID/origin
/// internals, no existence bit, no host metadata.
#[test]
fn url_failures_share_one_opaque_class() {
    assert_eq!(
        format!("{}", BlobUrlError::Malformed),
        format!("{}", BlobUrlError::Unavailable),
    );
    for error in [BlobUrlError::Malformed, BlobUrlError::Unavailable] {
        let text = format!("{error:?} {error}");
        for secret in ["uuid", "token", "partition", "capability", "handle", "path"] {
            assert!(!text.contains(secret), "leak in {text}");
        }
    }
    let store = BlobUrlStore::new();
    let owner = descriptor(EnvironmentKind::Window, "https://a.test", 1, 1).key();
    let foreign = descriptor(EnvironmentKind::Window, "https://a.test", 2, 2).key();
    let data = memory_blob(b"bytes", "text/plain");
    let url = format_blob_url("https://a.test", &format_uuid_v4([7; 16]));
    store
        .insert_capped(url.clone(), owner.clone(), data, 10)
        .expect("insert");
    assert_eq!(
        resolve_err(&store, "blob:not-a-url", &owner),
        Err(BlobUrlError::Malformed)
    );
    assert_eq!(
        resolve_err(
            &store,
            &format_blob_url("https://a.test", &format_uuid_v4([8; 16])),
            &owner
        ),
        Err(BlobUrlError::Unavailable)
    );
    // Foreign partition is indistinguishable from missing.
    assert_eq!(
        resolve_err(&store, &url, &foreign),
        resolve_err(
            &store,
            &format_blob_url("https://a.test", &format_uuid_v4([9; 16])),
            &owner
        )
    );
    store.revoke(&url);
    assert_eq!(
        resolve_err(&store, &url, &owner),
        Err(BlobUrlError::Unavailable)
    );
}

/// M6-URL-01: a taken UUID is a collision, never a silent overwrite.
#[test]
fn url_collision_never_overwrites() {
    let store = BlobUrlStore::new();
    let owner = descriptor(EnvironmentKind::Window, "https://a.test", 1, 1).key();
    let url = format_blob_url("https://a.test", &format_uuid_v4([3; 16]));
    let first = memory_blob(b"first", "text/plain");
    let second = memory_blob(b"second", "text/plain");
    store
        .insert_capped(url.clone(), owner.clone(), Arc::clone(&first), 10)
        .expect("first insert");
    assert_eq!(
        store.insert_capped(url.clone(), owner.clone(), Arc::clone(&second), 10),
        Err(BlobUrlError::Collision)
    );
    let resolved = store.resolve(&url, &owner).expect("resolve");
    assert_eq!(resolved.size(), 5);
    assert_eq!(resolved.media_type(), "text/plain");
}

/// M6-URL-02: same origin is not enough — the partition and the per-global
/// nonce must also match (opaque origins share the `blob:null/` prefix).
#[test]
fn url_partition_and_nonce_isolation() {
    let store = BlobUrlStore::new();
    let data = memory_blob(b"secret", "text/plain");
    let owner = descriptor(EnvironmentKind::Window, "https://a.test", 11, 11);
    let same_origin_other_partition = descriptor(EnvironmentKind::Window, "https://a.test", 22, 22);
    let same_all_other_nonce = descriptor(EnvironmentKind::Window, "https://a.test", 11, 33);
    let opaque_a = EnvironmentDescriptor::opaque(EnvironmentKind::Window, 44, 44).expect("opaque");
    let opaque_b = EnvironmentDescriptor::opaque(EnvironmentKind::Window, 44, 55).expect("opaque");
    let url = format_blob_url("https://a.test", &format_uuid_v4([5; 16]));
    store
        .insert_capped(url.clone(), owner.key(), Arc::clone(&data), 10)
        .expect("insert");
    assert!(store.resolve(&url, &owner.key()).is_ok());
    assert_eq!(
        resolve_err(&store, &url, &same_origin_other_partition.key()),
        Err(BlobUrlError::Unavailable)
    );
    assert_eq!(
        resolve_err(&store, &url, &same_all_other_nonce.key()),
        Err(BlobUrlError::Unavailable)
    );
    let opaque_url = format_blob_url("null", &format_uuid_v4([6; 16]));
    store
        .insert_capped(opaque_url.clone(), opaque_a.key(), data, 10)
        .expect("opaque insert");
    assert!(store.resolve(&opaque_url, &opaque_a.key()).is_ok());
    assert_eq!(
        resolve_err(&store, &opaque_url, &opaque_b.key()),
        Err(BlobUrlError::Unavailable)
    );
}

/// M6-URL-02: the descriptor debug rendering redacts partition and nonce.
#[test]
fn url_descriptor_debug_redacts_partition() {
    let text = format!(
        "{:?}",
        descriptor(EnvironmentKind::Window, "https://a.test", 123, 456)
    );
    assert!(!text.contains("123"));
    assert!(!text.contains("456"));
}

/// M6-URL-04: the quota check and the insert are atomic; cap 0 always
/// fails; revoke-then-reuse frees exactly one slot.
#[test]
fn url_quota_is_atomic() {
    let store = BlobUrlStore::new();
    let owner = descriptor(EnvironmentKind::Window, "https://a.test", 1, 1).key();
    let data = memory_blob(b"x", "");
    assert_eq!(
        store.insert_capped(
            format_blob_url("https://a.test", &format_uuid_v4([1; 16])),
            owner.clone(),
            Arc::clone(&data),
            0
        ),
        Err(BlobUrlError::LimitExceeded)
    );
    for byte in [10_u8, 11] {
        store
            .insert_capped(
                format_blob_url("https://a.test", &format_uuid_v4([byte; 16])),
                owner.clone(),
                Arc::clone(&data),
                2,
            )
            .expect("slot");
    }
    assert_eq!(
        store.insert_capped(
            format_blob_url("https://a.test", &format_uuid_v4([12; 16])),
            owner.clone(),
            Arc::clone(&data),
            2
        ),
        Err(BlobUrlError::LimitExceeded)
    );
    assert_eq!(store.len(), 2);
    store.revoke(&format_blob_url(
        "https://a.test",
        &format_uuid_v4([10; 16]),
    ));
    store
        .insert_capped(
            format_blob_url("https://a.test", &format_uuid_v4([12; 16])),
            owner,
            data,
            2,
        )
        .expect("freed slot");
    assert_eq!(store.len(), 2);
}

/// M6-URL-06: revoke stops new resolutions but keeps already-handed-out
/// `Arc` reads alive; `clear` releases every strong reference.
#[test]
fn url_revoke_keeps_live_reads_and_clear_releases() {
    let store = BlobUrlStore::new();
    let owner = descriptor(EnvironmentKind::Window, "https://a.test", 1, 1).key();
    let data = memory_blob(b"live", "text/plain");
    let url = format_blob_url("https://a.test", &format_uuid_v4([9; 16]));
    store
        .insert_capped(url.clone(), owner.clone(), Arc::clone(&data), 10)
        .expect("insert");
    let resolved = store.resolve(&url, &owner).expect("resolve");
    store.revoke(&url);
    assert_eq!(
        resolve_err(&store, &url, &owner),
        Err(BlobUrlError::Unavailable)
    );
    // The handed-out Arc still reads the full payload after revoke.
    let materialized = resolved
        .blob_data()
        .materialize(
            &boa_fapi_core::limits::FileApiLimits::default(),
            &boa_fapi_core::cancellation::CancellationToken::new(),
        )
        .expect("materialize after revoke");
    assert_eq!(&materialized[..], b"live");
    assert_eq!(resolved.media_type(), "text/plain");
    assert_eq!(resolved.size(), 4);
    store.clear();
    assert!(store.is_empty());
}

/// M6-CLONE-01/02: Blob/File/FileList round-trips preserve bytes and
/// public metadata; the version is explicit and stable.
#[test]
fn clone_round_trips_preserve_bytes_and_metadata() {
    assert_eq!(CLONE_ENCODING_VERSION, 1);
    let blob = FileApiClonePayload::Blob(SerializedBlob {
        bytes: Bytes::from_static(b"hello"),
        media_type: String::from("text/plain"),
    });
    let file = FileApiClonePayload::File(SerializedFile {
        bytes: Bytes::from_static(b"data"),
        media_type: String::from("TEXT/PLAIN"),
        name: String::from("a:b"),
        last_modified: -42,
    });
    let list = FileApiClonePayload::FileList(vec![
        SerializedFile {
            bytes: Bytes::from_static(b"one"),
            media_type: String::new(),
            name: String::from("1.txt"),
            last_modified: 1,
        },
        SerializedFile {
            bytes: Bytes::from_static(b"two"),
            media_type: String::from("text/plain"),
            name: String::from("2.txt"),
            last_modified: 2,
        },
    ]);
    for payload in [blob, file, list] {
        let bytes = payload.encode().expect("encode");
        let decoded = FileApiClonePayload::decode(&bytes).expect("decode");
        assert_eq!(decoded, payload);
    }
}

/// M6-CLONE-04: malformed, truncated, overflowing and unknown-version
/// inputs are rejected without panic; trailing garbage is malformed too.
#[test]
fn clone_decode_rejects_bad_input() {
    let good = FileApiClonePayload::Blob(SerializedBlob {
        bytes: Bytes::from_static(b"abc"),
        media_type: String::from("text/plain"),
    })
    .encode()
    .expect("encode");
    assert_eq!(FileApiClonePayload::decode(b""), Err(CloneError::Malformed));
    assert_eq!(
        FileApiClonePayload::decode(&good[..good.len() - 1]),
        Err(CloneError::Malformed)
    );
    let mut trailing = good.clone();
    trailing.push(0);
    assert_eq!(
        FileApiClonePayload::decode(&trailing),
        Err(CloneError::Malformed)
    );
    assert_eq!(
        FileApiClonePayload::decode(b"XXXX00000001"),
        Err(CloneError::Malformed)
    );
    let mut future = good.clone();
    future[4..8].copy_from_slice(&999_u32.to_le_bytes());
    assert_eq!(
        FileApiClonePayload::decode(&future),
        Err(CloneError::UnsupportedVersion)
    );
    // Declared-4 GiB frame on a tiny input: limit, never allocated.
    let mut overflow = b"FCL1".to_vec();
    overflow.extend_from_slice(&1_u32.to_le_bytes());
    overflow.extend_from_slice(&boa_fapi_core::clone::SCF_BLOB_TAG.to_le_bytes());
    overflow.extend_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        FileApiClonePayload::decode(&overflow),
        Err(CloneError::LimitExceeded)
    );
    let huge = vec![0_u8; MAX_CLONE_BYTES + 1];
    assert_eq!(
        FileApiClonePayload::decode(&huge),
        Err(CloneError::LimitExceeded)
    );
    // Non-UTF8 name bytes decode as malformed; oversized strings hit limits.
    let mut bad_name = FileApiClonePayload::File(SerializedFile {
        bytes: Bytes::from_static(b"x"),
        media_type: String::new(),
        name: String::from("ok"),
        last_modified: 0,
    })
    .encode()
    .expect("encode");
    let name_pos = bad_name.len() - 8 - 2;
    bad_name[name_pos] = 0xFF;
    assert_eq!(
        FileApiClonePayload::decode(&bad_name),
        Err(CloneError::Malformed)
    );
    assert_eq!(
        boa_fapi_core::clone::serialized_blob(Bytes::from(vec![0; MAX_CLONE_BYTES + 1]), ""),
        Err(CloneError::LimitExceeded)
    );
}

/// M6-CLONE-04: the same-version fixture decodes byte-for-byte.
#[test]
fn clone_same_version_fixture() {
    // `FCL1` magic, version 1, blob tag, 3 bytes "abc", 10 bytes type.
    let mut fixture = b"FCL1".to_vec();
    fixture.extend_from_slice(&1_u32.to_le_bytes());
    fixture.extend_from_slice(&boa_fapi_core::clone::SCF_BLOB_TAG.to_le_bytes());
    fixture.extend_from_slice(&3_u32.to_le_bytes());
    fixture.extend_from_slice(b"abc");
    fixture.extend_from_slice(&10_u32.to_le_bytes());
    fixture.extend_from_slice(b"text/plain");
    assert_eq!(
        FileApiClonePayload::decode(&fixture),
        Ok(FileApiClonePayload::Blob(SerializedBlob {
            bytes: Bytes::from_static(b"abc"),
            media_type: String::from("text/plain"),
        }))
    );
}
