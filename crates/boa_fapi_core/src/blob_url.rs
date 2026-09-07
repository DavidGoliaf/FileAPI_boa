//! Isolated Blob URL store, environment identity and URL format helpers.
//!
//! This module is Boa-free: it owns the host-controlled environment
//! descriptor, the comparable same-partition key, the thread-safe URL store
//! and the `blob:<serialized-origin>/<uuid>` format/parse helpers. It never
//! touches JavaScript, paths, capabilities, OS handles or secrets.
//!
//! Secrecy rules (enforced by construction and by tests):
//!
//! - a URL carries only the serialized origin and a random UUID: no storage
//!   partition, no capability, no handle, no path, no partition key;
//! - [`BlobUrlStore::resolve`] checks the full [`EnvironmentKey`] (origin
//!   *and* partition *and* opaque nonce) before handing out the payload;
//! - malformed, unknown, revoked and foreign-partition URLs share one
//!   externally observable failure class: [`BlobUrlError::Malformed`] and
//!   [`BlobUrlError::Unavailable`] have the identical display string, and
//!   neither display carries a token, UUID, origin internals, an existence
//!   bit or host metadata.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::blob::BlobData;

/// Global kind selecting the compatibility surface of an environment.
///
/// Mirrors the `FileApiEnvironment` kind surface of the Boa bindings without
/// depending on them: the mapping between the two is a plain `From`
/// conversion owned by the bindings crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnvironmentKind {
    /// A window-like environment.
    Window,
    /// A dedicated worker.
    DedicatedWorker,
    /// A shared worker.
    SharedWorker,
    /// A service worker: Blob URL creation is forbidden.
    ServiceWorker,
}

/// Host-supplied descriptor of one logical global environment.
///
/// The descriptor is explicit host configuration: it is never derived from
/// a thread id, a context address, a process property or callback presence.
/// It carries the global kind, the serialized origin (the exact string
/// embedded in `blob:` URLs, `"null"` for opaque origins) and the opaque
/// storage-partition identity plus a per-global nonce. The partition and
/// the nonce never appear in a URL.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct EnvironmentDescriptor {
    kind: EnvironmentKind,
    serialized_origin: String,
    partition: u64,
    nonce: u64,
}

impl EnvironmentDescriptor {
    /// Maximum accepted serialized-origin length (DoS bound, documented).
    pub const MAX_ORIGIN_LEN: usize = 512;

    /// Builds a tuple-origin descriptor.
    ///
    /// Returns [`BlobUrlError::Malformed`] when `serialized_origin` is
    /// empty, longer than [`Self::MAX_ORIGIN_LEN`], or contains whitespace
    /// or non-printable-ASCII characters. The origin may contain `/`
    /// characters (e.g. `https://host/`); URL parsing splits on the last
    /// `/`, so lookup stays exact.
    pub fn new(
        kind: EnvironmentKind,
        serialized_origin: impl Into<String>,
        partition: u64,
        nonce: u64,
    ) -> Result<Self, BlobUrlError> {
        let serialized_origin = serialized_origin.into();
        validate_origin(&serialized_origin)?;
        Ok(Self {
            kind,
            serialized_origin,
            partition,
            nonce,
        })
    }

    /// Builds an opaque-origin descriptor.
    ///
    /// The serialized origin is the fixed string `"null"`, but the key
    /// carries the caller-supplied `nonce`, so two opaque globals never
    /// share a key even though their URLs share the `blob:null/` prefix.
    /// The host must supply a fresh nonce per global; uniqueness (not
    /// secrecy) is required here — URL unguessability comes from the UUID.
    pub fn opaque(kind: EnvironmentKind, partition: u64, nonce: u64) -> Result<Self, BlobUrlError> {
        Ok(Self {
            kind,
            serialized_origin: String::from("null"),
            partition,
            nonce,
        })
    }

    /// Returns the global kind.
    pub fn kind(&self) -> EnvironmentKind {
        self.kind
    }

    /// Returns the exact serialized origin embedded in `blob:` URLs.
    pub fn serialized_origin(&self) -> &str {
        &self.serialized_origin
    }

    /// Returns the comparable same-partition key of this descriptor.
    pub fn key(&self) -> EnvironmentKey {
        EnvironmentKey {
            origin: self.serialized_origin.clone(),
            partition: self.partition,
            nonce: self.nonce,
        }
    }

    /// Returns `true` for the service-worker kind, where Blob URL creation
    /// is forbidden.
    pub fn creation_forbidden(&self) -> bool {
        matches!(self.kind, EnvironmentKind::ServiceWorker)
    }
}

impl std::fmt::Debug for EnvironmentDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The partition and nonce are host internals: they never appear in
        // debug output, tracing payloads or test assertions.
        f.debug_struct("EnvironmentDescriptor")
            .field("kind", &self.kind)
            .field("serialized_origin", &self.serialized_origin)
            .field("partition", &"<redacted>")
            .field("nonce", &"<redacted>")
            .finish()
    }
}

