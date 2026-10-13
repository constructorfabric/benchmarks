#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 4: `cpt-cf-mini-chat-algo-quota-billing-settle` (the pure planning)
//! and the settlement formulas of
//! `cpt-cf-mini-chat-dod-quota-billing-settlement`.

use mini_chat_sdk::{ModelTier, PolicySnapshot, SettlementMethod, UsageTokens};

use super::{
    BucketDelta, FallbackCause, SettleInput, SettlePlan, ToolCallCounts, bucket_deltas,
    estimated_charged_tokens, plan_settlement,
};
use crate::domain::service::quota::arith::CreditError;
use crate::domain::service::quota::test_support::{model, snapshot};
use crate::domain::service::quota::types::QuotaBucket::{TierPremium, Total};

fn usage(input: u64, output: u64) -> UsageTokens {
    UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: 7,
        cache_write_input_tokens: 8,
        reasoning_tokens: 9,
    }
}

/// Snapshot version 7 with the standard model `s` (1.0 credit per 1K tokens
/// both ways) and the premium model `p` (2.5 credits per 1K tokens).
fn snap() -> PolicySnapshot {
    snapshot(
        7,
        vec![
            model("p", ModelTier::Premium)
                .mult(2_500_000_000, 2_500_000_000)
                .build(),
            model("s", ModelTier::Standard).build(),
            model("asym", ModelTier::Standard)
                .mult(2_000_000_000, 4_000_000_000)
                .build(),
            model("zero", ModelTier::Standard).mult(0, 1).build(),
            model("huge", ModelTier::Standard)
                .mult(10_000_000_001, 1)
                .build(),
        ],
    )
}

/// The persisted columns of the DESIGN 5.10 standard turn: `reserve_tokens`
/// 1500, `max_output_tokens_applied` 500, reserved credits `1_500_000`, floor 50.
fn base<'a>(
    snapshot: Option<&'a PolicySnapshot>,
    method: SettlementMethod,
    usage: Option<&'a UsageTokens>,
) -> SettleInput<'a> {
    SettleInput {
        reserve_tokens: 1500,
        max_output_tokens_applied: 500,
        reserved_credits_micro: 1_500_000,
        minimal_generation_floor_applied: 50,
        effective_model: "s",
        snapshot,
        method,
        usage,
        overshoot_tolerance_factor: 1.10,
    }
}

// ---- actual ------------------------------------------------------------------------------------------

#[test]
fn actual_settlement_debits_the_credits_of_the_provider_usage() {
    let snapshot = snap();
    let usage = usage(900, 300);

    let plan = plan_settlement(&base(
        Some(&snapshot),
        SettlementMethod::Actual,
        Some(&usage),
    ));

    assert_eq!(
        plan,
        SettlePlan {
            method: SettlementMethod::Actual,
            committed_credits_micro: 1_200_000,
            telemetry_input_tokens: 900,
            telemetry_output_tokens: 300,
            overshoot_capped: false,
            tier: Some(ModelTier::Standard),
            fallback: None,
        }
    );
}

#[test]
fn cache_and_reasoning_tokens_do_not_change_the_credits() {
    let snapshot = snap();
    let mut with_extras = usage(900, 300);
    with_extras.reasoning_tokens = 5_000_000;
    with_extras.cache_read_input_tokens = 5_000_000;

    let plan = plan_settlement(&base(
        Some(&snapshot),
        SettlementMethod::Actual,
        Some(&with_extras),
    ));

    assert_eq!(plan.committed_credits_micro, 1_200_000);
}

#[test]
fn a_completed_turn_without_a_usage_object_settles_actual_at_zero() {
    let snapshot = snap();

    let plan = plan_settlement(&base(Some(&snapshot), SettlementMethod::Actual, None));

    assert_eq!(plan.method, SettlementMethod::Actual);
    assert_eq!(plan.committed_credits_micro, 0);
    assert_eq!(
        (plan.telemetry_input_tokens, plan.telemetry_output_tokens),
        (0, 0)
    );
    assert_eq!(plan.fallback, None);
}

#[test]
fn the_premium_tier_comes_from_the_snapshot_entry_of_the_effective_model() {
    let snapshot = snap();
    let usage = usage(900, 300);
    let mut input = base(Some(&snapshot), SettlementMethod::Actual, Some(&usage));
    input.effective_model = "p";

    let plan = plan_settlement(&input);

    assert_eq!(plan.tier, Some(ModelTier::Premium));
    // 1200 tokens at 2.5 credits per 1K
    assert_eq!(plan.committed_credits_micro, 3_000_000);
}

// ---- overshoot cap -----------------------------------------------------------------------------------

