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
fn fully_consistent_closure_yields_no_violations() {
    let snap = Snapshot::new(
        vec![t(1, None), t(2, Some(1))],
        vec![c(1, 1), c(2, 2), c(1, 2)],
    );
    assert!(classify(&snap).is_empty());
}

#[test]
fn missing_descendant_endpoint_is_reported() {
    let snap = Snapshot::new(vec![t(1, None)], vec![c(1, 1), c(1, 99)]);
    let v = classify(&snap);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].category, IntegrityCategory::StaleClosureRow);
    assert_eq!(v[0].tenant_id, Some(Uuid::from_u128(99)));
}

#[test]
fn missing_ancestor_endpoint_is_reported() {
    let snap = Snapshot::new(vec![t(1, None)], vec![c(1, 1), c(99, 1)]);
    let v = classify(&snap);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].tenant_id, Some(Uuid::from_u128(99)));
}

#[test]
fn ancestry_not_in_walk_is_reported() {
    // 1 and 2 both root; closure asserts (1,2) which is not in the walk.
    let snap = Snapshot::new(
        vec![t(1, None), t(2, None)],
        vec![c(1, 1), c(2, 2), c(1, 2)],
    );
    let v = classify(&snap);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].category, IntegrityCategory::StaleClosureRow);
    assert_eq!(v[0].tenant_id, Some(Uuid::from_u128(2)));
}

#[test]
fn self_rows_are_valid() {
    let snap = Snapshot::new(vec![t(1, None)], vec![c(1, 1)]);
    assert!(classify(&snap).is_empty());
}

#[test]
fn missing_endpoint_emits_one_violation_per_row() {
    // Closure `(99, 99)` with neither endpoint present in tenants.
    // The check report MUST emit ONE `StaleClosureRow` violation
    // (matching the planner's single DELETE per `(a, d)` pair) so
    // that detected-vs-repaired counts stay aligned on dashboards.
    let snap = Snapshot::new(vec![], vec![c(99, 99)]);
    let v = classify(&snap);
    assert_eq!(v.len(), 1, "one row, one violation, not two");
    assert_eq!(v[0].category, IntegrityCategory::StaleClosureRow);
    let detail = &v[0].details;
    assert!(
        detail.contains("missing ancestor and descendant"),
        "violation detail must call out both missing endpoints, got: {detail}"
    );
}

#[test]
fn missing_one_endpoint_emits_one_violation() {
    // Strict edge `(1, 99)` where only the ancestor (`1`) exists in
    // the snapshot — exactly one violation pointing at the missing
    // descendant.
    let snap = Snapshot::new(vec![t(1, None)], vec![c(1, 1), c(1, 99)]);
    let v = classify(&snap);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].category, IntegrityCategory::StaleClosureRow);
    assert!(
        v[0].details.contains("missing descendant"),
        "got: {}",
        v[0].details
    );
}
