//! Settlement computation tests: the actual, estimated and released formulas, the persisted
//! floor and the clamp to the reserve, the telemetry tokens, the overshoot rule (within and
//! above the tolerance, floating-point ratio, boundary) with the DESIGN example, and the credit
//! computation failures.
//!
//! Slice S1-all: algorithm `cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation`,
//! state `cpt-cf-mini-chat-state-quota-and-billing-turn-reserve` (settled states), definition
//! of done `cpt-cf-mini-chat-dod-quota-and-billing-settlement-outcomes`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::{ModelCatalogEntry, ModelTier};

use super::{SettlementComputation, TurnBilling, compute_settlement};
use crate::domain::quota::billing::SettlementMethod;
use crate::domain::quota::credits::CreditError;
use crate::test_support::quota::{model, usage};

const TOLERANCE: f64 = 1.10;

/// The persisted columns of the DESIGN 5.10 standard turn.
fn design_turn() -> TurnBilling {
    TurnBilling {
        reserve_tokens: 1500,
        max_output_tokens_applied: 500,
        reserved_credits_micro: 1_500_000,
        minimal_generation_floor_applied: 50,
        policy_version_applied: 3,
        effective_model: "standard-1".to_owned(),
    }
}

fn standard() -> ModelCatalogEntry {
    model("standard-1", ModelTier::Standard, 1_000_000_000)
}

/// The DESIGN overshoot example: a reserve of 10,000 tokens and 2,500,000 credits (0.25 credit
/// per 1,000 tokens).
fn overshoot_turn() -> TurnBilling {
    TurnBilling {
        reserve_tokens: 10_000,
        max_output_tokens_applied: 500,
        reserved_credits_micro: 2_500_000,
        minimal_generation_floor_applied: 50,
        policy_version_applied: 3,
        effective_model: "quarter".to_owned(),
    }
}

fn quarter() -> ModelCatalogEntry {
    model("quarter", ModelTier::Standard, 250_000_000)
}

fn settle(
    billing: &TurnBilling,
    method: SettlementMethod,
    reported: Option<(u64, u64)>,
    entry: &ModelCatalogEntry,
    tolerance: f64,
) -> SettlementComputation {
    let tokens = reported.map(|(input, output)| usage(input, output));
    compute_settlement(billing, method, tokens.as_ref(), entry, tolerance).expect("settles")
}

// ── actual ───────────────────────────────────────────────────────────────────

#[test]
fn an_actual_settlement_commits_the_credits_of_the_provider_usage() {
    let settled = settle(
        &design_turn(),
        SettlementMethod::Actual,
        Some((900, 300)),
        &standard(),
        TOLERANCE,
    );
    assert_eq!(settled.method, SettlementMethod::Actual);
    assert_eq!(settled.actual_credits_micro, 1_200_000);
    assert_eq!(settled.committed_credits_micro, 1_200_000);
    assert_eq!(settled.charged_tokens, 1200);
    assert_eq!(settled.telemetry_input_tokens, 900);
    assert_eq!(settled.telemetry_output_tokens, 300);
    assert!(!settled.overshoot);
    assert!(!settled.overshoot_capped);
}

#[test]
fn a_completed_turn_without_any_usage_settles_zero_credits() {
    let settled = settle(
        &design_turn(),
        SettlementMethod::Actual,
        None,
        &standard(),
        TOLERANCE,
    );
    assert_eq!(settled.committed_credits_micro, 0);
    assert_eq!(settled.charged_tokens, 0);
    assert_eq!(settled.telemetry_input_tokens, 0);
    assert_eq!(settled.telemetry_output_tokens, 0);
    assert!(!settled.overshoot);
}

