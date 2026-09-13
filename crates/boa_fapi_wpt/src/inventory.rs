//! Pinned upstream inventory: parsing and primary-disposition accounting.
//!
//! `wpt-inventory.json` (schema 1) pins the full `FileAPI/**` tree at the
//! upstream commit. Every path is accounted for exactly once either by an
//! execution (a manifest file with matching `upstream_path`) or by an exact
//! file-level exclusion in `expectations.json`. The mapping is validated as
//! a bijection before a release verdict is formed (M9E-R1 §4).

use std::collections::BTreeMap;

use thiserror::Error;

use crate::manifest::{Json, MAX_PATH_LEN, ManifestError, ManifestSource, is_hex, parse_json};

/// Inventory schema accepted by this harness.
pub const INVENTORY_SCHEMA_VERSION: u32 = 1;

/// Primary disposition of one inventory path (M9E-R1 §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Disposition {
    /// A raw upstream test file executed directly (byte-identical).
    ExecutedDirect,
    /// A deterministic adapter executes upstream assertions.
    ExecutedAdapted,
    /// Upstream file/subtests are not executed because of an audited host
    /// capability, with owner/reason/review date.
    ExcludedCapability,
    /// Metadata/non-test artifact from the closed allow-list.
    UnsupportedArtifact,
}

impl Disposition {
    /// Renders the canonical token.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::ExecutedDirect => "executed-direct",
            Self::ExecutedAdapted => "executed-adapted",
            Self::ExcludedCapability => "excluded-capability",
            Self::UnsupportedArtifact => "unsupported-artifact",
        }
    }
}

/// One pinned inventory entry.
#[derive(Debug, Clone)]
pub struct InventoryEntry {
    /// Upstream `FileAPI/**` path.
    pub path: String,
    /// Upstream git blob SHA (provenance only).
    pub blob_sha: String,
    /// Raw content size in bytes.
    pub size: u64,
    /// Raw-content SHA-256 (evidence, verified against `--upstream-root`).
    pub sha256: String,
}

/// Parsed inventory.
#[derive(Debug, Clone)]
pub struct Inventory {
    /// Pinned upstream repository URL.
    pub repository: String,
    /// Pinned upstream commit SHA.
    pub commit: String,
    /// Inventory scope (always `FileAPI/`).
    pub scope: String,
    /// Entries in inventory order.
    pub entries: Vec<InventoryEntry>,
    by_path: BTreeMap<String, usize>,
}

impl Inventory {
    /// Looks up an entry by upstream path.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&InventoryEntry> {
        self.by_path
            .get(path)
            .and_then(|index| self.entries.get(*index))
    }

    /// Number of pinned paths.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when the inventory pins no path.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Inventory load/validation errors.
