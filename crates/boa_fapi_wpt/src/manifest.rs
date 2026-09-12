//! `wpt-manifest.json` schema: pinned source, files, expectations.
//!
//! The manifest is the single source of truth for a strict run: which
//! adapted files execute, their SHA-256 integrity, the capability each
//! subtest needs, and the exact expected status. Unknown statuses,
//! wildcard expectations and expired `review_by` dates are load errors,
//! never silent `NOTRUN`.

use std::collections::BTreeMap;
use std::fmt;

use thiserror::Error;

/// Manifest schema version accepted by this harness.
pub const SCHEMA_VERSION: u32 = 2;

/// Minimal schema version accepted by the offline `--smoke` developer
/// path (legacy M7 manifests). Strict mode always requires
/// [`SCHEMA_VERSION`].
pub const MIN_SMOKE_SCHEMA_VERSION: u32 = 1;

/// Terminal per-subtest statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExpectedStatus {
    /// The subtest must pass.
    Pass,
    /// A recorded open defect: actual FAIL satisfies the strict
    /// comparison (keeps the release gate red, see `report::release_green`).
    Fail,
    /// The subtest cannot run: exact capability gap recorded.
    NotRun,
}

impl ExpectedStatus {
    /// Parses the exact status token (`PASS` / `FAIL` / `NOTRUN`).
    pub fn parse(token: &str) -> Result<Self, ManifestError> {
        match token {
            "PASS" => Ok(Self::Pass),
            "FAIL" => Ok(Self::Fail),
            "NOTRUN" => Ok(Self::NotRun),
            _ => Err(ManifestError::UnknownStatus(token.to_owned())),
        }
    }

    /// Renders the canonical status token.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::NotRun => "NOTRUN",
        }
    }
}

/// Provenance of one manifest file: how the executed bytes relate to the
/// pinned upstream file (M9-E §5 fidelity gate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Executed bytes are the raw pinned upstream bytes, byte-identical
    /// (`sha256 == upstream_sha256`, no `adapter`).
    Direct,
    /// Project-owned file executed through a deterministic adapter/patch
    /// (`adapter` id required, e.g. the FileList host fixture).
    Adapted,
}

impl Provenance {
    /// Parses the exact provenance token (`direct` / `adapted`).
    pub fn parse(token: &str) -> Result<Self, ManifestError> {
        match token {
            "direct" => Ok(Self::Direct),
            "adapted" => Ok(Self::Adapted),
            _ => Err(ManifestError::BadType("files[].provenance".to_owned())),
        }
    }

    /// Renders the canonical provenance token.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Adapted => "adapted",
        }
    }
}

/// Classification of one subtest row (M9-E §4 accounting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    /// Implemented behavior: PASS, or a recorded open defect (FAIL with
    /// an `issue` link — satisfies strict, keeps release red).
    Supported,
    /// Missing host capability from the closed allow-list
    /// ([`CAPABILITY_ALLOW_LIST`]): exact NOTRUN exclusion.
    UnsupportedHostCapability,
    /// Harness gap: never a valid long-term exclusion — breaks the
    /// release gate (load error in strict manifests).
    HarnessGap,
    /// Project-owned acceptance row (never presented as WPT).
    ProjectAcceptance,
}

impl Classification {
    /// Parses the exact classification token.
    pub fn parse(token: &str) -> Result<Self, ManifestError> {
        match token {
            "supported" => Ok(Self::Supported),
            "unsupported-host-capability" => Ok(Self::UnsupportedHostCapability),
            "harness-gap" => Ok(Self::HarnessGap),
            "project-acceptance" => Ok(Self::ProjectAcceptance),
            _ => Err(ManifestError::BadType(
                "subtests[].classification".to_owned(),
            )),
        }
    }

    /// Renders the canonical classification token.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::UnsupportedHostCapability => "unsupported-host-capability",
            Self::HarnessGap => "harness-gap",
            Self::ProjectAcceptance => "project-acceptance",
        }
    }
}

/// Closed capability allow-list (M9-E §4): `browser-only` is never valid;
/// browser-only tests are recorded under their concrete missing
/// capability instead.
pub const CAPABILITY_ALLOW_LIST: [&str; 13] = [
    "html-file-input",
    "navigation",
    "fetch",
    "mediasource",
    "worker-runtime",
    "network-wpt-server",
    "blob-constructor",
    "blob-slice",
    "promise-reads",
    "file-constructor",
    "filereader",
    "filelist",
    "blob-url",
];

/// Mandatory WPT groups: every strict manifest covers all six roots plus
/// the pinned `FileAPI/root` and `FileAPI/url` groups produced by the
/// deterministic generator.
pub const REQUIRED_GROUPS: [&str; 6] = [
    "FileAPI/blob",
    "FileAPI/file",
    "FileAPI/filelist-section",
    "FileAPI/reading-data-section",
    "FileAPI/root",
    "FileAPI/url",
];

/// One adapted corpus file entry.
#[derive(Debug, Clone)]
pub struct ManifestFile {
    /// Repository-relative adapted path, e.g. `corpus/blob-constructor.js`.
    pub path: String,
    /// Upstream `FileAPI/**` path this file adapts.
    pub upstream_path: String,
    /// Upstream git blob SHA (hex, provenance only — never evidence).
    pub upstream_blob_sha: String,
    /// Raw-content SHA-256 (hex, lowercase) of the pinned upstream bytes
    /// (M9-E §3 evidence, verified against `--upstream-root`).
    pub upstream_sha256: String,
    /// SHA-256 (hex, lowercase) of the stored corpus file bytes.
    pub sha256: String,
    /// WPT group, e.g. `FileAPI/blob`.
    pub group: String,
    /// Capability the file needs, e.g. `blob-constructor`.
    pub capability: String,
    /// Fidelity provenance: `direct` (byte-identical upstream) or
    /// `adapted` (deterministic adapter/patch + `adapter` id).
    pub provenance: Provenance,
    /// Adapter id for `adapted` files (e.g. `m9e-filelist-fixture-01`);
    /// always empty for `direct` files.
    pub adapter: String,
    /// Optional per-file host fixture hook name (only `filelist`).
    pub fixture: Option<String>,
    /// Subtests in file order (stable report order).
    pub subtests: Vec<ManifestSubtest>,
}

impl ManifestFile {
    /// Returns the fixture hook name, if declared.
    #[must_use]
    pub fn fixture(&self) -> Option<&str> {
        self.fixture.as_deref()
    }
}

/// One subtest expectation: exact IDs, no wildcards.
#[derive(Debug, Clone)]
pub struct ManifestSubtest {
    /// Test ID (file-level name).
    pub test: String,
    /// Subtest name (`-` for single-assertion files).
    pub subtest: String,
    /// Per-run timeout in milliseconds.
    pub timeout_ms: u64,
    /// Expected terminal status.
    pub expected: ExpectedStatus,
    /// Machine-readable gap reason (required unless PASS).
    pub reason: String,
    /// Capability the subtest needs.
    pub capability: String,
    /// Owner of the expectation entry.
    pub owner: String,
    /// Review date `YYYY-MM-DD`; expired entries fail strict loads.
    pub review_by: String,
    /// Trace row in `docs/spec-matrix.md`, e.g. `M9E-WPT-02`.
    pub trace: String,
    /// Accounting classification (M9-E §4).
    pub classification: Classification,
    /// Normative spec section, e.g. `FileAPI WD Blob`.
    pub spec_section: String,
    /// Open-defect link (required for expected FAIL) or
    /// capability-gap reference (required for NOTRUN).
    pub issue: String,
}

/// Pinned upstream source block.
#[derive(Debug, Clone)]
pub struct ManifestSource {
    /// Upstream repository URL.
    pub repository: String,
    /// Immutable upstream commit SHA (hex).
    pub commit: String,
    /// License identifier of the upstream corpus.
    pub license: String,
}

/// Loaded manifest: source, files keyed by path (sorted), default timeout.
#[derive(Debug, Clone)]
pub struct Manifest {
    schema_version: u32,
    /// Pinned upstream source.
    pub source: ManifestSource,
    /// Corpus root relative to the manifest directory.
    pub corpus_root: String,
    /// Default per-subtest timeout in milliseconds.
    pub default_timeout_ms: u64,
    /// Files in manifest order.
    pub files: Vec<ManifestFile>,
}

