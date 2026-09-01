//! Real-Postgres integration tests for the hierarchy integrity
//! check + repair pipelines. Mirrors a deliberately small subset of
//! the `SQLite`-backed `integrity_integration.rs` cases — only the
//! ones that EXERCISE Postgres-specific behaviour the `SQLite` path
//! cannot:
//!
//! * **FK-bypass scenarios.** Postgres enforces
//!   `fk_tenants_parent`, `fk_tenant_closure_ancestor`,
//!   `fk_tenant_closure_descendant`, plus `ck_tenants_root_depth`.
//!   Seeding orphaned children, dangling closure rows, or self-loop
//!   tenants requires a one-time `DROP CONSTRAINT` on the auxiliary
//!   DDL connection, exercised here through `common::pg::*`. The
//!   `SQLite` path does not enable `PRAGMA foreign_keys`, so the
//!   same test there silently passes whether the FKs are dropped
//!   or not.
//! * **Single-root partial index.** The Postgres
//!   `ux_tenants_single_root` index uses a constant-expression key
//!   (`((1))`) that genuinely rejects a second root row. The
//!   `SQLite` variant collapses to a `COALESCE(parent_id, '')`
//!   sentinel and has subtly different semantics; pinning the
//!   Postgres-side `RootCountAnomaly` classifier here keeps the
//!   partial-index syntax under test.
//! * **Real `SERIALIZABLE` snapshot isolation.** Postgres'
//!   `SERIALIZABLE` is true serializable-snapshot isolation —
//!   contended transactions surface real `40001` aborts. The
//!   `SQLite` path simulates `SERIALIZABLE` via `BEGIN IMMEDIATE`,
//!   which is busy-locked rather than `SI`. The repair-on-Postgres
//!   test below drives the SERIALIZABLE code path against the real
//!   isolation level so the un-contended SI path is exercised
//!   end-to-end. The retry-on-`40001` branch of
//!   `with_serializable_retry` is NOT exercised here — that needs
//!   a deterministic conflicting writer, which the v1 suite skips
//!   to keep CI-time bounded; coverage of the retry branch is
//!   tracked as a follow-up.
//!
//! Tests that do not need any of the above (e.g. `MissingClosureSelfRow`,
//! pure barrier divergence, status divergence) stay in the `SQLite`
//! file and are NOT duplicated here — running them on Postgres would
//! just cost a container per case without surfacing new behaviour.
//!
//! Gated behind `#[cfg(feature = "integration")]` so the default
//! `cargo test` run does not require Docker. Enable explicitly:
//! `cargo test -p cf-gears-account-management --features integration
//!  --test integrity_integration_pg`.

#![cfg(feature = "integration")]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use account_management::domain::tenant::TenantRepo;
use account_management::domain::tenant::integrity::IntegrityCategory;
use uuid::Uuid;

use common::pg::{bring_up_postgres, drop_constraint, drop_unique_root_index};
use common::*;

// ---------------------------------------------------------------------
// Negative control — a clean two-node tree under real Postgres FKs +
// the partial unique index produces zero violations.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_clean_two_node_tree_yields_no_violations() {
    let h = bring_up_postgres()
        .await
        .expect("postgres testcontainer (Docker daemon required)");
    seed_clean_two_node_tree(&h.provider)
        .await
        .expect("seed clean tree");
    let viols = h
        .repo
        .run_integrity_check(&allow_all())
        .await
        .expect("integrity check");
    assert!(
        viols.is_empty(),
        "clean tree must surface zero violations on Postgres: {viols:?}"
    );
}

// ---------------------------------------------------------------------
// `OrphanedChild` — child whose `parent_id` does not resolve. Requires
// dropping `fk_tenants_parent` so the row can be inserted in the first
// place.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_orphan_classifier_detects_seeded_orphan() {
    let h = bring_up_postgres().await.expect("postgres");
    drop_constraint(&h.ddl_conn, "tenants", "fk_tenants_parent")
        .await
        .expect("drop fk_tenants_parent");

    let phantom = Uuid::new_v4();
    let child = Uuid::new_v4();
    insert_tenant(
        &h.provider,
        child,
        Some(phantom),
        "orphan",
        ACTIVE,
        false,
        1,
    )
    .await
    .expect("seed orphan child");
    insert_closure(&h.provider, child, child, 0, ACTIVE)
        .await
        .expect("self-row");

    let viols = h
        .repo
        .run_integrity_check(&allow_all())
        .await
        .expect("integrity check");
    assert!(
        count_for(&viols, IntegrityCategory::OrphanedChild) >= 1,
        "expected OrphanedChild on Postgres, got {viols:?}"
    );
}

// ---------------------------------------------------------------------
// `Cycle` — a 1-cycle (`parent_id = self.id`). Requires dropping
// both `fk_tenants_parent` (so the self-reference can be inserted)
// and `ck_tenants_root_depth` (so depth>0 with no walk-resolvable
// root passes the `(parent_id IS NOT NULL AND depth > 0)` arm).
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_cycle_classifier_detects_self_loop() {
    let h = bring_up_postgres().await.expect("postgres");
    drop_constraint(&h.ddl_conn, "tenants", "fk_tenants_parent")
        .await
        .expect("drop fk_tenants_parent");
    drop_constraint(&h.ddl_conn, "tenants", "ck_tenants_root_depth")
        .await
        .expect("drop ck_tenants_root_depth");

    let a = Uuid::new_v4();
    insert_tenant(&h.provider, a, Some(a), "self-loop", ACTIVE, false, 1)
        .await
        .expect("seed self-loop");

    let viols = h
        .repo
        .run_integrity_check(&allow_all())
        .await
        .expect("integrity check");
    assert!(
        count_for(&viols, IntegrityCategory::Cycle) >= 1,
        "expected Cycle (self-loop) on Postgres, got {viols:?}"
    );
}

