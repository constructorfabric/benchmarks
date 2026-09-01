//! Closure-table maintenance helpers.
//!
//! Pure functions that build the set of `tenant_closure` rows to insert
//! at **activation** time (saga step 3 of `algo-create-tenant-saga`). No
//! SQL here — the repository implementation (`infra/storage/repo_impl.rs`
//! in a later phase) is responsible for writing the rows in the same
//! transaction as the owning `tenants` update.
//!
//! The invariants these helpers preserve (DESIGN §3.1, FEATURE §3):
//!
//! 1. **Self-row invariant** — every SDK-visible tenant has a `(id, id)`
//!    row with `barrier = 0` and `descendant_status = tenants.status`.
//! 2. **Coverage invariant** — exactly one row per strict ancestor along
//!    the `parent_id` chain is emitted in addition to the self-row.
//! 3. **Barrier materialization invariant** — `barrier = 1` on `(A, D)`
//!    iff some tenant on the strict `A → D` path (excluding `A`, including
//!    `D`) has `self_managed = true`; else `0`. Self-rows always `0`.
//! 4. **Status denormalization invariant** — `descendant_status` is copied
//!    from `tenants.status` at the time of write.
//! 5. **Provisioning exclusion invariant** — these helpers only emit rows
//!    for SDK-visible statuses. Passing [`TenantStatus::Provisioning`] as
//!    the status to [`build_activation_rows`] is a contract violation
//!    guarded by a release-mode `assert!` (the bug surfaces at the
//!    panic site, not three audit ticks later).

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::tenant::model::{TenantModel, TenantStatus};

/// Plain-Rust representation of a `tenant_closure` row.
///
/// The repository adapter is responsible for converting this into the
/// `SeaORM` `ActiveModel` before INSERT.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosureRow {
    pub ancestor_id: Uuid,
    pub descendant_id: Uuid,
    /// `0` for the self-row; `0` or `1` for strict-ancestor rows per the
    /// barrier materialization rule.
    pub barrier: i16,
    /// One of `{1=Active, 2=Suspended, 3=Deleted}`. Never `0=Provisioning`
    /// per the provisioning-exclusion invariant.
    pub descendant_status: i16,
}

impl ClosureRow {
    #[must_use]
    pub fn is_self_row(&self) -> bool {
        self.ancestor_id == self.descendant_id
    }
}

