//! Versioned structured-clone payload for `Blob`, `File` and `FileList`.
//!
//! This module is Boa-free: it owns the versioned *encoding* of already-
//! materialized immutable bytes plus public `Blob`/`File`/`FileList`
//! metadata. Encoding from live JS objects and decoding into live JS
//! objects are the bindings crate's job; this module never touches
//! JavaScript, paths, capabilities, OS handles or snapshot identities.
//!
//! Encoding properties (enforced by construction and by tests):
//!
//! - stable version [`CLONE_ENCODING_VERSION`]: same-version fixtures
//!   decode; unknown future versions are rejected, never misread;
//! - checked arithmetic everywhere: byte lengths, string lengths, the file
//!   count and the total payload are bounded by explicit ceilings before
//!   any allocation, so malformed/truncated/overflowing input fails as
//!   [`CloneError`] without panic and without partial output;
//! - least surprise for text: colorspaces are raw UTF-8 bytes;
//!   `SerializedBlob.media_type` and `SerializedFile.media_type` are
//!   normalized ASCII-lowercase MIME strings, `SerializedFile.name` is the
//!   sanitized display name (`/` already became `:` at the JS boundary);
//! - SCF tag mapping (documented in the crate ADR): `SCF_BLOB_TAG` and
//!   `SCF_FILE_TAG` are the only tags this encoding assigns; they live in
//!   the private SCF namespace range and are stable with the version above.

use bytes::Bytes;

use crate::mime::normalize_blob_type;

/// Current encoding version: bumped only with a documented migration, and
/// only ever rejected (never silently reinterpreted) by older decoders.
pub const CLONE_ENCODING_VERSION: u32 = 1;

/// SCF tag assigned to the `Blob` payload shape.
pub const SCF_BLOB_TAG: u32 = 0x424C_4F42;
/// SCF tag assigned to the `File` payload shape.
pub const SCF_FILE_TAG: u32 = 0x4649_4C45;
/// SCF tag assigned to the `FileList` payload shape.
pub const SCF_FILE_LIST_TAG: u32 = 0x464C_5354;

/// Hard ceilings for a single decode (DoS bounds, documented).
pub const MAX_CLONE_BYTES: usize = 256 * 1024 * 1024;
/// Maximum UTF-8 name/type byte length accepted by the decoder.
pub const MAX_CLONE_STRING_BYTES: usize = 1024 * 1024;
/// Maximum `File` entries accepted in one `FileList` payload.
pub const MAX_CLONE_FILES: usize = 100_000;

/// Maximum entries a *single encode* call may emit (the per-context
/// `max_blob_urls_per_global`-style quota has no clone analogue: clone
/// payloads are bounded by byte ceilings instead).
pub const MAX_ENCODE_FILES: usize = 100_000;

/// Materialized `Blob` payload: immutable bytes plus the public type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerializedBlob {
    /// The immutable content bytes.
    pub bytes: Bytes,
    /// The normalized public media type.
    pub media_type: String,
}

/// Materialized `File` payload: content plus the public metadata JS sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerializedFile {
    /// The immutable content bytes.
    pub bytes: Bytes,
    /// The normalized public media type.
    pub media_type: String,
    /// The sanitized display name (`/` already became `:` at the boundary).
    pub name: String,
    /// The `lastModified` timestamp (opaque i64, no clock involved).
    pub last_modified: i64,
}

/// Structured-clone payload for `Blob` / `File` / `FileList`.
///
/// Carries materialized immutable bytes and public metadata only — never a
/// host path, filesystem capability, OS handle or snapshot identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileApiClonePayload {
    /// A single `Blob`.
    Blob(SerializedBlob),
    /// A single `File`.
    File(SerializedFile),
    /// An ordered `FileList` (identity holds only within the result).
    FileList(Vec<SerializedFile>),
}

impl FileApiClonePayload {
    /// Returns the SCF tag of this payload's shape.
    pub fn tag(&self) -> u32 {
        match self {
            Self::Blob(_) => SCF_BLOB_TAG,
            Self::File(_) => SCF_FILE_TAG,
            Self::FileList(_) => SCF_FILE_LIST_TAG,
        }
    }

    /// Encodes this payload with [`CLONE_ENCODING_VERSION`].
    ///
    /// Fails as [`CloneError`] (without partial bytes) when a length or the
    /// file count exceeds the documented ceilings.
    pub fn encode(&self) -> Result<Vec<u8>, CloneError> {
        encode_payload(self)
    }

    /// Decodes one payload, enforcing version and checked bounds.
    ///
    /// Malformed, truncated, overflowing or unknown-version input fails as
    /// [`CloneError`] without panic and without partial output.
    pub fn decode(input: &[u8]) -> Result<Self, CloneError> {
        decode_payload(input)
    }
}