#[test]
fn a_usage_object_with_only_some_fields_counts_the_missing_ones_as_zero() {
    let settled = settle(
        &design_turn(),
        SettlementMethod::Actual,
        Some((1000, 0)),
        &standard(),
        TOLERANCE,
    );
    assert_eq!(settled.committed_credits_micro, 1_000_000);
    let settled = settle(
        &design_turn(),
        SettlementMethod::Actual,
        Some((0, 500)),
        &standard(),
        TOLERANCE,
    );
    assert_eq!(settled.committed_credits_micro, 500_000);
}

#[test]
fn usage_equal_to_the_reserve_is_not_an_overshoot() {
    let settled = settle(
        &design_turn(),
        SettlementMethod::Actual,
        Some((1000, 500)),
        &standard(),
        TOLERANCE,
    );
    assert_eq!(settled.committed_credits_micro, 1_500_000);
    assert!(!settled.overshoot);
}

// ── estimated ────────────────────────────────────────────────────────────────

#[test]
fn an_estimated_settlement_charges_the_estimated_input_and_the_persisted_floor() {
    let settled = settle(
        &design_turn(),
        SettlementMethod::Estimated,
        None,
        &standard(),
        TOLERANCE,
    );
    // estimated_input_tokens = 1500 - 500 = 1000; charged_output_tokens = 50
    assert_eq!(settled.charged_tokens, 1050);
    assert_eq!(settled.actual_credits_micro, 1_050_000);
    assert_eq!(settled.committed_credits_micro, 1_050_000);
    assert!(settled.committed_credits_micro < design_turn().reserved_credits_micro);
    assert_eq!(settled.telemetry_input_tokens, 0);
    assert_eq!(settled.telemetry_output_tokens, 0);
    assert!(!settled.overshoot);
}

#[test]
fn an_estimated_settlement_ignores_any_usage_it_is_given() {
    let settled = settle(
        &design_turn(),
        SettlementMethod::Estimated,
        Some((900, 300)),
        &standard(),
        TOLERANCE,
    );
    assert_eq!(settled.committed_credits_micro, 1_050_000);
    assert_eq!(settled.telemetry_input_tokens, 0);
    assert_eq!(settled.telemetry_output_tokens, 0);
}

#[test]
fn the_charged_tokens_never_exceed_the_reserve() {
    // a persisted floor above the output cap cannot raise the charge above the reserve
    let mut turn = design_turn();
    turn.minimal_generation_floor_applied = 600;
    let settled = settle(
        &turn,
        SettlementMethod::Estimated,
        None,
        &standard(),
        TOLERANCE,
    );
    assert_eq!(settled.charged_tokens, 1500);

    // a floor equal to the output cap charges exactly the reserve
    let mut full = design_turn();
    full.minimal_generation_floor_applied = 500;
    let settled = settle(
        &full,
        SettlementMethod::Estimated,
        None,
        &standard(),
        TOLERANCE,
    );
    assert_eq!(settled.charged_tokens, 1500);
}

#[test]
fn the_estimated_credits_never_charge_the_full_reserve_when_the_floor_is_smaller() {
    let settled = settle(
        &design_turn(),
        SettlementMethod::Estimated,
        None,
        &standard(),
        TOLERANCE,
    );
    assert_ne!(settled.committed_credits_micro, 1_500_000);
}

#[test]
fn the_estimated_settlement_is_reproducible_from_the_persisted_fields_only() {
    let turn = design_turn();
    let first = settle(
        &turn,
        SettlementMethod::Estimated,
        None,
        &standard(),
        TOLERANCE,
    );
    // the catalog budgets of the entry are not read: change every budget
    let mut changed = standard();
    changed.estimation_budgets.safety_margin_pct = 500;
    changed.estimation_budgets.fixed_overhead_tokens = 9_999;
    changed.estimation_budgets.minimal_generation_floor = 12_345;
    assert_eq!(
        settle(
            &turn,
            SettlementMethod::Estimated,
            None,
            &changed,
            TOLERANCE
        ),
        first
    );
}

// ── released ─────────────────────────────────────────────────────────────────

