use super::*;
use proptest::collection::vec;
use proptest::prelude::*;
use time::OffsetDateTime;

fn t(id: u128, parent: Option<u128>, depth: u32, self_managed: bool) -> TenantModel {
    let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("epoch");
    TenantModel {
        id: Uuid::from_u128(id),
        parent_id: parent.map(Uuid::from_u128),
        name: format!("t{id:x}"),
        status: TenantStatus::Active,
        self_managed,
        tenant_type_uuid: Uuid::from_u128(0xAA),
        depth,
        created_at: now,
        updated_at: now,
        deleted_at: None,
    }
}

#[test]
fn activation_for_root_emits_only_self_row() {
    let rows = build_activation_rows(Uuid::from_u128(0x1), TenantStatus::Active, false, &[]);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].is_self_row());
    assert_eq!(rows[0].barrier, 0);
    assert_eq!(rows[0].descendant_status, 1);
}

#[test]
fn activation_for_child_emits_self_plus_all_ancestors() {
    let parent = t(0x10, Some(0x20), 1, false);
    let gp = t(0x20, None, 0, false);
    let rows = build_activation_rows(
        Uuid::from_u128(0x01),
        TenantStatus::Active,
        false,
        &[parent.clone(), gp.clone()],
    );
    assert_eq!(rows.len(), 3, "self + 2 ancestors");
    assert!(rows[0].is_self_row());
    assert_eq!(rows[1].ancestor_id, parent.id);
    assert_eq!(rows[2].ancestor_id, gp.id);
    // Without any self-managed flag on the path, barriers are all 0.
    assert!(rows.iter().all(|r| r.barrier == 0));
}

#[test]
fn barrier_materialization_when_parent_is_self_managed() {
    // Path: gp --- parent(self_managed=true) --- child(not sm)
    // For ancestor `gp`, the strict (gp, child] path is {parent, child}.
    // `parent.self_managed = true` → barrier(gp → child) = 1.
    // For ancestor `parent`, the strict (parent, child] path is {child}.
    // `child.self_managed = false` → barrier(parent → child) = 0.
    let parent = t(0x10, Some(0x20), 1, true);
    let gp = t(0x20, None, 0, false);
    let rows = build_activation_rows(
        Uuid::from_u128(0x01),
        TenantStatus::Active,
        false,
        &[parent, gp],
    );
    // rows[0] = self; rows[1] = parent; rows[2] = gp
    assert_eq!(
        rows[1].barrier, 0,
        "parent -> child: only child on path, not sm"
    );
    assert_eq!(
        rows[2].barrier, 1,
        "gp -> child: parent on path is self_managed"
    );
}

#[test]
fn barrier_materialization_when_child_is_self_managed() {
    // Path: gp --- parent(not sm) --- child(self_managed=true)
    // The child is on every strict (A, child] path; so barrier is 1
    // for EVERY strict ancestor row.
    let parent = t(0x10, Some(0x20), 1, false);
    let gp = t(0x20, None, 0, false);
    let rows = build_activation_rows(
        Uuid::from_u128(0x01),
        TenantStatus::Active,
        true,
        &[parent, gp],
    );
    assert_eq!(rows[0].barrier, 0, "self-row barrier always 0");
    assert_eq!(rows[1].barrier, 1);
    assert_eq!(rows[2].barrier, 1);
}

#[test]
fn barrier_when_no_self_managed_anywhere_is_zero_everywhere() {
    let parent = t(0x10, Some(0x20), 1, false);
    let gp = t(0x20, None, 0, false);
    let rows = build_activation_rows(
        Uuid::from_u128(0x01),
        TenantStatus::Active,
        false,
        &[parent, gp],
    );
    assert!(rows.iter().all(|r| r.barrier == 0));
}

#[test]
fn descendant_status_is_copied_from_child_status() {
    let rows = build_activation_rows(Uuid::from_u128(0x01), TenantStatus::Active, false, &[]);
    assert_eq!(rows[0].descendant_status, 1);
}

#[test]
fn self_row_detection() {
    let id = Uuid::from_u128(0x1);
    assert!(
        ClosureRow {
            ancestor_id: id,
            descendant_id: id,
            barrier: 0,
            descendant_status: 1,
        }
        .is_self_row()
    );
    assert!(
        !ClosureRow {
            ancestor_id: id,
            descendant_id: Uuid::from_u128(0x2),
            barrier: 0,
            descendant_status: 1,
        }
        .is_self_row()
    );
}

