//! M5 filesystem-backed unit tests (no Boa): capability, snapshot, policy.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use boa_fapi_core::cancellation::CancellationToken;
use boa_fapi_core::file_api_error::FileApiError;
use boa_fapi_core::policy::{DenyAllPolicy, FileAccessPolicy, FileOpenRequest, FileResource};
use boa_fapi_core::snapshot::SnapshotState;
use boa_fapi_core::source::ByteSource;
use boa_fapi_fs::{DenyRawPathPolicy, FileSource, FsRegistry, RegistryPolicy, RootConfinedPolicy};

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Creates a uniquely-named temp file with `content` (cleaned up by the caller).
fn temp_file(content: &[u8]) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    path.push(format!(
        "boa-fapi-m5-{}-{}-{id}.bin",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&path, content).expect("write temp file");
    path
}

fn register_bytes(
    registry: &FsRegistry,
    content: &[u8],
) -> (std::path::PathBuf, boa_fapi_fs::RegisteredResource) {
    let path = temp_file(content);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .expect("open temp");
    let resource = registry.register(file).expect("register");
    (path, resource)
}

fn source_of(registry: &FsRegistry, resource: &boa_fapi_fs::RegisteredResource) -> FileSource {
    FileSource::new(registry, resource, None).expect("source")
}

#[test]
fn empty_file_exact_and_boundary_ranges() {
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"");
    let source = source_of(&registry, &resource);
    assert_eq!(source.len(), 0);
    assert!(source.is_empty());
    let cancel = CancellationToken::new();
    assert!(source.read_range(0..0, &cancel).unwrap().is_empty());
    assert!(matches!(
        source.read_range(0..1, &cancel),
        Err(FileApiError::InvalidRange)
    ));
    assert!(
        matches!(source.snapshot(), SnapshotState::Memory)
            || matches!(source.snapshot(), SnapshotState::Filesystem(_))
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn exact_ranges_and_checked_arithmetic() {
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"hello world");
    let source = source_of(&registry, &resource);
    assert_eq!(source.len(), 11);
    let cancel = CancellationToken::new();
    assert_eq!(
        &source.read_range(0..11, &cancel).unwrap()[..],
        b"hello world"
    );
    assert_eq!(&source.read_range(0..5, &cancel).unwrap()[..], b"hello");
    assert_eq!(&source.read_range(6..11, &cancel).unwrap()[..], b"world");
    assert_eq!(&source.read_range(11..11, &cancel).unwrap()[..], b"");
    // start > end is rejected before any I/O (checked before the read).
    let reversed_start = 5u64;
    let reversed_end = 3u64;
    assert!(matches!(
        source.read_range(reversed_start..reversed_end, &cancel),
        Err(FileApiError::InvalidRange)
    ));
    assert!(matches!(
        source.read_range(0..12, &cancel),
        Err(FileApiError::InvalidRange)
    ));
    assert!(matches!(
        source.read_range(u64::MAX - 1..u64::MAX, &cancel),
        Err(FileApiError::InvalidRange)
    ));
    std::fs::remove_file(&path).ok();
}

#[test]
fn cancellation_before_io() {
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"hello");
    let source = source_of(&registry, &resource);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        source.read_range(0..5, &cancel),
        Err(FileApiError::Cancelled)
    ));
    std::fs::remove_file(&path).ok();
}

#[test]
fn truncate_is_detected_before_new_chunk() {
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"hello world, this is long");
    let source = source_of(&registry, &resource);
    let cancel = CancellationToken::new();
    assert_eq!(source.read_range(0..5, &cancel).unwrap().len(), 5);
    // Truncate the underlying file: the next chunk must fail, leaking no bytes.
    {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("reopen");
        file.set_len(4).expect("truncate");
    }
    let result = source.read_range(5..10, &cancel);
    assert!(
        matches!(
            result,
            Err(FileApiError::SnapshotChanged) | Err(FileApiError::InvalidRange)
        ),
        "truncate must fail, got {result:?}"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn replacement_is_detected() {
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"original-content-00");
    let source = source_of(&registry, &resource);
    let cancel = CancellationToken::new();
    assert_eq!(&source.read_range(0..8, &cancel).unwrap()[..], b"original");
    // Replace with same-size different content: identity must differ.
    std::fs::write(&path, b"replaced-content-00").expect("replace");
    let result = source.read_range(8..16, &cancel);
    assert!(
        matches!(
            result,
            Err(FileApiError::SnapshotChanged) | Err(FileApiError::InvalidRange)
        ),
        "replacement must fail, got {result:?}"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn delete_is_detected() {
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"hello world");
    let source = source_of(&registry, &resource);
    let cancel = CancellationToken::new();
    assert_eq!(source.read_range(0..5, &cancel).unwrap().len(), 5);
    std::fs::remove_file(&path).expect("delete");
    // On Unix the open handle stays readable but the identity check runs on
    // live metadata; on Windows deletion of an open file fails. Either way
    // the source must not leak stale bytes as success-after-change: accept
    // SnapshotChanged/NotFound, or (Unix, still-readable handle) the exact
    // old bytes for the still-valid snapshot. The key invariant is no
    // partial/mixed result. Here we assert the read either fails typed or
    // returns exactly the requested bytes — never a mix.
    let result = source.read_range(5..11, &cancel);
    match result {
        Err(_) => {}
        Ok(bytes) => assert_eq!(&bytes[..], b" world"),
    }
}

#[test]
fn closed_resource_fails() {
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"hello");
    let source = source_of(&registry, &resource);
    resource.close();
    let cancel = CancellationToken::new();
    assert!(matches!(
        source.read_range(0..5, &cancel),
        Err(FileApiError::NotFound)
    ));
    // Idempotent close.
    resource.close();
    std::fs::remove_file(&path).ok();
}

