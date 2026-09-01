//! Service-level integration tests for the **direct-child carve-out
//! across self-managed barriers** on `TenantService::list_children`
//! and `TenantService::get_tenant`.
//!
//! See [`account_management::domain::tenant::service::scope_util`] for
//! the invariant table this suite encodes. Topology is hand-wired (not
//! built through `create_tenant`) so the closure `barrier` column is
//! pinned by the seeder rather than the runtime planner — the suite
//! is asserting the **read** carve-out independently of any
//! conversion-time barrier-materialisation logic.
//!
//! Topology shared across every test:
//!
//! ```text
//! root      (caller's subject_tenant; managed)
//! ├─ x      (managed, depth=1)
//! │  ├─ y   (self-managed, depth=2)            ← barrier child of x
//! │  │  └─ yc (managed, depth=3)                 ← grandchild via y
//! │  └─ xc  (managed, depth=2)                   ← regular grandchild via x
//! └─ s      (self-managed, depth=1)             ← barrier child of root
//!    └─ sc  (managed, depth=2)                   ← grandchild via s
//! ```
//!
//! Closure rules (`barrier = 1` iff any tenant on the strict
//! `(ancestor, descendant]` path has `self_managed = true`):
//!
//! | (A, D)        | barrier |
//! |---------------|---------|
//! | (root, root)  | 0       |
//! | (root, x)     | 0       |
//! | (root, xc)    | 0       |
//! | (root, s)     | 1       |
//! | (root, sc)    | 1       |
//! | (root, y)     | 1       |
//! | (root, yc)    | 1       |
//! | (x, x)        | 0       |
//! | (x, xc)       | 0       |
//! | (x, y)        | 1       |
//! | (x, yc)       | 1       |
//! | (s, s)        | 0       |
//! | (s, sc)       | 0       |
//! | (y, y)        | 0       |
//! | (y, yc)       | 0       |
//!
//! The PDP-side mock (`mock_enforcer` in `tests/common/mod.rs`) emits
//! `InTenantSubtree(RESOURCE_ID, root=caller.subject_tenant)` with the
//! production-default `BarrierMode::Respect`, so the caller's
//! Respect-visible set is `{root, x, xc}` — exactly the managed
//! subtree of root.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::doc_markdown
)]

mod common;

use account_management::domain::error::DomainError;
use toolkit_odata::ODataQuery;
use uuid::Uuid;

use common::*;

/// Topology IDs used by every test in this file.
struct Topology {
    root: Uuid,
    x: Uuid,
    xc: Uuid,
    s: Uuid,
    sc: Uuid,
    y: Uuid,
    yc: Uuid,
}

impl Topology {
    fn new() -> Self {
        Self {
            root: Uuid::from_u128(0x6000_0001),
            x: Uuid::from_u128(0x6000_0002),
            xc: Uuid::from_u128(0x6000_0003),
            s: Uuid::from_u128(0x6000_0004),
            sc: Uuid::from_u128(0x6000_0005),
            y: Uuid::from_u128(0x6000_0006),
            yc: Uuid::from_u128(0x6000_0007),
        }
    }
}

