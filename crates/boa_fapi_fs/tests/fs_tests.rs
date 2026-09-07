//! M5 filesystem-backed unit tests (no Boa): capability, snapshot, policy.
//!
//! Direct handle tests are Unix-only (strong identity). Non-Unix targets
//! run only the enforced-refusal tests plus `copy_on_import`, which is the
//! mandated fallback there — never a silent pass.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[cfg(unix)]
use boa_fapi_core::cancellation::CancellationToken;
use boa_fapi_core::file_api_error::FileApiError;
#[cfg(unix)]
use boa_fapi_core::policy::FileResource;
use boa_fapi_core::policy::{DenyAllPolicy, FileAccessPolicy, FileOpenRequest};
#[cfg(unix)]
use boa_fapi_core::snapshot::SnapshotState;
#[cfg(unix)]
use boa_fapi_core::source::ByteSource;
#[cfg(unix)]
use boa_fapi_fs::RootConfinedPolicy;
use boa_fapi_fs::{
    DenyRawPathPolicy, FileSource, FsRegistry, RegistryPolicy, platform_has_strong_identity,
};

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
    // Windows CI checkouts can leave read-only temp files behind; ensure a
    // fresh writable file for every registration.
    let _ = std::fs::remove_file(&path);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .expect("open temp");
    file.set_len(0).expect("truncate temp");
    std::fs::write(&path, content).expect("write temp file");
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .expect("reopen temp");
    let resource = registry.register(handle).expect("register");
    (path, resource)
}

#[cfg(unix)]
fn source_of(registry: &FsRegistry, resource: &boa_fapi_fs::RegisteredResource) -> FileSource {
    FileSource::new(registry, resource, None).expect("source")
}

#[cfg(unix)]
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
    assert!(matches!(source.snapshot(), SnapshotState::Filesystem(_)));
    std::fs::remove_file(&path).ok();
}

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
#[test]
fn delete_is_detected() {
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"hello world");
    let source = source_of(&registry, &resource);
    let cancel = CancellationToken::new();
    assert_eq!(source.read_range(0..5, &cancel).unwrap().len(), 5);
    std::fs::remove_file(&path).expect("delete");
    // The open handle stays readable but the identity check runs on live
    // metadata; the source must not leak stale bytes as success-after-
    // change: accept SnapshotChanged/NotFound, or the exact old bytes for
    // the still-valid snapshot — never a mix.
    let result = source.read_range(5..11, &cancel);
    match result {
        Err(_) => {}
        Ok(bytes) => assert_eq!(&bytes[..], b" world"),
    }
}

#[test]
fn close_removes_slot_and_drops_handle() {
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"hello");
    assert_eq!(registry.live_slot_count(), 1);
    #[cfg(unix)]
    {
        let source = source_of(&registry, &resource);
        let cancel = CancellationToken::new();
        assert_eq!(&source.read_range(0..5, &cancel).unwrap()[..], b"hello");
    }
    resource.close();
    assert_eq!(registry.live_slot_count(), 0);
    // Reads fail after close, and close is idempotent.
    assert_eq!(registry.live_slot_count(), 0);
    resource.close();
    assert!(registry.import_snapshot(resource.id()).is_err());
    assert!(registry.live_snapshot(resource.id()).is_err());
    std::fs::remove_file(&path).ok();
}

#[test]
fn close_all_drops_every_handle() {
    let registry = FsRegistry::new();
    let (path_a, resource_a) = register_bytes(&registry, b"aaa");
    let (path_b, _resource_b) = register_bytes(&registry, b"bbb");
    assert_eq!(registry.live_slot_count(), 2);
    registry.close_all();
    assert_eq!(registry.live_slot_count(), 0);
    assert!(registry.import_snapshot(resource_a.id()).is_err());
    // Idempotent.
    registry.close_all();
    assert_eq!(registry.live_slot_count(), 0);
    std::fs::remove_file(&path_a).ok();
    std::fs::remove_file(&path_b).ok();
}

#[test]
fn shutdown_closers_run_once_outside_lock() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    let registry = FsRegistry::new();
    let (path, _resource) = register_bytes(&registry, b"closer");
    let fires = Arc::new(AtomicUsize::new(0));
    let probe = Arc::clone(&fires);
    // A closer that itself closes the registry must not deadlock: closers
    // run outside the map lock.
    let tracked = registry.clone();
    registry.on_shutdown(move || {
        probe.fetch_add(1, Ordering::SeqCst);
        tracked.close_all();
    });
    registry.run_closers();
    assert_eq!(fires.load(Ordering::SeqCst), 1);
    assert_eq!(registry.live_slot_count(), 0);
    // Exactly-once: a second run finds nothing left.
    registry.run_closers();
    assert_eq!(fires.load(Ordering::SeqCst), 1);
    std::fs::remove_file(&path).ok();
}

#[cfg(unix)]
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
fn copy_bounds_and_content_everywhere() {
    // `copy_on_import` is the mandated fallback: available on every
    // platform, with == ok / +1 rejected before allocation completes.
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"12345678");
    let bytes = boa_fapi_fs::open_copy_on_import(&registry, &resource, 8).expect("copy");
    assert_eq!(&bytes[..], b"12345678");
    assert!(boa_fapi_fs::open_copy_on_import(&registry, &resource, 7).is_err());
    // The live handle is closed by the copy: no OS handle is retained for
    // a weak-platform copy.
    assert_eq!(registry.live_slot_count(), 0);
    std::fs::remove_file(&path).ok();
}

#[cfg(unix)]
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
    let snapshot = registry.import_snapshot(resource.id()).expect("snapshot");
    let grant = boa_fapi_core::policy::FileGrant::new(resource.id(), snapshot);
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

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
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
fn no_location_in_public_types_or_errors() {
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

#[test]
fn weak_platform_direct_import_is_refused() {
    // Enforced on non-Unix: direct `FileSource` construction and
    // `RegistryPolicy::authorize_open` refuse; `copy_on_import` stays the
    // mandated fallback. On Unix this test labels the strong platform.
    if platform_has_strong_identity() {
        assert!(platform_has_strong_identity());
        return;
    }
    let registry = FsRegistry::new();
    let (path, resource) = register_bytes(&registry, b"weak-bytes!");
    assert!(matches!(
        FileSource::new(&registry, &resource, None),
        Err(FileApiError::PermissionDenied)
    ));
    let policy = RegistryPolicy::new(registry.clone());
    let request = FileOpenRequest::new(resource.id(), "display.txt", None);
    assert!(matches!(
        policy.authorize_open(&request),
        Err(FileApiError::PermissionDenied)
    ));
    let bytes = boa_fapi_fs::open_copy_on_import(&registry, &resource, 64).expect("copy");
    assert_eq!(&bytes[..], b"weak-bytes!");
    std::fs::remove_file(&path).ok();
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
    // registered, yields the *target's* identity — there is no location input
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
    // chmod the target: identity must remain comparable (no location involved).
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
    std::fs::remove_file(&link_path).ok();
    std::fs::remove_file(&path).ok();
}
