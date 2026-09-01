//! Pure-Rust integrity classifiers operating over an in-memory
//! [`Snapshot`](super::snapshot::Snapshot).
//!
//! Each submodule implements one classifier as a synchronous,
//! DB-free function over `&Snapshot`. The 8-file layout matches the
//! 8 canonical classifier names from phase-01 spec; the ten
//! [`IntegrityCategory`](crate::domain::tenant::integrity::IntegrityCategory)
//! variants are produced by these eight files (some classifiers emit
//! more than one category — see `orphan.rs` and `barrier.rs` for the
//! grouped cases).
//!
//! The single entry point used by the rest of the audit pipeline is
//! [`run`], which dispatches to each classifier in fixed order and
//! returns the concatenated `Vec<Violation>`. The aggregation into the
//! per-category [`IntegrityReport`](crate::domain::tenant::integrity::IntegrityReport)
//! lives in `audit::run_classifiers`.

mod barrier;
mod cycle;
mod depth;
mod extra_edge;
mod orphan;
mod root;
mod self_row;
mod strict_ancestor;

use crate::domain::tenant::integrity::Violation;

use super::snapshot::Snapshot;

/// Run every classifier in order and return the concatenated violations.
pub(super) fn run(snap: &Snapshot) -> Vec<Violation> {
    let mut all = Vec::new();
    all.extend(orphan::classify(snap));
    all.extend(cycle::classify(snap));
    all.extend(depth::classify(snap));
    all.extend(self_row::classify(snap));
    all.extend(strict_ancestor::classify(snap));
    all.extend(extra_edge::classify(snap));
    all.extend(root::classify(snap));
    all.extend(barrier::classify(snap));
    all
}