impl Manifest {
    /// Returns the validated schema version (1 = legacy smoke-only,
    /// 2 = strict M9-E gate).
    #[must_use]
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Rebuilds the manifest with cloned fields (worker fan-out across
    /// OS threads: only validated records cross the boundary, never live
    /// JS state).
    #[must_use]
    pub fn for_worker(
        &self,
        source: ManifestSource,
        corpus_root: String,
        default_timeout_ms: u64,
        files: Vec<ManifestFile>,
    ) -> Self {
        Self {
            schema_version: self.schema_version,
            source,
            corpus_root,
            default_timeout_ms,
            files,
        }
    }
}

/// Fallible manifest errors (library uses `thiserror`, never panics).
#[derive(Debug, Error)]
pub enum ManifestError {
    /// The JSON value is not an object.
    #[error("manifest root must be an object")]
    RootNotObject,
    /// A required field is missing.
    #[error("manifest is missing required field `{0}`")]
    MissingField(String),
    /// A field has the wrong JSON type.
    #[error("manifest field `{0}` has an unexpected type")]
    BadType(String),
    /// Unsupported schema version.
    #[error("unsupported manifest schema version {0}")]
    BadSchema(u32),
    /// Unknown status token.
    #[error("unknown status token `{0}`")]
    UnknownStatus(String),
    /// Non-`PASS` expectation without a gap reason.
    #[error("subtest `{0} :: {1}` needs a gap reason for {2}")]
    MissingReason(String, String, String),
    /// Non-`PASS` expectation without capability/owner/review/trace.
    #[error("subtest `{0} :: {1}` needs `{2}` for {3}")]
    MissingFieldFor(String, String, String, String),
    /// Expected FAIL without an open-defect `issue` link.
    #[error("subtest `{0} :: {1}` expects FAIL without an open defect record")]
    MissingIssue(String, String),
    /// `browser-only` is never a valid capability.
    #[error("subtest `{0} :: {1}` uses forbidden capability `browser-only`")]
    BrowserOnly(String, String),
    /// Unknown capability (not in the closed allow-list).
    #[error("subtest `{0} :: {1}` uses unknown capability `{2}`")]
    BadCapability(String, String, String),
    /// `supported` subtest wrongly marked NOTRUN.
    #[error("subtest `{0} :: {1}` is supported and must not be NOTRUN")]
    SupportedNotRun(String, String),
    /// A `harness-gap` exclusion (breaks the release gate).
    #[error("subtest `{0} :: {1}` records a harness gap (release gate is red)")]
    HarnessGap(String, String),
    /// Malformed hex hash.
    #[error("manifest field `{0}` is not lowercase hex")]
    BadHex(String),
    /// Duplicate test/subtest entry.
    #[error("duplicate expectation `{0} :: {1}`")]
    Duplicate(String, String),
    /// Duplicate JSON object key (recursive: root, source, files, subtests).
    /// Only the key name is revealed, never the value or a path.
    #[error("duplicate JSON key `{0}`")]
    DuplicateJsonKey(String),
    /// Malformed review date.
    #[error("subtest `{0} :: {1}` has a malformed review_by date")]
    BadDate(String, String),
    /// Duplicate subtest name inside one manifest file (variant A: the
    /// harness keys results by subtest name, so any collision inside a
    /// file would merge two rows into one verdict).
    #[error("duplicate subtest name `{1}` in file `{0}`")]
    DuplicateSubtest(String, String),
    /// Invalid corpus path (traversal, absolute, symlink, suffix, ...).
    /// The detail carries only the manifest-relative logical path, never
    /// an absolute path.
    #[error("invalid corpus path `{0}`")]
    BadPath(String),
    /// Symlinks are forbidden in the corpus root and candidates.
    #[error("symlinks are not allowed in corpus")]
    Symlink(String),
    /// Expired `review_by` date.
    #[error("subtest `{0} :: {1}` review_by {2} has expired")]
    Expired(String, String, String),
    /// Wildcards are forbidden in expectations.
    #[error("subtest `{0} :: {1}` must not contain wildcards")]
    Wildcard(String, String),
}

impl fmt::Display for Manifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "manifest: {} files from {}",
            self.files.len(),
            self.source.commit
        )
    }
}

/// Minimal JSON value model (no new dependency: harness parses the two
/// manifest-shaped files itself with a small recursive-descent parser).
///
/// Duplicate object keys are rejected by the parser (see
/// [`ManifestError::DuplicateJsonKey`]): no manifest with duplicate keys
/// ever reaches validation.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// JSON null.
    Null,
    /// JSON boolean.
    Bool(bool),
    /// JSON number (kept as raw text; integers parsed on demand).
    Num(String),
    /// JSON string.
    Str(String),
    /// JSON array.
    Arr(Vec<Json>),
    /// JSON object (insertion order; callers sort when serializing).
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// Reads the field `name` of a JSON object.
    pub fn field(&self, name: &str) -> Option<&Json> {
        match self {
            Self::Obj(entries) => entries.iter().find(|(k, _)| k == name).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Requires the field `name` of a JSON object.
    pub fn need(&self, name: &str) -> Result<&Json, ManifestError> {
        self.field(name)
            .ok_or_else(|| ManifestError::MissingField(name.to_owned()))
    }

    /// Views the value as a string slice.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Views the value as an array slice.
    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Self::Arr(items) => Some(items),
            _ => None,
        }
    }

    /// Views the value as a `u64` (JSON integer literal only).
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Num(raw) => raw.parse::<u64>().ok(),
            _ => None,
        }
    }

    /// Views the value as a `u32` (JSON integer literal only).
    pub fn as_u32(&self) -> Option<u32> {
        match self {
            Self::Num(raw) => raw.parse::<u32>().ok(),
            _ => None,
        }
    }
}

/// Parses a JSON document (objects, arrays, strings with escapes,
/// numbers, `true`/`false`/`null`; ASCII whitespace between tokens).
///
/// Duplicate object keys are rejected by the parser itself (see
/// [`ManifestError::DuplicateJsonKey`]): no manifest with duplicate keys
/// ever reaches validation. Depth is bounded to avoid stack exhaustion on
/// hostile input.
pub fn parse_json(text: &str) -> Result<Json, ManifestError> {
    // Defense in depth: manifests are small checked-in files; refuse
    // megabyte-scale inputs before parsing (the array cap below is the
    // second layer, the depth cap in `value` the third).
    if text.len() > 1024 * 1024 {
        return Err(ManifestError::BadType("document too large".to_owned()));
    }
    let bytes = text.as_bytes();
    let mut parser = JsonParser { bytes, pos: 0 };
    parser.skip_ws();
    let value = parser.value(0)?;
    parser.skip_ws();
    if parser.pos != bytes.len() {
        return Err(ManifestError::BadType("trailing characters".to_owned()));
    }
    Ok(value)
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, ManifestError> {
        if depth > 64 {
            return Err(ManifestError::BadType("nesting too deep".to_owned()));
        }
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.object(depth.saturating_add(1)),
            Some(b'[') => self.array(depth.saturating_add(1)),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(ManifestError::BadType("value".to_owned())),
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json, ManifestError> {
        if self.bytes.len() >= self.pos + word.len()
            && &self.bytes[self.pos..self.pos + word.len()] == word.as_bytes()
        {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(ManifestError::BadType("literal".to_owned()))
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, ManifestError> {
        if depth > 64 {
            return Err(ManifestError::BadType("nesting too deep".to_owned()));
        }
        self.pos += 1;
        let mut entries = Vec::new();
        self.skip_ws();
        if self.eat(b'}') {
            return Ok(Json::Obj(entries));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(ManifestError::BadType("object key".to_owned()));
            }
            let key = self.string()?;
            // F14: duplicate JSON object keys are a load error at every
            // level (root, source, files, subtests) — never first-wins or
            // last-wins. Only the key name is revealed.
            if entries.iter().any(|(k, _)| *k == key) {
                return Err(ManifestError::DuplicateJsonKey(key));
            }
            self.skip_ws();
            if !self.eat(b':') {
                return Err(ManifestError::BadType("object colon".to_owned()));
            }
            let value = self.value(depth.saturating_add(1))?;
            entries.push((key, value));
            self.skip_ws();
            if self.eat(b'}') {
                return Ok(Json::Obj(entries));
            }
            if !self.eat(b',') {
                return Err(ManifestError::BadType("object separator".to_owned()));
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, ManifestError> {
        if depth > 64 {
            return Err(ManifestError::BadType("nesting too deep".to_owned()));
        }
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.eat(b']') {
            return Ok(Json::Arr(items));
        }
        loop {
            if items.len() > 100_000 {
                return Err(ManifestError::BadType("array too large".to_owned()));
            }
            items.push(self.value(depth.saturating_add(1))?);
            self.skip_ws();
            if self.eat(b']') {
                return Ok(Json::Arr(items));
            }
            if !self.eat(b',') {
                return Err(ManifestError::BadType("array separator".to_owned()));
            }
            self.skip_ws();
        }
    }

    fn string(&mut self) -> Result<String, ManifestError> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err(ManifestError::BadType("unterminated string".to_owned()));
            };
            if byte == b'"' {
                self.pos += 1;
                return Ok(out);
            }
            if byte == b'\\' {
                self.pos += 1;
                let Some(esc) = self.peek() else {
                    return Err(ManifestError::BadType("unterminated escape".to_owned()));
                };
                self.pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000C}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        if self.pos + 4 > self.bytes.len() {
                            return Err(ManifestError::BadType("bad unicode escape".to_owned()));
                        }
                        let hex = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
                            .map_err(|_| ManifestError::BadType("bad unicode escape".to_owned()))?;
                        let unit = u32::from_str_radix(hex, 16)
                            .map_err(|_| ManifestError::BadType("bad unicode escape".to_owned()))?;
                        self.pos += 4;
                        if (0xD800..0xE000).contains(&unit) {
                            // Surrogate halves never appear alone in valid
                            // JSON output: reject instead of emitting U+FFFD
                            // (which would silently corrupt names/hashes).
                            return Err(ManifestError::BadType("lone surrogate escape".to_owned()));
                        }
                        let ch = char::from_u32(unit).ok_or_else(|| {
                            ManifestError::BadType("bad unicode escape".to_owned())
                        })?;
                        out.push(ch);
                    }
                    _ => return Err(ManifestError::BadType("bad escape".to_owned())),
                }
            } else if byte < 0x20 {
                return Err(ManifestError::BadType("control character".to_owned()));
            } else if byte < 0x80 {
                out.push(byte as char);
                self.pos += 1;
            } else {
                // Multi-byte UTF-8: consume the full code point (rejecting
                // lone continuation bytes and truncated sequences) instead
                // of pushing one `char` per byte (which would emit U+FFFD
                // per byte and corrupt names/hashes).
                let rest = &self.bytes[self.pos..];
                let text = std::str::from_utf8(rest)
                    .map_err(|_| ManifestError::BadType("bad utf-8".to_owned()))?;
                let mut chars = text.chars();
                let Some(ch) = chars.next() else {
                    return Err(ManifestError::BadType("bad utf-8".to_owned()));
                };
                self.pos += ch.len_utf8();
                out.push(ch);
            }
        }
    }

    fn number(&mut self) -> Result<Json, ManifestError> {
        let start = self.pos;
        if self.eat(b'-') {
            // Sign consumed; digits follow.
        }
        let mut digits = 0;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
            digits += 1;
        }
        if digits == 0 || digits > 20 {
            return Err(ManifestError::BadType("number".to_owned()));
        }
        if self.eat(b'.') {
            let mut frac = 0;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
                frac += 1;
            }
            if frac == 0 {
                return Err(ManifestError::BadType("number".to_owned()));
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            let mut exp = 0;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
                exp += 1;
            }
            if exp == 0 {
                return Err(ManifestError::BadType("number".to_owned()));
            }
        }
        let raw = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| ManifestError::BadType("number".to_owned()))?;
        Ok(Json::Num(raw.to_owned()))
    }
}

