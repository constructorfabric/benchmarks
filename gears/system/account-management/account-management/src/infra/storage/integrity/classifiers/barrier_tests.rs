use super::*;
use crate::domain::tenant::model::TenantStatus;
use crate::infra::storage::integrity::snapshot::{ClosureSnap, TenantSnap};

fn t(id: u128, parent: Option<u128>, self_managed: bool, status: TenantStatus) -> TenantSnap {
    TenantSnap {
        id: Uuid::from_u128(id),
        parent_id: parent.map(Uuid::from_u128),
        status,
        depth: 0,
        self_managed,
    }
}

fn c(a: u128, d: u128, barrier: i16, status: TenantStatus) -> ClosureSnap {
    ClosureSnap {
        ancestor_id: Uuid::from_u128(a),
        descendant_id: Uuid::from_u128(d),
        barrier,
        descendant_status: status,
    }
}

#[test]
fn empty_input_yields_no_violations() {
    let snap = Snapshot::new(vec![], vec![]);
    assert!(classify(&snap).is_empty());
}

#[test]
fn consistent_barrier_and_status_yield_no_violations() {
    // 1 -> 2; nothing self-managed; statuses match.
    let snap = Snapshot::new(
        vec![
            t(1, None, false, TenantStatus::Active),
            t(2, Some(1), false, TenantStatus::Active),
        ],
        vec![
            c(1, 1, 0, TenantStatus::Active),
            c(2, 2, 0, TenantStatus::Active),
            c(1, 2, 0, TenantStatus::Active),
        ],
    );
    assert!(classify(&snap).is_empty());
}

#[test]
fn descendant_status_divergence_is_reported() {
    let snap = Snapshot::new(
        vec![
            t(1, None, false, TenantStatus::Active),
            t(2, Some(1), false, TenantStatus::Suspended),
        ],
        vec![
            c(1, 1, 0, TenantStatus::Active),
            c(2, 2, 0, TenantStatus::Active), // diverges from Suspended
            c(1, 2, 0, TenantStatus::Active),
        ],
    );
    let v = classify(&snap);
    // Both (2,2) and (1,2) carry stale status; report each.
    let cats: Vec<_> = v.iter().map(|x| x.category).collect();
    assert!(cats.contains(&IntegrityCategory::DescendantStatusDivergence));
    let dn_count = cats
        .iter()
        .filter(|c| **c == IntegrityCategory::DescendantStatusDivergence)
        .count();
    assert_eq!(dn_count, 2);
}

#[test]
fn missing_barrier_when_self_managed_present_is_reported() {
    // 1 -> 2 (self_managed). Closure (1,2) should have barrier=1.
    let snap = Snapshot::new(
        vec![
            t(1, None, false, TenantStatus::Active),
            t(2, Some(1), true, TenantStatus::Active),
        ],
        vec![
            c(1, 1, 0, TenantStatus::Active),
            c(2, 2, 0, TenantStatus::Active),
            c(1, 2, 0, TenantStatus::Active), // expected barrier=1
        ],
    );
    let v = classify(&snap);
    assert!(
        v.iter()
            .any(|x| x.category == IntegrityCategory::BarrierColumnDivergence)
    );
}

#[test]
fn extra_barrier_when_no_self_managed_is_reported() {
    // 1 -> 2; nothing self_managed. Closure (1,2) has barrier=1 but expected 0.
    let snap = Snapshot::new(
        vec![
            t(1, None, false, TenantStatus::Active),
            t(2, Some(1), false, TenantStatus::Active),
        ],
        vec![
            c(1, 1, 0, TenantStatus::Active),
            c(2, 2, 0, TenantStatus::Active),
            c(1, 2, 1, TenantStatus::Active),
        ],
    );
    let v = classify(&snap);
    let count = v
        .iter()
        .filter(|n| n.category == IntegrityCategory::BarrierColumnDivergence)
        .count();
    assert_eq!(count, 1);
}

#[test]
fn intermediate_self_managed_ancestor_locks_barrier_above_only() {
    // Three-node chain `1 -> 2 -> 3`. Tenant `2` is self_managed.
    // Per the `(A, D]` strict-path semantic (ancestor excluded,
    // descendant included), the expected barriers are:
    //   (1, 2) = 1  ← `2` is on the strict (1, 2] path, and `2`
    //                  is itself self_managed.
    //   (2, 3) = 0  ← `2` is *excluded* from the strict (2, 3] path,
    //                  so `2`'s self_managed flag does NOT lift the
    //                  barrier on this edge. `3` itself is not
    //                  self_managed.
    //   (1, 3) = 1  ← `2` and `3` together form the strict (1, 3]
    //                  path; `2` being self_managed lifts the
    //                  barrier.
    // The test pins the off-by-one boundary that the strict
    // `(A, D]` definition is designed to enforce.
    let snap_consistent = Snapshot::new(
        vec![
            t(1, None, false, TenantStatus::Active),
            t(2, Some(1), true, TenantStatus::Active),
            t(3, Some(2), false, TenantStatus::Active),
        ],
        vec![
            c(1, 1, 0, TenantStatus::Active),
            c(2, 2, 0, TenantStatus::Active),
            c(3, 3, 0, TenantStatus::Active),
            c(1, 2, 1, TenantStatus::Active),
            c(2, 3, 0, TenantStatus::Active),
            c(1, 3, 1, TenantStatus::Active),
        ],
    );
    assert!(
        classify(&snap_consistent).is_empty(),
        "all three barriers consistent with the (A, D] semantic"
    );

    // Flip `(2, 3).barrier = 1`: a divergence because `2` is
    // excluded from the strict (2, 3] path, so the only candidate
    // (`3`) is not self_managed and the expected value is 0.
    let snap_divergent = Snapshot::new(
        vec![
            t(1, None, false, TenantStatus::Active),
            t(2, Some(1), true, TenantStatus::Active),
            t(3, Some(2), false, TenantStatus::Active),
        ],
        vec![
            c(1, 1, 0, TenantStatus::Active),
            c(2, 2, 0, TenantStatus::Active),
            c(3, 3, 0, TenantStatus::Active),
            c(1, 2, 1, TenantStatus::Active),
            c(2, 3, 1, TenantStatus::Active), // diverges from expected 0
            c(1, 3, 1, TenantStatus::Active),
        ],
    );
    let v = classify(&snap_divergent);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].category, IntegrityCategory::BarrierColumnDivergence);
    assert_eq!(v[0].tenant_id, Some(Uuid::from_u128(3)));
}

#[test]
fn self_rows_are_not_checked_for_barrier_divergence() {
    // Seed barrier=1 (intentionally wrong for a self-row whose expected
    // barrier is 0) to pin the contract: the classifier must skip
    // self-rows entirely, not just happen to pass because barrier
    // matches the expected value.
    let snap = Snapshot::new(
        vec![t(1, None, true, TenantStatus::Active)],
        vec![c(1, 1, 1, TenantStatus::Active)],
    );
    assert!(classify(&snap).is_empty());
}
