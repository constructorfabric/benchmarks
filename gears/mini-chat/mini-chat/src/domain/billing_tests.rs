#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn credits_use_per_component_ceiling() {
    // 1 token at 1 micro-credit per 1M tokens rounds up to 1 per component.
    assert_eq!(credits_micro(1, 1, 1, 1).unwrap(), 2);
    assert_eq!(credits_micro(0, 0, 5, 5).unwrap(), 0);
    // DESIGN §5.10 example: 1000 in + 500 out at 2.5 credits per 1K tokens.
    assert_eq!(
        credits_micro(1_000, 500, 2_500_000_000, 2_500_000_000).unwrap(),
        3_750_000
    );
    assert_eq!(
        credits_micro(1_000, 500, 1_000_000_000, 1_000_000_000).unwrap(),
        1_500_000
    );
}

#[test]
fn credits_reject_out_of_range_inputs() {
    assert_eq!(
        credits_micro(-1, 0, 1, 1),
        Err(CreditError::InvalidTokenCount(-1))
    );
    assert_eq!(
        credits_micro(MAX_TOKENS + 1, 0, 1, 1),
        Err(CreditError::InvalidTokenCount(MAX_TOKENS + 1))
    );
    assert_eq!(credits_micro(1, 1, 0, 1), Err(CreditError::ZeroMultiplier));
    assert_eq!(
        credits_micro(1, 1, MAX_MULTIPLIER + 1, 1),
        Err(CreditError::InvalidMultiplier(MAX_MULTIPLIER + 1))
    );
}

#[test]
fn text_estimate_applies_overhead_and_margin() {
    let b = EstimationBudgets::default(); // 4 bpt, 100 overhead, 10% margin
    // 40 bytes -> 10 tokens + 100 = 110 * 1.10 = 121
    assert_eq!(estimate_text_tokens(40, &b), 121);
    // empty message -> overhead only before margin
    assert_eq!(estimate_text_tokens(0, &b), 110);
    // 41 bytes -> ceil(41/4)=11 + 100 = 111 * 110 / 100 = 122.1 -> 123
    assert_eq!(estimate_text_tokens(41, &b), 123);
}

#[test]
fn reserve_counts_surcharges_only_for_included_tools() {
    let b = EstimationBudgets::default();
    let base = ReserveInputs {
        message_bytes: 40,
        prior_context_tokens: 100,
        ..Default::default()
    };
    let r = compute_reserve(&base, &b, 1000, 1_000_000, 1_000_000).unwrap();
    assert_eq!(r.estimated_input_tokens, 221);
    assert_eq!(r.reserve_tokens, 1221);
    assert_eq!(r.reserved_credits_micro, 221 + 1000);

    let tools = ReserveInputs {
        image_count: 2,
        file_search: true,
        web_search: true,
        code_interpreter: true,
        ..base
    };
    let r = compute_reserve(&tools, &b, 1000, 1_000_000, 1_000_000).unwrap();
    assert_eq!(r.estimated_input_tokens, 221 + 2000 + 500 + 500 + 1000);
}

#[test]
fn billing_derivation_matches_normative_table() {
    assert_eq!(
        derive_billing("completed", None, false),
        (BillingOutcome::Completed, SettlementMethod::Actual)
    );
    assert_eq!(
        derive_billing("cancelled", None, true),
        (BillingOutcome::Aborted, SettlementMethod::Estimated)
    );
    assert_eq!(
        derive_billing("failed", Some("orphan_timeout"), false),
        (BillingOutcome::Aborted, SettlementMethod::Estimated)
    );
    assert_eq!(
        derive_billing("failed", Some("provider_error"), true),
        (BillingOutcome::Failed, SettlementMethod::Actual)
    );
    assert_eq!(
        derive_billing("failed", Some("rate_limited"), false),
        (BillingOutcome::Failed, SettlementMethod::Estimated)
    );
    assert_eq!(
        derive_billing("failed", Some("web_search_calls_exceeded"), false),
        (BillingOutcome::Failed, SettlementMethod::Estimated)
    );
    assert_eq!(
        derive_billing("failed", Some("turn_setup_failed"), false),
        (BillingOutcome::Failed, SettlementMethod::Released)
    );
    assert_eq!(
        derive_billing("failed", Some("something_new"), true),
        (BillingOutcome::Failed, SettlementMethod::Estimated)
    );
}

#[test]
fn overshoot_within_tolerance_charges_actual_and_beyond_caps_at_reserve() {
    let reserve = PersistedReserve {
        reserve_tokens: 10_000,
        max_output_tokens_applied: 1_000,
        reserved_credits_micro: 2_500_000,
        minimal_generation_floor_applied: 50,
    };
    // 10_500 / 10_000 = 1.05 <= 1.10
    let c = settle_actual(&reserve, 10_000, 500, 1_000_000_000, 1_000_000_000, 1.10).unwrap();
    assert!(c.overshoot);
    assert!(!c.overshoot_capped);
    assert_eq!(c.credits_micro, 10_500_000);
    // 11_500 / 10_000 = 1.15 > 1.10 -> capped
    let c = settle_actual(&reserve, 11_000, 500, 1_000_000_000, 1_000_000_000, 1.10).unwrap();
    assert!(c.overshoot_capped);
    assert_eq!(c.credits_micro, 2_500_000);
}

#[test]
fn estimated_settlement_charges_input_estimate_plus_floor() {
    let reserve = PersistedReserve {
        reserve_tokens: 1_221,
        max_output_tokens_applied: 1_000,
        reserved_credits_micro: 1_221,
        minimal_generation_floor_applied: 50,
    };
    let c = settle_estimated(&reserve, 1_000_000, 1_000_000).unwrap();
    assert_eq!(c.credits_micro, 221 + 50);
}

#[test]
fn quota_warning_flags_follow_threshold() {
    assert_eq!(remaining_percentage(100, 0), 100);
    assert_eq!(remaining_percentage(100, 81), 19);
    assert_eq!(remaining_percentage(1000, 995), 0);
    assert_eq!(remaining_percentage(0, 0), 0);
    assert_eq!(warning_flags(20, 80), (true, false));
    assert_eq!(warning_flags(21, 80), (false, false));
    assert_eq!(warning_flags(0, 80), (true, true));
}