/// Returns `true` for lowercase hex strings of `len` bytes length.
fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len * 2
        && text
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Maximum accepted timeout (5 minutes): bounds `--timeout-ms` and the
/// per-subtest `timeout_ms` against unbounded/hung runs.
pub const MAX_TIMEOUT_MS: u64 = 300_000;

/// Maximum accepted corpus file bytes (1 MiB of adapted JS).
pub const MAX_CORPUS_BYTES: usize = 1024 * 1024;

/// Maximum accepted lengths for manifest text fields.
pub const MAX_PATH_LEN: usize = 256;
/// Maximum accepted lengths for test/subtest names.
pub const MAX_NAME_LEN: usize = 256;
/// Maximum accepted lengths for reason/owner/trace fields.
pub const MAX_META_LEN: usize = 512;

/// Validates a logical adapted path: exactly `corpus/<name>.js` with no
/// leading slash, drive prefix, `..` segment, NUL/control characters or
/// non-`.js` suffix. This is a logical name, never an OS path: joining
/// happens only through [`resolve_corpus_path`] after this check.
pub fn check_logical_path(path: &str) -> Result<String, ManifestError> {
    if path.is_empty() || path.len() > MAX_PATH_LEN {
        return Err(ManifestError::BadPath(path.to_owned()));
    }
    if path.contains('*') {
        return Err(ManifestError::Wildcard(path.to_owned(), String::new()));
    }
    let Some(relative) = path.strip_prefix("corpus/") else {
        return Err(ManifestError::BadPath(path.to_owned()));
    };
    if relative.is_empty() || !relative.ends_with(".js") {
        return Err(ManifestError::BadPath(path.to_owned()));
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(ManifestError::BadPath(path.to_owned()));
    }
    if path.as_bytes().iter().any(|b| *b < 0x20 || *b == 0x7F) {
        return Err(ManifestError::BadPath(path.to_owned()));
    }
    if relative.contains('\\') {
        return Err(ManifestError::BadPath(path.to_owned()));
    }
    if path.len() > 2 && path.as_bytes()[1] == b':' {
        return Err(ManifestError::BadPath(path.to_owned()));
    }
    for segment in relative.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(ManifestError::BadPath(path.to_owned()));
        }
    }
    Ok(relative.to_owned())
}

/// Validates `corpus_root` (manifest-relative): non-empty, no absolute
/// form, no backslash, no empty segment, no `..`.
///
/// NOTE: resolution failures at runtime distinguish `BadPath` (policy
/// violation: traversal/absolute/suffix) from I/O errors by mapping
/// the latter to `BadPath("cannot resolve …")` with the logical path
/// only — absolute filesystem paths never surface.
pub fn check_corpus_root(root: &str) -> Result<(), ManifestError> {
    if root.is_empty() || root.len() > MAX_PATH_LEN {
        return Err(ManifestError::BadPath(root.to_owned()));
    }
    if root.starts_with('/') || root.starts_with('\\') {
        return Err(ManifestError::BadPath(root.to_owned()));
    }
    if root
        .as_bytes()
        .iter()
        .any(|b| *b < 0x20 || *b == 0x7F || *b == b'\\')
    {
        return Err(ManifestError::BadPath(root.to_owned()));
    }
    if root.len() > 2 && root.as_bytes()[1] == b':' {
        return Err(ManifestError::BadPath(root.to_owned()));
    }
    for segment in root.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(ManifestError::BadPath(root.to_owned()));
        }
    }
    Ok(())
}

/// Resolves a validated logical path strictly inside the canonical corpus
/// root: `manifest_dir` + `corpus_root` are canonicalized first, the
/// candidate is joined via [`std::path::Path`], canonicalized, and required
/// to stay strictly inside the root. Every component of the root and the
/// candidate is checked with `symlink_metadata`: any symlink in either is
/// a launch error. Returns the canonical candidate path.
pub fn resolve_corpus_path(
    manifest_path: &str,
    corpus_root: &str,
    logical_path: &str,
) -> Result<std::path::PathBuf, ManifestError> {
    use std::path::{Component, Path};
    check_corpus_root(corpus_root)?;
    let relative = check_logical_path(logical_path)?;
    let manifest_dir = Path::new(manifest_path)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    // Canonicalize the manifest directory first: a symlinked CWD must not
    // smuggle the root outside the repository. A non-existent directory
    // is a load error (never fall back to another root).
    // NOTE: on Windows, `canonicalize` returns verbatim `\\?\`-prefixed
    // paths; `starts_with` below compares canonical-vs-canonical, so the
    // prefix is consistent on both sides.
    //
    // Empty parent means "the manifest file lives in the CWD": resolve
    // the CWD itself instead of canonicalizing the empty path (which
    // fails with NotFound on Windows and would brick root-level runs).
    let canonical_dir = if manifest_dir.as_os_str().is_empty() {
        std::env::current_dir()
    } else {
        manifest_dir.canonicalize()
    }
    .map_err(|_| ManifestError::BadPath(format!("cannot resolve {logical_path}")))?;
    let root = canonical_dir.join(corpus_root);
    reject_symlinks(&canonical_dir)?;
    // The root itself must exist as a real directory: missing roots are
    // launch errors, not silent fallbacks. (`symlink_metadata` follows no
    // links: a symlinked root is rejected here before `canonicalize`
    // would resolve it away.)
    let root_meta = match std::fs::symlink_metadata(&root) {
        Ok(meta) => meta,
        Err(_) => {
            return Err(ManifestError::BadPath(format!(
                "cannot resolve {logical_path}"
            )));
        }
    };
    if !root_meta.is_dir() || root_meta.file_type().is_symlink() {
        return Err(ManifestError::Symlink(logical_path.to_owned()));
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|_| ManifestError::BadPath(format!("cannot resolve {logical_path}")))?;
    reject_symlinks(&canonical_root)?;
    // The logical name was validated segment-by-segment above; rebuild it
    // through `Path` components (never string concatenation) as defense
    // in depth against separator confusion.
    let mut candidate = canonical_root.clone();
    for part in relative.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(ManifestError::BadPath(logical_path.to_owned()));
        }
        candidate.push(Path::new(part));
    }
    // The candidate must exist as a real file (not a directory, not a
    // symlink): `canonicalize` would otherwise resolve links away before
    // we can reject them.
    let candidate_meta = std::fs::symlink_metadata(&candidate)
        .map_err(|_| ManifestError::BadPath(format!("cannot resolve {logical_path}")))?;
    if !candidate_meta.is_file() || candidate_meta.file_type().is_symlink() {
        return Err(ManifestError::Symlink(logical_path.to_owned()));
    }
    let canonical = candidate
        .canonicalize()
        .map_err(|_| ManifestError::BadPath(format!("cannot resolve {logical_path}")))?;
    if !canonical.starts_with(&canonical_root) || canonical == canonical_root {
        return Err(ManifestError::BadPath(format!(
            "cannot resolve {logical_path}"
        )));
    }
    // Component-level symlink check on the resolved candidate path (the
    // pre-canonical check above already rejected a symlinked final file).
    reject_symlinks(&canonical)?;
    // Defensive: the logical path must reproduce itself (no `.`/`..`
    // survived the join), and only `.js` files resolve.
    let mut rebuilt = std::path::PathBuf::new();
    let mut count = 0;
    for component in canonical
        .strip_prefix(&canonical_root)
        .map_err(|_| ManifestError::BadPath(format!("cannot resolve {logical_path}")))?
        .components()
    {
        match component {
            Component::Normal(part) => {
                rebuilt.push(part);
                count += 1;
            }
            _ => return Err(ManifestError::BadPath(logical_path.to_owned())),
        }
    }
    if count == 0 || canonical.extension().and_then(|e| e.to_str()) != Some("js") {
        return Err(ManifestError::BadPath(logical_path.to_owned()));
    }
    let _ = rebuilt;
    Ok(canonical)
}

/// Rejects any symlink component of `path` (parents included).
///
/// Missing trailing components (a candidate file that exists) are fine —
/// the canonical check after join is authoritative. Every existing prefix
/// must be a real directory, never a symlink.
///
/// NOTE: drive-root prefixes (`D:\`) have no parent metadata to check —
/// `symlink_metadata` on a drive root fails on Windows; only the corpus
/// root itself and everything below it are symlink-checked.
fn reject_symlinks(path: &std::path::Path) -> Result<(), ManifestError> {
    use std::path::Component;
    let mut current = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                current.push(component.as_os_str());
                continue;
            }
            Component::CurDir | Component::ParentDir => {
                return Err(ManifestError::Symlink(path.to_string_lossy().into_owned()));
            }
            Component::Normal(_) => {}
        }
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(ManifestError::Symlink(path.to_string_lossy().into_owned()));
            }
            Ok(_) => {}
            // Missing components are fine (candidate file itself); the
            // canonical check after join is authoritative.
            Err(_) => {}
        }
    }
    Ok(())
}

/// Checks `YYYY-MM-DD` as a real calendar date (month/day validated,
/// leap years included); expiry compares lexicographically, which is
/// order-correct for this shape.
fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(|b| b.is_ascii_digit());
    if !digits(0..4) || !digits(5..7) || !digits(8..10) {
        return false;
    }
    let month: u32 = text[5..7].parse().unwrap_or(0);
    let day: u32 = text[8..10].parse().unwrap_or(0);
    let year: i32 = text[0..4].parse().unwrap_or(0);
    if !(1..=12).contains(&month) || day < 1 {
        return false;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    day <= max_day
}