// Closure-row invariant property tests (AC#18).
//
// Generates 1,000 randomized tenant hierarchies bounded to depth ≤ 8,
// fan-out ≤ 5, and ≤ 50 total tenants, then calls
// `build_activation_rows` for every tenant in the tree and asserts
// the closure-row invariants documented at the top of `closure.rs`:
//
// 1. Self-row invariant — every tenant has a `(id, id)` row with
//    `barrier = 0` and `descendant_status = 1` (Active).
// 2. Coverage invariant — exactly one row per strict ancestor.
// 3. Barrier materialization — `barrier = 1` iff some tenant on the
//    strict `(ancestor, descendant]` path has `self_managed = true`.
// 4. No row references a non-existent tenant.
// 5. The total row count for a tenant equals
//    `1 + ancestor_chain.len()`.

/// Bounds for the generated hierarchies. Kept tight on purpose:
/// 50 tenants × 1000 cases × depth-8 walks = ~ low-six-figure
/// `build_activation_rows` calls per `cargo test` run, which still
/// completes well under a second locally.
const MAX_DEPTH: u32 = 8;
const MAX_FAN_OUT: usize = 5;
const MAX_TENANTS: usize = 50;

/// Generator-friendly tenant description. `parent` is an index into
/// the flat spec vector or `None` for the root (index 0).
#[allow(unknown_lints, de0309_must_have_domain_model)]
#[derive(Debug, Clone)]
struct TenantSpec {
    parent: Option<usize>,
    self_managed: bool,
}

/// Strategy for a single tenant's `self_managed` flag. Skewed neither
/// way so the barrier materialization branch sees both 0 and 1
/// outcomes within a single 1,000-case run.
fn self_managed_strategy() -> impl Strategy<Value = bool> {
    any::<bool>()
}

/// Strategy that builds a tree by choosing each non-root tenant's
/// parent uniformly from the prefix of already-generated tenants
/// whose subtree depth is still below `MAX_DEPTH` and whose fan-out
/// budget has not been exhausted.
fn hierarchy_strategy() -> impl Strategy<Value = Vec<TenantSpec>> {
    // Range of tenant counts: must include the root, and capped at
    // MAX_TENANTS.
    (1usize..=MAX_TENANTS).prop_flat_map(|n| {
        // Generate `n` self_managed flags AND, for each non-root
        // tenant `i ∈ [1, n)`, a uniform "parent picker" usize that
        // we resolve against the eligible-parents set inside
        // `prop_map`. Storing the raw u32 lets proptest shrink it
        // independently of the tree shape.
        let flags = vec(self_managed_strategy(), n);
        let parent_pickers = vec(any::<u32>(), n.saturating_sub(1));
        (Just(n), flags, parent_pickers).prop_map(|(n, flags, pickers)| {
            let mut specs: Vec<TenantSpec> = Vec::with_capacity(n);
            let mut depth: Vec<u32> = Vec::with_capacity(n);
            let mut child_count: Vec<usize> = Vec::with_capacity(n);
            // Root.
            specs.push(TenantSpec {
                parent: None,
                self_managed: flags[0],
            });
            depth.push(0);
            child_count.push(0);
            for (i, picker) in pickers.into_iter().enumerate() {
                let candidate_idx = i + 1;
                // Eligible parents: any existing tenant with
                // depth < MAX_DEPTH and fewer than MAX_FAN_OUT
                // children so far.
                let eligible: Vec<usize> = (0..candidate_idx)
                    .filter(|p| depth[*p] < MAX_DEPTH && child_count[*p] < MAX_FAN_OUT)
                    .collect();
                if eligible.is_empty() {
                    // Tree is saturated; truncate the hierarchy at
                    // the current size.
                    break;
                }
                let parent = eligible[(picker as usize) % eligible.len()];
                child_count[parent] += 1;
                let new_depth = depth[parent] + 1;
                depth.push(new_depth);
                child_count.push(0);
                specs.push(TenantSpec {
                    parent: Some(parent),
                    self_managed: flags[candidate_idx],
                });
            }
            specs
        })
    })
}

