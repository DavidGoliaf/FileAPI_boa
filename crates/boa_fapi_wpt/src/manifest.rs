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
pub const SCHEMA_VERSION: u32 = 1;

/// Terminal per-subtest statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExpectedStatus {
    /// The subtest must pass.
    Pass,
    /// The subtest cannot run: exact capability gap recorded.
    NotRun,
}

impl ExpectedStatus {
    /// Parses the exact status token (`PASS` / `NOTRUN` only).
    pub fn parse(token: &str) -> Result<Self, ManifestError> {
        match token {
            "PASS" => Ok(Self::Pass),
            "NOTRUN" => Ok(Self::NotRun),
            _ => Err(ManifestError::UnknownStatus(token.to_owned())),
        }
    }

    /// Renders the canonical status token.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::NotRun => "NOTRUN",
        }
    }
}

/// One adapted corpus file entry.
#[derive(Debug, Clone)]
pub struct ManifestFile {
    /// Repository-relative adapted path, e.g. `corpus/blob-constructor.js`.
    pub path: String,
    /// Upstream `FileAPI/**` path this file adapts.
    pub upstream_path: String,
    /// Upstream git blob SHA (hex, provenance only).
    pub upstream_blob_sha: String,
    /// SHA-256 (hex, lowercase) of the stored adapted file bytes.
    pub sha256: String,
    /// WPT group, e.g. `FileAPI/blob`.
    pub group: String,
    /// Capability the file needs, e.g. `blob-constructor`.
    pub capability: String,
    /// Subtests in file order (stable report order).
    pub subtests: Vec<ManifestSubtest>,
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
    /// Machine-readable gap reason (required for `NOTRUN`).
    pub reason: String,
    /// Capability the subtest needs.
    pub capability: String,
    /// Owner of the expectation entry.
    pub owner: String,
    /// Review date `YYYY-MM-DD`; expired entries fail strict loads.
    pub review_by: String,
    /// Trace row in `docs/spec-matrix.md`, e.g. `M7-WPT-01`.
    pub trace: String,
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
    /// Pinned upstream source.
    pub source: ManifestSource,
    /// Default per-subtest timeout in milliseconds.
    pub default_timeout_ms: u64,
    /// Files in manifest order.
    pub files: Vec<ManifestFile>,
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
    /// Malformed hex hash.
    #[error("manifest field `{0}` is not lowercase hex")]
    BadHex(String),
    /// Duplicate test/subtest entry.
    #[error("duplicate expectation `{0} :: {1}`")]
    Duplicate(String, String),
    /// Malformed review date.
    #[error("subtest `{0} :: {1}` has a malformed review_by date")]
    BadDate(String, String),
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
/// Duplicate object keys keep the last value (manifest loader rejects
/// duplicate *expectations* separately). Depth is bounded to avoid
/// stack exhaustion on hostile input.
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
            self.skip_ws();
            if !self.eat(b':') {
                return Err(ManifestError::BadType("object colon".to_owned()));
            }
            let value = self.value(depth.saturating_add(1))?;
            if let Some(slot) = entries.iter_mut().find(|(k, _)| *k == key) {
                slot.1 = value;
            } else {
                entries.push((key, value));
            }
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

/// Checks `YYYY-MM-DD` shape (calendar validity is not required; expiry
/// compares lexicographically, which is order-correct for this shape).
fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(|b| b.is_ascii_digit())
        && bytes[5..7].iter().all(|b| b.is_ascii_digit())
        && bytes[8..10].iter().all(|b| b.is_ascii_digit())
}

