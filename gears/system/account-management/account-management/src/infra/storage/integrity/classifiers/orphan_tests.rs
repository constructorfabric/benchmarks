use super::*;
use crate::infra::storage::integrity::snapshot::{ClosureSnap, TenantSnap};
use uuid::Uuid;

fn t(id: u128, parent: Option<u128>, status: TenantStatus) -> TenantSnap {
    TenantSnap {
        id: Uuid::from_u128(id),
        parent_id: parent.map(Uuid::from_u128),
        status,
        depth: 0,
        self_managed: false,
    }
}

fn closure(a: u128, d: u128) -> ClosureSnap {
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
fn valid_tree_yields_no_violations() {
    let snap = Snapshot::new(
        vec![
            t(1, None, TenantStatus::Active),
            t(2, Some(1), TenantStatus::Active),
        ],
        vec![closure(1, 1), closure(2, 2), closure(1, 2)],
    );
    assert!(classify(&snap).is_empty());
}

#[test]
fn orphan_child_is_reported() {
    let snap = Snapshot::new(vec![t(2, Some(99), TenantStatus::Active)], vec![]);
    let v = classify(&snap);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].category, IntegrityCategory::OrphanedChild);
    assert_eq!(v[0].tenant_id, Some(Uuid::from_u128(2)));
}

#[test]
fn broken_parent_reference_when_parent_is_deleted() {
    let snap = Snapshot::new(
        vec![
            t(1, None, TenantStatus::Deleted),
            t(2, Some(1), TenantStatus::Active),
        ],
        vec![closure(1, 1), closure(2, 2), closure(1, 2)],
    );
    let v = classify(&snap);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].category, IntegrityCategory::BrokenParentReference);
    assert_eq!(v[0].tenant_id, Some(Uuid::from_u128(2)));
}

#[test]
fn deleted_child_under_deleted_parent_is_silent() {
    let snap = Snapshot::new(
        vec![
            t(1, None, TenantStatus::Deleted),
            t(2, Some(1), TenantStatus::Deleted),
        ],
        vec![closure(1, 1), closure(2, 2), closure(1, 2)],
    );
    assert!(classify(&snap).is_empty());
}

#[test]
fn multi_violation_scenario_collects_all() {
    let snap = Snapshot::new(
        vec![
            t(1, None, TenantStatus::Deleted),
            t(2, Some(1), TenantStatus::Active), // BrokenParentReference
            t(3, Some(99), TenantStatus::Active), // OrphanedChild
        ],
        vec![closure(1, 1), closure(2, 2), closure(3, 3), closure(1, 2)],
    );
    let v = classify(&snap);
    assert_eq!(v.len(), 2);
    let cats: Vec<_> = v.iter().map(|x| x.category).collect();
    assert!(cats.contains(&IntegrityCategory::OrphanedChild));
    assert!(cats.contains(&IntegrityCategory::BrokenParentReference));
}
