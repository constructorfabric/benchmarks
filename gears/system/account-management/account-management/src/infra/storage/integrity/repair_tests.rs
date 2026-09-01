//! Unit tests for the pure-Rust closure repair planner.
//!
//! Mirrors the `B.4`-`B.9` + `C.11` cases from the integrity-repair
//! spec — instead of seeding a real `(tenants, tenant_closure)` pair
//! and asserting DB-side rows post-repair, each test hand-builds a
//! [`Snapshot`] in the broken state and asserts the planner emits the
//! exact set of INSERT / UPDATE / DELETE ops that the apply layer
//! would issue. This decouples planner correctness from the (still
//! pending) production-DB integration scaffold the retention pipeline
//! is also waiting on.

#![allow(
    clippy::expect_used,
    clippy::missing_panics_doc,
    reason = "test helpers"
)]

use super::*;
use crate::domain::tenant::integrity::IntegrityCategory;
use crate::domain::tenant::model::TenantStatus;
use crate::infra::storage::integrity::run_classifiers;
use crate::infra::storage::integrity::snapshot::{ClosureSnap, TenantSnap};

fn t(id: u128, parent: Option<u128>, status: TenantStatus, self_managed: bool) -> TenantSnap {
    // Depth defaulted to 0 — tests that need to keep the deferred
    // bucket clean MUST use [`td`] with the correct chain depth.
    TenantSnap {
        id: Uuid::from_u128(id),
        parent_id: parent.map(Uuid::from_u128),
        status,
        depth: 0,
        self_managed,
    }
}