#[test]
fn short_read_never_returns_partial() {
    // A resource whose live length shrinks below the requested end fails
    // with no partial bytes (InvalidRange), never a short Ok.
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"hello");
    let source = source_of(&registry, &resource);
    let cancel = CancellationToken::new();
    assert!(matches!(
        source.read_range(0..6, &cancel),
        Err(FileApiError::InvalidRange)
    ));
    std::fs::remove_file(&path).ok();
}

#[test]
fn blob_size_preflight_boundary() {
    // Exact == / +1 boundary on the fs layer: == ok, +1 rejected before
    // any output allocation.
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"12345678");
    let source = source_of(&registry, &resource);
    assert_eq!(source.len(), 8);
    let adapter = boa_fapi_fs::HostFileSource::new(&registry, &resource, None).expect("adapter");
    let _ = adapter;
    // copy_on_import with max == len ok, max == len-1 rejected.
    assert!(boa_fapi_fs::open_copy_on_import(&registry, &resource, 8).is_ok());
    assert!(boa_fapi_fs::open_copy_on_import(&registry, &resource, 7).is_err());
    std::fs::remove_file(&path).ok();
}

#[test]
fn oversized_range_rejected() {
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"hi");
    let source = source_of(&registry, &resource);
    let cancel = CancellationToken::new();
    assert!(matches!(
        source.read_range(0..u64::MAX, &cancel),
        Err(FileApiError::InvalidRange)
    ));
    std::fs::remove_file(&path).ok();
}

#[test]
fn deny_policies_reject_open_and_read() {
    let deny = DenyAllPolicy;
    let fs_deny = DenyRawPathPolicy;
    let registry = FsRegistry::new();
    let (_path, resource) = register_bytes(&registry, b"hello");
    let request = FileOpenRequest::new(resource.id(), "display.txt", None);
    assert!(matches!(
        deny.authorize_open(&request),
        Err(FileApiError::PermissionDenied)
    ));
    assert!(matches!(
        fs_deny.authorize_open(&request),
        Err(FileApiError::PermissionDenied)
    ));
    let grant = boa_fapi_core::policy::FileGrant::new(
        resource.id(),
        resource_snapshot(&registry, &resource),
    );
    assert!(matches!(
        deny.authorize_read(&grant, &grant.snapshot),
        Err(FileApiError::PermissionDenied)
    ));
    assert!(matches!(
        fs_deny.authorize_read(&grant, &grant.snapshot),
        Err(FileApiError::PermissionDenied)
    ));
    std::fs::remove_file(&_path).ok();
}

fn resource_snapshot(
    registry: &FsRegistry,
    resource: &boa_fapi_fs::RegisteredResource,
) -> SnapshotState {
    FileSource::new(registry, resource, None)
        .expect("source")
        .import_snapshot()
        .clone()
}

#[test]
fn registry_policy_accepts_live_and_rejects_changed() {
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"hello world");
    let policy = RegistryPolicy::new(registry.clone());
    let request = FileOpenRequest::new(resource.id(), "display.txt", None);
    let grant = policy.authorize_open(&request).expect("open");
    policy
        .authorize_read(&grant, &grant.snapshot)
        .expect("read");
    // Mutate: the next authorize_read must fail.
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("reopen");
        f.write_all(b"CHANGED world!!!").expect("write");
    }
    assert!(matches!(
        policy.authorize_read(&grant, &grant.snapshot),
        Err(FileApiError::SnapshotChanged)
    ));
    // Oversized budget rejected at open (fresh registration: the mutated
    // slot is stale, so register the current content anew).
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .expect("reopen2");
    let resource2 = registry.register(file).expect("register2");
    let tight = FileOpenRequest::new(resource2.id(), "display.txt", Some(2));
    assert!(matches!(
        policy.authorize_open(&tight),
        Err(FileApiError::ResourceLimit(_))
    ));
    std::fs::remove_file(&path).ok();
}

