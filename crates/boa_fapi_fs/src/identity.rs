//! Opaque platform identity captured with safe Rust APIs.
//!
//! The identity hash mixes the platform-provided file identity (Unix
//! `dev`/`ino`, Windows `file_index`/`volume_serial`, where the platform
//! exposes them through safe `Metadata` APIs) with the size and
//! modification time. It never encodes a path, handle value, or secret
//! name. When the platform cannot provide a stable identity, callers must
//! use `copy_on_import` (see [`crate::source::open_copy_on_import`]) or
//! refuse the import; the fallback is recorded in the ADR.

use std::time::SystemTime;

use boa_fapi_core::snapshot::FileSnapshot;

/// Captures the opaque identity of an already-open file.
///
/// `file` must be opened read-only by the host before calling. Only
/// `Metadata` reads run here: no path handling, no directory traversal.
pub(crate) fn capture(file: &std::fs::File) -> Result<FileSnapshot, std::io::Error> {
    let metadata = file.metadata()?;
    Ok(snapshot_of(&metadata))
}

/// Builds a snapshot from already-read metadata.
///
/// Separated so unit tests can probe the comparison logic without I/O.
pub(crate) fn snapshot_of(metadata: &std::fs::Metadata) -> FileSnapshot {
    let size = metadata.len();
    let (mtime_secs, mtime_nanos) = match metadata.modified() {
        Ok(time) => system_time_parts(time),
        Err(_) => (None, None),
    };
    FileSnapshot::new(
        identity_hash(metadata, size, mtime_secs, mtime_nanos),
        size,
        mtime_secs,
        mtime_nanos,
    )
}

/// Splits a `SystemTime` into seconds/nanos since the Unix epoch.
///
/// Returns `(None, None)` for pre-epoch or unrepresentable times.
fn system_time_parts(time: SystemTime) -> (Option<u64>, Option<u32>) {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => Some((duration.as_secs(), duration.subsec_nanos())).unzip(),
        Err(_) => (None, None),
    }
}

/// Mixes the platform identity, size, and mtime into one opaque `u64`.
///
/// Uses the FNV-1a hash over platform-provided fields only. The result is
/// opaque: it reveals nothing about the path, handle, or location.
fn identity_hash(
    metadata: &std::fs::Metadata,
    size: u64,
    mtime_secs: Option<u64>,
    mtime_nanos: Option<u32>,
) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    let mut mix = |word: u64| {
        hash ^= word;
        hash = hash.wrapping_mul(FNV_PRIME);
    };

    platform_identity(metadata, &mut mix);
    mix(size);
    mix(mtime_secs.unwrap_or(u64::MAX));
    mix(u64::from(mtime_nanos.unwrap_or(u32::MAX)));
    hash
}

/// Feeds platform-provided file identity fields into `mix`.
///
/// Unix: `dev` + `ino` via `MetadataExt`. Windows: `file_index` +
/// `volume_serial_number` via `MetadataExt`. Other platforms: no stable
/// identity is available through safe APIs, so only size/mtime feed the
/// hash — callers on such platforms must prefer `copy_on_import` or
/// refuse the import (documented in the ADR).
#[cfg(unix)]
fn platform_identity(metadata: &std::fs::Metadata, mix: &mut dyn FnMut(u64)) {
    use std::os::unix::fs::MetadataExt;
    mix(metadata.dev());
    mix(metadata.ino());
}

#[cfg(windows)]
fn platform_identity(metadata: &std::fs::Metadata, mix: &mut dyn FnMut(u64)) {
    use std::os::windows::fs::MetadataExt;
    // Stable on 1.91 without `windows_by_handle`: fall back to the always
    // available attributes plus size/mtime (mixed by the caller). Windows
    // hosts needing strict replacement detection should use
    // `copy_on_import` (documented in the ADR).
    mix(u64::from(metadata.file_attributes()));
    mix(metadata.creation_time());
}

#[cfg(not(any(unix, windows)))]
fn platform_identity(_metadata: &std::fs::Metadata, _mix: &mut dyn FnMut(u64)) {}