#[derive(Debug, Error)]
pub enum InventoryError {
    /// Malformed JSON document (duplicate keys, wrong type, too large).
    #[error("inventory parse error: {0}")]
    Parse(#[from] ManifestError),
    /// The root value is not an object.
    #[error("inventory root must be an object")]
    RootNotObject,
    /// A required field is missing.
    #[error("inventory is missing required field `{0}`")]
    MissingField(String),
    /// A field has the wrong type or shape.
    #[error("inventory field `{0}` is malformed")]
    BadField(String),
    /// Unsupported schema version.
    #[error("unsupported inventory schema version {0}")]
    BadSchema(u32),
    /// The pinned repository/commit does not match the manifest source.
    #[error("inventory source mismatch")]
    SourceMismatch,
    /// `file_count` disagrees with the parsed entry count.
    #[error("inventory file_count mismatch")]
    CountMismatch,
    /// Duplicate inventory path.
    #[error("duplicate inventory entry `{0}`")]
    Duplicate(String),
    /// A path is outside the `FileAPI/` scope or malformed.
    #[error("bad inventory entry `{0}`")]
    BadEntry(String),
}

/// Parses and validates `wpt-inventory.json`.
///
/// The pinned repository/commit literals must match the manifest `source`
/// (M9-E §3); duplicate, malformed or out-of-scope paths are load errors.
pub fn load_inventory(text: &str, source: &ManifestSource) -> Result<Inventory, InventoryError> {
    let root = parse_json(text)?;
    if !matches!(root, Json::Obj(_)) {
        return Err(InventoryError::RootNotObject);
    }
    let schema = root
        .need("schema_version")?
        .as_u32()
        .ok_or_else(|| InventoryError::BadField("schema_version".to_owned()))?;
    if schema != INVENTORY_SCHEMA_VERSION {
        return Err(InventoryError::BadSchema(schema));
    }
    let repository = root
        .need("repository")?
        .as_str()
        .ok_or_else(|| InventoryError::BadField("repository".to_owned()))?
        .to_owned();
    let commit = root
        .need("commit")?
        .as_str()
        .ok_or_else(|| InventoryError::BadField("commit".to_owned()))?
        .to_owned();
    if repository != source.repository || commit != source.commit {
        return Err(InventoryError::SourceMismatch);
    }
    let scope = root
        .need("scope")?
        .as_str()
        .ok_or_else(|| InventoryError::BadField("scope".to_owned()))?
        .to_owned();
    if scope != "FileAPI/" {
        return Err(InventoryError::BadField("scope".to_owned()));
    }
    let file_count = root
        .need("file_count")?
        .as_u64()
        .ok_or_else(|| InventoryError::BadField("file_count".to_owned()))?;
    // `inventory_sha256` is a self-identifying digest of the generated file;
    // presence and shape are validated, the digest itself is regenerated by
    // `tools/gen-wpt-inventory.py` (the pretty-printed checked-in form is
    // not the sorted compact form the generator hashes).
    let inventory_sha = root
        .need("inventory_sha256")?
        .as_str()
        .ok_or_else(|| InventoryError::BadField("inventory_sha256".to_owned()))?;
    if !is_hex(inventory_sha, 32) {
        return Err(InventoryError::BadField("inventory_sha256".to_owned()));
    }
    let files = root
        .need("files")?
        .as_arr()
        .ok_or_else(|| InventoryError::BadField("files".to_owned()))?;
    if files.is_empty() {
        return Err(InventoryError::BadField("files".to_owned()));
    }
    if file_count as usize != files.len() {
        return Err(InventoryError::CountMismatch);
    }
    let mut entries = Vec::with_capacity(files.len());
    let mut by_path = BTreeMap::new();
    for entry in files {
        let path = entry
            .need("path")?
            .as_str()
            .ok_or_else(|| InventoryError::BadField("files[].path".to_owned()))?
            .to_owned();
        let blob_sha = entry
            .need("blob_sha")?
            .as_str()
            .ok_or_else(|| InventoryError::BadField("files[].blob_sha".to_owned()))?
            .to_owned();
        let size = entry
            .field("size")
            .and_then(Json::as_u64)
            .ok_or_else(|| InventoryError::BadField("files[].size".to_owned()))?;
        let sha256 = entry
            .need("sha256")?
            .as_str()
            .ok_or_else(|| InventoryError::BadField("files[].sha256".to_owned()))?
            .to_owned();
        if path.is_empty()
            || path.len() > MAX_PATH_LEN
            || !path.starts_with("FileAPI/")
            || path.contains('\\')
            || path.as_bytes().iter().any(|b| *b < 0x20 || *b == 0x7F)
        {
            return Err(InventoryError::BadEntry(path));
        }
        if !is_hex(&blob_sha, 20) {
            return Err(InventoryError::BadEntry(path));
        }
        if !is_hex(&sha256, 32) {
            return Err(InventoryError::BadEntry(path));
        }
        if by_path.contains_key(&path) {
            return Err(InventoryError::Duplicate(path));
        }
        by_path.insert(path.clone(), entries.len());
        entries.push(InventoryEntry {
            path,
            blob_sha,
            size,
            sha256,
        });
    }
    Ok(Inventory {
        repository,
        commit,
        scope,
        entries,
        by_path,
    })
}

/// The executed side of the disposition mapping: an upstream path claimed by
/// exactly one manifest execution.
#[derive(Debug, Clone)]
pub struct ExecutedClaim {
    /// Upstream path claimed.
    pub upstream_path: String,
    /// Primary disposition.
    pub disposition: Disposition,
}

/// Resolved primary dispositions for every inventory path.
#[derive(Debug, Clone)]
pub struct Dispositions {
    /// Primary disposition for every inventory path (bijection).
    pub by_path: BTreeMap<String, Disposition>,
    /// Executed claims in manifest order (one per claiming manifest file).
    pub executed: Vec<ExecutedClaim>,
}

/// Disposition resolution errors (M9E-R1 §4.1/§4.2).
#[derive(Debug, Error)]
pub enum DispositionError {
    /// A manifest execution claims a path absent from the inventory.
    #[error("executed upstream path not in inventory `{0}`")]
    ExecutedNotInInventory(String),
    /// A file-level exclusion references a path absent from the inventory.
    #[error("file-level exclusion not in inventory `{0}`")]
    ExclusionNotInInventory(String),
    /// Two claims (execution or exclusion) target one path.
    #[error("duplicate primary disposition for `{0}`")]
    DuplicateDisposition(String),
    /// A path is both executed and excluded.
    #[error("path `{0}` is both executed and excluded")]
    ExecutedAndExcluded(String),
    /// A path has no execution and no exclusion.
    #[error("inventory path `{0}` is unaccounted")]
    Unaccounted(String),
    /// A non-executed path has no file-level exclusion.
    #[error("inventory path `{0}` has no file-level exclusion")]
    MissingExclusion(String),
}

impl Dispositions {
    /// Count of executed-direct paths.
    #[must_use]
    pub fn executed_direct(&self) -> usize {
        self.executed
            .iter()
            .filter(|c| c.disposition == Disposition::ExecutedDirect)
            .count()
    }

