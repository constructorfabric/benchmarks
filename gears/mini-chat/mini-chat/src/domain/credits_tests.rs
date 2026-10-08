use super::*;

fn budgets() -> EstimationBudgets {
    EstimationBudgets::default()
}

fn reserve() -> TurnReserve {
    TurnReserve {
        reserve_tokens: 1_000,
        max_output_tokens_applied: 400,
        reserved_credits_micro: 1_400,
        minimal_generation_floor_applied: 50,
    }
}

#[test]
fn credits_round_up_per_component() {
    // 1 token at 1.5 credits/token rounds up to 2 micro-credits per component
    assert_eq!(credits_micro(1, 1, 1_500_000, 1_500_000), Ok(4));
    assert_eq!(credits_micro(1_000, 500, 1_000_000, 2_000_000), Ok(2_000));
    assert_eq!(credits_micro(0, 0, 1, 1), Ok(0));
    assert_eq!(
        credits_micro(42, 7, 3_000_000, 15_000_000),
        Ok(42 * 3 + 7 * 15)
    );
}

#[test]
fn credits_reject_out_of_range() {
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
        credits_micro(1, 1, MAX_MULT + 1, 1),
        Err(CreditError::InvalidMultiplier(MAX_MULT + 1))
    );
    assert!(credits_micro(MAX_TOKENS, MAX_TOKENS, MAX_MULT, MAX_MULT).is_ok());
}

#[test]
fn text_estimate_uses_budgets() {
    let b = budgets();
    // ceil((ceil(10 / 4) + 100) * 110 / 100) = ceil(103 * 1.1) = 114
    assert_eq!(estimate_text_tokens("0123456789", &b), 114);
    // empty text still carries the fixed overhead
    assert_eq!(estimate_text_tokens("", &b), 110);
    // multi-byte characters count by bytes
    assert_eq!(
        estimate_text_tokens("\u{e9}\u{e9}\u{e9}", &b),
        estimate_text_tokens("123456", &b)
    );
}

#[test]
fn estimated_input_includes_surcharges() {
    let b = budgets();
    let base = ReserveInputs {
        message_tokens: 100,
        prior_context_tokens: 50,
        ..ReserveInputs::default()
    };
    assert_eq!(estimated_input_tokens(&base, &b), 150);
    let all = ReserveInputs {
        image_count: 2,
        file_search: true,
        web_search: true,
        code_interpreter: true,
        ..base
    };
    let expected = 150
        + 2 * i64::from(b.image_token_budget)
        + i64::from(b.tool_surcharge_tokens)
        + i64::from(b.web_search_surcharge_tokens)
        + i64::from(b.code_interpreter_surcharge_tokens);
    assert_eq!(estimated_input_tokens(&all, &b), expected);
}

#[test]
fn billing_derivation_table() {
    use BillingOutcome as B;
    use SettlementMethod as S;
    assert_eq!(
        derive_billing("completed", None, false),
        (B::Completed, S::Actual)
    );
    assert_eq!(
        derive_billing("completed", None, true),
        (B::Completed, S::Actual)
    );
    assert_eq!(
        derive_billing("cancelled", None, true),
        (B::Aborted, S::Estimated)
    );
    assert_eq!(
        derive_billing("failed", Some("orphan_timeout"), false),
        (B::Aborted, S::Estimated)
    );
    assert_eq!(
        derive_billing("failed", Some("provider_error"), true),
        (B::Failed, S::Actual)
    );
    assert_eq!(
        derive_billing("failed", Some("provider_error"), false),
        (B::Failed, S::Estimated)
    );
    assert_eq!(
        derive_billing("failed", Some("rate_limited"), false),
        (B::Failed, S::Estimated)
    );
    for code in ["context_length_exceeded", "turn_setup_failed"] {
        assert_eq!(
            derive_billing("failed", Some(code), true),
            (B::Failed, S::Released),
            "{code}"
        );
    }
    assert_eq!(
        derive_billing("failed", Some("something_new"), false),
        (B::Failed, S::Estimated)
    );
    assert_eq!(B::Aborted.as_str(), "aborted");
    assert_eq!(S::Estimated.as_str(), "estimated");
}

#[test]
fn settle_released_charges_nothing() {
    let s = settle(
        SettlementMethod::Released,
        &reserve(),
        Some((10, 10)),
        1_000_000,
        1_000_000,
        1.1,
    )
    .unwrap();
    assert_eq!(s.committed_credits_micro, 0);
    assert_eq!(
        (s.telemetry_input_tokens, s.telemetry_output_tokens),
        (0, 0)
    );
}

#[test]
fn settle_estimated_uses_input_estimate_and_floor() {
    let s = settle(
        SettlementMethod::Estimated,
        &reserve(),
        None,
        2_000_000,
        3_000_000,
        1.1,
    )
    .unwrap();
    // estimated input = 1000 - 400 = 600; output floor 50
    assert_eq!(s.committed_credits_micro, 600 * 2 + 50 * 3);
    assert!(!s.overshoot);
}

#[test]
fn settle_actual_within_tolerance() {
    let s = settle(
        SettlementMethod::Actual,
        &reserve(),
        Some((800, 250)),
        1_000_000,
        1_000_000,
        1.1,
    )
    .unwrap();
    // 1050 tokens / 1000 reserved = 1.05 <= 1.10: actual credits committed
    assert!(s.overshoot);
    assert!(!s.overshoot_capped);
    assert_eq!(s.committed_credits_micro, 1_050);
    assert_eq!(
        (s.telemetry_input_tokens, s.telemetry_output_tokens),
        (800, 250)
    );
}

#[test]
fn settle_actual_overshoot_is_capped_at_reserve() {
    let s = settle(
        SettlementMethod::Actual,
        &reserve(),
        Some((5_000, 0)),
        1_000_000,
        1_000_000,
        1.1,
    )
    .unwrap();
    assert!(s.overshoot && s.overshoot_capped);
    assert_eq!(s.committed_credits_micro, 1_400);
    assert_eq!(s.telemetry_input_tokens, 5_000);
}

#[test]
fn settle_actual_without_usage_is_zero() {
    let s = settle(
        SettlementMethod::Actual,
        &reserve(),
        None,
        1_000_000,
        1_000_000,
        1.1,
    )
    .unwrap();
    assert_eq!(s.committed_credits_micro, 0);
}