/// Resolves the effective capability: the subtest-level value wins; an
/// empty subtest value inherits the file-level capability.
fn effective_capability(file_capability: &str, sub_capability: &str) -> String {
    if sub_capability.is_empty() {
        file_capability.to_owned()
    } else {
        sub_capability.to_owned()
    }
}

/// Validates one closed-list capability value (schema 2): non-empty,
/// listed in [`CAPABILITY_ALLOW_LIST`], never `browser-only` (which is
/// not in the list by design and gets its own error).
fn check_capability(test: &str, subtest: &str, capability: &str) -> Result<(), ManifestError> {
    if capability == "browser-only" {
        return Err(ManifestError::BrowserOnly(
            test.to_owned(),
            subtest.to_owned(),
        ));
    }
    if !CAPABILITY_ALLOW_LIST.contains(&capability) {
        return Err(ManifestError::BadCapability(
            test.to_owned(),
            subtest.to_owned(),
            capability.to_owned(),
        ));
    }
    Ok(())
}

/// Enforces the M9-E §4 accounting rules for one manifest row:
///
/// - `supported` is never `NOTRUN`;
/// - expected `FAIL` needs a non-empty open-defect `issue` link;
/// - `harness-gap` breaks the release gate (load error);
/// - `unsupported-host-capability` needs a closed-list capability
///   (never `browser-only`).
fn check_expectation_rules(
    test: &str,
    subtest: &str,
    expected: ExpectedStatus,
    classification: Classification,
    capability: &str,
    reason: &str,
    issue: &str,
) -> Result<(), ManifestError> {
    check_capability(test, subtest, capability)?;
    match classification {
        Classification::Supported => {
            if expected == ExpectedStatus::NotRun {
                return Err(ManifestError::SupportedNotRun(
                    test.to_owned(),
                    subtest.to_owned(),
                ));
            }
            if expected == ExpectedStatus::Fail && issue.is_empty() {
                return Err(ManifestError::MissingIssue(
                    test.to_owned(),
                    subtest.to_owned(),
                ));
            }
        }
        Classification::UnsupportedHostCapability => {
            if reason.is_empty() {
                return Err(ManifestError::MissingReason(
                    test.to_owned(),
                    subtest.to_owned(),
                    expected.token().to_owned(),
                ));
            }
        }
        Classification::HarnessGap => {
            return Err(ManifestError::HarnessGap(
                test.to_owned(),
                subtest.to_owned(),
            ));
        }
        Classification::ProjectAcceptance => {}
    }
    Ok(())
}

/// One parsed `expectations.json` row (M9-E §4): the exact
/// `(upstream_path, test, subtest)` id plus the full accounting record.
#[derive(Debug, Clone)]
pub struct ExpectationRow {
    /// Pinned upstream `FileAPI/**` path.
    pub upstream_path: String,
    /// Test ID (file-level name).
    pub test: String,
    /// Subtest name.
    pub subtest: String,
    /// Expected terminal status.
    pub expected: ExpectedStatus,
    /// Accounting classification.
    pub classification: Classification,
    /// Closed-list capability.
    pub capability: String,
    /// Exact gap/defect reason.
    pub reason: String,
    /// Owner of the entry.
    pub owner: String,
    /// Review date `YYYY-MM-DD`.
    pub review_by: String,
    /// Trace row, e.g. `M9E-WPT-03`.
    pub trace: String,
    /// Normative spec section.
    pub spec_section: String,
    /// Open-defect link (FAIL) or gap reference (NOTRUN).
    pub issue: String,
    /// `direct`, the fixture adapter id, or empty (file-level exclusion).
    pub adapter: String,
}

/// Loads and validates an `expectations.json` document (schema 1).
///
/// Rules: exact `(upstream_path, test, subtest)` rows, no wildcards or
/// file-level catch-alls (every row names a subtest), closed-list
/// capabilities (never `browser-only`), the §4 classification matrix
/// (same rules as manifest rows), and the
/// pinned repository/commit literals matching the manifest source.
/// Expired `review_by` fails the load.
pub fn load_expectations(
    text: &str,
    today: &str,
    source: &ManifestSource,
) -> Result<Vec<ExpectationRow>, ManifestError> {
    let root = parse_json(text)?;
    if !matches!(root, Json::Obj(_)) {
        return Err(ManifestError::RootNotObject);
    }
    let schema = root
        .need("schema_version")?
        .as_u32()
        .ok_or_else(|| ManifestError::BadType("schema_version".to_owned()))?;
    if schema != 1 {
        return Err(ManifestError::BadSchema(schema));
    }
    let repository = root
        .need("repository")?
        .as_str()
        .ok_or_else(|| ManifestError::BadType("repository".to_owned()))?;
    let commit = root
        .need("commit")?
        .as_str()
        .ok_or_else(|| ManifestError::BadType("commit".to_owned()))?;
    if repository != source.repository || commit != source.commit {
        return Err(ManifestError::BadType("expectations source".to_owned()));
    }
    if !is_hex(commit, 20) {
        return Err(ManifestError::BadHex("commit".to_owned()));
    }
    let rows = root
        .need("expectations")?
        .as_arr()
        .ok_or_else(|| ManifestError::BadType("expectations".to_owned()))?;
    if rows.is_empty() || rows.len() > 100_000 {
        return Err(ManifestError::BadType("expectations".to_owned()));
    }
    let mut out = Vec::new();
    let mut seen: BTreeMap<(String, String, String), ()> = BTreeMap::new();
    for row in rows {
        let upstream_path = row
            .need("upstream_path")?
            .as_str()
            .ok_or_else(|| ManifestError::BadType("upstream_path".to_owned()))?
            .to_owned();
        let test = row
            .need("test")?
            .as_str()
            .ok_or_else(|| ManifestError::BadType("test".to_owned()))?
            .to_owned();
        let subtest = row
            .need("subtest")?
            .as_str()
            .ok_or_else(|| ManifestError::BadType("subtest".to_owned()))?
            .to_owned();
        let status = row
            .need("status")?
            .as_str()
            .ok_or_else(|| ManifestError::BadType("status".to_owned()))?;
        let expected = ExpectedStatus::parse(status)?;
        let classification = Classification::parse(
            row.need("classification")?
                .as_str()
                .ok_or_else(|| ManifestError::BadType("classification".to_owned()))?,
        )?;
        let capability = row
            .need("capability")?
            .as_str()
            .ok_or_else(|| ManifestError::BadType("capability".to_owned()))?
            .to_owned();
        let reason = row
            .field("reason")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_owned();
        let owner = row
            .field("owner")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_owned();
        let review_by = row
            .field("review_by")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_owned();
        let trace = row
            .field("trace")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_owned();
        let spec_section = row
            .field("spec_section")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_owned();
        let issue = row
            .field("issue")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_owned();
        let adapter = row
            .field("adapter")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_owned();
        if upstream_path.is_empty()
            || test.is_empty()
            || subtest.is_empty()
            || upstream_path.len() > MAX_PATH_LEN
            || test.len() > MAX_NAME_LEN
            || subtest.len() > MAX_NAME_LEN
        {
            return Err(ManifestError::BadType("expectation id".to_owned()));
        }
        if !upstream_path.starts_with("FileAPI/") {
            return Err(ManifestError::BadType("upstream_path".to_owned()));
        }
        if test.contains('*') || subtest.contains('*') || upstream_path.contains('*') {
            return Err(ManifestError::Wildcard(test.clone(), subtest.clone()));
        }
        for value in [
            &reason,
            &owner,
            &review_by,
            &trace,
            &spec_section,
            &issue,
            &adapter,
        ] {
            if value.len() > MAX_META_LEN {
                return Err(ManifestError::BadType(
                    "expectation meta too long".to_owned(),
                ));
            }
        }
        if adapter.contains('*') {
            return Err(ManifestError::Wildcard(test.clone(), subtest.clone()));
        }
        if owner.is_empty() || review_by.is_empty() || trace.is_empty() {
            return Err(ManifestError::MissingFieldFor(
                test.clone(),
                subtest.clone(),
                "owner/review_by/trace".to_owned(),
                expected.token().to_owned(),
            ));
        }
        if !is_date(&review_by) {
            return Err(ManifestError::BadDate(test.clone(), subtest.clone()));
        }
        if review_by.as_str() < today {
            return Err(ManifestError::Expired(
                test.clone(),
                subtest.clone(),
                review_by.clone(),
            ));
        }
        if expected == ExpectedStatus::NotRun && reason.is_empty() {
            return Err(ManifestError::MissingReason(
                test.clone(),
                subtest.clone(),
                expected.token().to_owned(),
            ));
        }
        check_expectation_rules(
            &test,
            &subtest,
            expected,
            classification,
            &capability,
            &reason,
            &issue,
        )?;
        // `DYNAMIC:` tracker rows cover dynamic-title matrices that never
        // execute (the harness cannot name them byte-exactly yet): they
        // must be NOTRUN exclusions, never PASS/FAIL.
        if subtest.starts_with("DYNAMIC:") && expected != ExpectedStatus::NotRun {
            return Err(ManifestError::BadType(
                "DYNAMIC tracker must be NOTRUN".to_owned(),
            ));
        }
        if seen
            .insert((upstream_path.clone(), test.clone(), subtest.clone()), ())
            .is_some()
        {
            return Err(ManifestError::Duplicate(test, subtest));
        }
        out.push(ExpectationRow {
            upstream_path,
            test,
            subtest,
            expected,
            classification,
            capability,
            reason,
            owner,
            review_by,
            trace,
            spec_section,
            issue,
            adapter,
        });
    }
    Ok(out)
}