#[test]
fn a_released_settlement_charges_nothing_and_releases_the_whole_reserve() {
    let settled = settle(
        &design_turn(),
        SettlementMethod::Released,
        Some((900, 300)),
        &standard(),
        TOLERANCE,
    );
    assert_eq!(settled.method, SettlementMethod::Released);
    assert_eq!(settled.committed_credits_micro, 0);
    assert_eq!(settled.actual_credits_micro, 0);
    assert_eq!(settled.charged_tokens, 0);
    assert_eq!(settled.telemetry_input_tokens, 0);
    assert_eq!(settled.telemetry_output_tokens, 0);
    assert!(!settled.overshoot);
}

// ── overshoot ────────────────────────────────────────────────────────────────

#[test]
fn an_overshoot_above_the_tolerance_commits_the_reserve_and_keeps_the_telemetry_uncapped() {
    // 11,000 + 500 = 11,500 tokens against a reserve of 10,000: factor 1.15 > 1.10
    let settled = settle(
        &overshoot_turn(),
        SettlementMethod::Actual,
        Some((11_000, 500)),
        &quarter(),
        TOLERANCE,
    );
    assert!(settled.overshoot);
    assert!(settled.overshoot_capped);
    assert_eq!(settled.actual_credits_micro, 2_875_000);
    assert_eq!(settled.committed_credits_micro, 2_500_000);
    assert_eq!(
        settled.charged_tokens, 10_000,
        "the committed tokens are the reserve"
    );
    assert_eq!(settled.telemetry_input_tokens, 11_000);
    assert_eq!(settled.telemetry_output_tokens, 500);
}

#[test]
fn an_overshoot_within_the_tolerance_commits_the_actual_credits() {
    // 10,500 tokens: factor 1.05
    let settled = settle(
        &overshoot_turn(),
        SettlementMethod::Actual,
        Some((10_000, 500)),
        &quarter(),
        TOLERANCE,
    );
    assert!(settled.overshoot);
    assert!(!settled.overshoot_capped);
    assert_eq!(settled.committed_credits_micro, 2_625_000);
    assert_eq!(settled.charged_tokens, 10_500);
    assert_eq!(settled.telemetry_input_tokens, 10_000);
    assert_eq!(settled.telemetry_output_tokens, 500);
}

#[test]
fn a_factor_exactly_at_the_tolerance_is_within_it() {
    // 11,000 / 10,000 = 1.10
    let settled = settle(
        &overshoot_turn(),
        SettlementMethod::Actual,
        Some((10_500, 500)),
        &quarter(),
        TOLERANCE,
    );
    assert!(settled.overshoot);
    assert!(!settled.overshoot_capped);
    assert_eq!(settled.committed_credits_micro, 2_750_000);
}

#[test]
fn one_token_above_the_boundary_is_capped() {
    let settled = settle(
        &overshoot_turn(),
        SettlementMethod::Actual,
        Some((10_501, 500)),
        &quarter(),
        TOLERANCE,
    );
    assert!(settled.overshoot_capped);
    assert_eq!(settled.committed_credits_micro, 2_500_000);
}

#[test]
fn the_ratio_is_computed_in_floating_point_not_with_integer_division() {
    // 11,900 / 10,000 is 1.19: integer division would give 1 and look within the tolerance
    let settled = settle(
        &overshoot_turn(),
        SettlementMethod::Actual,
        Some((11_400, 500)),
        &quarter(),
        TOLERANCE,
    );
    assert!(settled.overshoot_capped);
    assert_eq!(settled.committed_credits_micro, 2_500_000);
}

#[test]
fn one_token_above_the_reserve_is_an_overshoot_within_the_tolerance() {
    let settled = settle(
        &overshoot_turn(),
        SettlementMethod::Actual,
        Some((10_001, 0)),
        &quarter(),
        TOLERANCE,
    );
    assert!(settled.overshoot);
    assert!(!settled.overshoot_capped);
}