/// Seed the topology described in the gear docs into a fresh harness.
#[allow(
    clippy::cognitive_complexity,
    reason = "linear, hand-wired seed of a small fixed topology; splitting it into per-subtree helpers would obscure the closure-row table the gear doc pins"
)]
async fn seed_topology(h: &Harness, t: &Topology) {
    seed_root(h, t.root).await;
    // Direct managed child `x`.
    insert_tenant(&h.provider, t.x, Some(t.root), "x", ACTIVE, false, 1)
        .await
        .expect("seed x");
    insert_closure(&h.provider, t.x, t.x, 0, ACTIVE)
        .await
        .expect("seed (x, x)");
    insert_closure(&h.provider, t.root, t.x, 0, ACTIVE)
        .await
        .expect("seed (root, x)");
    // Direct self-managed child `s` (barrier from root).
    insert_tenant(&h.provider, t.s, Some(t.root), "s", ACTIVE, true, 1)
        .await
        .expect("seed s");
    insert_closure(&h.provider, t.s, t.s, 0, ACTIVE)
        .await
        .expect("seed (s, s)");
    insert_closure(&h.provider, t.root, t.s, 1, ACTIVE)
        .await
        .expect("seed (root, s)");
    // `xc` — managed grandchild via x.
    insert_tenant(&h.provider, t.xc, Some(t.x), "xc", ACTIVE, false, 2)
        .await
        .expect("seed xc");
    insert_closure(&h.provider, t.xc, t.xc, 0, ACTIVE)
        .await
        .expect("seed (xc, xc)");
    insert_closure(&h.provider, t.x, t.xc, 0, ACTIVE)
        .await
        .expect("seed (x, xc)");
    insert_closure(&h.provider, t.root, t.xc, 0, ACTIVE)
        .await
        .expect("seed (root, xc)");
    // `sc` — descendant under self-managed `s` (barrier from root + x).
    insert_tenant(&h.provider, t.sc, Some(t.s), "sc", ACTIVE, false, 2)
        .await
        .expect("seed sc");
    insert_closure(&h.provider, t.sc, t.sc, 0, ACTIVE)
        .await
        .expect("seed (sc, sc)");
    insert_closure(&h.provider, t.s, t.sc, 0, ACTIVE)
        .await
        .expect("seed (s, sc)");
    insert_closure(&h.provider, t.root, t.sc, 1, ACTIVE)
        .await
        .expect("seed (root, sc)");
    // `y` — self-managed direct child of `x` (barrier from root + x).
    insert_tenant(&h.provider, t.y, Some(t.x), "y", ACTIVE, true, 2)
        .await
        .expect("seed y");
    insert_closure(&h.provider, t.y, t.y, 0, ACTIVE)
        .await
        .expect("seed (y, y)");
    insert_closure(&h.provider, t.x, t.y, 1, ACTIVE)
        .await
        .expect("seed (x, y)");
    insert_closure(&h.provider, t.root, t.y, 1, ACTIVE)
        .await
        .expect("seed (root, y)");
    // `yc` — managed descendant under self-managed `y` (barrier from
    // root + x).
    insert_tenant(&h.provider, t.yc, Some(t.y), "yc", ACTIVE, false, 3)
        .await
        .expect("seed yc");
    insert_closure(&h.provider, t.yc, t.yc, 0, ACTIVE)
        .await
        .expect("seed (yc, yc)");
    insert_closure(&h.provider, t.y, t.yc, 0, ACTIVE)
        .await
        .expect("seed (y, yc)");
    insert_closure(&h.provider, t.x, t.yc, 1, ACTIVE)
        .await
        .expect("seed (x, yc)");
    insert_closure(&h.provider, t.root, t.yc, 1, ACTIVE)
        .await
        .expect("seed (root, yc)");
}