/// `t` with explicit depth — required by tests that assert the
/// deferred bucket is empty (`DepthMismatch` would otherwise count
/// tenants with a non-zero parent walk).
fn td(
    id: u128,
    parent: Option<u128>,
    depth: i32,
    status: TenantStatus,
    self_managed: bool,
) -> TenantSnap {
    TenantSnap {
        id: Uuid::from_u128(id),
        parent_id: parent.map(Uuid::from_u128),
        status,
        depth,
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

/// Self-rows + transitive ancestor rows for a 1->2->3 chain with the
/// given status / `self_managed` shape and consistent stored depth.
/// Used as a baseline "clean snapshot" the tests then perturb.
fn clean_chain_1_2_3(status: TenantStatus) -> Snapshot {
    Snapshot::new(
        vec![
            td(1, None, 0, status, false),
            td(2, Some(1), 1, status, false),
            td(3, Some(2), 2, status, false),
        ],
        vec![
            c(1, 1, 0, status),
            c(2, 2, 0, status),
            c(3, 3, 0, status),
            c(1, 2, 0, status),
            c(2, 3, 0, status),
            c(1, 3, 0, status),
        ],
    )
}

fn count_for(report: &[(IntegrityCategory, usize)], cat: IntegrityCategory) -> usize {
    report
        .iter()
        .find(|(c, _)| *c == cat)
        .map_or(0, |(_, n)| *n)
}

// ---------------------------------------------------------------------
// Clean snapshots — every healthy shape MUST produce an empty plan.
// ---------------------------------------------------------------------

#[test]
fn cycle_tail_is_skipped_by_planner() {
    // Topology: tenant `4` → `2` → `3` → `2` (cycle at `2 ↔ 3`).
    // Tenant `4` is upstream of the cycle (cycle-tail), not in
    // `cycle_members`. The closure target for `4` is structurally
    // undefined while the cycle persists — the planner MUST NOT
    // insert/update closure rows for cycle-tail tenants and MUST
    // surface them in the deferred bucket so operators triage the
    // cycle first.
    let snap = Snapshot::new(
        vec![
            t(2, Some(3), TenantStatus::Active, false),
            t(3, Some(2), TenantStatus::Active, false),
            t(4, Some(2), TenantStatus::Active, false),
        ],
        // Empty closure — if `4` were treated as healthy, the planner
        // would emit inserts for `(4,4)`, `(2,4)`, `(3,4)`. We assert
        // it does NOT.
        vec![],
    );
    let report = run_classifiers(&snap);
    let plan = compute_repair_plan(&snap, &report);
    assert!(
        plan.inserts.is_empty(),
        "cycle-tail tenant must NOT trigger closure inserts; got {:?}",
        plan.inserts
    );
    assert!(plan.deletes.is_empty());
    assert!(plan.barrier_updates.is_empty());
    assert!(plan.status_updates.is_empty());
    // The `cycle` classifier reports exactly the cycle members
    // themselves (tenants `2` and `3`); tenant `4` is upstream of
    // the cycle (cycle-tail) and is surfaced via the planner's
    // `affected` set, NOT the cycle classifier — so the deferred
    // `Cycle` count is exactly 2. Pinning `==` rather than `>=`
    // catches a future regression where the classifier or planner
    // starts double-counting cycle-tail tenants.
    assert_eq!(
        count_for(&plan.deferred_per_category, IntegrityCategory::Cycle),
        2,
        "exactly the two cycle members must populate the deferred bucket: {:?}",
        plan.deferred_per_category
    );
}

#[test]
fn clean_snapshot_yields_empty_plan() {
    let snap = clean_chain_1_2_3(TenantStatus::Active);
    let plan = compute_repair_plan(&snap, &run_classifiers(&snap));
    assert!(plan.inserts.is_empty(), "no inserts: {:?}", plan.inserts);
    assert!(plan.deletes.is_empty(), "no deletes: {:?}", plan.deletes);
    assert!(plan.barrier_updates.is_empty());
    assert!(plan.status_updates.is_empty());
    assert_eq!(plan.total_ops(), 0);
    // All five derivable categories present at zero.
    assert_eq!(plan.repaired_per_category.len(), 5);
    for (_, count) in &plan.repaired_per_category {
        assert_eq!(*count, 0);
    }
    // All five non-derivable categories present at zero.
    assert_eq!(plan.deferred_per_category.len(), 5);
    for (_, count) in &plan.deferred_per_category {
        assert_eq!(*count, 0);
    }
}

#[test]
fn empty_snapshot_yields_empty_plan() {
    let snap = Snapshot::new(vec![], vec![]);
    let plan = compute_repair_plan(&snap, &run_classifiers(&snap));
    assert_eq!(plan.total_ops(), 0);
}

// ---------------------------------------------------------------------
// B.4 — Missing self-row → INSERT (id, id, 0, status).
// ---------------------------------------------------------------------

#[test]
fn missing_self_row_insert_is_planned() {
    // Tenant 2 lacks its (2, 2) self-row.
    let snap = Snapshot::new(
        vec![
            t(1, None, TenantStatus::Active, false),
            t(2, Some(1), TenantStatus::Active, false),
        ],
        vec![
            c(1, 1, 0, TenantStatus::Active),
            c(1, 2, 0, TenantStatus::Active),
        ],
    );
    let plan = compute_repair_plan(&snap, &run_classifiers(&snap));
    // (2, 2) self-row is the only missing row.
    assert_eq!(plan.inserts.len(), 1);
    let ins = &plan.inserts[0];
    assert_eq!(ins.ancestor_id, Uuid::from_u128(2));
    assert_eq!(ins.descendant_id, Uuid::from_u128(2));
    assert_eq!(ins.barrier, 0);
    assert_eq!(ins.descendant_status, TenantStatus::Active);
    assert_eq!(
        count_for(
            &plan.repaired_per_category,
            IntegrityCategory::MissingClosureSelfRow
        ),
        1
    );
    assert_eq!(plan.deletes, Vec::<(Uuid, Uuid)>::new());
}

#[test]
fn missing_self_row_repair_idempotent_on_clean_input() {
    let snap = clean_chain_1_2_3(TenantStatus::Active);
    let plan = compute_repair_plan(&snap, &run_classifiers(&snap));
    assert_eq!(plan.total_ops(), 0);
}

// ---------------------------------------------------------------------
// B.5 — Closure coverage gap: parent + child + only self-rows, no
// (parent, child) row → INSERT with derived barrier.
// ---------------------------------------------------------------------

#[test]
fn closure_coverage_gap_insert_is_planned() {
    // 1 -> 2 chain with only self-rows. Both (1, 2) edge missing.
    let snap = Snapshot::new(
        vec![
            t(1, None, TenantStatus::Active, false),
            t(2, Some(1), TenantStatus::Active, false),
        ],
        vec![
            c(1, 1, 0, TenantStatus::Active),
            c(2, 2, 0, TenantStatus::Active),
        ],
    );
    let plan = compute_repair_plan(&snap, &run_classifiers(&snap));
    assert_eq!(plan.inserts.len(), 1);
    let ins = &plan.inserts[0];
    assert_eq!(ins.ancestor_id, Uuid::from_u128(1));
    assert_eq!(ins.descendant_id, Uuid::from_u128(2));
    assert_eq!(ins.barrier, 0);
    assert_eq!(
        count_for(
            &plan.repaired_per_category,
            IntegrityCategory::ClosureCoverageGap
        ),
        1
    );
}

#[test]
fn closure_coverage_gap_with_self_managed_descendant_uses_barrier_one() {
    // 1 -> 2; tenant 2 is self_managed → strict (1, 2) row barrier=1.
    let snap = Snapshot::new(
        vec![
            t(1, None, TenantStatus::Active, false),
            t(2, Some(1), TenantStatus::Active, true),
        ],
        vec![
            c(1, 1, 0, TenantStatus::Active),
            c(2, 2, 0, TenantStatus::Active),
        ],
    );
    let plan = compute_repair_plan(&snap, &run_classifiers(&snap));
    assert_eq!(plan.inserts.len(), 1);
    assert_eq!(
        plan.inserts[0].barrier, 1,
        "self_managed descendant flips barrier"
    );
}

#[test]
fn closure_coverage_gap_above_self_managed_node_inherits_barrier() {
    // 1 -> 2 -> 3 chain; tenant 2 is self_managed. Strict edges:
    //   (2, 3): self_managed walk = {3}, barrier = 0 (3 not self_managed,
    //   2 not yet on the strict (2,3] path because (2,3] excludes A).
    //   Wait: walk semantics — `(A, D]` includes D, excludes A. So
    //   for (2, 3) row: path is [3]. 3 is not self_managed → barrier=0.
    //   For (1, 3) row: path is [2, 3]. 2 is self_managed → barrier=1.
    //   For (1, 2) row: path is [2]. 2 is self_managed → barrier=1.
    let snap = Snapshot::new(
        vec![
            t(1, None, TenantStatus::Active, false),
            t(2, Some(1), TenantStatus::Active, true),
            t(3, Some(2), TenantStatus::Active, false),
        ],
        vec![
            c(1, 1, 0, TenantStatus::Active),
            c(2, 2, 0, TenantStatus::Active),
            c(3, 3, 0, TenantStatus::Active),
        ],
    );
    let plan = compute_repair_plan(&snap, &run_classifiers(&snap));
    let mut by_pair: std::collections::HashMap<(Uuid, Uuid), i16> =
        std::collections::HashMap::new();
    for ins in &plan.inserts {
        by_pair.insert((ins.ancestor_id, ins.descendant_id), ins.barrier);
    }
    assert_eq!(by_pair[&(Uuid::from_u128(1), Uuid::from_u128(2))], 1);
    assert_eq!(by_pair[&(Uuid::from_u128(2), Uuid::from_u128(3))], 0);
    assert_eq!(by_pair[&(Uuid::from_u128(1), Uuid::from_u128(3))], 1);
}

// ---------------------------------------------------------------------
// B.6 — Stale descendant_status → bulk UPDATE per tenant.
// ---------------------------------------------------------------------

#[test]
fn stale_descendant_status_update_is_planned_per_tenant() {
    // Tenant 2 is Active in tenants but every closure row carrying
    // descendant_id = 2 still says Suspended.
    let snap = Snapshot::new(
        vec![
            t(1, None, TenantStatus::Active, false),
            t(2, Some(1), TenantStatus::Active, false),
        ],
        vec![
            c(1, 1, 0, TenantStatus::Active),
            c(2, 2, 0, TenantStatus::Suspended),
            c(1, 2, 0, TenantStatus::Suspended),
        ],
    );
    let plan = compute_repair_plan(&snap, &run_classifiers(&snap));
    // One bulk update per tenant.
    assert_eq!(plan.status_updates.len(), 1);
    let upd = &plan.status_updates[0];
    assert_eq!(upd.descendant_id, Uuid::from_u128(2));
    assert_eq!(upd.new_status, TenantStatus::Active);
    assert_eq!(
        count_for(
            &plan.repaired_per_category,
            IntegrityCategory::DescendantStatusDivergence
        ),
        1
    );
}

// ---------------------------------------------------------------------
// B.7 — Barrier divergence → UPDATE.
// ---------------------------------------------------------------------

#[test]
fn barrier_divergence_update_is_planned() {
    // Tenant 2 is self_managed; (1, 2) row should have barrier=1 but
    // is stored with barrier=0 (drift).
    let snap = Snapshot::new(
        vec![
            t(1, None, TenantStatus::Active, false),
            t(2, Some(1), TenantStatus::Active, true),
        ],
        vec![
            c(1, 1, 0, TenantStatus::Active),
            c(2, 2, 0, TenantStatus::Active),
            c(1, 2, 0, TenantStatus::Active), // wrong: should be 1
        ],
    );
    let plan = compute_repair_plan(&snap, &run_classifiers(&snap));
    assert_eq!(plan.barrier_updates.len(), 1);
    let upd = &plan.barrier_updates[0];
    assert_eq!(upd.ancestor_id, Uuid::from_u128(1));
    assert_eq!(upd.descendant_id, Uuid::from_u128(2));
    assert_eq!(upd.new_barrier, 1);
    assert_eq!(
        count_for(
            &plan.repaired_per_category,
            IntegrityCategory::BarrierColumnDivergence
        ),
        1
    );
}

// ---------------------------------------------------------------------
// B.8 — Stale closure row → DELETE.
// ---------------------------------------------------------------------

#[test]
fn stale_closure_row_with_missing_endpoint_is_deleted() {
    // (1, 99) — descendant 99 not in tenants. Stale → DELETE.
    let snap = Snapshot::new(
        vec![t(1, None, TenantStatus::Active, false)],
        vec![
            c(1, 1, 0, TenantStatus::Active),
            c(1, 99, 0, TenantStatus::Active),
        ],
    );
    let plan = compute_repair_plan(&snap, &run_classifiers(&snap));
    assert_eq!(
        plan.deletes,
        vec![(Uuid::from_u128(1), Uuid::from_u128(99))]
    );
    assert_eq!(
        count_for(
            &plan.repaired_per_category,
            IntegrityCategory::StaleClosureRow
        ),
        1
    );
}

#[test]
fn stale_closure_row_ancestry_not_in_walk_is_deleted() {
    // 1 -> 2; row (3, 2) where 3 exists but is unrelated to 2's parent
    // walk. Both endpoints present, no derivable ancestry → DELETE.
    let snap = Snapshot::new(
        vec![
            t(1, None, TenantStatus::Active, false),
            t(2, Some(1), TenantStatus::Active, false),
            t(3, None, TenantStatus::Active, false), // separate root — root anomaly handled in deferred
        ],
        vec![
            c(1, 1, 0, TenantStatus::Active),
            c(2, 2, 0, TenantStatus::Active),
            c(3, 3, 0, TenantStatus::Active),
            c(1, 2, 0, TenantStatus::Active),
            c(3, 2, 0, TenantStatus::Active), // bogus ancestry
        ],
    );
    let plan = compute_repair_plan(&snap, &run_classifiers(&snap));
    assert!(
        plan.deletes
            .contains(&(Uuid::from_u128(3), Uuid::from_u128(2))),
        "bogus (3,2) row must be deleted, got {:?}",
        plan.deletes
    );
    assert_eq!(
        count_for(
            &plan.repaired_per_category,
            IntegrityCategory::StaleClosureRow
        ),
        plan.deletes.len()
    );
}

// ---------------------------------------------------------------------
// B.9 — Repair MUST NOT touch tenants. The non-derivable categories
// surface in the deferred bucket; the planner emits zero closure ops
// for them. This locks the "closure-only" invariant in code.
// ---------------------------------------------------------------------

#[test]
fn non_derivable_violations_only_populate_deferred_bucket() {
    // Three orphans: tenants 2, 3, 4 all reference missing parent 99.
    // Closure has only their self-rows (consistent with parent being
    // missing — no strict ancestor edges to derive).
    let snap = Snapshot::new(
        vec![
            t(2, Some(99), TenantStatus::Active, false),
            t(3, Some(99), TenantStatus::Active, false),
            t(4, Some(99), TenantStatus::Active, false),
        ],
        vec![
            c(2, 2, 0, TenantStatus::Active),
            c(3, 3, 0, TenantStatus::Active),
            c(4, 4, 0, TenantStatus::Active),
        ],
    );
    let plan = compute_repair_plan(&snap, &run_classifiers(&snap));
    // No closure ops — orphans are skipped.
    assert_eq!(
        plan.total_ops(),
        0,
        "orphan tenants must not yield repair ops"
    );
    // Deferred bucket carries the three orphan violations.
    assert_eq!(
        count_for(
            &plan.deferred_per_category,
            IntegrityCategory::OrphanedChild
        ),
        3
    );
}

#[test]
fn cycle_members_are_skipped_by_planner() {
    // 1 -> 2 -> 1 cycle plus a healthy tenant 3 -> nothing (root).
    // Cycle members 1 and 2 must NOT receive any repair ops; tenant 3
    // is healthy and consistent.
    let snap = Snapshot::new(
        vec![
            t(1, Some(2), TenantStatus::Active, false),
            t(2, Some(1), TenantStatus::Active, false),
            t(3, None, TenantStatus::Active, false),
        ],
        vec![
            // Tenant 3 has its consistent self-row.
            c(3, 3, 0, TenantStatus::Active),
            // Tenants 1 and 2: deliberately broken closure (no
            // self-rows); the planner must NOT auto-create them
            // because cycle membership disqualifies repair.
        ],
    );
    let plan = compute_repair_plan(&snap, &run_classifiers(&snap));
    let cycle_members_touched: Vec<_> = plan
        .inserts
        .iter()
        .filter(|ins| {
            ins.descendant_id == Uuid::from_u128(1) || ins.descendant_id == Uuid::from_u128(2)
        })
        .collect();
    assert!(
        cycle_members_touched.is_empty(),
        "cycle members must not be repaired: {cycle_members_touched:?}"
    );
    assert_eq!(
        count_for(&plan.deferred_per_category, IntegrityCategory::Cycle),
        2,
        "both cycle members must show up in the deferred bucket"
    );
}

#[test]
fn root_count_anomaly_does_not_block_per_tenant_repair() {
    // Two roots (1 and 2) — RootCountAnomaly fires but each tenant's
    // own closure is internally consistent. Per-tenant repair MUST
    // still run; root_count is reported as deferred only.
    let snap = Snapshot::new(
        vec![
            t(1, None, TenantStatus::Active, false),
            t(2, None, TenantStatus::Active, false),
        ],
        vec![
            // Tenant 1 missing self-row — repairable.
            c(2, 2, 0, TenantStatus::Active),
        ],
    );
    let plan = compute_repair_plan(&snap, &run_classifiers(&snap));
    assert_eq!(plan.inserts.len(), 1);
    assert_eq!(plan.inserts[0].ancestor_id, Uuid::from_u128(1));
    assert_eq!(plan.inserts[0].descendant_id, Uuid::from_u128(1));
    assert_eq!(
        count_for(
            &plan.deferred_per_category,
            IntegrityCategory::RootCountAnomaly
        ),
        1
    );
}

// ---------------------------------------------------------------------
// C.11 — Idempotency: running repair on the post-repair state yields
// an empty plan.
// ---------------------------------------------------------------------

#[test]
fn repair_then_repair_is_noop() {
    // Start with missing self-row + barrier divergence + status drift.
    let snap_before = Snapshot::new(
        vec![
            t(1, None, TenantStatus::Active, false),
            t(2, Some(1), TenantStatus::Active, true),
        ],
        vec![
            c(1, 1, 0, TenantStatus::Active),
            // (2,2) self-row missing
            c(1, 2, 0, TenantStatus::Suspended), // wrong barrier (should be 1) AND wrong status
        ],
    );
    let plan = compute_repair_plan(&snap_before, &run_classifiers(&snap_before));
    assert!(plan.total_ops() > 0, "first plan must have work");

    // Apply the plan to the snapshot (in-memory) and re-run.
    let mut tenants = snap_before.tenants().to_vec();
    let mut closure: Vec<ClosureSnap> = snap_before.closure().to_vec();
    // DELETE
    closure.retain(|row| !plan.deletes.contains(&(row.ancestor_id, row.descendant_id)));
    // INSERT
    for ins in &plan.inserts {
        closure.push(ClosureSnap {
            ancestor_id: ins.ancestor_id,
            descendant_id: ins.descendant_id,
            barrier: ins.barrier,
            descendant_status: ins.descendant_status,
        });
    }
    // UPDATE barrier
    for upd in &plan.barrier_updates {
        for row in &mut closure {
            if row.ancestor_id == upd.ancestor_id && row.descendant_id == upd.descendant_id {
                row.barrier = upd.new_barrier;
            }
        }
    }
    // UPDATE status (bulk per descendant)
    for upd in &plan.status_updates {
        for row in &mut closure {
            if row.descendant_id == upd.descendant_id {
                row.descendant_status = upd.new_status;
            }
        }
    }
    tenants.sort_by_key(|t| t.id);

    let snap_after = Snapshot::new(tenants, closure);
    let plan_after = compute_repair_plan(&snap_after, &run_classifiers(&snap_after));
    assert_eq!(
        plan_after.total_ops(),
        0,
        "second repair must be a no-op; got {plan_after:?}"
    );
}