/// `reserve_tokens` `10_000` with 500 output tokens: reserved credits `10_000_000`.
fn overshoot_input<'a>(
    snapshot: &'a PolicySnapshot,
    usage: &'a UsageTokens,
    tolerance: f64,
) -> SettleInput<'a> {
    SettleInput {
        reserve_tokens: 10_000,
        max_output_tokens_applied: 500,
        reserved_credits_micro: 10_000_000,
        minimal_generation_floor_applied: 50,
        effective_model: "s",
        snapshot: Some(snapshot),
        method: SettlementMethod::Actual,
        usage: Some(usage),
        overshoot_tolerance_factor: tolerance,
    }
}

#[test]
fn an_overshoot_beyond_the_tolerance_commits_the_reserve_but_keeps_the_token_telemetry() {
    let snapshot = snap();
    let usage = usage(11_000, 500);

    // 11_500 / 10_000 = 1.15 > 1.10
    let plan = plan_settlement(&overshoot_input(&snapshot, &usage, 1.10));

    assert!(plan.overshoot_capped);
    assert_eq!(plan.committed_credits_micro, 10_000_000);
    assert_eq!(plan.telemetry_input_tokens, 11_000);
    assert_eq!(plan.telemetry_output_tokens, 500);
    assert_eq!(plan.method, SettlementMethod::Actual);
}

#[test]
fn an_overshoot_of_exactly_the_tolerance_commits_the_actual_credits() {
    let snapshot = snap();
    let usage = usage(10_500, 500);

    // 11_000 / 10_000 = 1.10 exactly
    let plan = plan_settlement(&overshoot_input(&snapshot, &usage, 1.10));

    assert!(!plan.overshoot_capped);
    assert_eq!(plan.committed_credits_micro, 11_000_000);
}

#[test]
fn one_token_above_the_tolerance_is_capped() {
    let snapshot = snap();
    let usage = usage(10_501, 500);

    // 11_001 / 10_000 = 1.1001 > 1.10
    let plan = plan_settlement(&overshoot_input(&snapshot, &usage, 1.10));

    assert!(plan.overshoot_capped);
    assert_eq!(plan.committed_credits_micro, 10_000_000);
}

#[test]
fn the_factor_is_a_floating_point_ratio_never_an_integer_division() {
    let snapshot = snap();
    // 1.5 and 1.15 both truncate to 1 with integer division, which would
    // wrongly pass the 1.10 tolerance.
    for (input_tokens, expected_capped) in [(14_500, true), (11_000, true), (10_400, false)] {
        let usage = usage(input_tokens, 500);

        let plan = plan_settlement(&overshoot_input(&snapshot, &usage, 1.10));

        assert_eq!(plan.overshoot_capped, expected_capped, "{input_tokens}");
    }
}

#[test]
fn the_tolerance_comes_from_the_configuration() {
    let snapshot = snap();
    let usage = usage(11_000, 500); // factor 1.15

    assert!(plan_settlement(&overshoot_input(&snapshot, &usage, 1.0)).overshoot_capped);
    assert!(plan_settlement(&overshoot_input(&snapshot, &usage, 1.10)).overshoot_capped);
    assert!(!plan_settlement(&overshoot_input(&snapshot, &usage, 1.15)).overshoot_capped);
    assert!(!plan_settlement(&overshoot_input(&snapshot, &usage, 1.5)).overshoot_capped);
}

#[test]
fn actual_tokens_at_or_below_the_reserve_never_cap() {
    let snapshot = snap();
    for total in [0_u64, 9_999, 10_000] {
        let usage = usage(total, 0);

        let plan = plan_settlement(&overshoot_input(&snapshot, &usage, 1.0));

        assert!(!plan.overshoot_capped, "{total}");
        assert_eq!(
            plan.committed_credits_micro,
            i64::try_from(total).unwrap() * 1000
        );
    }
}

// ---- estimated ---------------------------------------------------------------------------------------

#[test]
fn estimated_settlement_charges_the_estimated_input_and_the_floor() {
    let snapshot = snap();

    let plan = plan_settlement(&base(Some(&snapshot), SettlementMethod::Estimated, None));

    // estimated_input_tokens = 1500 - 500 = 1000, floor 50: 1050 tokens at 1.0 per 1K
    assert_eq!(
        plan,
        SettlePlan {
            method: SettlementMethod::Estimated,
            committed_credits_micro: 1_050_000,
            telemetry_input_tokens: 0,
            telemetry_output_tokens: 0,
            overshoot_capped: false,
            tier: Some(ModelTier::Standard),
            fallback: None,
        }
    );
}

