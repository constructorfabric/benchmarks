//! The budget arithmetic of `cpt-cf-oagw-algo-budget-allocate`.
//!
//! Covers the rejection ADR 0003's worked example performs at a ratio of 1.0,
//! the acceptance-with-warning the same arithmetic shows at a ratio above 1.0,
//! the in-budget acceptance, the `unlimited` mode, and the first-come-first-
//! served charge of the `shared` pool.

// @cpt-dod:cpt-cf-oagw-dod-rate-limit-tests:p1

#![allow(clippy::expect_used, clippy::unwrap_used)]

use oagw::domain::ratelimit::{BudgetAllocation, BudgetMode, BudgetOutcome, allocate_budget};

fn budget(total: u64, overcommit_ratio_percent: u64) -> BudgetAllocation {
    BudgetAllocation {
        total,
        overcommit_ratio_percent,
    }
}

#[test]
fn a_sum_above_the_total_at_a_ratio_of_one_is_rejected() {
    // The write path's rejection: the children's declared allocations plus the
    // candidate exceed the parent's `total` at an `overcommit_ratio` of 1.0,
    // which is answered 400.
    let outcome = allocate_budget(
        Some(budget(1000, 100)),
        BudgetMode::Allocated,
        1000,
        1000,
        100,
    );
    assert_eq!(outcome, BudgetOutcome::Rejected { allocated: 1100 });
}

#[test]
fn the_same_sum_at_a_ratio_above_one_is_accepted_with_a_warning() {
    // The same sum at a ratio of 1.5 fits the ceiling the ratio sets but still
    // exceeds the parent's `total`, which is the acceptance ADR 0003's worked
    // arithmetic warns about rather than rejects.
    let outcome = allocate_budget(
        Some(budget(1000, 150)),
        BudgetMode::Allocated,
        1000,
        1000,
        100,
    );
    assert_eq!(
        outcome,
        BudgetOutcome::Accepted {
            allocated: 1100,
            over_total: true,
        }
    );
}

#[test]
fn a_sum_inside_the_total_is_accepted_without_a_warning() {
    let outcome = allocate_budget(
        Some(budget(1000, 100)),
        BudgetMode::Allocated,
        1000,
        500,
        100,
    );
    assert_eq!(
        outcome,
        BudgetOutcome::Accepted {
            allocated: 600,
            over_total: false,
        }
    );
}

#[test]
fn a_sum_at_the_ceiling_is_accepted() {
    // The ceiling the ratio computes is inclusive: a sum that reaches it
    // exactly is not above it.
    let outcome = allocate_budget(
        Some(budget(1000, 150)),
        BudgetMode::Allocated,
        1000,
        1400,
        100,
    );
    assert!(matches!(outcome, BudgetOutcome::Accepted { .. }));
}

#[test]
fn a_parent_with_no_budget_tracks_nothing() {
    // A parent with no budget at all is `unlimited` rather than a rejection,
    // because a mode that tracks nothing cannot be exceeded.
    let outcome = allocate_budget(None, BudgetMode::Allocated, 0, 1000, 1000);
    assert_eq!(outcome, BudgetOutcome::Unlimited);
}

#[test]
fn the_shared_pool_charges_first_come_first_served() {
    // ADR 0003's Example 2: tenants A, B and C share a 5000 per minute pool
    // with no individual guarantee, so each charge is taken from whatever the
    // pool still holds.
    let outcome = allocate_budget(
        Some(budget(5000, 100)),
        BudgetMode::Shared,
        5000,
        0,
        100,
    );
    assert_eq!(outcome, BudgetOutcome::Shared { remaining: 4900 });

    let drained = allocate_budget(Some(budget(5000, 100)), BudgetMode::Shared, 40, 0, 100);
    assert_eq!(drained, BudgetOutcome::Shared { remaining: 0 });
}

#[test]
fn the_unlimited_mode_tracks_nothing() {
    // The mode ADR 0003 declares as the default for a leaf tenant validates no
    // allocation and charges no pool.
    let outcome = allocate_budget(Some(budget(1000, 100)), BudgetMode::Unlimited, 1000, 900, 500);
    assert_eq!(outcome, BudgetOutcome::Unlimited);
}

#[test]
fn adr_0003_example_one_validates_the_partner_allocation() {
    // Example 1's partner: a parent of 10000 per minute holding a child
    // allocated 5000 at a ratio of 1.2 accepts a further 1000 and warns,
    // because the sum of 6000 is above the 5000 total but inside the 6000
    // ceiling the ratio sets.
    let outcome = allocate_budget(
        Some(budget(5000, 120)),
        BudgetMode::Allocated,
        5000,
        5000,
        1000,
    );
    assert_eq!(
        outcome,
        BudgetOutcome::Accepted {
            allocated: 6000,
            over_total: true,
        }
    );
}