    /// Count of executed-adapted paths.
    #[must_use]
    pub fn executed_adapted(&self) -> usize {
        self.executed
            .iter()
            .filter(|c| c.disposition == Disposition::ExecutedAdapted)
            .count()
    }

    /// Count of excluded (capability + artifact) paths.
    #[must_use]
    pub fn excluded(&self) -> usize {
        self.by_path
            .values()
            .filter(|d| {
                matches!(
                    d,
                    Disposition::ExcludedCapability | Disposition::UnsupportedArtifact
                )
            })
            .count()
    }
}

/// Resolves the bijection inventory ↔ disposition (M9E-R1 §4.1).
///
/// Executed dispositions come from the manifest (`direct` always; `adapted`
/// only when the file is not a project-acceptance smoke); every remaining
/// inventory path must have exactly one file-level exclusion row (subtest
/// exactly `file-level exclusion`). Missing, extra, duplicate or
/// contradictory claims are errors.
pub fn resolve_dispositions(
    inventory: &Inventory,
    executed: &[ExecutedClaim],
    excluded_paths: &[String],
) -> Result<Dispositions, DispositionError> {
    let mut by_path: BTreeMap<String, Disposition> = inventory
        .entries
        .iter()
        .map(|e| (e.path.clone(), Disposition::ExcludedCapability))
        .collect();
    // Mark executed claims first; duplicates and unknown paths are errors.
    for claim in executed {
        if !by_path.contains_key(&claim.upstream_path) {
            return Err(DispositionError::ExecutedNotInInventory(
                claim.upstream_path.clone(),
            ));
        }
        let slot = by_path
            .get_mut(&claim.upstream_path)
            .ok_or_else(|| DispositionError::ExecutedNotInInventory(claim.upstream_path.clone()))?;
        if *slot != Disposition::ExcludedCapability || executed_duplicate(executed, claim) {
            return Err(DispositionError::DuplicateDisposition(
                claim.upstream_path.clone(),
            ));
        }
        *slot = claim.disposition;
    }
    for path in excluded_paths {
        if !by_path.contains_key(path) {
            return Err(DispositionError::ExclusionNotInInventory(path.clone()));
        }
        let slot = by_path
            .get_mut(path)
            .ok_or_else(|| DispositionError::ExclusionNotInInventory(path.clone()))?;
        match slot {
            Disposition::ExcludedCapability => {}
            // Already executed: contradiction.
            _ => {
                return Err(DispositionError::ExecutedAndExcluded(path.clone()));
            }
        }
    }
    // Detect duplicate exclusions: a path listed twice.
    {
        let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
        for path in excluded_paths {
            if seen.insert(path.as_str(), ()).is_some() {
                return Err(DispositionError::DuplicateDisposition(path.clone()));
            }
        }
    }
    // Every non-executed path must be claimed by an exclusion. The default
    // in `by_path` is `ExcludedCapability`, so an unclaimed path currently
    // looks excluded; recompute from the explicit exclusion set instead.
    let exclusion_set: BTreeMap<&str, ()> =
        excluded_paths.iter().map(|p| (p.as_str(), ())).collect();
    for entry in &inventory.entries {
        let executed_here = executed.iter().any(|c| c.upstream_path == entry.path);
        if !executed_here && !exclusion_set.contains_key(entry.path.as_str()) {
            return Err(DispositionError::MissingExclusion(entry.path.clone()));
        }
    }
    Ok(Dispositions {
        by_path,
        executed: executed.to_vec(),
    })
}

/// Returns `true` when `claim` appears more than once in `executed`.
fn executed_duplicate(executed: &[ExecutedClaim], claim: &ExecutedClaim) -> bool {
    executed
        .iter()
        .filter(|c| c.upstream_path == claim.upstream_path)
        .count()
        > 1
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn source() -> ManifestSource {
        ManifestSource {
            repository: "https://github.com/web-platform-tests/wpt".to_owned(),
            commit: "0968c868d8095217d18d86b34c7f21dccae58768".to_owned(),
            license: "BSD-3-Clause".to_owned(),
        }
    }

    fn inventory_text() -> String {
        let sha = "a".repeat(64);
        let blob = "b".repeat(40);
        format!(
            "{{\"schema_version\":1,\"repository\":\"https://github.com/web-platform-tests/wpt\",\"commit\":\"0968c868d8095217d18d86b34c7f21dccae58768\",\"scope\":\"FileAPI/\",\"file_count\":2,\"inventory_sha256\":\"{i}\",\"files\":[{{\"path\":\"FileAPI/blob/a.any.js\",\"blob_sha\":\"{blob}\",\"size\":1,\"sha256\":\"{sha}\"}},{{\"path\":\"FileAPI/reading-data-section/b.any.js\",\"blob_sha\":\"{blob}\",\"size\":1,\"sha256\":\"{sha}\"}}]}}",
            i = "c".repeat(64)
        )
    }

    #[test]
    fn loads_and_validates_inventory() {
        let inv = load_inventory(&inventory_text(), &source()).expect("load");
        assert_eq!(inv.len(), 2);
        assert!(inv.get("FileAPI/blob/a.any.js").is_some());
    }

    #[test]
    fn rejects_source_mismatch_and_duplicates() {
        let mut bad = inventory_text().replace(
            "0968c868d8095217d18d86b34c7f21dccae58768",
            "0".repeat(40).as_str(),
        );
        assert!(load_inventory(&bad, &source()).is_err());
        bad = inventory_text().replace(
            "FileAPI/reading-data-section/b.any.js",
            "FileAPI/blob/a.any.js",
        );
        assert!(matches!(
            load_inventory(&bad, &source()),
            Err(InventoryError::Duplicate(_))
        ));
    }

    #[test]
    fn disposition_bijection_rejects_missing_and_contradiction() {
        let inv = load_inventory(&inventory_text(), &source()).expect("load");
        let executed = vec![ExecutedClaim {
            upstream_path: "FileAPI/blob/a.any.js".to_owned(),
            disposition: Disposition::ExecutedDirect,
        }];
        // Missing exclusion for b.any.js.
        assert!(matches!(
            resolve_dispositions(&inv, &executed, &[]),
            Err(DispositionError::MissingExclusion(_))
        ));
        // Executed and excluded at once.
        assert!(matches!(
            resolve_dispositions(&inv, &executed, &["FileAPI/blob/a.any.js".to_owned()]),
            Err(DispositionError::ExecutedAndExcluded(_))
        ));
        // Valid bijection.
        let ok = resolve_dispositions(
            &inv,
            &executed,
            &["FileAPI/reading-data-section/b.any.js".to_owned()],
        )
        .expect("bijection");
        assert_eq!(ok.executed_direct(), 1);
        assert_eq!(ok.excluded(), 1);
        // Fake path.
        assert!(matches!(
            resolve_dispositions(&inv, &executed, &["FileAPI/nope.js".to_owned()]),
            Err(DispositionError::ExclusionNotInInventory(_))
        ));
        // Duplicate disposition (same exclusion twice).
        assert!(matches!(
            resolve_dispositions(
                &inv,
                &executed,
                &[
                    "FileAPI/reading-data-section/b.any.js".to_owned(),
                    "FileAPI/reading-data-section/b.any.js".to_owned(),
                ]
            ),
            Err(DispositionError::DuplicateDisposition(_))
        ));
    }
}