#[test]
fn estimated_settlement_ignores_any_usage_that_was_passed() {
    let snapshot = snap();
    let usage = usage(100_000, 100_000);

    let plan = plan_settlement(&base(
        Some(&snapshot),
        SettlementMethod::Estimated,
        Some(&usage),
    ));

    assert_eq!(plan.committed_credits_micro, 1_050_000);
    assert_eq!(
        (plan.telemetry_input_tokens, plan.telemetry_output_tokens),
        (0, 0)
    );
    assert!(!plan.overshoot_capped);
}

#[test]
fn estimated_credits_use_the_input_and_output_multipliers_separately() {
    let snapshot = snap();
    let mut input = base(Some(&snapshot), SettlementMethod::Estimated, None);
    input.effective_model = "asym";

    let plan = plan_settlement(&input);

    // 1000 input at 2 per 1K + 50 output at 4 per 1K = 2_000_000 + 200_000
    assert_eq!(plan.committed_credits_micro, 2_200_000);
}

#[test]
fn estimated_settlement_reads_only_persisted_columns_for_the_token_split() {
    let snapshot = snap();
    let mut input = base(Some(&snapshot), SettlementMethod::Estimated, None);
    input.reserve_tokens = 3000;
    input.max_output_tokens_applied = 1000;
    input.minimal_generation_floor_applied = 100;

    // estimated input 2000, floor 100 -> 2100 tokens
    assert_eq!(plan_settlement(&input).committed_credits_micro, 2_100_000);
}

#[test]
fn the_estimated_credits_are_bounded_by_the_reserve_when_the_floor_exceeds_the_output_cap() {
    let snapshot = snap();
    let mut input = base(Some(&snapshot), SettlementMethod::Estimated, None);
    input.reserve_tokens = 1500;
    input.max_output_tokens_applied = 500;
    input.minimal_generation_floor_applied = 600; // a corrupt row: floor above the cap

    // input 1000 + floor 600 would be 1600; the debit is bounded by the reserve 1500
    assert_eq!(plan_settlement(&input).committed_credits_micro, 1_500_000);
}

#[test]
fn the_charged_tokens_are_bounded_by_the_reserve() {
    assert_eq!(estimated_charged_tokens(1500, 500, 50), 1050);
    // reserve 1500, input 1000, floor 600: 1600 is bounded by 1500
    assert_eq!(estimated_charged_tokens(1500, 500, 600), 1500);
    assert_eq!(estimated_charged_tokens(1500, 500, 500), 1500);
    assert_eq!(estimated_charged_tokens(1500, 1500, 50), 50);
}

// ---- released ----------------------------------------------------------------------------------------

#[test]
fn released_settlement_debits_nothing() {
    let snapshot = snap();
    let usage = usage(5, 5);

    let plan = plan_settlement(&base(
        Some(&snapshot),
        SettlementMethod::Released,
        Some(&usage),
    ));

    assert_eq!(plan.method, SettlementMethod::Released);
    assert_eq!(plan.committed_credits_micro, 0);
    assert_eq!(
        (plan.telemetry_input_tokens, plan.telemetry_output_tokens),
        (0, 0)
    );
    assert_eq!(plan.fallback, None);
}

#[test]
fn released_settlement_never_takes_the_fallback() {
    let plan = plan_settlement(&base(None, SettlementMethod::Released, None));

    assert_eq!(plan.method, SettlementMethod::Released);
    assert_eq!(plan.committed_credits_micro, 0);
    assert_eq!(plan.fallback, None);
}

// ---- deterministic fallback --------------------------------------------------------------------------

#[test]
fn an_unavailable_snapshot_settles_estimated_at_the_reserved_credits() {
    let usage = usage(900, 300);
    for method in [SettlementMethod::Actual, SettlementMethod::Estimated] {
        let plan = plan_settlement(&base(None, method, Some(&usage)));

        assert_eq!(
            plan,
            SettlePlan {
                method: SettlementMethod::Estimated,
                committed_credits_micro: 1_500_000,
                telemetry_input_tokens: 0,
                telemetry_output_tokens: 0,
                overshoot_capped: false,
                tier: None,
                fallback: Some(FallbackCause::SnapshotUnavailable),
            },
            "{method:?}"
        );
    }
}

#[test]
fn an_effective_model_missing_from_the_snapshot_takes_the_fallback() {
    let snapshot = snap();
    let usage = usage(900, 300);
    let mut input = base(Some(&snapshot), SettlementMethod::Actual, Some(&usage));
    input.effective_model = "not-in-the-catalog";

    let plan = plan_settlement(&input);

    assert_eq!(plan.method, SettlementMethod::Estimated);
    assert_eq!(plan.committed_credits_micro, 1_500_000);
    assert_eq!(plan.tier, None);
    assert_eq!(plan.fallback, Some(FallbackCause::ModelMissing));
}

