use super::*;

#[test]
fn per_component_ceiling() {
    // 1 token at 1 micro-credit per 1M tokens rounds up to 1 per component.
    assert_eq!(credits_micro_checked(1, 1, 1, 1).unwrap(), 2);
    assert_eq!(credits_micro_checked(0, 0, 5, 5).unwrap(), 0);
    // 1000 input * 3_000_000 / 1e6 = 3000; 500 output * 15_000_000 / 1e6 = 7500
    assert_eq!(credits_micro_checked(1000, 500, 3_000_000, 15_000_000).unwrap(), 10_500);
    // sum-vs-component rounding difference is intentional
    assert_eq!(credits_micro_checked(1, 1, 500_000, 500_000).unwrap(), 2);
}

#[test]
fn bounds_are_enforced() {
    assert_eq!(credits_micro_checked(-1, 0, 1, 1), Err(CreditError::InvalidTokenCount(-1)));
    assert_eq!(
        credits_micro_checked(MAX_TOKENS + 1, 0, 1, 1),
        Err(CreditError::InvalidTokenCount(MAX_TOKENS + 1))
    );
    assert_eq!(credits_micro_checked(1, 1, 0, 1), Err(CreditError::ZeroMultiplier));
    assert_eq!(
        credits_micro_checked(1, 1, 1, MAX_MULTIPLIER + 1),
        Err(CreditError::InvalidMultiplier(MAX_MULTIPLIER + 1))
    );
    assert!(credits_micro_checked(MAX_TOKENS, MAX_TOKENS, MAX_MULTIPLIER, MAX_MULTIPLIER).is_ok());
}

#[test]
fn text_estimate_formula() {
    let b = EstimationBudgets::default(); // 4 bytes/token, 100 overhead, 10%
    // empty: ceil((0 + 100) * 110 / 100) = 110
    assert_eq!(estimate_text_tokens(0, &b), 110);
    // 401 bytes: ceil(401/4)=101; (101+100)*110/100 = 221.1 -> 222
    assert_eq!(estimate_text_tokens(401, &b), 222);
    let zero_bpt = EstimationBudgets { bytes_per_token_conservative: 0, ..b };
    assert_eq!(estimate_text_tokens(10, &zero_bpt), estimate_text_tokens(10, &EstimationBudgets { bytes_per_token_conservative: 1, ..EstimationBudgets::default() }));
}

#[test]
fn reserve_includes_surcharges_and_caps_output() {
    let b = EstimationBudgets::default();
    let inputs = ReserveInputs {
        message_bytes: 0,
        prior_context_tokens: 1000,
        image_count: 2,
        tools: ToolFlags { file_search: true, web_search: true, code_interpreter: true },
    };
    let r = compute_reserve(&inputs, &b, 32_768, 4_096, 1_000_000, 3_000_000);
    assert_eq!(r.estimated_input_tokens, 110 + 1000 + 2000 + 500 + 500 + 1000);
    assert_eq!(r.max_output_tokens_applied, 4096);
    assert_eq!(r.reserve_tokens, r.estimated_input_tokens + 4096);
    assert_eq!(
        r.reserved_credits_micro,
        credits_micro_checked(r.estimated_input_tokens, 4096, 1_000_000, 3_000_000).unwrap()
    );
    let none = compute_reserve(&ReserveInputs::default(), &b, 100, 4096, 1, 1);
    assert_eq!(none.estimated_input_tokens, 110);
    assert_eq!(none.max_output_tokens_applied, 100);
}

#[test]
fn reserve_with_zero_multiplier_is_unavailable() {
    let r = compute_reserve(&ReserveInputs::default(), &EstimationBudgets::default(), 10, 10, 0, 1);
    assert_eq!(r.reserved_credits_micro, i64::MAX);
}

#[test]
fn estimated_settlement_uses_floor() {
    // reserve 1110 tokens = 110 input + 1000 output; floor 50
    let c = estimated_settlement_credits(1110, 1000, 50, 1_000_000, 1_000_000).unwrap();
    assert_eq!(c, credits_micro_checked(110, 50, 1_000_000, 1_000_000).unwrap());
}
