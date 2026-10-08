#![allow(clippy::unwrap_used)]

use mini_chat_sdk::UsageTokens;

use super::{Reserve, Terminal, classify, settle};

fn usage(i: i64, o: i64) -> UsageTokens {
    UsageTokens { input_tokens: i, output_tokens: o, ..UsageTokens::default() }
}

#[test]
fn derivation_table() {
    let c = classify(&Terminal::Completed, None);
    assert_eq!((c.billing_outcome, c.method), ("completed", "actual"));
    for code in ["provider_error", "provider_timeout", "rate_limited", "web_search_calls_exceeded",
        "code_interpreter_calls_exceeded", "agentic_iterations_exceeded", "unexpected_tool_use", "message_persistence_failed"] {
        let c = classify(&Terminal::Failed(code.into()), Some(&usage(3, 0)));
        assert_eq!((c.billing_outcome, c.method), ("failed", "actual"), "{code}");
        let c = classify(&Terminal::Failed(code.into()), Some(&usage(0, 0)));
        assert_eq!((c.billing_outcome, c.method), ("failed", "estimated"), "{code}");
        let c = classify(&Terminal::Failed(code.into()), None);
        assert_eq!(c.method, "estimated");
    }
    for code in ["context_length_exceeded", "validation_error", "input_too_long", "turn_setup_failed"] {
        let c = classify(&Terminal::Failed(code.into()), None);
        assert_eq!((c.billing_outcome, c.method), ("failed", "released"));
    }
    let c = classify(&Terminal::Cancelled, Some(&usage(5, 5)));
    assert_eq!((c.billing_outcome, c.method), ("aborted", "estimated"));
    let c = classify(&Terminal::Orphan, None);
    assert_eq!((c.billing_outcome, c.method), ("aborted", "estimated"));
    let c = classify(&Terminal::Failed("weird".into()), Some(&usage(5, 5)));
    assert_eq!((c.billing_outcome, c.method), ("failed", "estimated"));
}

const R: Reserve = Reserve {
    reserve_tokens: 10_000,
    max_output_tokens_applied: 2_000,
    reserved_credits_micro: 2_500_000,
    minimal_generation_floor_applied: 50,
};

#[test]
fn actual_within_and_beyond_tolerance() {
    // design example: 11000 + 500 = 1.15 > 1.10 -> capped at reserve
    let s = settle("actual", R, Some(&usage(11_000, 500)), 1_000_000, 1_000_000, 1.10).unwrap();
    assert!(s.overshoot && s.overshoot_capped);
    assert_eq!(s.committed_credits_micro, 2_500_000);
    assert_eq!(s.telemetry_input_tokens, 11_000);
    // 1.05 within tolerance -> actual credits
    let s = settle("actual", R, Some(&usage(10_000, 500)), 1_000_000, 1_000_000, 1.10).unwrap();
    assert!(s.overshoot && !s.overshoot_capped);
    assert_eq!(s.committed_credits_micro, 10_000 + 500);
    // no usage -> zero charge on actual
    let s = settle("actual", R, None, 1_000_000, 1_000_000, 1.10).unwrap();
    assert_eq!(s.committed_credits_micro, 0);
}

#[test]
fn estimated_and_released() {
    let s = settle("estimated", R, None, 1_000_000_000, 1_000_000_000, 1.1).unwrap();
    // est input = 8000, floor 50 -> (8000 + 50) * 1000 micro
    assert_eq!(s.committed_credits_micro, 8_050_000);
    assert_eq!(s.telemetry_input_tokens, 0);
    assert!(s.count_tool_calls);
    let s = settle("released", R, None, 1, 1, 1.1).unwrap();
    assert_eq!(s.committed_credits_micro, 0);
    assert!(!s.count_tool_calls);
    assert!(settle("estimated", R, None, 0, 1, 1.1).is_err());
}