/// Validates a serialized origin without parsing it as a full URL.
///
/// Full WHATWG URL parsing is out of scope by design; this is a narrow
/// shape check only (non-empty, bounded, printable ASCII, no whitespace).
fn validate_origin(origin: &str) -> Result<(), BlobUrlError> {
    if origin.is_empty() || origin.len() > EnvironmentDescriptor::MAX_ORIGIN_LEN {
        return Err(BlobUrlError::Malformed);
    }
    let ok = origin
        .bytes()
        .all(|b| matches!(b, 0x21..=0x7E) && !b.is_ascii_whitespace());
    if ok {
        Ok(())
    } else {
        Err(BlobUrlError::Malformed)
    }
}

/// Comparable internal key for same-partition checks.
///
/// Never serialized into a URL and never exposed to JavaScript. Equality
/// covers the serialized origin, the opaque storage partition and the
/// per-global nonce: one matching origin alone is never sufficient.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EnvironmentKey {
    origin: String,
    partition: u64,
    nonce: u64,
}

/// Typed Rust error for Blob URL operations.
///
/// Malformed, unknown, revoked and foreign-partition URLs share one
/// externally observable failure class: [`BlobUrlError::Malformed`] and
/// [`BlobUrlError::Unavailable`] render the identical display string, which
/// carries no token, UUID, origin internals, existence bit or host
/// metadata. Limit/shutdown denials are distinct variants because they
/// describe the caller's own quota/state, not another entry's existence.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BlobUrlError {
    /// The URL is not a well-formed store URL.
    #[error("blob URL is not available")]
    Malformed,
    /// The URL is unknown, revoked, or owned by another partition.
    ///
    /// A single class on purpose: callers cannot distinguish absence from
    /// foreign ownership.
    #[error("blob URL is not available")]
    Unavailable,
    /// A freshly generated UUID collided with a live entry (or the
    /// collision-retry budget was exhausted): nothing was overwritten.
    #[error("blob URL is not available")]
    Collision,
    /// The per-global URL quota is exhausted; nothing was stored.
    #[error("blob URL quota exceeded")]
    LimitExceeded,
    /// Blob URL creation is forbidden in this environment (service worker).
    #[error("blob URL creation is not allowed")]
    Forbidden,
    /// The object passed for URL creation carries no Blob brand.
    #[error("not a Blob object")]
    InvalidObject,
    /// The runtime is shut down.
    #[error("the File API runtime is shut down")]
    Shutdown,
    /// No cryptographic entropy is available on this platform.
    #[error("blob URL is not available")]
    EntropyUnavailable,
    /// The store lock was poisoned; no entry was touched.
    #[error("internal blob URL error")]
    Internal,
}

/// Host-side resolution result: the shared payload plus public metadata.
///
/// Carries no path, capability, OS handle, partition key or URL token —
/// only the immutable bytes (through the shared [`BlobData`]), the media
/// type and the checked length.
#[derive(Debug, Clone)]
pub struct ResolvedBlob {
    data: Arc<BlobData>,
}

impl ResolvedBlob {
    /// Wraps already-authorized shared payload data.
    pub fn new(data: Arc<BlobData>) -> Self {
        Self { data }
    }

    /// Returns the shared immutable payload.
    pub fn blob_data(&self) -> &Arc<BlobData> {
        &self.data
    }

    /// Returns the normalized media type.
    pub fn media_type(&self) -> &str {
        self.data.media_type()
    }

    /// Returns the checked byte length.
    pub fn size(&self) -> u64 {
        self.data.size()
    }
}

/// One store entry: shared payload, environment ownership, creation order.
///
/// The creation sequence is metadata only: it never seeds UUID generation
/// and never appears in a URL.
#[derive(Debug, Clone)]
struct BlobUrlEntry {
    data: Arc<BlobData>,
    owner: EnvironmentKey,
    #[allow(dead_code)]
    seq: u64,
}

/// Thread-safe Blob URL store with amortized O(1) operations.
///
/// The map key is the full URL string; every mutating lookup holds the
/// mutex only for the map operation itself, never across I/O or JS calls.
#[derive(Debug, Default)]
pub struct BlobUrlStore {
    entries: Mutex<HashMap<String, BlobUrlEntry>>,
    next_seq: AtomicU64,
}