#[test]
fn the_configured_tolerance_decides_the_cap() {
    let usage_of = Some((11_000, 500));
    // 1.0: no overshoot allowed
    let strict = settle(
        &overshoot_turn(),
        SettlementMethod::Actual,
        Some((10_001, 0)),
        &quarter(),
        1.0,
    );
    assert!(strict.overshoot_capped);
    // 1.5: 1.15 is within
    let lenient = settle(
        &overshoot_turn(),
        SettlementMethod::Actual,
        usage_of,
        &quarter(),
        1.5,
    );
    assert!(lenient.overshoot);
    assert!(!lenient.overshoot_capped);
    assert_eq!(lenient.committed_credits_micro, 2_875_000);
}

#[test]
fn estimated_and_released_settlements_never_exceed_the_reserve() {
    for method in [SettlementMethod::Estimated, SettlementMethod::Released] {
        let settled = settle(
            &overshoot_turn(),
            method,
            Some((50_000, 50_000)),
            &quarter(),
            TOLERANCE,
        );
        assert!(!settled.overshoot, "{method:?}");
        assert!(!settled.overshoot_capped, "{method:?}");
        assert!(settled.committed_credits_micro <= overshoot_turn().reserved_credits_micro);
    }
}

// ── multipliers and failures ─────────────────────────────────────────────────

#[test]
fn the_multipliers_come_from_the_entry_of_the_applied_snapshot() {
    let cheap = model("standard-1", ModelTier::Standard, 500_000_000);
    let dear = model("standard-1", ModelTier::Standard, 2_000_000_000);
    let on_cheap = settle(
        &design_turn(),
        SettlementMethod::Actual,
        Some((900, 300)),
        &cheap,
        TOLERANCE,
    );
    let on_dear = settle(
        &design_turn(),
        SettlementMethod::Actual,
        Some((900, 300)),
        &dear,
        TOLERANCE,
    );
    assert_eq!(on_cheap.committed_credits_micro, 600_000);
    assert_eq!(on_dear.committed_credits_micro, 2_400_000);
}

#[test]
fn an_out_of_range_provider_token_count_fails_the_computation() {
    let tokens = usage(10_000_001, 0);
    let error = compute_settlement(
        &design_turn(),
        SettlementMethod::Actual,
        Some(&tokens),
        &standard(),
        TOLERANCE,
    )
    .unwrap_err();
    assert_eq!(error, CreditError::InvalidTokenCount { tokens: 10_000_001 });
}

#[test]
fn a_zero_multiplier_fails_the_computation_on_every_priced_method() {
    let free = model("standard-1", ModelTier::Standard, 0);
    let tokens = usage(1, 1);
    for method in [SettlementMethod::Actual, SettlementMethod::Estimated] {
        let error = compute_settlement(&design_turn(), method, Some(&tokens), &free, TOLERANCE)
            .unwrap_err();
        assert_eq!(error, CreditError::ZeroMultiplier, "{method:?}");
    }
}

#[test]
fn a_multiplier_above_the_range_fails_the_computation() {
    let huge = model("standard-1", ModelTier::Standard, 10_000_000_001);
    let error = compute_settlement(
        &design_turn(),
        SettlementMethod::Estimated,
        None,
        &huge,
        TOLERANCE,
    )
    .unwrap_err();
    assert_eq!(
        error,
        CreditError::MultiplierOutOfRange {
            multiplier: 10_000_000_001
        }
    );
}

#[test]
fn the_computation_is_deterministic() {
    let first = settle(
        &design_turn(),
        SettlementMethod::Actual,
        Some((900, 300)),
        &standard(),
        TOLERANCE,
    );
    for _ in 0..3 {
        assert_eq!(
            settle(
                &design_turn(),
                SettlementMethod::Actual,
                Some((900, 300)),
                &standard(),
                TOLERANCE
            ),
            first
        );
    }
}