// =============================================================================
// list_children — direct-child carve-out
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_children_root_surfaces_self_managed_direct_child() {
    // Caller P (= root) listing their own direct children must see
    // both the managed child `x` and the self-managed child `s`.
    // Under the pre-carve-out scope `s` would be hidden by
    // `(root, s).barrier = 1`.
    let h = setup_sqlite().await.expect("sqlite");
    let t = Topology::new();
    seed_topology(&h, &t).await;
    let services = build_services(&h);

    let page = services
        .tenant_service
        .list_children(&ctx_for(t.root), t.root, &ODataQuery::default())
        .await
        .expect("list");

    let mut ids: Vec<Uuid> = page.items.iter().map(|tt| tt.id.0).collect();
    ids.sort();
    let mut expected = vec![t.x, t.s];
    expected.sort();
    assert_eq!(
        ids, expected,
        "list_children(root) must surface BOTH the managed child x AND the self-managed direct child s"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_children_intermediate_surfaces_self_managed_direct_child() {
    // Caller P (= root) listing children of an intermediate managed
    // tenant `x` must also see `x`'s self-managed direct child `y`,
    // not just the managed `xc`. This pins the broader invariant
    // ("direct children of any Respect-visible tenant"), not the
    // narrower "direct children of caller only".
    let h = setup_sqlite().await.expect("sqlite");
    let t = Topology::new();
    seed_topology(&h, &t).await;
    let services = build_services(&h);

    let page = services
        .tenant_service
        .list_children(&ctx_for(t.root), t.x, &ODataQuery::default())
        .await
        .expect("list");

    let mut ids: Vec<Uuid> = page.items.iter().map(|tt| tt.id.0).collect();
    ids.sort();
    let mut expected = vec![t.xc, t.y];
    expected.sort();
    assert_eq!(
        ids, expected,
        "list_children(x) must surface both xc (managed) and y (self-managed direct child of x)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_children_past_barrier_collapses_to_not_found() {
    // Caller P (= root) listing children OF a past-barrier tenant
    // (`s` or `y`) must collapse to NotFound — the parent-existence
    // gate runs under the original Respect-scope and `s` / `y` are
    // not reachable. This is the load-bearing safety invariant: the
    // carve-out lifts direct-child *identity*, never opens a listing
    // surface ROOTED at a past-barrier tenant (which would leak its
    // children, the very thing the barrier protects).
    let h = setup_sqlite().await.expect("sqlite");
    let t = Topology::new();
    seed_topology(&h, &t).await;
    let services = build_services(&h);

    for past_barrier_parent in [t.s, t.y, t.sc, t.yc] {
        let err = services
            .tenant_service
            .list_children(
                &ctx_for(t.root),
                past_barrier_parent,
                &ODataQuery::default(),
            )
            .await
            .expect_err("list_children rooted at a past-barrier tenant MUST collapse to NotFound");
        assert!(
            matches!(err, DomainError::NotFound { .. }),
            "list_children({past_barrier_parent}) expected NotFound; got {err:?}"
        );
    }
}

// =============================================================================
// get_tenant — direct-child carve-out fallback
// =============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_tenant_direct_self_managed_child_of_caller_is_identity_readable() {
    // Caller P (= root) reading `S` (their own self-managed direct
    // child) must succeed via the fallback: standard Respect-scope
    // read returns None, the relaxed-scope retry surfaces the row,
    // and the parent-reachability re-check on `S.parent_id = root`
    // succeeds (root is the caller's own self-row).
    let h = setup_sqlite().await.expect("sqlite");
    let t = Topology::new();
    seed_topology(&h, &t).await;
    let services = build_services(&h);

    let s = services
        .tenant_service
        .get_tenant(&ctx_for(t.root), t.s)
        .await
        .expect("get_tenant(s) must succeed via the direct-child carve-out");
    assert_eq!(s.id.0, t.s);
    assert_eq!(s.parent_id.map(|p| p.0), Some(t.root));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_tenant_self_managed_direct_child_of_intermediate_is_identity_readable() {
    // Caller P (= root) reading `Y` (self-managed direct child of
    // managed intermediate `X`). Same fallback path as the previous
    // test; parent-reachability check resolves `Y.parent_id = X`
    // under Respect-scope, and `X` IS Respect-reachable
    // (`(root, x).barrier = 0`), so the read succeeds.
    let h = setup_sqlite().await.expect("sqlite");
    let t = Topology::new();
    seed_topology(&h, &t).await;
    let services = build_services(&h);

    let y = services
        .tenant_service
        .get_tenant(&ctx_for(t.root), t.y)
        .await
        .expect("get_tenant(y) must succeed via the carve-out (parent x is Respect-reachable)");
    assert_eq!(y.id.0, t.y);
    assert_eq!(y.parent_id.map(|p| p.0), Some(t.x));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_tenant_grandchild_through_self_managed_remains_not_found() {
    // Caller P (= root) reading `SC` / `YC` — descendants STRICTLY
    // BELOW a self-managed boundary. The fallback finds the row
    // under relaxed scope, but the parent-reachability re-check on
    // `SC.parent_id = S` (or `YC.parent_id = Y`) fails because S/Y
    // are not Respect-reachable. This is the negative half of the
    // carve-out — the bound that prevents subtree leak.
    let h = setup_sqlite().await.expect("sqlite");
    let t = Topology::new();
    seed_topology(&h, &t).await;
    let services = build_services(&h);

    for past_barrier_descendant in [t.sc, t.yc] {
        let err = services
            .tenant_service
            .get_tenant(&ctx_for(t.root), past_barrier_descendant)
            .await
            .expect_err("get_tenant on a row whose parent is past a barrier MUST return NotFound");
        assert!(
            matches!(err, DomainError::NotFound { .. }),
            "get_tenant({past_barrier_descendant}) expected NotFound; got {err:?}",
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_tenant_managed_descendant_uses_fast_path() {
    // Regression guard: a normally-reachable tenant (`xc`, managed
    // grandchild via `x`) must NOT take the fallback branch — it
    // resolves through the standard Respect-scope read. The
    // observable contract is identical (same row returned); this
    // test just pins that the carve-out is additive, not a rewrite
    // of the existing happy path.
    let h = setup_sqlite().await.expect("sqlite");
    let t = Topology::new();
    seed_topology(&h, &t).await;
    let services = build_services(&h);

    let xc = services
        .tenant_service
        .get_tenant(&ctx_for(t.root), t.xc)
        .await
        .expect("get_tenant(xc) succeeds on the fast path");
    assert_eq!(xc.id.0, t.xc);
    assert_eq!(xc.parent_id.map(|p| p.0), Some(t.x));
}
