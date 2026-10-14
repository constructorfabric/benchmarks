//! Billing outcome derivation tests: every row of the authoritative mapping table of DESIGN
//! 5.8, the usage-known rule, the unknown code, the absent outcome of `quota_exceeded` and the
//! rule that the outcome is derived from the trigger and the code, never from the state string
//! alone. Also the turn billing state machine vocabulary.
//!
//! Slice S1-all: algorithm `cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation`,
//! state `cpt-cf-mini-chat-state-quota-and-billing-turn-billing`, definition of done
//! `cpt-cf-mini-chat-dod-quota-and-billing-settlement-outcomes`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::UsageTokens;
use tracing_test::traced_test;

use super::{BillingOutcome, SettlementMethod, TerminalState, derive_billing};
use crate::test_support::quota::usage;

const PROVIDER_CODES: [&str; 3] = ["provider_error", "provider_timeout", "rate_limited"];
const TOOL_CODES: [&str; 5] = [
    "web_search_calls_exceeded",
    "code_interpreter_calls_exceeded",
    "agentic_iterations_exceeded",
    "unexpected_tool_use",
    "message_persistence_failed",
];
const PRE_PROVIDER_CODES: [&str; 4] = [
    "context_length_exceeded",
    "validation_error",
    "input_too_long",
    "turn_setup_failed",
];

fn derive(
    state: TerminalState,
    code: Option<&str>,
    usage: Option<&UsageTokens>,
) -> (BillingOutcome, SettlementMethod) {
    let derived = derive_billing(state, code, usage).expect("a billing outcome");
    (derived.outcome, derived.method)
}

#[test]
fn the_wire_values_are_the_usage_event_values() {
    assert_eq!(TerminalState::Completed.as_str(), "completed");
    assert_eq!(TerminalState::Failed.as_str(), "failed");
    assert_eq!(TerminalState::Cancelled.as_str(), "cancelled");
    assert_eq!(BillingOutcome::Completed.as_str(), "completed");
    assert_eq!(BillingOutcome::Failed.as_str(), "failed");
    assert_eq!(BillingOutcome::Aborted.as_str(), "aborted");
    assert_eq!(SettlementMethod::Actual.as_str(), "actual");
    assert_eq!(SettlementMethod::Estimated.as_str(), "estimated");
    assert_eq!(SettlementMethod::Released.as_str(), "released");
}

// ── completed ────────────────────────────────────────────────────────────────

#[test]
fn a_completed_turn_is_always_actual() {
    let reported = usage(900, 300);
    for usage in [Some(&reported), None] {
        assert_eq!(
            derive(TerminalState::Completed, None, usage),
            (BillingOutcome::Completed, SettlementMethod::Actual)
        );
    }
}

#[test]
fn a_completed_turn_with_zero_usage_is_still_actual() {
    let zero = usage(0, 0);
    assert_eq!(
        derive(TerminalState::Completed, None, Some(&zero)),
        (BillingOutcome::Completed, SettlementMethod::Actual)
    );
}

// ── failed after the provider call started ───────────────────────────────────

#[test]
fn provider_and_tool_failures_are_actual_with_known_usage_and_estimated_without() {
    let known = usage(10, 0);
    let known_output = usage(0, 5);
    let zero = usage(0, 0);
    for code in PROVIDER_CODES.iter().chain(TOOL_CODES.iter()) {
        for usage in [Some(&known), Some(&known_output)] {
            assert_eq!(
                derive(TerminalState::Failed, Some(code), usage),
                (BillingOutcome::Failed, SettlementMethod::Actual),
                "{code}"
            );
        }
        for usage in [None, Some(&zero)] {
            assert_eq!(
                derive(TerminalState::Failed, Some(code), usage),
                (BillingOutcome::Failed, SettlementMethod::Estimated),
                "{code}: usage with both fields zero is unknown"
            );
        }
    }
}

#[test]
fn a_tool_limit_failure_is_never_released_because_the_provider_was_already_called() {
    for code in TOOL_CODES {
        let derived = derive_billing(TerminalState::Failed, Some(code), None).unwrap();
        assert_ne!(derived.method, SettlementMethod::Released, "{code}");
    }
}

// ── failed before the provider call ──────────────────────────────────────────

#[test]
fn pre_provider_failures_are_released_whatever_the_usage() {
    let known = usage(100, 100);
    for code in PRE_PROVIDER_CODES {
        for usage in [None, Some(&known)] {
            assert_eq!(
                derive(TerminalState::Failed, Some(code), usage),
                (BillingOutcome::Failed, SettlementMethod::Released),
                "{code}"
            );
        }
    }
}

