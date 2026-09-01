//! Strict-ancestor classifier — every strict `(ancestor, descendant)`
//! pair derived by the parent-id walk MUST appear as a closure row.
//!
//! Emits [`IntegrityCategory::ClosureCoverageGap`] for any descendant
//! whose strict ancestor along the parent chain is missing from
//! `tenant_closure`. Tenants that participate in cycles (parent walk
//! does not terminate) are not reported — those are surfaced by the
//! cycle classifier.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::domain::tenant::integrity::{IntegrityCategory, Violation};

use super::super::snapshot::Snapshot;

pub(super) fn classify(snap: &Snapshot) -> Vec<Violation> {
    let mut parent_of: HashMap<Uuid, Option<Uuid>> = HashMap::with_capacity(snap.tenants().len());
    for t in snap.tenants() {
        parent_of.insert(t.id, t.parent_id);
    }

    let mut out = Vec::new();
    let cap = snap.tenants().len();
    for t in snap.tenants() {
        let mut visited: HashSet<Uuid> = HashSet::new();
        visited.insert(t.id);
        let mut cursor = t.parent_id;
        let mut steps = 0usize;
        // Collect candidate gaps eagerly and only flush them if the
        // walk completes acyclically. A cycle / cap / orphan break
        // means an earlier classifier already owns the violation, and
        // emitting a gap here on the way to that anomaly produces a
        // double-report. See gear doc and the cycle classifier.
        let mut pending_gaps: Vec<Uuid> = Vec::new();
        let mut abort = false;
        while let Some(anc) = cursor {
            if !visited.insert(anc) {
                abort = true;
                break; // cycle — handled by cycle classifier
            }
            steps += 1;
            if steps > cap {
                abort = true;
                break;
            }
            if !snap.has_tenant(anc) {
                abort = true;
                break; // orphan parent — handled by orphan classifier
            }
            if !snap.has_closure_edge(anc, t.id) {
                pending_gaps.push(anc);
            }
            cursor = parent_of.get(&anc).copied().flatten();
        }
        if !abort {
            for anc in pending_gaps {
                out.push(Violation {
                    category: IntegrityCategory::ClosureCoverageGap,
                    tenant_id: Some(t.id),
                    details: format!(
                        "closure gap: ancestor {anc} missing for descendant {tid}",
                        tid = t.id
                    ),
                });
            }
        }
    }
    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::tenant::model::TenantStatus;
    use crate::infra::storage::integrity::snapshot::{ClosureSnap, TenantSnap};

    fn t(id: u128, parent: Option<u128>) -> TenantSnap {
        TenantSnap {
            id: Uuid::from_u128(id),
            parent_id: parent.map(Uuid::from_u128),
            status: TenantStatus::Active,
            depth: 0,
            self_managed: false,
        }
    }

    fn c(a: u128, d: u128) -> ClosureSnap {
        ClosureSnap {
            ancestor_id: Uuid::from_u128(a),
            descendant_id: Uuid::from_u128(d),
            barrier: 0,
            descendant_status: TenantStatus::Active,
        }
    }

    #[test]
    fn empty_input_yields_no_violations() {
        let snap = Snapshot::new(vec![], vec![]);
        assert!(classify(&snap).is_empty());
    }

    #[test]
    fn full_closure_yields_no_violations() {
        // 1 -> 2 -> 3, full closure including transitive (1,3).
        let snap = Snapshot::new(
            vec![t(1, None), t(2, Some(1)), t(3, Some(2))],
            vec![c(1, 1), c(2, 2), c(3, 3), c(1, 2), c(2, 3), c(1, 3)],
        );
        assert!(classify(&snap).is_empty());
    }

    #[test]
    fn missing_transitive_ancestor_is_reported() {
        // (1, 3) missing — strict ancestor 1 of descendant 3 not present.
        let snap = Snapshot::new(
            vec![t(1, None), t(2, Some(1)), t(3, Some(2))],
            vec![c(1, 1), c(2, 2), c(3, 3), c(1, 2), c(2, 3)],
        );
        let v = classify(&snap);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].category, IntegrityCategory::ClosureCoverageGap);
        assert_eq!(v[0].tenant_id, Some(Uuid::from_u128(3)));
    }

    #[test]
    fn missing_direct_parent_edge_is_reported() {
        let snap = Snapshot::new(
            vec![t(1, None), t(2, Some(1))],
            vec![c(1, 1), c(2, 2)], // (1,2) missing
        );
        let v = classify(&snap);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].tenant_id, Some(Uuid::from_u128(2)));
    }

    #[test]
    fn orphan_parent_does_not_double_report() {
        // Parent 99 is missing (orphan classifier territory). No closure
        // gap should be reported for the missing strict ancestor.
        let snap = Snapshot::new(vec![t(2, Some(99))], vec![c(2, 2)]);
        assert!(classify(&snap).is_empty());
    }

    #[test]
    fn cycle_member_does_not_emit_gap_before_revisit() {
        // 1 ↔ 2 with no closure edges beyond self-rows. Walking from
        // tenant 1 visits 2 (gap candidate (2,1)) and then loops back
        // to 1 → cycle break. The gap MUST be deferred to the cycle
        // classifier; emitting it here would double-report cycle
        // members.
        let snap = Snapshot::new(vec![t(1, Some(2)), t(2, Some(1))], vec![c(1, 1), c(2, 2)]);
        assert!(
            classify(&snap).is_empty(),
            "cycle members must not produce ClosureCoverageGap"
        );
    }
}