fn make_model(idx: usize, spec: &TenantSpec, parent_uuid: Option<Uuid>, depth: u32) -> TenantModel {
    let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("epoch");
    TenantModel {
        id: Uuid::from_u128((idx as u128) + 1),
        parent_id: parent_uuid,
        name: format!("t{idx}"),
        status: TenantStatus::Active,
        self_managed: spec.self_managed,
        tenant_type_uuid: Uuid::from_u128(0xAA),
        depth,
        created_at: now,
        updated_at: now,
        deleted_at: None,
    }
}

/// Build all `TenantModel`s for a flat spec vector, returning a
/// parallel `Vec<TenantModel>` indexed by the same spec indices.
fn materialize(specs: &[TenantSpec]) -> Vec<TenantModel> {
    let mut models: Vec<TenantModel> = Vec::with_capacity(specs.len());
    let mut depths: Vec<u32> = Vec::with_capacity(specs.len());
    for (idx, spec) in specs.iter().enumerate() {
        let (parent_uuid, depth) = match spec.parent {
            None => (None, 0),
            Some(p) => (Some(models[p].id), depths[p] + 1),
        };
        depths.push(depth);
        models.push(make_model(idx, spec, parent_uuid, depth));
    }
    models
}

/// Walk the strict ancestor chain (parent first, then grandparent,
/// ..., root) for `idx`, returning a slice of `TenantModel`
/// references suitable for [`build_activation_rows`].
fn ancestor_chain(specs: &[TenantSpec], models: &[TenantModel], idx: usize) -> Vec<TenantModel> {
    let mut out = Vec::new();
    let mut cursor = specs[idx].parent;
    while let Some(p) = cursor {
        out.push(models[p].clone());
        cursor = specs[p].parent;
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 1_000,
        // Property tests are deterministic given the seed; disable
        // the persistence sink so cargo test runs the same shape on
        // every invocation without writing to .proptest-regressions.
        failure_persistence: None,
        ..ProptestConfig::default()
    })]
    #[test]
    fn closure_invariants_hold_for_random_hierarchies(specs in hierarchy_strategy()) {
        let models = materialize(&specs);
        let ids: std::collections::HashSet<Uuid> = models.iter().map(|m| m.id).collect();
        for (idx, model) in models.iter().enumerate() {
            let chain = ancestor_chain(&specs, &models, idx);
            let rows = build_activation_rows(
                model.id,
                TenantStatus::Active,
                model.self_managed,
                &chain,
            );
            // Invariant 5: row count = 1 + ancestor-chain length.
            prop_assert_eq!(rows.len(), 1 + chain.len(), "row count = self + ancestors");
            // Invariant 1: every tenant has a self-row.
            let self_row = rows
                .iter()
                .find(|r| r.is_self_row())
                .expect("self-row exists");
            prop_assert_eq!(self_row.ancestor_id, model.id);
            prop_assert_eq!(self_row.descendant_id, model.id);
            prop_assert_eq!(self_row.barrier, 0i16, "self-row barrier always 0");
            prop_assert_eq!(self_row.descendant_status, 1i16, "Active = 1");
            // Invariant 2: every strict ancestor appears exactly once.
            for ancestor in &chain {
                let count = rows
                    .iter()
                    .filter(|r| r.ancestor_id == ancestor.id && r.descendant_id == model.id)
                    .count();
                prop_assert_eq!(count, 1, "exactly one row per strict ancestor");
            }
            // Invariant 3: barrier materialization rule.
            for (i, ancestor) in chain.iter().enumerate() {
                // Strict (ancestor, child] path = closer ancestors + child.
                let closer = &chain[..i];
                let any_self_managed = model.self_managed
                    || closer.iter().any(|t| t.self_managed);
                let row = rows
                    .iter()
                    .find(|r| r.ancestor_id == ancestor.id && r.descendant_id == model.id)
                    .expect("ancestor row");
                prop_assert_eq!(
                    row.barrier,
                    i16::from(any_self_managed),
                    "barrier = 1 iff any tenant on the strict path is self-managed",
                );
            }
            // Invariant 4: no row references an unknown tenant.
            for row in &rows {
                prop_assert!(ids.contains(&row.ancestor_id));
                prop_assert!(ids.contains(&row.descendant_id));
            }
        }
    }
}