/// Resolves manifest rows against expectation rows by exact
/// `(upstream_path, test, subtest)` id (M9-E §4 drift gate).
///
/// Every manifest subtest needs exactly one expectations row with the
/// same id, status, classification, capability, reason, owner,
/// `review_by`, trace, section, and issue; any drift (status, reason,
/// capability, owner, review, trace, classification, section, issue) is
/// a load error. Expectations rows without a manifest file are either
/// `DYNAMIC:` trackers or file-level exclusions (subtest exactly
/// `file-level exclusion`) — anything else is a load error. Adapter
/// agreement: manifest `direct` files require expectations `adapter ==
/// "direct"`; the `adapted` FileList fixture requires the fixture
/// adapter id on both sides.
pub fn resolve_expectations(
    manifest: &Manifest,
    rows: &[ExpectationRow],
) -> Result<(), ManifestError> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut by_id: BTreeMap<(String, String, String), &ExpectationRow> = BTreeMap::new();
    for row in rows {
        if by_id
            .insert(
                (
                    row.upstream_path.clone(),
                    row.test.clone(),
                    row.subtest.clone(),
                ),
                row,
            )
            .is_some()
        {
            return Err(ManifestError::Duplicate(
                row.test.clone(),
                row.subtest.clone(),
            ));
        }
    }
    let mut covered: BTreeSet<(String, String, String)> = BTreeSet::new();
    for file in &manifest.files {
        for sub in &file.subtests {
            let key = (
                file.upstream_path.clone(),
                sub.test.clone(),
                sub.subtest.clone(),
            );
            let Some(row) = by_id.get(&key) else {
                return Err(ManifestError::MissingField(format!(
                    "no expectation for `{}`",
                    sub.subtest
                )));
            };
            if row.expected != sub.expected
                || row.classification != sub.classification
                || row.capability != sub.capability
                || row.reason != sub.reason
                || row.owner != sub.owner
                || row.review_by != sub.review_by
                || row.trace != sub.trace
                || row.spec_section != sub.spec_section
                || row.issue != sub.issue
            {
                return Err(ManifestError::BadType(format!(
                    "expectation drift for `{}`",
                    sub.subtest
                )));
            }
            let expected_adapter = match file.provenance {
                Provenance::Direct => "direct",
                Provenance::Adapted => file.adapter.as_str(),
            };
            if row.adapter != expected_adapter {
                return Err(ManifestError::BadType(format!(
                    "adapter drift for `{}`",
                    sub.subtest
                )));
            }
            covered.insert(key);
        }
    }
    for row in rows {
        let key = (
            row.upstream_path.clone(),
            row.test.clone(),
            row.subtest.clone(),
        );
        if covered.contains(&key) {
            continue;
        }
        // Rows without a manifest file: only `DYNAMIC:` trackers and
        // file-level exclusions exist by design.
        let tracker = row.subtest.starts_with("DYNAMIC:");
        let file_level = row.subtest == "file-level exclusion";
        if !tracker && !file_level {
            return Err(ManifestError::MissingField(format!(
                "expectation without manifest file for `{}`",
                row.subtest
            )));
        }
    }
    Ok(())
}

/// Collects the `DYNAMIC:` tracker rows attached to one manifest file.
///
/// Trackers never execute (the worker never sees them): they attach to
/// the parent report here so totals cover every gate id. Only rows whose
/// `upstream_path` matches the file and whose subtest starts with
/// `DYNAMIC:` attach; file-level exclusions attach to no file.
#[must_use]
pub fn tracker_subtests(
    file: &ManifestFile,
    rows: &[ExpectationRow],
) -> Vec<crate::runner::SubtestResult> {
    let mut out = Vec::new();
    for row in rows {
        if row.upstream_path != file.upstream_path {
            continue;
        }
        if !row.subtest.starts_with("DYNAMIC:") {
            continue;
        }
        out.push(crate::runner::SubtestResult {
            test: row.test.clone(),
            subtest: row.subtest.clone(),
            actual: crate::runner::ActualStatus::NotRun,
            expected: crate::manifest::ExpectedStatus::NotRun,
            detail: crate::runner::scrub_detail(&format!("notrun: {}", row.reason)),
            trace: row.trace.clone(),
            elapsed_ms: 0,
        });
    }
    out.sort_by(|a, b| a.test.cmp(&b.test).then(a.subtest.cmp(&b.subtest)));
    out
}
/// Loads and validates a manifest document (strict: schema 2 only).
///
/// `today` is the `YYYY-MM-DD` review clock (UTC date at load time);
/// expectations with `review_by` before `today` fail the load so stale
/// capability gaps break strict runs instead of lingering silently.
/// Duplicate file `path` entries are rejected (same as duplicate
/// expectations): two files must never claim one adapted path.
pub fn load_manifest(text: &str, today: &str) -> Result<Manifest, ManifestError> {
    let root = parse_json(text)?;
    if !matches!(root, Json::Obj(_)) {
        return Err(ManifestError::RootNotObject);
    }
    let schema = root
        .need("schema_version")?
        .as_u32()
        .ok_or_else(|| ManifestError::BadType("schema_version".to_owned()))?;
    // Strict mode requires schema 2; the offline smoke path additionally
    // accepts the legacy schema 1 (see `load_manifest_for_mode`).
    if schema != SCHEMA_VERSION {
        return Err(ManifestError::BadSchema(schema));
    }
    load_manifest_schema2(&root, today, SCHEMA_VERSION)
}

/// Loads a manifest for one CLI mode: `--smoke` accepts schema 1 (legacy
/// adapted manifest) or schema 2; `--strict` requires schema 2.
pub fn load_manifest_for_mode(
    text: &str,
    today: &str,
    strict: bool,
) -> Result<Manifest, ManifestError> {
    let root = parse_json(text)?;
    if !matches!(root, Json::Obj(_)) {
        return Err(ManifestError::RootNotObject);
    }
    let schema = root
        .need("schema_version")?
        .as_u32()
        .ok_or_else(|| ManifestError::BadType("schema_version".to_owned()))?;
    if strict {
        if schema != SCHEMA_VERSION {
            return Err(ManifestError::BadSchema(schema));
        }
        return load_manifest_schema2(&root, today, schema);
    }
    if schema != SCHEMA_VERSION && schema != MIN_SMOKE_SCHEMA_VERSION {
        return Err(ManifestError::BadSchema(schema));
    }
    if schema == MIN_SMOKE_SCHEMA_VERSION {
        return load_manifest_schema1(&root, today);
    }
    load_manifest_schema2(&root, today, schema)
}

/// Shared schema-1 body (legacy M7 adapted manifests, smoke-only).
fn load_manifest_schema1(root: &Json, today: &str) -> Result<Manifest, ManifestError> {
    load_manifest_inner(root, today, MIN_SMOKE_SCHEMA_VERSION, false)
}

/// Shared schema-2 body (M9-E strict manifests).
fn load_manifest_schema2(root: &Json, today: &str, schema: u32) -> Result<Manifest, ManifestError> {
    load_manifest_inner(root, today, schema, true)
}