impl BlobUrlStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            next_seq: AtomicU64::new(0),
        }
    }

    /// Inserts `url` unless the quota is exhausted or the URL is taken.
    ///
    /// The quota check and the insert run atomically under one lock hold,
    /// so concurrent creators cannot overshoot `cap`. A taken URL yields
    /// [`BlobUrlError::Collision`] and the live entry is left untouched:
    /// silent overwrites never happen. `cap == 0` always fails.
    pub fn insert_capped(
        &self,
        url: String,
        owner: EnvironmentKey,
        data: Arc<BlobData>,
        cap: usize,
    ) -> Result<(), BlobUrlError> {
        let mut entries = self.entries.lock().map_err(|_| BlobUrlError::Internal)?;
        if entries.len() >= cap {
            return Err(BlobUrlError::LimitExceeded);
        }
        if entries.contains_key(&url) {
            return Err(BlobUrlError::Collision);
        }
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        entries.insert(url, BlobUrlEntry { data, owner, seq });
        Ok(())
    }

    /// Resolves `url` for `requester`.
    ///
    /// Parses the store URL shape, then requires entry ownership by the
    /// full [`EnvironmentKey`] before handing out the shared payload.
    /// Malformed input yields [`BlobUrlError::Malformed`]; unknown, revoked
    /// and foreign-partition URLs all yield [`BlobUrlError::Unavailable`]
    /// with the identical display string.
    pub fn resolve(
        &self,
        url: &str,
        requester: &EnvironmentKey,
    ) -> Result<ResolvedBlob, BlobUrlError> {
        parse_blob_url(url)?;
        let entries = self.entries.lock().map_err(|_| BlobUrlError::Internal)?;
        let Some(entry) = entries.get(url) else {
            return Err(BlobUrlError::Unavailable);
        };
        if entry.owner != *requester {
            return Err(BlobUrlError::Unavailable);
        }
        Ok(ResolvedBlob {
            data: Arc::clone(&entry.data),
        })
    }

    /// Revokes `url` idempotently.
    ///
    /// Removing the entry only stops *new* resolutions: reads that already
    /// hold the `Arc<BlobData>` continue to completion. Malformed URLs and
    /// URLs owned by another partition are silent no-ops, so revoke can
    /// never serve as an enumeration oracle. Never fails.
    pub fn revoke(&self, url: &str) {
        if parse_blob_url(url).is_err() {
            return;
        }
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(url);
        }
    }

    /// Removes every entry, releasing all strong payload references.
    pub fn clear(&self) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.clear();
        }
    }

    /// Returns the number of live entries.
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .map(|entries| entries.len())
            .unwrap_or(0)
    }

    /// Returns `true` when no entry is live.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Formats 16 random bytes as an RFC 4122 version-4 UUID string.
///
/// The version and variant bits are set here (`xxxx4xxx-` and `8/9/a/b`),
/// so callers pass raw CSPRNG output; counters, timestamps and predictable
/// PRNGs are forbidden as the byte source by contract, not by code — the
/// production entropy source documents its CSPRNG property.
pub fn format_uuid_v4(random: [u8; 16]) -> String {
    let mut b = random;
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(36);
    for (index, byte) in b.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0F) as usize] as char);
    }
    out
}

/// Serializes a store URL: `blob:<serialized-origin>/<uuid>`.
pub fn format_blob_url(origin: &str, uuid: &str) -> String {
    let mut url = String::with_capacity(5 + origin.len() + 1 + uuid.len());
    url.push_str("blob:");
    url.push_str(origin);
    url.push('/');
    url.push_str(uuid);
    url
}

/// Parsed store URL shape: serialized origin plus UUID token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedBlobUrl<'a> {
    /// The serialized origin between `blob:` and the last `/`.
    pub origin: &'a str,
    /// The UUID token after the last `/`.
    pub uuid: &'a str,
}

/// Parses the store URL shape without resolving anything.
///
/// Requires the `blob:` scheme, a non-empty origin and a strict version-4
/// UUID token (36 characters, hyphens at 8/13/18/23, `4` version nibble,
/// `8/9/a/b` variant nibble). Anything else is [`BlobUrlError::Malformed`].
/// A well-shaped but unknown URL still needs a store lookup, which reports
/// [`BlobUrlError::Unavailable`] — never a distinct "not found" oracle.
pub fn parse_blob_url(url: &str) -> Result<ParsedBlobUrl<'_>, BlobUrlError> {
    let rest = url.strip_prefix("blob:").ok_or(BlobUrlError::Malformed)?;
    let (origin, uuid) = rest.rsplit_once('/').ok_or(BlobUrlError::Malformed)?;
    if origin.is_empty() || origin.len() > EnvironmentDescriptor::MAX_ORIGIN_LEN {
        return Err(BlobUrlError::Malformed);
    }
    if !is_uuid_v4(uuid) {
        return Err(BlobUrlError::Malformed);
    }
    Ok(ParsedBlobUrl { origin, uuid })
}

/// Checks the strict version-4 UUID shape.
fn is_uuid_v4(token: &str) -> bool {
    let bytes = token.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 8 | 13 | 18 | 23) {
            if *byte != b'-' {
                return false;
            }
        } else if !byte.is_ascii_hexdigit() {
            return false;
        }
    }
    bytes[14] == b'4' && matches!(bytes[19], b'8' | b'9' | b'a' | b'b' | b'A' | b'B')
}