#[test]
fn a_token_count_above_ten_million_takes_the_fallback_and_keeps_the_snapshot_tier() {
    let snapshot = snap();
    let usage = usage(10_000_001, 0);
    let mut input = base(Some(&snapshot), SettlementMethod::Actual, Some(&usage));
    input.effective_model = "p";

    let plan = plan_settlement(&input);

    assert_eq!(plan.method, SettlementMethod::Estimated);
    assert_eq!(plan.committed_credits_micro, 1_500_000);
    assert_eq!(plan.tier, Some(ModelTier::Premium), "the entry exists");
    assert_eq!(
        plan.fallback,
        Some(FallbackCause::Credit(CreditError::InvalidTokenCount {
            field: "input_tokens",
            value: 10_000_001
        }))
    );
    assert_eq!(
        (plan.telemetry_input_tokens, plan.telemetry_output_tokens),
        (0, 0)
    );
}

#[test]
fn an_invalid_or_zero_snapshot_multiplier_takes_the_fallback() {
    let snapshot = snap();
    let usage = usage(900, 300);
    for (model, expected) in [
        ("zero", CreditError::ZeroMultiplier { field: "in_mult" }),
        (
            "huge",
            CreditError::InvalidMultiplier {
                field: "in_mult",
                value: 10_000_000_001,
            },
        ),
    ] {
        let mut input = base(Some(&snapshot), SettlementMethod::Actual, Some(&usage));
        input.effective_model = model;

        let plan = plan_settlement(&input);

        assert_eq!(plan.method, SettlementMethod::Estimated, "{model}");
        assert_eq!(plan.committed_credits_micro, 1_500_000, "{model}");
        assert_eq!(
            plan.fallback,
            Some(FallbackCause::Credit(expected)),
            "{model}"
        );
        assert_eq!(plan.tier, Some(ModelTier::Standard), "{model}");
    }
}

#[test]
fn an_estimated_settlement_with_an_invalid_token_split_takes_the_fallback() {
    let snapshot = snap();
    let mut input = base(Some(&snapshot), SettlementMethod::Estimated, None);
    input.reserve_tokens = 20_000_000;
    input.max_output_tokens_applied = 500;

    let plan = plan_settlement(&input);

    assert!(matches!(
        plan.fallback,
        Some(FallbackCause::Credit(CreditError::InvalidTokenCount { .. }))
    ));
    assert_eq!(plan.committed_credits_micro, 1_500_000);
}

#[test]
fn the_fallback_never_evaluates_the_overshoot_cap() {
    let usage = usage(11_000, 500); // would be a 1.15 overshoot

    let plan = plan_settlement(&SettleInput {
        reserve_tokens: 10_000,
        max_output_tokens_applied: 500,
        reserved_credits_micro: 10_000_000,
        minimal_generation_floor_applied: 50,
        effective_model: "s",
        snapshot: None,
        method: SettlementMethod::Actual,
        usage: Some(&usage),
        overshoot_tolerance_factor: 1.10,
    });

    assert!(!plan.overshoot_capped);
    assert_eq!(plan.committed_credits_micro, 10_000_000);
}

#[test]
fn the_fallback_is_deterministic_for_the_same_persisted_columns() {
    let first = plan_settlement(&base(None, SettlementMethod::Estimated, None));
    let second = plan_settlement(&base(None, SettlementMethod::Estimated, None));

    assert_eq!(first, second);
}

// ---- bucket deltas -----------------------------------------------------------------------------------

fn actual_plan() -> SettlePlan {
    SettlePlan {
        method: SettlementMethod::Actual,
        committed_credits_micro: 1_200_000,
        telemetry_input_tokens: 900,
        telemetry_output_tokens: 300,
        overshoot_capped: false,
        tier: Some(ModelTier::Standard),
        fallback: None,
    }
}

#[test]
fn a_standard_turn_updates_the_total_row_only() {
    let deltas = bucket_deltas(&actual_plan(), 1_500_000, ToolCallCounts::default(), false);

    assert_eq!(
        deltas,
        vec![BucketDelta {
            bucket: Total,
            reserved_credits_micro: -1_500_000,
            spent_credits_micro: 1_200_000,
            calls: 1,
            input_tokens: 900,
            output_tokens: 300,
            web_search_calls: 0,
            code_interpreter_calls: 0,
        }]
    );
}

