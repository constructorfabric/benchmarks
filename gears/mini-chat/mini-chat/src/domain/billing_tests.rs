use mini_chat_sdk::{TerminalState, UsageTokens};

use super::{BillingOutcome, Settlement, SettlementMethod, TurnReserve, derive, settle_amount};
use crate::domain::estimation::{CreditError, credits_micro};

fn usage(input: i64, output: i64) -> UsageTokens {
    UsageTokens {
        input_tokens: input,
        output_tokens: output,
        ..UsageTokens::default()
    }
}

#[test]
fn billing_derivation_table() {
    use BillingOutcome::{Aborted, Completed, Failed};
    use SettlementMethod::{Actual, Estimated, Released};
    let some = usage(10, 5);
    let zero = usage(0, 0);

    // completed: always actual, even without usage.
    assert_eq!(
        derive(TerminalState::Completed, None, Some(&some)),
        (Completed, Actual)
    );
    assert_eq!(
        derive(TerminalState::Completed, None, None),
        (Completed, Actual)
    );
    assert_eq!(
        derive(TerminalState::Completed, None, Some(&zero)),
        (Completed, Actual)
    );

    // Provider / mid-turn failures: actual with non-zero usage, else estimated.
    for code in [
        "provider_error",
        "provider_timeout",
        "rate_limited",
        "web_search_calls_exceeded",
        "code_interpreter_calls_exceeded",
        "agentic_iterations_exceeded",
        "unexpected_tool_use",
        "message_persistence_failed",
    ] {
        assert_eq!(
            derive(TerminalState::Failed, Some(code), Some(&some)),
            (Failed, Actual),
            "{code} with usage"
        );
        assert_eq!(
            derive(TerminalState::Failed, Some(code), None),
            (Failed, Estimated),
            "{code} without usage"
        );
        assert_eq!(
            derive(TerminalState::Failed, Some(code), Some(&zero)),
            (Failed, Estimated),
            "{code} with usage {{0,0}}"
        );
    }
    // One non-zero field is enough.
    assert_eq!(
        derive(
            TerminalState::Failed,
            Some("provider_error"),
            Some(&usage(0, 1))
        ),
        (Failed, Actual)
    );

    // Pre-provider failures: released.
    for code in [
        "context_length_exceeded",
        "validation_error",
        "input_too_long",
        "turn_setup_failed",
    ] {
        assert_eq!(
            derive(TerminalState::Failed, Some(code), Some(&some)),
            (Failed, Released),
            "{code}"
        );
    }

    // Aborted: cancelled and orphan timeout, always estimated.
    assert_eq!(
        derive(TerminalState::Cancelled, None, None),
        (Aborted, Estimated)
    );
    assert_eq!(
        derive(TerminalState::Cancelled, None, Some(&some)),
        (Aborted, Estimated)
    );
    assert_eq!(
        derive(TerminalState::Failed, Some("orphan_timeout"), None),
        (Aborted, Estimated)
    );
    assert_eq!(
        derive(TerminalState::Failed, Some("orphan_timeout"), Some(&some)),
        (Aborted, Estimated)
    );

    // Unknown code (or none) on a failed turn: failed / estimated.
    assert_eq!(
        derive(TerminalState::Failed, Some("something_new"), Some(&some)),
        (Failed, Estimated)
    );
    assert_eq!(
        derive(TerminalState::Failed, None, None),
        (Failed, Estimated)
    );
}

const MULTS: (i64, i64) = (100_000_000, 200_000_000);

fn turn() -> TurnReserve {
    // estimated_input_tokens = 10000 - 2000 = 8000.
    TurnReserve {
        reserve_tokens: 10_000,
        max_output_tokens_applied: 2_000,
        reserved_credits_micro: 2_500_000,
        minimal_generation_floor_applied: 50,
    }
}

#[test]
fn settlement_overshoot_cap() {
    // 11500 / 10000 = 1.15 > 1.10: capped at the reserve.
    let s = settle_amount(
        SettlementMethod::Actual,
        &turn(),
        Some(&usage(11_000, 500)),
        1.10,
        MULTS,
    )
    .unwrap();
    assert_eq!(
        s,
        Settlement {
            committed_credits_micro: 2_500_000,
            actual_tokens_for_telemetry: Some((11_000, 500)),
            overshoot: true,
        }
    );

    // 10500 / 10000 = 1.05 <= 1.10: actual credits.
    let s = settle_amount(
        SettlementMethod::Actual,
        &turn(),
        Some(&usage(10_500, 0)),
        1.10,
        MULTS,
    )
    .unwrap();
    assert_eq!(
        s.committed_credits_micro,
        credits_micro(10_500, 0, MULTS.0, MULTS.1).unwrap()
    );
    assert_eq!(s.actual_tokens_for_telemetry, Some((10_500, 0)));
    assert!(s.overshoot);

    // Within the reserve: actual credits, no overshoot.
    let s = settle_amount(
        SettlementMethod::Actual,
        &turn(),
        Some(&usage(3_000, 700)),
        1.10,
        MULTS,
    )
    .unwrap();
    assert_eq!(
        s.committed_credits_micro,
        credits_micro(3_000, 700, MULTS.0, MULTS.1).unwrap()
    );
    assert!(!s.overshoot);

    // Completed without usage: 0 credits.
    let s = settle_amount(SettlementMethod::Actual, &turn(), None, 1.10, MULTS).unwrap();
    assert_eq!(s.committed_credits_micro, 0);
    assert_eq!(s.actual_tokens_for_telemetry, None);

    // Out-of-range provider usage fails the settlement.
    assert_eq!(
        settle_amount(
            SettlementMethod::Actual,
            &turn(),
            Some(&usage(10_000_001, 0)),
            1.10,
            MULTS,
        ),
        Err(CreditError::TokenCountOutOfRange(10_000_001))
    );
}

#[test]
fn estimated_settlement() {
    let s = settle_amount(
        SettlementMethod::Estimated,
        &turn(),
        Some(&usage(9_999, 9_999)),
        1.10,
        MULTS,
    )
    .unwrap();
    assert_eq!(
        s,
        Settlement {
            committed_credits_micro: credits_micro(8_000, 50, MULTS.0, MULTS.1).unwrap(),
            actual_tokens_for_telemetry: None,
            overshoot: false,
        }
    );

    let s = settle_amount(SettlementMethod::Released, &turn(), None, 1.10, MULTS).unwrap();
    assert_eq!(
        s,
        Settlement {
            committed_credits_micro: 0,
            actual_tokens_for_telemetry: None,
            overshoot: false,
        }
    );
}
