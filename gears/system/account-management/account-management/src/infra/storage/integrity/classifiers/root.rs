//! Root classifier — single-root invariant.
//!
//! A healthy whole tree has exactly one tenant with `parent_id IS NULL`.
//! The classifier emits [`IntegrityCategory::RootCountAnomaly`] when:
//!
//! * `root_count > 1` — multiple roots break the invariant.
//! * `root_count == 0 && total_count > 0` — zero roots but the gear
//!   has tenants (anomalous; bootstrap requires a single root).

use crate::domain::tenant::integrity::{IntegrityCategory, Violation};

use super::super::snapshot::Snapshot;

pub(super) fn classify(snap: &Snapshot) -> Vec<Violation> {
    let total = snap.tenants().len();
    let root_count = snap
        .tenants()
        .iter()
        .filter(|t| t.parent_id.is_none())
        .count();
    let mut out = Vec::new();
    if root_count > 1 {
        out.push(Violation {
            category: IntegrityCategory::RootCountAnomaly,
            tenant_id: None,
            details: format!("found {root_count} roots (parent_id IS NULL); expected 1"),
        });
    } else if root_count == 0 && total > 0 {
        out.push(Violation {
            category: IntegrityCategory::RootCountAnomaly,
            tenant_id: None,
            details: "no root tenant present but gear has tenants".to_owned(),
        });
    }
    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::tenant::model::TenantStatus;
    use crate::infra::storage::integrity::snapshot::TenantSnap;
    use uuid::Uuid;

    fn t(id: u128, parent: Option<u128>) -> TenantSnap {
        TenantSnap {
            id: Uuid::from_u128(id),
            parent_id: parent.map(Uuid::from_u128),
            status: TenantStatus::Active,
            depth: 0,
            self_managed: false,
        }
    }

    #[test]
    fn empty_input_yields_no_violations() {
        let snap = Snapshot::new(vec![], vec![]);
        assert!(classify(&snap).is_empty());
    }

    #[test]
    fn single_root_is_clean() {
        let snap = Snapshot::new(vec![t(1, None), t(2, Some(1))], vec![]);
        assert!(classify(&snap).is_empty());
    }

    #[test]
    fn multiple_roots_are_reported() {
        let snap = Snapshot::new(vec![t(1, None), t(2, None)], vec![]);
        let v = classify(&snap);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].category, IntegrityCategory::RootCountAnomaly);
        assert!(v[0].details.contains("2 roots"));
    }

    #[test]
    fn zero_roots_with_tenants_is_reported() {
        // 1 -> 2 -> 1 (no root)
        let snap = Snapshot::new(vec![t(1, Some(2)), t(2, Some(1))], vec![]);
        let v = classify(&snap);
        assert_eq!(v.len(), 1);
        assert!(v[0].details.contains("no root"));
    }
}