/// Shared manifest body: `strict_fields` selects schema-2 validation
/// (`upstream_sha256`, provenance/adapter/fixture, classification,
/// spec section, issue, capability allow-list, harness-gap release rule).
fn load_manifest_inner(
    root: &Json,
    today: &str,
    _schema: u32,
    strict_fields: bool,
) -> Result<Manifest, ManifestError> {
    let source_json = root.need("source")?;
    let source = ManifestSource {
        repository: source_json
            .need("repository")?
            .as_str()
            .ok_or_else(|| ManifestError::BadType("source.repository".to_owned()))?
            .to_owned(),
        commit: source_json
            .need("commit")?
            .as_str()
            .ok_or_else(|| ManifestError::BadType("source.commit".to_owned()))?
            .to_owned(),
        license: source_json
            .need("license")?
            .as_str()
            .ok_or_else(|| ManifestError::BadType("source.license".to_owned()))?
            .to_owned(),
    };
    if !is_hex(&source.commit, 20) {
        return Err(ManifestError::BadHex("source.commit".to_owned()));
    }
    // Source identity: exact canonical URL only (F16) + non-empty license.
    if source.repository != "https://github.com/web-platform-tests/wpt"
        || source.license.is_empty()
        || source.repository.len() > 512
        || source.license.len() > 64
    {
        return Err(ManifestError::BadType("source".to_owned()));
    }
    let default_timeout_ms = root
        .need("default_timeout_ms")?
        .as_u64()
        .ok_or_else(|| ManifestError::BadType("default_timeout_ms".to_owned()))?;
    if default_timeout_ms == 0 || default_timeout_ms > MAX_TIMEOUT_MS {
        return Err(ManifestError::BadType("default_timeout_ms".to_owned()));
    }
    let corpus_root = root
        .need("corpus_root")?
        .as_str()
        .ok_or_else(|| ManifestError::BadType("corpus_root".to_owned()))?
        .to_owned();
    check_corpus_root(&corpus_root)?;
    let files_json = root
        .need("files")?
        .as_arr()
        .ok_or_else(|| ManifestError::BadType("files".to_owned()))?;
    if files_json.len() > 10_000 {
        return Err(ManifestError::BadType("files too large".to_owned()));
    }
    let mut files = Vec::new();
    let mut seen: BTreeMap<(String, String), ()> = BTreeMap::new();
    let mut seen_paths: BTreeMap<String, ()> = BTreeMap::new();
    for file_json in files_json {
        let path = file_json
            .need("path")?
            .as_str()
            .ok_or_else(|| ManifestError::BadType("files[].path".to_owned()))?
            .to_owned();
        let upstream_path = file_json
            .need("upstream_path")?
            .as_str()
            .ok_or_else(|| ManifestError::BadType("files[].upstream_path".to_owned()))?
            .to_owned();
        let upstream_blob_sha = file_json
            .need("upstream_blob_sha")?
            .as_str()
            .ok_or_else(|| ManifestError::BadType("files[].upstream_blob_sha".to_owned()))?
            .to_owned();
        let upstream_sha256 = if strict_fields {
            file_json
                .need("upstream_sha256")?
                .as_str()
                .ok_or_else(|| ManifestError::BadType("files[].upstream_sha256".to_owned()))?
                .to_owned()
        } else {
            file_json
                .field("upstream_sha256")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_owned()
        };
        let sha256 = file_json
            .need("sha256")?
            .as_str()
            .ok_or_else(|| ManifestError::BadType("files[].sha256".to_owned()))?
            .to_owned();
        let group = file_json
            .need("group")?
            .as_str()
            .ok_or_else(|| ManifestError::BadType("files[].group".to_owned()))?
            .to_owned();
        let capability = file_json
            .need("capability")?
            .as_str()
            .ok_or_else(|| ManifestError::BadType("files[].capability".to_owned()))?
            .to_owned();
        // Provenance + adapter + fixture (schema 2 only): `direct` files
        // carry no adapter and no fixture; `adapted` files require a
        // non-empty adapter id; only the `filelist` fixture exists.
        let (provenance, adapter, fixture) = if strict_fields {
            let provenance = Provenance::parse(
                file_json
                    .need("provenance")?
                    .as_str()
                    .ok_or_else(|| ManifestError::BadType("files[].provenance".to_owned()))?,
            )?;
            let adapter = file_json
                .field("adapter")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_owned();
            if adapter.contains('*') || adapter.len() > MAX_META_LEN {
                return Err(ManifestError::Wildcard(path.clone(), String::new()));
            }
            match provenance {
                Provenance::Direct if !adapter.is_empty() => {
                    return Err(ManifestError::BadType(
                        "files[].adapter must be empty for direct".to_owned(),
                    ));
                }
                Provenance::Adapted if adapter.is_empty() => {
                    return Err(ManifestError::BadType(
                        "files[].adapter required for adapted".to_owned(),
                    ));
                }
                _ => {}
            }
            let fixture = file_json
                .field("fixture")
                .and_then(Json::as_str)
                .map(str::to_owned);
            if let Some(fixture) = fixture.as_deref() {
                if fixture != "filelist" {
                    return Err(ManifestError::BadType("files[].fixture".to_owned()));
                }
                if provenance != Provenance::Adapted {
                    return Err(ManifestError::BadType(
                        "files[].fixture needs adapted provenance".to_owned(),
                    ));
                }
            }
            (provenance, adapter, fixture)
        } else {
            (Provenance::Adapted, String::new(), None)
        };
        if path.is_empty() || upstream_path.is_empty() || group.is_empty() || capability.is_empty()
        {
            return Err(ManifestError::BadType("files[] empty field".to_owned()));
        }
        // Strict logical path policy (F2): only `corpus/<name>.js`, no
        // traversal, absolute form, drive prefix or control characters.
        // The normalized relative part is unused here — resolution happens
        // in `resolve_corpus_path` — but validation runs at load so bad
        // manifests fail before any filesystem access.
        check_logical_path(&path)?;
        // Upstream provenance: non-empty `FileAPI/` path without wildcards.
        if upstream_path.contains('*')
            || !upstream_path.starts_with("FileAPI/")
            || upstream_path.len() > MAX_PATH_LEN
        {
            return Err(ManifestError::Wildcard(path.clone(), String::new()));
        }
        if capability.contains('*') || capability.len() > MAX_META_LEN {
            return Err(ManifestError::Wildcard(path.clone(), String::new()));
        }
        if group.len() > MAX_PATH_LEN || !REQUIRED_GROUPS.contains(&group.as_str()) {
            return Err(ManifestError::BadType("files[].group".to_owned()));
        }
        // File-level capabilities obey the same closed allow-list as
        // subtest rows (schema 2): `browser-only` never validates.
        if strict_fields {
            check_capability(&path, &path, &capability)?;
        }
        if seen_paths.insert(path.clone(), ()).is_some() {
            return Err(ManifestError::Duplicate(path.clone(), String::new()));
        }
        if !is_hex(&upstream_blob_sha, 20) {
            return Err(ManifestError::BadHex(
                "files[].upstream_blob_sha".to_owned(),
            ));
        }
        // `upstream_blob_sha` is provenance only (M9-E §3): the strict gate
        // never treats it as content evidence — `upstream_sha256` below is.
        if strict_fields && !is_hex(&upstream_sha256, 32) {
            return Err(ManifestError::BadHex("files[].upstream_sha256".to_owned()));
        }
        if !is_hex(&sha256, 32) {
            return Err(ManifestError::BadHex("files[].sha256".to_owned()));
        }
        let subtests_json = file_json
            .need("subtests")?
            .as_arr()
            .ok_or_else(|| ManifestError::BadType("files[].subtests".to_owned()))?;
        if subtests_json.is_empty() || subtests_json.len() > 10_000 {
            return Err(ManifestError::BadType("files[].subtests".to_owned()));
        }
        let mut subtests = Vec::new();
        // Variant A (F17): subtest names are unique inside one manifest
        // file regardless of test ID — the runner keys harness results by
        // subtest name, so a collision would merge two rows into one
        // verdict and a false strict PASS. Same names across different
        // files stay allowed.
        let mut seen_names: BTreeMap<String, ()> = BTreeMap::new();
        for sub_json in subtests_json {
            let test = sub_json
                .need("test")?
                .as_str()
                .ok_or_else(|| ManifestError::BadType("subtests[].test".to_owned()))?
                .to_owned();
            let subtest = sub_json
                .need("subtest")?
                .as_str()
                .ok_or_else(|| ManifestError::BadType("subtests[].subtest".to_owned()))?
                .to_owned();
            if test.contains('*') || subtest.contains('*') {
                return Err(ManifestError::Wildcard(test.clone(), subtest.clone()));
            }
            if test.is_empty()
                || subtest.is_empty()
                || test.len() > MAX_NAME_LEN
                || subtest.len() > MAX_NAME_LEN
            {
                return Err(ManifestError::BadType("subtests[].test".to_owned()));
            }
            if seen.insert((test.clone(), subtest.clone()), ()).is_some() {
                return Err(ManifestError::Duplicate(test, subtest));
            }
            if seen_names.insert(subtest.clone(), ()).is_some() {
                return Err(ManifestError::DuplicateSubtest(path.clone(), subtest));
            }
            let status_token = sub_json
                .need("status")?
                .as_str()
                .ok_or_else(|| ManifestError::BadType("subtests[].status".to_owned()))?;
            let expected = ExpectedStatus::parse(status_token)?;
            let timeout_ms = match sub_json.field("timeout_ms") {
                Some(value) => value
                    .as_u64()
                    .ok_or_else(|| ManifestError::BadType("subtests[].timeout_ms".to_owned()))?,
                None => default_timeout_ms,
            };
            if timeout_ms == 0 || timeout_ms > MAX_TIMEOUT_MS {
                return Err(ManifestError::BadType("subtests[].timeout_ms".to_owned()));
            }
            let reason = sub_json
                .field("reason")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_owned();
            let sub_capability = sub_json
                .field("capability")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_owned();
            let owner = sub_json
                .field("owner")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_owned();
            let review_by = sub_json
                .field("review_by")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_owned();
            let trace = sub_json
                .field("trace")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_owned();
            let classification = if strict_fields {
                Classification::parse(sub_json.need("classification")?.as_str().ok_or_else(
                    || ManifestError::BadType("subtests[].classification".to_owned()),
                )?)?
            } else {
                Classification::Supported
            };
            let spec_section = if strict_fields {
                sub_json
                    .field("spec_section")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_owned()
            } else {
                String::new()
            };
            let issue = if strict_fields {
                sub_json
                    .field("issue")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_owned()
            } else {
                String::new()
            };
            // Every subtest carries the full expectation record: lengths
            // are bounded; `reason` non-empty only has meaning for
            // non-PASS (PASS rows keep it empty by convention, but the
            // loader does not reject a stale reason on PASS — strict
            // compares enum statuses, never the reason text).
            for value in [
                &reason,
                &sub_capability,
                &owner,
                &review_by,
                &trace,
                &spec_section,
                &issue,
            ] {
                if value.len() > MAX_META_LEN {
                    return Err(ManifestError::BadType(
                        "subtests[] meta too long".to_owned(),
                    ));
                }
            }
            if strict_fields {
                check_expectation_rules(
                    &test,
                    &subtest,
                    expected,
                    classification,
                    &effective_capability(&capability, &sub_capability),
                    &reason,
                    &issue,
                )?;
            }
            if expected != ExpectedStatus::Pass {
                if reason.is_empty() {
                    return Err(ManifestError::MissingReason(
                        test,
                        subtest,
                        expected.token().to_owned(),
                    ));
                }
                // Schema-1 legacy path (smoke-only): NOTRUN rows still need
                // the full record, but skip the schema-2 classification
                // matrix (already enforced above when `strict_fields`).
                // `supported` + NOTRUN stays rejected even in schema 1:
                // a supported capability is never a gap.
                if !strict_fields && sub_capability == "supported" {
                    return Err(ManifestError::SupportedNotRun(test, subtest));
                }
                for (field, value) in [
                    ("capability", &sub_capability),
                    ("owner", &owner),
                    ("review_by", &review_by),
                    ("trace", &trace),
                ] {
                    if value.is_empty() {
                        return Err(ManifestError::MissingFieldFor(
                            test.clone(),
                            subtest.clone(),
                            field.to_owned(),
                            expected.token().to_owned(),
                        ));
                    }
                }
                if !is_date(&review_by) {
                    return Err(ManifestError::BadDate(test, subtest));
                }
                if review_by.as_str() < today {
                    return Err(ManifestError::Expired(test, subtest, review_by));
                }
            }
            subtests.push(ManifestSubtest {
                test,
                subtest,
                timeout_ms,
                expected,
                reason,
                capability: effective_capability(&capability, &sub_capability),
                owner,
                review_by,
                trace,
                classification,
                spec_section,
                issue,
            });
        }
        files.push(ManifestFile {
            path,
            upstream_path,
            upstream_blob_sha,
            upstream_sha256,
            sha256,
            group,
            capability,
            provenance,
            adapter,
            fixture,
            subtests,
        });
    }
    if files.is_empty() {
        return Err(ManifestError::BadType("files".to_owned()));
    }
    // Manifest completeness: all six mandatory groups are present.
    {
        let mut groups: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for file in &files {
            groups.insert(file.group.as_str());
        }
        for required in REQUIRED_GROUPS {
            if !groups.contains(required) {
                return Err(ManifestError::BadType("files[] missing group".to_owned()));
            }
        }
    }
    Ok(Manifest {
        schema_version: _schema,
        source,
        corpus_root,
        default_timeout_ms,
        files,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn today() -> &'static str {
        "2026-09-08"
    }

    fn minimal_manifest(status: &str) -> String {
        minimal_manifest_schema(status, 2)
    }

    /// Schema-2 fixture with a NOTRUN gap row carrying the full §4 record
    /// (used by the reason/expiry tests). `status` applies to the first
    /// subtest only; the remaining rows stay PASS.
    fn minimal_manifest_schema(status: &str, schema: u32) -> String {
        let sha = "e".repeat(64);
        let upstream_sha = "f".repeat(64);
        let gap = |st: &str| {
            if st == "NOTRUN" {
                "\"status\": \"NOTRUN\", \"reason\": \"needs worker\", \"capability\": \"worker-runtime\", \"owner\": \"o\", \"review_by\": \"2099-01-01\", \"trace\": \"M9E-WPT-03\", \"classification\": \"unsupported-host-capability\", \"spec_section\": \"FileAPI WD Blob\", \"issue\": \"QUESTIONS.md Q1-Q3\""
            } else if st == "FAIL" {
                "\"status\": \"FAIL\", \"reason\": \"open defect\", \"capability\": \"blob-constructor\", \"owner\": \"o\", \"review_by\": \"2099-01-01\", \"trace\": \"M9E-WPT-02\", \"classification\": \"supported\", \"spec_section\": \"FileAPI WD Blob\", \"issue\": \"docs/reviews/M9E-handoff.md\""
            } else {
                "\"status\": \"PASS\", \"reason\": \"\", \"capability\": \"blob-constructor\", \"owner\": \"\", \"review_by\": \"\", \"trace\": \"\", \"classification\": \"supported\", \"spec_section\": \"\", \"issue\": \"\""
            }
        };
        let file = |path: &str,
                    upstream: &str,
                    group: &str,
                    test: &str,
                    subtest: &str,
                    st: &str| {
            let row = gap(st);
            format!(
                "{{\"path\": \"{path}\", \"upstream_path\": \"{upstream}\", \"upstream_blob_sha\": \"43c29ada4d5455410ab40c79c5982de2b973d2ba\", \"upstream_sha256\": \"{upstream_sha}\", \"sha256\": \"{sha}\", \"group\": \"{group}\", \"capability\": \"blob-constructor\", \"provenance\": \"direct\", \"subtests\": [{{\"test\": \"{test}\", \"subtest\": \"{subtest}\", {row}}}]}}"
            )
        };
        format!(
            "{{\"schema_version\": {schema}, \"source\": {{\"repository\": \"https://github.com/web-platform-tests/wpt\", \"commit\": \"0968c868d8095217d18d86b34c7f21dccae58768\", \"license\": \"BSD-3-Clause\"}}, \"corpus_root\": \"crates/boa_fapi_wpt/corpus\", \"default_timeout_ms\": 5000, \"files\": [{}, {}, {}, {}, {}, {}]}}",
            file(
                "corpus/a.js",
                "FileAPI/blob/a.any.js",
                "FileAPI/blob",
                "t",
                "s",
                status
            ),
            file(
                "corpus/b.js",
                "FileAPI/file/b.any.js",
                "FileAPI/file",
                "t2",
                "s2",
                "PASS"
            ),
            file(
                "corpus/c.js",
                "FileAPI/filelist-section/c.any.js",
                "FileAPI/filelist-section",
                "t3",
                "s3",
                "PASS"
            ),
            file(
                "corpus/d.js",
                "FileAPI/reading-data-section/d.any.js",
                "FileAPI/reading-data-section",
                "t4",
                "s4",
                "PASS"
            ),
            file(
                "corpus/e.js",
                "FileAPI/root/e.any.js",
                "FileAPI/root",
                "t5",
                "s5",
                "PASS"
            ),
            file(
                "corpus/f.js",
                "FileAPI/url/f.any.js",
                "FileAPI/url",
                "t6",
                "s6",
                "PASS"
            ),
        )
    }

    fn minimal_manifest_smoke1() -> String {
        // Legacy schema-1 shape (smoke-only): no schema-2 fields.
        let sha = "e".repeat(64);
        let file = |path: &str,
                    upstream: &str,
                    group: &str,
                    test: &str,
                    subtest: &str,
                    st: &str| {
            format!(
                "{{\"path\": \"{path}\", \"upstream_path\": \"{upstream}\", \"upstream_blob_sha\": \"43c29ada4d5455410ab40c79c5982de2b973d2ba\", \"sha256\": \"{sha}\", \"group\": \"{group}\", \"capability\": \"blob\", \"subtests\": [{{\"test\": \"{test}\", \"subtest\": \"{subtest}\", \"status\": \"{st}\", \"reason\": \"needs Dom\", \"capability\": \"c\", \"owner\": \"o\", \"review_by\": \"2099-01-01\", \"trace\": \"M7-WPT-05\"}}]}}"
            )
        };
        format!(
            "{{\"schema_version\": 1, \"source\": {{\"repository\": \"https://github.com/web-platform-tests/wpt\", \"commit\": \"0968c868d8095217d18d86b34c7f21dccae58768\", \"license\": \"BSD-3-Clause\"}}, \"corpus_root\": \"crates/boa_fapi_wpt/corpus\", \"default_timeout_ms\": 5000, \"files\": [{}, {}, {}, {}, {}, {}]}}",
            file(
                "corpus/a.js",
                "FileAPI/blob/a.any.js",
                "FileAPI/blob",
                "t",
                "s",
                "PASS"
            ),
            file(
                "corpus/b.js",
                "FileAPI/file/b.any.js",
                "FileAPI/file",
                "t2",
                "s2",
                "PASS"
            ),
            file(
                "corpus/c.js",
                "FileAPI/filelist-section/c.any.js",
                "FileAPI/filelist-section",
                "t3",
                "s3",
                "PASS"
            ),
            file(
                "corpus/d.js",
                "FileAPI/reading-data-section/d.any.js",
                "FileAPI/reading-data-section",
                "t4",
                "s4",
                "PASS"
            ),
            file(
                "corpus/e.js",
                "FileAPI/root/e.any.js",
                "FileAPI/root",
                "t5",
                "s5",
                "PASS"
            ),
            file(
                "corpus/f.js",
                "FileAPI/url/f.any.js",
                "FileAPI/url",
                "t6",
                "s6",
                "PASS"
            ),
        )
    }

    #[test]
    fn accepts_pass_manifest() {
        let manifest = load_manifest(&minimal_manifest("PASS"), today()).expect("load");
        assert_eq!(manifest.files.len(), 6);
        assert_eq!(manifest.files[0].subtests.len(), 1);
    }

    #[test]
    fn smoke_accepts_legacy_schema1_but_strict_rejects_it() {
        let legacy = minimal_manifest_smoke1();
        assert!(load_manifest(&legacy, today()).is_err());
        assert!(load_manifest_for_mode(&legacy, today(), false).is_ok());
        assert!(load_manifest_for_mode(&legacy, today(), true).is_err());
        let current = minimal_manifest("PASS");
        assert!(load_manifest_for_mode(&current, today(), false).is_ok());
        assert!(load_manifest_for_mode(&current, today(), true).is_ok());
    }

    #[test]
    fn rejects_fail_without_issue_and_browser_only() {
        // `supported` + FAIL without an issue link is not a recorded defect.
        let no_issue = minimal_manifest("FAIL").replacen(
            "\"issue\": \"docs/reviews/M9E-handoff.md\"",
            "\"issue\": \"\"",
            1,
        );
        assert!(matches!(
            load_manifest(&no_issue, today()),
            Err(ManifestError::MissingIssue(_, _))
        ));
        // `browser-only` never validates as a capability.
        let browser = minimal_manifest("PASS").replacen(
            "\"capability\": \"blob-constructor\"",
            "\"capability\": \"browser-only\"",
            1,
        );
        assert!(matches!(
            load_manifest(&browser, today()),
            Err(ManifestError::BrowserOnly(_, _))
        ));
    }

    #[test]
    fn rejects_supported_notrun_and_harness_gap() {
        // `supported` is never NOTRUN: flip the first row's status to
        // NOTRUN while keeping the `supported` classification.
        let notrun = minimal_manifest("NOTRUN").replacen(
            "\"classification\": \"unsupported-host-capability\"",
            "\"classification\": \"supported\"",
            1,
        );
        assert!(matches!(
            load_manifest(&notrun, today()),
            Err(ManifestError::SupportedNotRun(_, _))
        ));
        // `harness-gap` breaks the release gate (load error).
        let gap = minimal_manifest("PASS").replacen(
            "\"classification\": \"supported\"",
            "\"classification\": \"harness-gap\"",
            1,
        );
        assert!(matches!(
            load_manifest(&gap, today()),
            Err(ManifestError::HarnessGap(_, _))
        ));
    }

    #[test]
    fn rejects_unknown_status_and_wildcards() {
        // Unknown status token on the first row.
        let mut unknown = minimal_manifest("PASS");
        unknown = unknown.replacen("\"status\": \"PASS\"", "\"status\": \"MAYBE\"", 1);
        assert!(matches!(
            load_manifest(&unknown, today()),
            Err(ManifestError::UnknownStatus(_))
        ));
        let wild = minimal_manifest("PASS").replace("\"test\": \"t\"", "\"test\": \"t*\"");
        assert!(matches!(
            load_manifest(&wild, today()),
            Err(ManifestError::Wildcard(_, _))
        ));
    }

    #[test]
    fn rejects_missing_reason_and_expired_review() {
        // NOTRUN gap without a reason fails the load.
        let mut no_reason = minimal_manifest("NOTRUN");
        no_reason = no_reason.replacen("\"reason\": \"needs worker\"", "\"reason\": \"\"", 1);
        assert!(matches!(
            load_manifest(&no_reason, today()),
            Err(ManifestError::MissingReason(_, _, _))
        ));
        let expired = minimal_manifest("NOTRUN").replace("2099-01-01", "2020-01-01");
        assert!(matches!(
            load_manifest(&expired, today()),
            Err(ManifestError::Expired(_, _, _))
        ));
    }

    #[test]
    fn rejects_bad_schema_and_bad_hash() {
        let bad_schema =
            minimal_manifest("PASS").replace("\"schema_version\": 2", "\"schema_version\": 3");
        assert!(matches!(
            load_manifest(&bad_schema, today()),
            Err(ManifestError::BadSchema(3))
        ));
        let bad_hash = minimal_manifest("PASS").replace(&"e".repeat(64), "zz");
        assert!(matches!(
            load_manifest(&bad_hash, today()),
            Err(ManifestError::BadHex(_))
        ));
    }

    #[test]
    fn rejects_duplicate_json_keys_recursive() {
        // Same key twice at root.
        let dup_root = minimal_manifest("PASS").replace(
            "\"default_timeout_ms\": 5000",
            "\"default_timeout_ms\": 5000, \"default_timeout_ms\": 5000",
        );
        assert!(matches!(
            load_manifest(&dup_root, today()),
            Err(ManifestError::DuplicateJsonKey(k)) if k == "default_timeout_ms"
        ));
        // Same key twice inside `source`.
        let dup_source = minimal_manifest("PASS").replace(
            "\"license\": \"BSD-3-Clause\"",
            "\"license\": \"BSD-3-Clause\", \"license\": \"MIT\"",
        );
        assert!(matches!(
            load_manifest(&dup_source, today()),
            Err(ManifestError::DuplicateJsonKey(k)) if k == "license"
        ));
        // Same key twice inside a subtest, with different value types.
        // `replacen(..., 1)` hits the first `"trace"` occurrence — the
        // file-level... no: subtest rows carry the trace, so anchor on
        // the full subtest-trace pair to land inside a subtest object.
        let dup_sub = minimal_manifest("PASS").replacen(
            "\"subtest\": \"s\", \"status\": \"PASS\", \"reason\": \"\", \"capability\": \"blob-constructor\", \"owner\": \"\", \"review_by\": \"\", \"trace\": \"\"",
            "\"subtest\": \"s\", \"status\": \"PASS\", \"reason\": \"\", \"capability\": \"blob-constructor\", \"owner\": \"\", \"review_by\": \"\", \"trace\": \"\", \"trace\": 7",
            1,
        );
        assert!(matches!(
            load_manifest(&dup_sub, today()),
            Err(ManifestError::DuplicateJsonKey(k)) if k == "trace"
        ));
        // Error reveals only the key name, never the value.
        let error = format!("{}", ManifestError::DuplicateJsonKey("sha256".to_owned()));
        assert!(!error.contains("other-value"));
    }

    #[test]
    fn rejects_malicious_repository_and_bad_subtest_collision() {
        // Prefix-attack repository.
        let evil_repo = minimal_manifest("PASS").replace(
            "https://github.com/web-platform-tests/wpt\"",
            "https://github.com/web-platform-tests/wpt-malicious\"",
        );
        assert!(matches!(
            load_manifest(&evil_repo, today()),
            Err(ManifestError::BadType(_))
        ));
        // Extra path, query, fragment, userinfo and http:// are rejected.
        for bad in [
            "https://github.com/web-platform-tests/wpt/other",
            "https://github.com/web-platform-tests/wpt?x=1",
            "https://github.com/web-platform-tests/wpt#frag",
            "https://user@github.com/web-platform-tests/wpt",
            "http://github.com/web-platform-tests/wpt",
            "https://github.com/web-platform-tests/wpt/",
        ] {
            let variant =
                minimal_manifest("PASS").replace("https://github.com/web-platform-tests/wpt", bad);
            assert!(
                matches!(
                    load_manifest(&variant, today()),
                    Err(ManifestError::BadType(_))
                ),
                "must reject {bad}"
            );
        }
        // Canonical valid URL loads.
        assert_eq!(
            load_manifest(&minimal_manifest("PASS"), today())
                .expect("canonical repo")
                .source
                .repository,
            "https://github.com/web-platform-tests/wpt"
        );
        // Same subtest name under two test IDs in one file → load error.
        // Build the collision by direct JSON assembly: parse the base
        // fixture is overkill — instead append a second subtest object
        // into the first file's subtests array by anchoring on the
        // first row's full text and duplicating it with a new test id.
        let base = minimal_manifest("PASS");
        let first_row = "\"test\": \"t\", \"subtest\": \"s\", \"status\": \"PASS\", \"reason\": \"\", \"capability\": \"blob-constructor\", \"owner\": \"\", \"review_by\": \"\", \"trace\": \"\", \"classification\": \"supported\", \"spec_section\": \"\", \"issue\": \"\"";
        let dup_row = "\"test\": \"t-other\", \"subtest\": \"same-but-once\", \"status\": \"PASS\", \"reason\": \"\", \"capability\": \"blob-constructor\", \"owner\": \"\", \"review_by\": \"\", \"trace\": \"\", \"classification\": \"supported\", \"spec_section\": \"\", \"issue\": \"\"";
        let mut collision = base.replacen(
            "\"test\": \"t\", \"subtest\": \"s\"",
            "\"test\": \"t\", \"subtest\": \"same-but-once\"",
            1,
        );
        // Insert the duplicate row right after the renamed first row:
        // anchor on the renamed row's subtest field, then splice before
        // the closing `]` of the first file's subtests array. The array
        // close follows the row's `}` — find both in order.
        let anchor2 = "\"test\": \"t\", \"subtest\": \"same-but-once\"";
        let pos = collision.find(anchor2).expect("fixture shape");
        let after = &collision[pos..];
        let row_end = after.find('}').expect("fixture shape");
        let mut with_dup = collision[..pos + row_end + 1].to_owned();
        with_dup.push_str(", {");
        with_dup.push_str(dup_row);
        with_dup.push('}');
        with_dup.push_str(&collision[pos + row_end + 1..]);
        collision = with_dup;
        let _ = first_row;
        // Also verify cross-file reuse stays allowed: give the SECOND file
        // the same subtest name — must still load.
        let mut cross_ok = minimal_manifest("PASS");
        cross_ok = cross_ok.replacen(
            "\"test\": \"t2\", \"subtest\": \"s2\"",
            "\"test\": \"t-x\", \"subtest\": \"same-but-once\"",
            1,
        );
        cross_ok = cross_ok.replacen(
            "\"test\": \"t\", \"subtest\": \"s\"",
            "\"test\": \"t\", \"subtest\": \"s-unique\"",
            1,
        );
        assert!(matches!(
            load_manifest(&collision, today()),
            Err(ManifestError::DuplicateSubtest(_, _))
        ));
        assert!(
            load_manifest(&cross_ok, today()).is_ok(),
            "same subtest name in different files must stay allowed"
        );
        // Same subtest name in different files stays allowed (minimal
        // fixture has unique names per file; cross-file reuse is covered
        // by the loader design — per-file `seen_names` map).
    }

    #[test]
    fn rejects_deep_nesting_surrogate_and_dup_path() {
        let mut deep = String::from("[");
        for _ in 0..80 {
            deep.push('[');
        }
        assert!(matches!(parse_json(&deep), Err(ManifestError::BadType(_))));
        let surrogate = minimal_manifest("PASS").replace("corpus/a.js", "corpus/\\ud800.js");
        assert!(matches!(
            load_manifest(&surrogate, today()),
            Err(ManifestError::BadType(_))
        ));
        let base = minimal_manifest("PASS");
        // Insert a second file entry with the same `path` before the
        // closing of the `files` array (i.e. right before the final `]`).
        let cut = base.rfind(']').expect("fixture shape changed");
        let dup_tail = ",{\"path\": \"corpus/a.js\", \"upstream_path\": \"FileAPI/blob/a.any.js\", \"upstream_blob_sha\": \"43c29ada4d5455410ab40c79c5982de2b973d2ba\", \"upstream_sha256\": \"".to_owned()
            + &"f".repeat(64)
            + "\", \"sha256\": \""
            + &"e".repeat(64)
            + "\", \"group\": \"FileAPI/blob\", \"capability\": \"blob-constructor\", \"provenance\": \"direct\", \"subtests\": [{\"test\": \"t2\", \"subtest\": \"s2\", \"status\": \"PASS\", \"reason\": \"\", \"capability\": \"blob-constructor\", \"owner\": \"\", \"review_by\": \"\", \"trace\": \"\", \"classification\": \"supported\", \"spec_section\": \"\", \"issue\": \"\"}]}";
        let mut dup_path = base[..cut].to_owned();
        dup_path.push_str(&dup_tail);
        dup_path.push_str(&base[cut..]);
        assert!(
            matches!(
                load_manifest(&dup_path, today()),
                Err(ManifestError::Duplicate(_, _))
            ),
            "duplicate adapted path must be rejected"
        );
    }
}
