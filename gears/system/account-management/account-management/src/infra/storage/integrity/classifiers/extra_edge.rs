//! Extra-edge classifier — closure rows that should not be present.
//!
//! Emits [`IntegrityCategory::StaleClosureRow`] in two flavours:
//!
//! 1. **Missing endpoint**: the closure row references a tenant
//!    (ancestor or descendant) that is not in the snapshot.
//! 2. **Ancestry not in walk**: both endpoints exist, but the asserted
//!    `(ancestor, descendant)` ancestry is not produced by the
//!    `parent_id` walk. Self-rows (`A = D`) are skipped — they're
//!    valid by invariant.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::domain::tenant::integrity::{IntegrityCategory, Violation};

use super::super::snapshot::Snapshot;

pub(super) fn classify(snap: &Snapshot) -> Vec<Violation> {
    // Pre-compute the set of `(ancestor, descendant)` edges produced by
    // the parent-id walk plus the self-rows (always valid). Closure rows
    // not in this set are reportable as ancestry-not-in-walk.
    let mut parent_of: HashMap<Uuid, Option<Uuid>> = HashMap::with_capacity(snap.tenants().len());
    for t in snap.tenants() {
        parent_of.insert(t.id, t.parent_id);
    }
    let mut walk_edges: HashSet<(Uuid, Uuid)> = HashSet::new();
    let cap = snap.tenants().len();
    for t in snap.tenants() {
        walk_edges.insert((t.id, t.id)); // self-rows are valid edges
        let mut visited: HashSet<Uuid> = HashSet::new();
        visited.insert(t.id);
        let mut cursor = t.parent_id;
        let mut steps = 0usize;
        while let Some(anc) = cursor {
            if !visited.insert(anc) {
                break; // cycle
            }
            steps += 1;
            if steps > cap {
                break;
            }
            if !snap.has_tenant(anc) {
                break; // orphan parent — stop walk
            }
            walk_edges.insert((anc, t.id));
            cursor = parent_of.get(&anc).copied().flatten();
        }
    }

    let mut out = Vec::new();
    // Track endpoints already reported as missing so we don't also flag
    // them as ancestry-not-in-walk.
    let mut missing_endpoint_seen: HashSet<(Uuid, Uuid)> = HashSet::new();
    for c in snap.closure() {
        let a_present = snap.has_tenant(c.ancestor_id);
        let d_present = snap.has_tenant(c.descendant_id);
        // A closure row with at least one missing endpoint is a single
        // stale row from the repair planner's perspective (one DELETE
        // per `(a, d)` pair). Emit at most one violation per row so
        // the check report's `StaleClosureRow` count matches the
        // planner's `repaired` count and dashboards do not show a
        // permanent detected-vs-repaired drift on corruption events
        // where both endpoints are absent.
        if !a_present || !d_present {
            missing_endpoint_seen.insert((c.ancestor_id, c.descendant_id));
            // Tag the violation by the missing side when exactly one
            // endpoint is absent (the operator-meaningful "what's
            // missing" id); when both are absent there is no clearly
            // meaningful tag, so we pick the descendant — which is
            // also the planner's per-category key for
            // `StaleClosureRow`.
            let (tagged_id, detail) = match (a_present, d_present) {
                (false, false) => (
                    c.descendant_id,
                    format!(
                        "closure({a} -> {d}) references missing ancestor and descendant",
                        a = c.ancestor_id,
                        d = c.descendant_id
                    ),
                ),
                (true, false) => (
                    c.descendant_id,
                    format!(
                        "closure row references missing descendant {id}",
                        id = c.descendant_id
                    ),
                ),
                (false, true) => (
                    c.ancestor_id,
                    format!(
                        "closure row references missing ancestor {id}",
                        id = c.ancestor_id
                    ),
                ),
                (true, true) => unreachable!("guarded by the surrounding `if`"),
            };
            out.push(Violation {
                category: IntegrityCategory::StaleClosureRow,
                tenant_id: Some(tagged_id),
                details: detail,
            });
        }
    }

    for c in snap.closure() {
        if c.ancestor_id == c.descendant_id {
            continue; // self-row — valid by invariant
        }
        if missing_endpoint_seen.contains(&(c.ancestor_id, c.descendant_id)) {
            continue;
        }
        if !snap.has_tenant(c.ancestor_id) || !snap.has_tenant(c.descendant_id) {
            continue;
        }
        if !walk_edges.contains(&(c.ancestor_id, c.descendant_id)) {
            out.push(Violation {
                category: IntegrityCategory::StaleClosureRow,
                tenant_id: Some(c.descendant_id),
                details: format!(
                    "closure({a} -> {d}) asserts ancestry not present in parent_id walk",
                    a = c.ancestor_id,
                    d = c.descendant_id,
                ),
            });
        }
    }
    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "extra_edge_tests.rs"]
mod extra_edge_tests;