/// Loads and validates a manifest document.
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
    if schema != SCHEMA_VERSION {
        return Err(ManifestError::BadSchema(schema));
    }
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
    let default_timeout_ms = root
        .need("default_timeout_ms")?
        .as_u64()
        .ok_or_else(|| ManifestError::BadType("default_timeout_ms".to_owned()))?;
    if default_timeout_ms == 0 {
        return Err(ManifestError::BadType("default_timeout_ms".to_owned()));
    }
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
        if path.is_empty() || upstream_path.is_empty() || group.is_empty() || capability.is_empty()
        {
            return Err(ManifestError::BadType("files[] empty field".to_owned()));
        }
        if path.contains('*') || upstream_path.contains('*') || capability.contains('*') {
            return Err(ManifestError::Wildcard(path.clone(), String::new()));
        }
        if seen_paths.insert(path.clone(), ()).is_some() {
            return Err(ManifestError::Duplicate(path.clone(), String::new()));
        }
        if !is_hex(&upstream_blob_sha, 20) {
            return Err(ManifestError::BadHex(
                "files[].upstream_blob_sha".to_owned(),
            ));
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
            if test.is_empty() || subtest.is_empty() {
                return Err(ManifestError::BadType("subtests[].test".to_owned()));
            }
            if seen.insert((test.clone(), subtest.clone()), ()).is_some() {
                return Err(ManifestError::Duplicate(test, subtest));
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
            if timeout_ms == 0 {
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
            if expected != ExpectedStatus::Pass {
                if reason.is_empty() {
                    return Err(ManifestError::MissingReason(
                        test,
                        subtest,
                        expected.token().to_owned(),
                    ));
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
                capability: if sub_capability.is_empty() {
                    capability.clone()
                } else {
                    sub_capability
                },
                owner,
                review_by,
                trace,
            });
        }
        files.push(ManifestFile {
            path,
            upstream_path,
            upstream_blob_sha,
            sha256,
            group,
            capability,
            subtests,
        });
    }
    if files.is_empty() {
        return Err(ManifestError::BadType("files".to_owned()));
    }
    Ok(Manifest {
        source,
        default_timeout_ms,
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn today() -> &'static str {
        "2026-09-08"
    }

    fn minimal_manifest(status: &str) -> String {
        format!(
            r#"{{"schema_version": 1, "source": {{"repository": "https://github.com/web-platform-tests/wpt", "commit": "0968c868d8095217d18d86b34c7f21dccae58768", "license": "BSD-3-Clause"}}, "default_timeout_ms": 5000, "files": [{{"path": "corpus/a.js", "upstream_path": "FileAPI/blob/a.any.js", "upstream_blob_sha": "43c29ada4d5455410ab40c79c5982de2b973d2ba", "sha256": "{}", "group": "FileAPI/blob", "capability": "blob", "subtests": [{{"test": "t", "subtest": "s", "status": "{status}", "reason": "needs Dom", "capability": "c", "owner": "o", "review_by": "2099-01-01", "trace": "M7-WPT-05"}}]}}]}}"#,
            "e".repeat(64)
        )
    }

    #[test]
    fn accepts_pass_manifest() {
        let manifest = load_manifest(&minimal_manifest("PASS"), today()).expect("load");
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].subtests.len(), 1);
    }

    #[test]
    fn rejects_unknown_status_and_wildcards() {
        assert!(matches!(
            load_manifest(&minimal_manifest("MAYBE"), today()),
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
        let mut no_reason = minimal_manifest("NOTRUN");
        no_reason = no_reason.replace("\"reason\": \"needs Dom\"", "\"reason\": \"\"");
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
            minimal_manifest("PASS").replace("\"schema_version\": 1", "\"schema_version\": 2");
        assert!(matches!(
            load_manifest(&bad_schema, today()),
            Err(ManifestError::BadSchema(2))
        ));
        let bad_hash = minimal_manifest("PASS").replace(&"e".repeat(64), "zz");
        assert!(matches!(
            load_manifest(&bad_hash, today()),
            Err(ManifestError::BadHex(_))
        ));
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
        let dup_tail = ",{\"path\": \"corpus/a.js\", \"upstream_path\": \"FileAPI/blob/a.any.js\", \"upstream_blob_sha\": \"43c29ada4d5455410ab40c79c5982de2b973d2ba\", \"sha256\": \"".to_owned()
            + &"e".repeat(64)
            + "\", \"group\": \"FileAPI/blob\", \"capability\": \"blob\", \"subtests\": [{\"test\": \"t2\", \"subtest\": \"s2\", \"status\": \"PASS\", \"reason\": \"\", \"capability\": \"\", \"owner\": \"\", \"review_by\": \"\", \"trace\": \"\"}]}";
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