/// Typed Rust error for clone encode/decode failures.
///
/// Display strings are fixed generics: they never carry payload bytes,
/// names, paths, capabilities or entry-existence detail.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CloneError {
    /// The input is too short or structurally invalid.
    #[error("clone payload is malformed")]
    Malformed,
    /// The encoding version is not supported by this decoder.
    #[error("clone payload version is not supported")]
    UnsupportedVersion,
    /// A length, count or total exceeds the documented ceilings.
    #[error("clone payload exceeds the configured limits")]
    LimitExceeded,
    /// The object passed for encoding carries no usable brand.
    #[error("not a clonable File API object")]
    InvalidObject,
    /// The payload kind does not match the decode entry point.
    #[error("clone payload kind mismatch")]
    UnexpectedKind,
    /// Materialization of the source failed (snapshot, permission,
    /// short-read or internal failure); nothing partial was produced.
    #[error("clone source could not be materialized")]
    SourceFailed,
    /// No host bridge is registered (or the feature is off).
    #[error("no structured-clone bridge is registered")]
    NoBridge,
    /// The runtime is shut down.
    #[error("the File API runtime is shut down")]
    Shutdown,
    /// An internal clone error occurred.
    #[error("internal clone error")]
    Internal,
}

/// Builds a `SerializedBlob` from already-materialized bytes.
///
/// Normalizes the media type with the M1 MIME rules; returns
/// [`CloneError::LimitExceeded`] when `bytes` exceeds
/// [`MAX_CLONE_BYTES`].
pub fn serialized_blob(bytes: Bytes, media_type: &str) -> Result<SerializedBlob, CloneError> {
    if bytes.len() > MAX_CLONE_BYTES {
        return Err(CloneError::LimitExceeded);
    }
    Ok(SerializedBlob {
        bytes,
        media_type: normalize_blob_type(media_type),
    })
}

/// Builds a `SerializedFile` from already-materialized bytes and public
/// metadata.
///
/// `name` must already be sanitized (`/` became `:` at the JS boundary);
/// the media type is normalized with the M1 MIME rules. String byte
/// lengths and the byte length are checked against the documented
/// ceilings; clock reads never happen here.
pub fn serialized_file(
    bytes: Bytes,
    media_type: &str,
    name: &str,
    last_modified: i64,
) -> Result<SerializedFile, CloneError> {
    if bytes.len() > MAX_CLONE_BYTES {
        return Err(CloneError::LimitExceeded);
    }
    if name.len() > MAX_CLONE_STRING_BYTES || media_type.len() > MAX_CLONE_STRING_BYTES {
        return Err(CloneError::LimitExceeded);
    }
    Ok(SerializedFile {
        bytes,
        media_type: normalize_blob_type(media_type),
        name: name.to_owned(),
        last_modified,
    })
}

fn encode_payload(payload: &FileApiClonePayload) -> Result<Vec<u8>, CloneError> {
    match payload {
        FileApiClonePayload::Blob(blob) => {
            let mut out = Vec::new();
            push_header(&mut out, SCF_BLOB_TAG)?;
            push_blob_body(&mut out, &blob.bytes, &blob.media_type)?;
            Ok(out)
        }
        FileApiClonePayload::File(file) => {
            let mut out = Vec::new();
            push_header(&mut out, SCF_FILE_TAG)?;
            push_file_body(&mut out, file)?;
            Ok(out)
        }
        FileApiClonePayload::FileList(files) => {
            if files.len() > MAX_ENCODE_FILES {
                return Err(CloneError::LimitExceeded);
            }
            // Bound the total before allocating: 16 bytes of framing per
            // file plus every file's own checked total.
            let mut total = 12_usize;
            for file in files {
                total = total
                    .checked_add(file_body_len(file)?)
                    .ok_or(CloneError::LimitExceeded)?;
                if total > MAX_CLONE_BYTES {
                    return Err(CloneError::LimitExceeded);
                }
            }
            let mut out = Vec::new();
            push_header(&mut out, SCF_FILE_LIST_TAG)?;
            push_u32(&mut out, files.len() as u32)?;
            for file in files {
                push_file_body(&mut out, file)?;
            }
            Ok(out)
        }
    }
}

/// Layout: `b"FCL1"` magic, u32 LE version, u32 LE SCF tag.
fn push_header(out: &mut Vec<u8>, tag: u32) -> Result<(), CloneError> {
    out.extend_from_slice(b"FCL1");
    push_u32(out, CLONE_ENCODING_VERSION)?;
    push_u32(out, tag)
}

fn push_u32(out: &mut Vec<u8>, value: u32) -> Result<(), CloneError> {
    if out.len() > MAX_CLONE_BYTES {
        return Err(CloneError::LimitExceeded);
    }
    out.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), CloneError> {
    let len = u32::try_from(bytes.len()).map_err(|_| CloneError::LimitExceeded)?;
    push_u32(out, len)?;
    let total = out
        .len()
        .checked_add(bytes.len())
        .ok_or(CloneError::LimitExceeded)?;
    if total > MAX_CLONE_BYTES {
        return Err(CloneError::LimitExceeded);
    }
    out.extend_from_slice(bytes);
    Ok(())
}

fn push_str(out: &mut Vec<u8>, text: &str) -> Result<(), CloneError> {
    push_bytes(out, text.as_bytes())
}

