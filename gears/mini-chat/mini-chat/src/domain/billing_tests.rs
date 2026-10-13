#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::{BillingOutcome, SettlementMethod, TokenUsage, UsageTerminalState};

use super::{SettlementBasis, derive_billing, settle, usage_known};

fn usage(input: i64, output: i64) -> TokenUsage {
    TokenUsage {
        input_tokens: input,
        output_tokens: output,
        ..TokenUsage::default()
    }
}

fn basis() -> SettlementBasis {
    SettlementBasis {
        reserve_tokens: 4209,
        max_output_tokens_applied: 4096,
        reserved_credits_micro: 12_401,
        minimal_generation_floor_applied: 50,
        in_mult: 1_000_000,
        out_mult: 3_000_000,
    }
}

#[test]
fn every_row_of_the_derivation_table_is_honoured() {
    use BillingOutcome::{Aborted, Completed, Failed};
    use SettlementMethod::{Actual, Estimated, Released};
    let known = usage(5, 1);
    let zero = usage(0, 0);
    let cases = [
        (UsageTerminalState::Completed, None, None, Completed, Actual),
        (
            UsageTerminalState::Completed,
            None,
            Some(&zero),
            Completed,
            Actual,
        ),
        (
            UsageTerminalState::Failed,
            Some("provider_error"),
            Some(&known),
            Failed,
            Actual,
        ),
        (
            UsageTerminalState::Failed,
            Some("provider_error"),
            None,
            Failed,
            Estimated,
        ),
        (
            UsageTerminalState::Failed,
            Some("provider_timeout"),
            Some(&zero),
            Failed,
            Estimated,
        ),
        (
            UsageTerminalState::Failed,
            Some("rate_limited"),
            None,
            Failed,
            Estimated,
        ),
        (
            UsageTerminalState::Failed,
            Some("web_search_calls_exceeded"),
            None,
            Failed,
            Estimated,
        ),
        (
            UsageTerminalState::Failed,
            Some("code_interpreter_calls_exceeded"),
            Some(&known),
            Failed,
            Actual,
        ),
        (
            UsageTerminalState::Failed,
            Some("message_persistence_failed"),
            Some(&known),
            Failed,
            Actual,
        ),
        (
            UsageTerminalState::Failed,
            Some("context_length_exceeded"),
            None,
            Failed,
            Released,
        ),
        (
            UsageTerminalState::Failed,
            Some("turn_setup_failed"),
            None,
            Failed,
            Released,
        ),
        (
            UsageTerminalState::Failed,
            Some("orphan_timeout"),
            None,
            Aborted,
            Estimated,
        ),
        (
            UsageTerminalState::Cancelled,
            None,
            Some(&known),
            Aborted,
            Estimated,
        ),
        (
            UsageTerminalState::Failed,
            Some("something_new"),
            None,
            Failed,
            Estimated,
        ),
    ];

    for (terminal, code, usage, outcome, method) in cases {
        assert_eq!(
            derive_billing(terminal, code, usage),
            (outcome, method),
            "{terminal:?} {code:?}"
        );
    }
}

#[test]
fn usage_is_known_only_with_a_non_zero_count() {
    assert!(!usage_known(None));
    assert!(!usage_known(Some(&usage(0, 0))));
    assert!(usage_known(Some(&usage(0, 1))));
}

#[test]
fn an_actual_settlement_charges_the_provider_usage() {
    let settlement = settle(
        &basis(),
        SettlementMethod::Actual,
        Some(&usage(10, 5)),
        1.10,
    )
    .unwrap();

    assert_eq!(settlement.committed_credits_micro, 25);
    assert_eq!(
        (
            settlement.telemetry_input_tokens,
            settlement.telemetry_output_tokens
        ),
        (10, 5)
    );
    assert!(!settlement.overshoot);
}

#[test]
fn an_estimated_settlement_charges_the_estimated_input_and_the_floor() {
    let settlement = settle(&basis(), SettlementMethod::Estimated, None, 1.10).unwrap();

    assert_eq!(settlement.committed_credits_micro, 263);
    assert_eq!(settlement.telemetry_input_tokens, 0);
}

#[test]
fn a_released_settlement_charges_nothing() {
    let settlement = settle(&basis(), SettlementMethod::Released, None, 1.10).unwrap();

    assert_eq!(settlement.committed_credits_micro, 0);
}

#[test]
fn an_overshoot_within_the_tolerance_is_charged_in_full() {
    // 4209 * 1.05 = 4419.45 -> 4400 tokens
    let settlement = settle(
        &basis(),
        SettlementMethod::Actual,
        Some(&usage(400, 4000)),
        1.10,
    )
    .unwrap();

    assert!(settlement.overshoot);
    assert_eq!(settlement.committed_credits_micro, 400 + 12_000);
}

#[test]
fn an_overshoot_beyond_the_tolerance_is_capped_at_the_reserve() {
    let settlement = settle(
        &basis(),
        SettlementMethod::Actual,
        Some(&usage(2000, 4000)),
        1.10,
    )
    .unwrap();

    assert!(settlement.overshoot);
    assert_eq!(settlement.committed_credits_micro, 12_401);
    assert_eq!(settlement.telemetry_input_tokens, 2000);
}

#[test]
fn a_zero_multiplier_fails_the_settlement() {
    let mut zero = basis();
    zero.in_mult = 0;

    assert!(settle(&zero, SettlementMethod::Actual, Some(&usage(1, 1)), 1.10).is_err());
}
