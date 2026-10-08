use mini_chat_sdk::EstimationBudgets;

use super::estimate_text_tokens;

#[test]
fn formula_matches_design() {
    let b = EstimationBudgets::default(); // bpt 4, overhead 100, margin 10
    // empty message: overhead only, plus margin
    assert_eq!(estimate_text_tokens(0, &b), 110);
    // 401 bytes -> ceil(401/4)=101 + 100 = 201 -> ceil(201*1.1)=222
    assert_eq!(estimate_text_tokens(401, &b), 222);
    let zero = EstimationBudgets {
        bytes_per_token_conservative: 0,
        fixed_overhead_tokens: 0,
        safety_margin_pct: 0,
        ..EstimationBudgets::default()
    };
    // bpt 0 is clamped to 1
    assert_eq!(estimate_text_tokens(7, &zero), 7);
}