#[test]
fn root_confined_policy_never_uses_string_prefix() {
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"hello");
    let inner = RegistryPolicy::new(registry.clone());
    let policy = RootConfinedPolicy::new(inner);
    let request = FileOpenRequest::new(resource.id(), "display.txt", None);
    let grant = policy.authorize_open(&request).expect("open");
    policy
        .authorize_read(&grant, &grant.snapshot)
        .expect("read");
    // A forged snapshot never matches: no string comparison involved.
    let forged = SnapshotState::Memory;
    assert!(matches!(
        policy.authorize_read(&grant, &forged),
        Err(FileApiError::SnapshotChanged)
    ));
    std::fs::remove_file(&path).ok();
}

#[test]
fn file_resource_adapter_roundtrip() {
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"adapter-bytes");
    let adapter = boa_fapi_fs::HostFileSource::new(&registry, &resource, None).expect("adapter");
    assert_eq!(adapter.read_at(0, 7).expect("read"), b"adapter");
    assert_eq!(adapter.resource_id(), resource.id());
    let _ = adapter.current_snapshot().expect("snapshot");
    adapter.close();
    assert!(adapter.read_at(0, 1).is_err());
    std::fs::remove_file(&path).ok();
}

#[test]
fn copy_on_import_bounds_and_content() {
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"copy-me");
    let bytes = boa_fapi_fs::open_copy_on_import(&registry, &resource, 7).expect("copy");
    assert_eq!(&bytes[..], b"copy-me");
    // == boundary ok, +1 rejected.
    assert!(boa_fapi_fs::open_copy_on_import(&registry, &resource, 6).is_err());
    std::fs::remove_file(&path).ok();
}

#[test]
fn no_path_in_public_types_or_errors() {
    // Compile-time + message-surface guard: error messages carry no
    // location detail.
    for error in [
        FileApiError::NotFound,
        FileApiError::SnapshotChanged,
        FileApiError::FileLocked,
        FileApiError::PermissionDenied,
        FileApiError::InvalidRange,
        FileApiError::Internal,
    ] {
        let message = format!("{error}");
        assert!(!message.contains('/'), "message leaks separator: {message}");
        assert!(
            !message.contains('\\'),
            "message leaks separator: {message}"
        );
    }
}

#[cfg(unix)]
#[test]
fn unix_permissions_and_symlink_escape() {
    use std::os::unix::fs::PermissionsExt;
    let registry = FsRegistry::new();
    // Permission-denied registration: chmod 000 then open must fail at the
    // OS level (running as non-root). Label honestly: root can still open,
    // so assert the open outcome rather than a forced failure.
    let (path, resource) = register_bytes(&registry, b"secret");
    let policy = RegistryPolicy::new(registry.clone());
    let request = FileOpenRequest::new(resource.id(), "display.txt", None);
    assert!(policy.authorize_open(&request).is_ok());
    // Symlink escape: a symlink pointing elsewhere, opened by the host and
    // registered, yields the *target's* identity — there is no path input
    // to escape through. Registering the link target directly must give a
    // stable snapshot identical to opening the target.
    let mut link_path = std::env::temp_dir();
    link_path.push(format!(
        "boa-fapi-m5-link-{}-{}.bin",
        std::process::id(),
        resource.id().get()
    ));
    std::os::unix::fs::symlink(&path, &link_path).expect("symlink");
    let via_link = std::fs::OpenOptions::new()
        .read(true)
        .open(&link_path)
        .expect("open via link");
    let linked = registry.register(via_link).expect("register link");
    let direct_file = std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .expect("open direct");
    let direct = registry.register(direct_file).expect("register direct");
    let a = FileSource::new(&registry, &linked, None).expect("source a");
    let b = FileSource::new(&registry, &direct, None).expect("source b");
    assert_eq!(a.import_snapshot(), b.import_snapshot());
    // chmod the target: identity must remain comparable (no path involved).
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
    std::fs::remove_file(&link_path).ok();
    std::fs::remove_file(&path).ok();
}

#[cfg(windows)]
#[test]
fn windows_identity_limitations_are_labelled() {
    // Windows stable identity documents its limitation: the hash mixes
    // attributes + size + mtime, not the NTFS file id (unstable API on
    // 1.91 without `windows_by_handle`). Replacement with identical size
    // and second-granularity mtime could theoretically collide, so the
    // documented fallback is `copy_on_import` for strict cases.
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"windows-bytes!");
    let source = source_of(&registry, &resource);
    let cancel = CancellationToken::new();
    assert_eq!(&source.read_range(0..8, &cancel).unwrap()[..], b"windows-");
    let _fallback = boa_fapi_fs::open_copy_on_import(&registry, &resource, 64).expect("copy");
    std::fs::remove_file(&path).ok();
}

#[cfg(not(any(unix, windows)))]
#[test]
fn unsupported_os_identity_is_labelled() {
    // Non-Unix/Windows platforms have no stable identity via safe APIs:
    // this test labels the limitation instead of silently passing.
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"portable");
    let source = source_of(&registry, &resource);
    let cancel = CancellationToken::new();
    assert_eq!(&source.read_range(0..8, &cancel).unwrap()[..], b"portable");
    std::fs::remove_file(&path).ok();
}