#[test]
fn a_premium_turn_also_moves_the_premium_row_but_not_its_token_or_tool_counters() {
    let tools = ToolCallCounts {
        web_search: 2,
        code_interpreter: 3,
        file_search: 4,
    };

    let deltas = bucket_deltas(&actual_plan(), 3_750_000, tools, true);

    assert_eq!(deltas.len(), 2);
    assert_eq!(deltas[0].bucket, Total, "total is written first");
    assert_eq!(deltas[1].bucket, TierPremium);
    assert_eq!(
        deltas[1],
        BucketDelta {
            bucket: TierPremium,
            reserved_credits_micro: -3_750_000,
            spent_credits_micro: 1_200_000,
            calls: 1,
            input_tokens: 0,
            output_tokens: 0,
            web_search_calls: 0,
            code_interpreter_calls: 0,
        }
    );
}

#[test]
fn completed_web_search_and_code_interpreter_calls_count_into_the_total_row() {
    let tools = ToolCallCounts {
        web_search: 2,
        code_interpreter: 3,
        file_search: 4,
    };

    let deltas = bucket_deltas(&actual_plan(), 1_500_000, tools, false);

    assert_eq!(deltas[0].web_search_calls, 2);
    assert_eq!(deltas[0].code_interpreter_calls, 3);
}

#[test]
fn estimated_settlements_count_tool_calls_but_add_no_tokens() {
    let plan = SettlePlan {
        method: SettlementMethod::Estimated,
        committed_credits_micro: 1_050_000,
        telemetry_input_tokens: 0,
        telemetry_output_tokens: 0,
        overshoot_capped: false,
        tier: Some(ModelTier::Standard),
        fallback: None,
    };
    let tools = ToolCallCounts {
        web_search: 1,
        code_interpreter: 1,
        file_search: 0,
    };

    let deltas = bucket_deltas(&plan, 1_500_000, tools, false);

    assert_eq!(deltas[0].web_search_calls, 1);
    assert_eq!(deltas[0].code_interpreter_calls, 1);
    assert_eq!((deltas[0].input_tokens, deltas[0].output_tokens), (0, 0));
    assert_eq!(deltas[0].spent_credits_micro, 1_050_000);
}

#[test]
fn a_fallback_settlement_still_counts_the_tool_calls_of_the_turn() {
    let plan = plan_settlement(&base(None, SettlementMethod::Actual, None));
    let tools = ToolCallCounts {
        web_search: 1,
        code_interpreter: 2,
        file_search: 0,
    };

    let deltas = bucket_deltas(&plan, 1_500_000, tools, false);

    assert_eq!(deltas[0].spent_credits_micro, 1_500_000);
    assert_eq!(deltas[0].reserved_credits_micro, -1_500_000);
    assert_eq!(deltas[0].web_search_calls, 1);
    assert_eq!(deltas[0].code_interpreter_calls, 2);
    assert_eq!((deltas[0].input_tokens, deltas[0].output_tokens), (0, 0));
}

#[test]
fn a_released_settlement_releases_the_reserve_counts_the_call_and_adds_nothing_else() {
    let plan = plan_settlement(&base(None, SettlementMethod::Released, None));
    let tools = ToolCallCounts {
        web_search: 2,
        code_interpreter: 2,
        file_search: 2,
    };

    let deltas = bucket_deltas(&plan, 1_500_000, tools, true);

    assert_eq!(
        deltas,
        vec![
            BucketDelta {
                bucket: Total,
                reserved_credits_micro: -1_500_000,
                spent_credits_micro: 0,
                calls: 1,
                input_tokens: 0,
                output_tokens: 0,
                web_search_calls: 0,
                code_interpreter_calls: 0,
            },
            BucketDelta {
                bucket: TierPremium,
                reserved_credits_micro: -1_500_000,
                spent_credits_micro: 0,
                calls: 1,
                input_tokens: 0,
                output_tokens: 0,
                web_search_calls: 0,
                code_interpreter_calls: 0,
            },
        ]
    );
}

#[test]
fn the_overshoot_cap_debits_the_reserve_and_the_deltas_keep_the_actual_tokens() {
    let snapshot = snap();
    let usage = usage(11_000, 500);
    let plan = plan_settlement(&overshoot_input(&snapshot, &usage, 1.10));

    let deltas = bucket_deltas(&plan, 10_000_000, ToolCallCounts::default(), false);

    assert_eq!(deltas[0].spent_credits_micro, 10_000_000);
    assert_eq!(deltas[0].reserved_credits_micro, -10_000_000);
    assert_eq!(deltas[0].input_tokens, 11_000);
    assert_eq!(deltas[0].output_tokens, 500);
}