// ── aborted ──────────────────────────────────────────────────────────────────

#[test]
fn a_cancelled_turn_is_aborted_and_estimated_even_when_usage_is_passed() {
    let known = usage(500, 500);
    for usage in [None, Some(&known)] {
        assert_eq!(
            derive(TerminalState::Cancelled, None, usage),
            (BillingOutcome::Aborted, SettlementMethod::Estimated)
        );
    }
}

#[test]
fn an_orphan_timeout_is_aborted_and_estimated_though_its_state_is_failed() {
    let known = usage(500, 500);
    for usage in [None, Some(&known)] {
        assert_eq!(
            derive(TerminalState::Failed, Some("orphan_timeout"), usage),
            (BillingOutcome::Aborted, SettlementMethod::Estimated)
        );
    }
}

#[test]
fn the_outcome_follows_the_code_not_the_state_string_alone() {
    let failed_provider = derive(TerminalState::Failed, Some("provider_error"), None);
    let failed_orphan = derive(TerminalState::Failed, Some("orphan_timeout"), None);
    assert_eq!(failed_provider.0, BillingOutcome::Failed);
    assert_eq!(failed_orphan.0, BillingOutcome::Aborted);
    assert_ne!(failed_provider, failed_orphan);
}

// ── no billing outcome and unknown codes ─────────────────────────────────────

#[test]
fn quota_exceeded_has_no_billing_outcome_because_no_reserve_exists() {
    assert_eq!(
        derive_billing(TerminalState::Failed, Some("quota_exceeded"), None),
        None
    );
    let known = usage(10, 10);
    assert_eq!(
        derive_billing(TerminalState::Failed, Some("quota_exceeded"), Some(&known)),
        None
    );
}

#[traced_test]
#[test]
fn an_unknown_code_is_failed_and_estimated_flagged_and_logged_as_a_critical_error() {
    let derived = derive_billing(TerminalState::Failed, Some("brand_new_code"), None).unwrap();
    assert_eq!(derived.outcome, BillingOutcome::Failed);
    assert_eq!(derived.method, SettlementMethod::Estimated);
    assert!(derived.unknown_error_code);
    assert!(logs_contain("brand_new_code"));
}

#[test]
fn an_unknown_code_is_estimated_even_with_known_usage() {
    let known = usage(10, 10);
    let derived =
        derive_billing(TerminalState::Failed, Some("brand_new_code"), Some(&known)).unwrap();
    assert_eq!(derived.method, SettlementMethod::Estimated);
    assert!(derived.unknown_error_code);
}

#[test]
fn a_known_code_is_not_flagged() {
    for code in PROVIDER_CODES
        .iter()
        .chain(TOOL_CODES.iter())
        .chain(PRE_PROVIDER_CODES.iter())
        .chain(["orphan_timeout"].iter())
    {
        let derived = derive_billing(TerminalState::Failed, Some(code), None).unwrap();
        assert!(!derived.unknown_error_code, "{code}");
    }
    assert!(
        !derive_billing(TerminalState::Cancelled, None, None)
            .unwrap()
            .unknown_error_code
    );
    assert!(
        !derive_billing(TerminalState::Completed, None, None)
            .unwrap()
            .unknown_error_code
    );
}

// ── the turn billing state machine ───────────────────────────────────────────

#[test]
fn billing_state_completed_failed_and_aborted_are_the_three_terminal_derivations() {
    // IN_PROGRESS -> COMPLETED
    assert_eq!(
        derive(TerminalState::Completed, None, None).0,
        BillingOutcome::Completed
    );
    // IN_PROGRESS -> FAILED: a provider error, a tool limit, a persistence failure, a
    // pre-provider error after the reserve was taken
    for code in PROVIDER_CODES
        .iter()
        .chain(TOOL_CODES.iter())
        .chain(PRE_PROVIDER_CODES.iter())
    {
        assert_eq!(
            derive(TerminalState::Failed, Some(code), None).0,
            BillingOutcome::Failed,
            "{code}"
        );
    }
    // IN_PROGRESS -> ABORTED: a disconnect or the orphan watchdog timeout
    assert_eq!(
        derive(TerminalState::Cancelled, None, None).0,
        BillingOutcome::Aborted
    );
    assert_eq!(
        derive(TerminalState::Failed, Some("orphan_timeout"), None).0,
        BillingOutcome::Aborted
    );
}
