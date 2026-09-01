//! Depth classifier — reports tenants whose stored `depth` disagrees
//! with the depth derived from the `parent_id` walk to the root.
//!
//! Tenants caught by the cycle classifier are intentionally excluded
//! (their parent walk never terminates), to avoid double-reporting the
//! same root-cause as both `Cycle` and `DepthMismatch`.

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
    for t in snap.tenants() {
        // Walk the parent chain bounding by `tenants.len()` to terminate
        // even if a cycle exists; cycles are reported separately.
        let mut steps: i32 = 0;
        let mut cursor: Option<Uuid> = t.parent_id;
        let mut path_set: HashSet<Uuid> = HashSet::new();
        path_set.insert(t.id);
        let cap = i32::try_from(snap.tenants().len()).unwrap_or(i32::MAX);
        let mut cycle_seen = false;
        while let Some(p) = cursor {
            if !path_set.insert(p) {
                cycle_seen = true;
                break;
            }
            steps += 1;
            if steps > cap {
                cycle_seen = true;
                break;
            }
            cursor = parent_of.get(&p).copied().flatten();
            if cursor.is_none() && !parent_of.contains_key(&p) {
                // Walked off the snapshot — this is an OrphanedChild
                // case, not a depth mismatch. Stop without emitting.
                cycle_seen = true;
                break;
            }
        }
        if cycle_seen {
            continue;
        }
        let walk = steps;
        if walk != t.depth {
            out.push(Violation {
                category: IntegrityCategory::DepthMismatch,
                tenant_id: Some(t.id),
                details: format!(
                    "tenant {tid} stored depth {stored} but walk yields {walk}",
                    tid = t.id,
                    stored = t.depth
                ),
            });
        }
    }
    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::tenant::model::TenantStatus;
    use crate::infra::storage::integrity::snapshot::TenantSnap;

    fn t(id: u128, parent: Option<u128>, depth: i32) -> TenantSnap {
        TenantSnap {
            id: Uuid::from_u128(id),
            parent_id: parent.map(Uuid::from_u128),
            status: TenantStatus::Active,
            depth,
            self_managed: false,
        }
    }

    #[test]
    fn empty_input_yields_no_violations() {
        let snap = Snapshot::new(vec![], vec![]);
        assert!(classify(&snap).is_empty());
    }

    #[test]
    fn correct_depths_yield_no_violations() {
        let snap = Snapshot::new(
            vec![t(1, None, 0), t(2, Some(1), 1), t(3, Some(2), 2)],
            vec![],
        );
        assert!(classify(&snap).is_empty());
    }

    #[test]
    fn single_depth_mismatch_is_reported() {
        let snap = Snapshot::new(
            vec![t(1, None, 0), t(2, Some(1), 5)], // expected 1, stored 5
            vec![],
        );
        let v = classify(&snap);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].category, IntegrityCategory::DepthMismatch);
        assert_eq!(v[0].tenant_id, Some(Uuid::from_u128(2)));
    }

    #[test]
    fn root_depth_mismatch_is_reported() {
        let snap = Snapshot::new(vec![t(1, None, 7)], vec![]);
        let v = classify(&snap);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].tenant_id, Some(Uuid::from_u128(1)));
    }

    #[test]
    fn cycle_members_are_not_double_reported() {
        let snap = Snapshot::new(vec![t(1, Some(2), 0), t(2, Some(1), 0)], vec![]);
        assert!(classify(&snap).is_empty());
    }

    #[test]
    fn orphan_child_does_not_cause_depth_violation() {
        // tenant 2 has parent 99 which is missing — handled by orphan classifier;
        // depth must not double-report.
        let snap = Snapshot::new(vec![t(2, Some(99), 1)], vec![]);
        assert!(classify(&snap).is_empty());
    }
}