/// Build the full set of closure rows to insert at activation time.
///
/// # Arguments
///
/// * `child_id` — the tenant being activated (saga step 3 descendant).
/// * `child_status` — must be a SDK-visible status; Phase 1 only calls
///   this with [`TenantStatus::Active`] (the `provisioning → active`
///   transition).
/// * `child_self_managed` — the child's own `self_managed` flag. The
///   child is always on every strict `(ancestor, child]` path so its
///   flag contributes to the barrier bit for every strict-ancestor row.
/// * `ancestor_chain` — strict ancestors ordered **nearest parent first**,
///   i.e. `[parent, grandparent, ..., root]`. Pass an **empty** slice
///   when activating the root tenant (root has no strict ancestors; only
///   the self-row is emitted).
///
/// The output contains exactly `1 + ancestor_chain.len()` rows: the
/// self-row plus one per strict ancestor. Order within the returned
/// vector is `[self, parent_row, grandparent_row, ..., root_row]`.
///
/// # Panics
///
/// Release-mode `assert!`s on every contract violation — provisioning
/// status, `child_id` appearing in `ancestor_chain`, duplicate IDs in
/// the chain, and any chain not in strict nearest-parent-first order
/// (each `chain[i].parent_id` must equal `chain[i+1].id`, with the
/// last entry rooted via `parent_id = None`). All hard asserts are
/// intentional: writing closure rows from a malformed chain would
/// corrupt `descendant_status` / ancestor pointers and the integrity
/// checker would only surface the damage retroactively.
// @cpt-begin:cpt-cf-account-management-algo-tenant-hierarchy-management-closure-maintenance:p1:inst-algo-closmnt-activation-rows
// @cpt-begin:cpt-cf-account-management-dod-tenant-hierarchy-management-closure-invariants:p1:inst-dod-closure-invariant-rows
#[must_use]
pub fn build_activation_rows(
    child_id: Uuid,
    child_status: TenantStatus,
    child_self_managed: bool,
    ancestor_chain: &[TenantModel],
) -> Vec<ClosureRow> {
    // Hard assertion (release-build live): violating the
    // provisioning-exclusion invariant would write `descendant_status`
    // = 0 into the closure table, which the integrity checker would
    // surface as `descendant_status_mismatch` after the fact. Fail
    // fast at the call site instead so the bug is visible in the
    // panic site, not three audit ticks later.
    assert!(
        child_status.is_sdk_visible(),
        "closure rows must not be written for provisioning tenants"
    );
    // A malformed `ancestor_chain` would write corrupt closure rows
    // (wrong ancestor pointers, missing self-row coverage). The
    // integrity classifier would only surface the bug after the bad
    // rows are already committed — fail fast at the call site instead.
    assert!(
        !ancestor_chain.iter().any(|t| t.id == child_id),
        "child_id must not appear in ancestor_chain"
    );
    assert!(
        ancestor_chain
            .iter()
            .map(|t| t.id)
            .collect::<std::collections::HashSet<_>>()
            .len()
            == ancestor_chain.len(),
        "ancestor_chain must contain unique IDs"
    );
    // Adjacency check — verifies the chain is the actual `parent_id`
    // walk in nearest-first order. For an entry at index `i`, its
    // `parent_id` MUST equal the next entry's `id` (i.e. `chain[i]`'s
    // parent is `chain[i+1]`). The last entry must be the root
    // (`parent_id IS NULL`). Catches a corrupt chain — e.g. a fallback
    // walk hitting a `parent_id` cycle that the cycle-cap miss-counted,
    // or a closure-table snapshot returning ancestors out of order —
    // before any row is written.
    let depth = ancestor_chain.len();
    for (i, ancestor) in ancestor_chain.iter().enumerate() {
        let expected_parent = if i + 1 < depth {
            Some(ancestor_chain[i + 1].id)
        } else {
            None
        };
        assert_eq!(
            ancestor.parent_id,
            expected_parent,
            "ancestor_chain not in nearest-first order: chain[{i}].parent_id \
             should be chain[{}].id (or None at root)",
            i + 1
        );
    }
    let status = child_status.as_smallint();
    let mut out = Vec::with_capacity(1 + ancestor_chain.len());

    // 1. Self-row — barrier is always 0 per the self-row invariant.
    out.push(ClosureRow {
        ancestor_id: child_id,
        descendant_id: child_id,
        barrier: 0,
        descendant_status: status,
    });

    // 2. Strict-ancestor rows.
    //
    // For an ancestor at index `i` of the nearest-first chain, the
    // strict path `(ancestor, child]` consists of:
    //   - the child itself, plus
    //   - every ancestor closer to the child than `ancestor` — i.e.
    //     `ancestor_chain[..i]`.
    //
    // `barrier = 1` iff any tenant on that set has `self_managed = true`.
    for (idx, ancestor) in ancestor_chain.iter().enumerate() {
        let closer = &ancestor_chain[..idx];
        let barrier_set = child_self_managed || closer.iter().any(|t| t.self_managed);
        out.push(ClosureRow {
            ancestor_id: ancestor.id,
            descendant_id: child_id,
            barrier: i16::from(barrier_set),
            descendant_status: status,
        });
    }

    out
}
// @cpt-end:cpt-cf-account-management-dod-tenant-hierarchy-management-closure-invariants:p1:inst-dod-closure-invariant-rows
// @cpt-end:cpt-cf-account-management-algo-tenant-hierarchy-management-closure-maintenance:p1:inst-algo-closmnt-activation-rows

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[allow(clippy::expect_used, clippy::unwrap_used, reason = "test helpers")]
#[path = "closure_tests.rs"]
mod closure_tests;
