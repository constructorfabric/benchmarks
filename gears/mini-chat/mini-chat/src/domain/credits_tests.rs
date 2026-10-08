use super::*;

#[test]
fn per_component_ceiling() {
    // 1 token * 1 micro-credit-per-1M rounds up to 1 per component.
    assert_eq!(credits_micro(1, 1, 1, 1).unwrap(), 2);
    assert_eq!(credits_micro(0, 0, 5, 5).unwrap(), 0);
    // DESIGN 5.10 example: premium reserve 3_750_000.
    assert_eq!(credits_micro(1000, 500, 2_500_000_000, 2_500_000_000).unwrap(), 3_750_000);
    // standard actual: 900 + 300 tokens at 1.0 credit/1K.
    assert_eq!(credits_micro(900, 300, 1_000_000_000, 1_000_000_000).unwrap(), 1_200_000);
}

#[test]
fn validation_bounds() {
    assert_eq!(credits_micro(-1, 0, 1, 1), Err(CreditsError::InvalidTokenCount(-1)));
    assert_eq!(credits_micro(10_000_001, 0, 1, 1), Err(CreditsError::InvalidTokenCount(10_000_001)));
    assert_eq!(credits_micro(1, 1, 0, 1), Err(CreditsError::ZeroMultiplier));
    assert_eq!(credits_micro(1, 1, 1, 10_000_000_001), Err(CreditsError::InvalidMultiplier(10_000_000_001)));
    assert!(credits_micro(10_000_000, 10_000_000, 10_000_000_000, 10_000_000_000).is_ok());
}

#[test]
fn text_estimate_formula() {
    let b = EstimationBudgets { bytes_per_token_conservative: 4, fixed_overhead_tokens: 100, safety_margin_pct: 10, ..EstimationBudgets::default() };
    // ceil(10/4)=3 ; (3+100)=103 ; *110/100 = 113.3 -> 114
    assert_eq!(estimate_text_tokens(10, &b), 114);
    // empty message: fixed overhead with margin
    assert_eq!(estimate_text_tokens(0, &b), 110);
    let zero = EstimationBudgets { bytes_per_token_conservative: 0, ..b };
    assert!(estimate_text_tokens(8, &zero) > 0, "bpt 0 clamps to 1");
}

#[test]
fn surcharges_sum() {
    let b = EstimationBudgets::default();
    let s = Surcharges { file_search: true, web_search: true, code_interpreter: true, images: 2 };
    assert_eq!(s.tokens(&b), 2 * 1000 + 500 + 500 + 1000);
    assert_eq!(s.tool_tokens(&b), 2000);
}

#[test]
fn reserve_combines_parts() {
    let b = EstimationBudgets::default();
    let r = reserve_estimate(0, 50, Surcharges::default(), &b, 100, 1_000_000, 1_000_000);
    assert_eq!(r.estimated_input_tokens, 110 + 50);
    assert_eq!(r.reserve_tokens, 260);
    assert_eq!(r.reserved_credits_micro, Some(160 + 100));
    let bad = reserve_estimate(0, 0, Surcharges::default(), &b, 100, 0, 1);
    assert_eq!(bad.reserved_credits_micro, None);
}