fn push_blob_body(out: &mut Vec<u8>, bytes: &Bytes, media_type: &str) -> Result<(), CloneError> {
    push_bytes(out, bytes)?;
    push_str(out, media_type)
}

fn push_file_body(out: &mut Vec<u8>, file: &SerializedFile) -> Result<(), CloneError> {
    push_bytes(out, &file.bytes)?;
    push_str(out, &file.media_type)?;
    push_str(out, &file.name)?;
    let timestamp = file.last_modified.to_le_bytes();
    let total = out
        .len()
        .checked_add(timestamp.len())
        .ok_or(CloneError::LimitExceeded)?;
    if total > MAX_CLONE_BYTES {
        return Err(CloneError::LimitExceeded);
    }
    out.extend_from_slice(&timestamp);
    Ok(())
}

/// Checked body length of one file entry (framing excluded).
fn file_body_len(file: &SerializedFile) -> Result<usize, CloneError> {
    file.bytes
        .len()
        .checked_add(4)
        .and_then(|total| total.checked_add(file.media_type.len()))
        .and_then(|total| total.checked_add(4))
        .and_then(|total| total.checked_add(file.name.len()))
        .and_then(|total| total.checked_add(4))
        .and_then(|total| total.checked_add(8))
        .ok_or(CloneError::LimitExceeded)
}

struct Cursor<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.input.len().saturating_sub(self.pos)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], CloneError> {
        if len > MAX_CLONE_BYTES || len > self.remaining() {
            return Err(CloneError::Malformed);
        }
        let end = self.pos.checked_add(len).ok_or(CloneError::Malformed)?;
        if end > self.input.len() {
            return Err(CloneError::Malformed);
        }
        let slice = &self.input[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn read_u32(&mut self) -> Result<u32, CloneError> {
        let bytes = self.take(4)?;
        let mut word = [0_u8; 4];
        word.copy_from_slice(bytes);
        Ok(u32::from_le_bytes(word))
    }

    fn read_magic(&mut self) -> Result<(), CloneError> {
        let magic = self.take(4)?;
        if magic == b"FCL1" {
            Ok(())
        } else {
            Err(CloneError::Malformed)
        }
    }

    fn read_bytes(&mut self) -> Result<Bytes, CloneError> {
        let len = self.read_u32()?;
        let len = usize::try_from(len).map_err(|_| CloneError::Malformed)?;
        if len > MAX_CLONE_BYTES {
            return Err(CloneError::LimitExceeded);
        }
        Ok(Bytes::copy_from_slice(self.take(len)?))
    }

    fn read_string(&mut self) -> Result<String, CloneError> {
        let bytes = self.read_bytes()?;
        if bytes.len() > MAX_CLONE_STRING_BYTES {
            return Err(CloneError::LimitExceeded);
        }
        std::str::from_utf8(&bytes)
            .map(str::to_owned)
            .map_err(|_| CloneError::Malformed)
    }

    fn finish(&self) -> Result<(), CloneError> {
        if self.pos == self.input.len() {
            Ok(())
        } else {
            Err(CloneError::Malformed)
        }
    }
}

fn decode_payload(input: &[u8]) -> Result<FileApiClonePayload, CloneError> {
    // Reject empty/truncated headers before any allocation.
    if input.len() < 12 || input.len() > MAX_CLONE_BYTES {
        if input.len() > MAX_CLONE_BYTES {
            return Err(CloneError::LimitExceeded);
        }
        return Err(CloneError::Malformed);
    }
    let mut cursor = Cursor::new(input);
    cursor.read_magic()?;
    let version = cursor.read_u32()?;
    if version != CLONE_ENCODING_VERSION {
        return Err(CloneError::UnsupportedVersion);
    }
    let tag = cursor.read_u32()?;
    let payload = match tag {
        SCF_BLOB_TAG => {
            let bytes = cursor.read_bytes()?;
            let media_type = cursor.read_string()?;
            FileApiClonePayload::Blob(SerializedBlob { bytes, media_type })
        }
        SCF_FILE_TAG => FileApiClonePayload::File(decode_file_body(&mut cursor)?),
        SCF_FILE_LIST_TAG => {
            let count = cursor.read_u32()?;
            let count = usize::try_from(count).map_err(|_| CloneError::Malformed)?;
            if count > MAX_CLONE_FILES {
                return Err(CloneError::LimitExceeded);
            }
            let mut files = Vec::new();
            for _ in 0..count {
                files.push(decode_file_body(&mut cursor)?);
            }
            FileApiClonePayload::FileList(files)
        }
        _ => return Err(CloneError::Malformed),
    };
    cursor.finish()?;
    Ok(payload)
}

fn decode_file_body(cursor: &mut Cursor<'_>) -> Result<SerializedFile, CloneError> {
    let bytes = cursor.read_bytes()?;
    let media_type = cursor.read_string()?;
    let name = cursor.read_string()?;
    let stamp = cursor.take(8)?;
    let mut word = [0_u8; 8];
    word.copy_from_slice(stamp);
    Ok(SerializedFile {
        bytes,
        media_type,
        name,
        last_modified: i64::from_le_bytes(word),
    })
}
