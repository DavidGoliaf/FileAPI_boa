//! Snapshot state for byte sources.

/// Describes the snapshot state of a byte source.
///
/// In M1 only `Memory` exists. Future milestones will add filesystem-backed variants.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SnapshotState {
    /// The source is an in-memory snapshot that never changes.
    Memory,
}
