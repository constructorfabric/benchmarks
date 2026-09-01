//! Self-row classifier — every SDK-visible tenant MUST have a
//! `(id, id)` row in `tenant_closure`. Reports
//! [`IntegrityCategory::MissingClosureSelfRow`] for any tenant whose
//! self-row is absent.
//!
//! Provisioning rows are filtered out at the loader, so every tenant in
//! the snapshot is SDK-visible — no per-row status check is needed.

use crate::domain::tenant::integrity::{IntegrityCategory, Violation};

use super::super::snapshot::Snapshot;

pub(super) fn classify(snap: &Snapshot) -> Vec<Violation> {
    let mut out = Vec::new();
    for t in snap.tenants() {
        if !snap.has_closure_edge(t.id, t.id) {
            out.push(Violation {
                category: IntegrityCategory::MissingClosureSelfRow,
                tenant_id: Some(t.id),
                details: format!("tenant {tid} lacks self-row in tenant_closure", tid = t.id),
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
    use crate::infra::storage::integrity::snapshot::{ClosureSnap, TenantSnap};
    use uuid::Uuid;

    fn t(id: u128) -> TenantSnap {
        TenantSnap {
            id: Uuid::from_u128(id),
            parent_id: None,
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
    fn complete_self_rows_yield_no_violations() {
        let snap = Snapshot::new(vec![t(1), t(2)], vec![c(1, 1), c(2, 2)]);
        assert!(classify(&snap).is_empty());
    }

    #[test]
    fn missing_self_row_is_reported() {
        let snap = Snapshot::new(vec![t(1), t(2)], vec![c(1, 1)]); // 2 missing
        let v = classify(&snap);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].category, IntegrityCategory::MissingClosureSelfRow);
        assert_eq!(v[0].tenant_id, Some(Uuid::from_u128(2)));
    }

    #[test]
    fn multiple_missing_self_rows_are_all_reported() {
        let snap = Snapshot::new(vec![t(1), t(2), t(3)], vec![c(1, 1)]);
        let v = classify(&snap);
        assert_eq!(v.len(), 2);
        let ids: std::collections::HashSet<_> = v.iter().filter_map(|x| x.tenant_id).collect();
        assert!(ids.contains(&Uuid::from_u128(2)));
        assert!(ids.contains(&Uuid::from_u128(3)));
    }
}
