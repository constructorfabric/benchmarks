//! Barrier + descendant-status materialization classifier.
//!
//! Reports the two materialization invariants that join `tenants` with
//! `tenant_closure`:
//!
//! * [`IntegrityCategory::BarrierColumnDivergence`] — for each strict
//!   `(A, D)` closure row (`A != D`), `barrier = 1` iff any tenant on
//!   the strict `(A, D]` parent-walk path is `self_managed`. Self-rows
//!   are skipped (the schema CHECK pins their barrier to 0; a
//!   divergence there is a different class of corruption).
//! * [`IntegrityCategory::DescendantStatusDivergence`] — for every
//!   closure row, `descendant_status` MUST equal `tenants.status` for
//!   the descendant. Tenants caught by the orphan-endpoint pass of
//!   `extra_edge` cannot be checked (descendant absent), so missing
//!   endpoints are skipped here.
//!
//! ## Complexity
//!
//! Expected `(ancestor, descendant) → barrier` pairs are derived once
//! up front from `tenants.parent_id` in `O(N × depth)` and stored in
//! a `HashMap`; the closure scan then becomes `O(|closure|)` lookups.
//! Total work is `O(N × depth + |closure|)`.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::domain::tenant::integrity::{IntegrityCategory, Violation};

use super::super::snapshot::Snapshot;

pub(super) fn classify(snap: &Snapshot) -> Vec<Violation> {
    let parent_of: HashMap<Uuid, Option<Uuid>> =
        snap.tenants().iter().map(|t| (t.id, t.parent_id)).collect();

    let expected = build_expected_barriers(snap, &parent_of);

    let mut out = Vec::new();
    for row in snap.closure() {
        // Status divergence — both endpoints must be present in tenants.
        if let Some(d) = snap.tenant(row.descendant_id)
            && row.descendant_status != d.status
        {
            let stored = row.descendant_status.as_smallint();
            let current = d.status.as_smallint();
            out.push(Violation {
                category: IntegrityCategory::DescendantStatusDivergence,
                tenant_id: Some(row.descendant_id),
                details: format!(
                    "closure({a} -> {d}).descendant_status={stored} but tenants.status={current}",
                    a = row.ancestor_id,
                    d = row.descendant_id
                ),
            });
        }

        // Barrier divergence — only for strict edges with both endpoints
        // present in the snapshot.
        if row.ancestor_id == row.descendant_id {
            continue;
        }
        if snap.tenant(row.ancestor_id).is_none() || snap.tenant(row.descendant_id).is_none() {
            continue;
        }
        let Some(&expected_barrier) = expected.get(&(row.ancestor_id, row.descendant_id)) else {
            // Pair is not produced by the parent-walk (orphan chain or
            // cycle off this descendant); the strict-ancestor /
            // extra-edge / cycle classifiers handle these. The spec
            // says `extra_edge` owns this case, so we stay silent
            // here rather than emit a redundant divergence.
            continue;
        };
        if row.barrier != expected_barrier {
            out.push(Violation {
                category: IntegrityCategory::BarrierColumnDivergence,
                tenant_id: Some(row.descendant_id),
                details: format!(
                    "closure({a} -> {d}).barrier={actual} but expected {expected_barrier}",
                    a = row.ancestor_id,
                    d = row.descendant_id,
                    actual = row.barrier
                ),
            });
        }
    }
    out
}

/// Walk `tenants.parent_id` once per tenant to build the expected
/// barrier value for every `(ancestor, descendant)` pair the
/// parent-walk produces. The barrier is `1` iff any tenant on the
/// strict `(ancestor, descendant]` path (ancestor excluded, descendant
/// included) is `self_managed`.
///
/// Tenants whose walk hits a cycle or an orphan parent contribute
/// only the prefix up to the dead-end; downstream classifiers
/// (`cycle`, `orphan`) surface those cases independently.
fn build_expected_barriers(
    snap: &Snapshot,
    parent_of: &HashMap<Uuid, Option<Uuid>>,
) -> HashMap<(Uuid, Uuid), i16> {
    let cap = snap.tenants().len();
    let mut expected: HashMap<(Uuid, Uuid), i16> = HashMap::with_capacity(cap.saturating_mul(2));

    for descendant in snap.tenants() {
        let mut visited: HashSet<Uuid> = HashSet::new();
        visited.insert(descendant.id);
        let mut has_self_managed = i16::from(descendant.self_managed);
        let mut cursor = descendant.parent_id;
        let mut steps = 0usize;
        while let Some(anc) = cursor {
            if !visited.insert(anc) || steps > cap {
                break; // cycle / runaway — handled by cycle classifier
            }
            steps += 1;
            let Some(anc_tenant) = snap.tenant(anc) else {
                break; // orphan parent — handled by orphan classifier
            };
            // Per spec the strict `(A, D]` path excludes A — so we
            // record the expected barrier first (using only nodes
            // strictly below A on the chain), then fold A's
            // `self_managed` into the next step's accumulator.
            expected.insert((anc, descendant.id), has_self_managed);
            if anc_tenant.self_managed {
                has_self_managed = 1;
            }
            cursor = parent_of.get(&anc).copied().flatten();
        }
    }
    expected
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "barrier_tests.rs"]
mod barrier_tests;