// ---------------------------------------------------------------------
// `RootCountAnomaly` — two roots. Requires dropping the
// `ux_tenants_single_root` partial unique index. Pins the Postgres
// `((1))` constant-expression-key form against accidental drift to
// the SQLite-style `COALESCE(parent_id, '')` form (those are NOT
// equivalent).
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_root_classifier_detects_two_roots() {
    let h = bring_up_postgres().await.expect("postgres");
    drop_unique_root_index(&h.ddl_conn)
        .await
        .expect("drop ux_tenants_single_root");

    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    insert_tenant(&h.provider, a, None, "root-a", ACTIVE, false, 0)
        .await
        .expect("seed root a");
    insert_tenant(&h.provider, b, None, "root-b", ACTIVE, false, 0)
        .await
        .expect("seed root b");
    insert_closure(&h.provider, a, a, 0, ACTIVE).await.unwrap();
    insert_closure(&h.provider, b, b, 0, ACTIVE).await.unwrap();

    let viols = h
        .repo
        .run_integrity_check(&allow_all())
        .await
        .expect("integrity check");
    assert!(
        count_for(&viols, IntegrityCategory::RootCountAnomaly) >= 1,
        "expected RootCountAnomaly on Postgres, got {viols:?}"
    );
}

// ---------------------------------------------------------------------
// `StaleClosureRow` — a closure row references a tenant id that has
// no row in `tenants`. Requires dropping the closure FKs so the
// dangling reference can be inserted.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_stale_closure_classifier_detects_dangling_descendant() {
    let h = bring_up_postgres().await.expect("postgres");
    drop_constraint(&h.ddl_conn, "tenant_closure", "fk_tenant_closure_ancestor")
        .await
        .expect("drop fk_tenant_closure_ancestor");
    drop_constraint(
        &h.ddl_conn,
        "tenant_closure",
        "fk_tenant_closure_descendant",
    )
    .await
    .expect("drop fk_tenant_closure_descendant");

    let root = Uuid::new_v4();
    insert_tenant(&h.provider, root, None, "root", ACTIVE, false, 0)
        .await
        .unwrap();
    insert_closure(&h.provider, root, root, 0, ACTIVE)
        .await
        .unwrap();
    let dangling = Uuid::new_v4();
    insert_closure(&h.provider, root, dangling, 0, ACTIVE)
        .await
        .expect("seed dangling closure row");

    let viols = h
        .repo
        .run_integrity_check(&allow_all())
        .await
        .expect("integrity check");
    assert!(
        count_for(&viols, IntegrityCategory::StaleClosureRow) >= 1,
        "expected StaleClosureRow on Postgres, got {viols:?}"
    );
}

// ---------------------------------------------------------------------
// Repair pipeline on Postgres — pins the dialect-specific
// transaction code-path under real `SERIALIZABLE` snapshot
// isolation. The SQLite path covers the same scenario but under
// `BEGIN IMMEDIATE` (busy-lock semantics, not SI).
//
// Scope: this test verifies the repair plan is computed AND applied
// correctly inside a SERIALIZABLE tx without a conflicting concurrent
// writer. It does **NOT** exercise the `with_serializable_retry`
// helper's retry-on-40001 branch — there is no contender here, so
// Postgres has no reason to raise a serialization failure. Coverage
// of the retry path itself is left to a future test that spawns a
// deterministic conflicting transaction; the function name + this
// comment are deliberately conservative so the test does not give
// false confidence about retry coverage.
//
// Scenario: missing strict `(root, child)` ancestor edge. Repair MUST
// insert it with the canonical barrier and `descendant_status`
// derived from `tenants`, leaving the post-repair snapshot clean.
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_repair_inserts_missing_coverage_gap_in_serializable_tx() {
    let h = bring_up_postgres().await.expect("postgres");
    let root = Uuid::new_v4();
    let child = Uuid::new_v4();
    insert_tenant(&h.provider, root, None, "root", ACTIVE, false, 0)
        .await
        .unwrap();
    insert_tenant(&h.provider, child, Some(root), "child", ACTIVE, false, 1)
        .await
        .unwrap();
    insert_closure(&h.provider, root, root, 0, ACTIVE)
        .await
        .unwrap();
    insert_closure(&h.provider, child, child, 0, ACTIVE)
        .await
        .unwrap();
    // (root, child) strict edge deliberately missing — `ClosureCoverageGap`.

    let report = h
        .repo
        .repair_derivable_closure_violations(&allow_all())
        .await
        .expect("repair");
    assert_eq!(
        repaired_count(&report, IntegrityCategory::ClosureCoverageGap),
        1,
        "expected exactly one closure-coverage-gap repair on Postgres"
    );

    let edge = fetch_closure_row(&h.provider, root, child)
        .await
        .unwrap()
        .expect("strict (root, child) edge inserted by repair");
    assert_eq!(edge.barrier, 0);
    assert_eq!(edge.descendant_status, ACTIVE);

    // Idempotency under real Postgres SI — second pass is a no-op.
    let again = h
        .repo
        .repair_derivable_closure_violations(&allow_all())
        .await
        .expect("idempotent repair");
    assert_eq!(again.total_repaired(), 0);

    // Post-repair integrity check passes — closes the loop.
    let viols = h
        .repo
        .run_integrity_check(&allow_all())
        .await
        .expect("post-repair integrity check");
    assert!(
        viols.is_empty(),
        "post-repair tree must be integrity-clean on Postgres: {viols:?}"
    );
}
