use super::*;

use BillingOutcome::{Aborted, Completed, Failed};
use SettlementMethod::{Actual, Estimated, Released};
use TurnTerminalState as S;

#[test]
fn completed_is_always_actual() {
    assert_eq!(
        derive_billing(S::Completed, None, true),
        (Completed, Actual)
    );
}

#[test]
fn completed_with_zero_usage_is_still_actual() {
    assert_eq!(
        derive_billing(S::Completed, None, false),
        (Completed, Actual)
    );
}

#[test]
fn provider_errors_with_usage_are_actual() {
    for code in ["provider_error", "provider_timeout", "rate_limited"] {
        assert_eq!(
            derive_billing(S::Failed, Some(code), true),
            (Failed, Actual)
        );
    }
}

#[test]
fn provider_error_without_usage_is_estimated() {
    for code in ["provider_error", "provider_timeout", "rate_limited"] {
        assert_eq!(
            derive_billing(S::Failed, Some(code), false),
            (Failed, Estimated)
        );
    }
}

#[test]
fn mid_turn_limit_codes_follow_provider_error_settlement() {
    for code in [
        "web_search_calls_exceeded",
        "code_interpreter_calls_exceeded",
        "agentic_iterations_exceeded",
        "unexpected_tool_use",
        "message_persistence_failed",
    ] {
        assert_eq!(
            derive_billing(S::Failed, Some(code), true),
            (Failed, Actual)
        );
        assert_eq!(
            derive_billing(S::Failed, Some(code), false),
            (Failed, Estimated),
            "{code} must never be released: the provider was already called"
        );
    }
}

#[test]
fn context_length_exceeded_is_released() {
    assert_eq!(
        derive_billing(S::Failed, Some("context_length_exceeded"), false),
        (Failed, Released)
    );
}

#[test]
fn pre_provider_codes_are_released_even_with_usage() {
    for code in [
        "context_length_exceeded",
        "validation_error",
        "input_too_long",
        "turn_setup_failed",
    ] {
        assert_eq!(
            derive_billing(S::Failed, Some(code), false),
            (Failed, Released)
        );
        assert_eq!(
            derive_billing(S::Failed, Some(code), true),
            (Failed, Released)
        );
    }
}

#[test]
fn cancelled_is_aborted_estimated() {
    assert_eq!(
        derive_billing(S::Cancelled, None, false),
        (Aborted, Estimated)
    );
    // The cancel path never uses usage the provider reported before the disconnect.
    assert_eq!(
        derive_billing(S::Cancelled, None, true),
        (Aborted, Estimated)
    );
}

#[test]
fn orphan_timeout_is_aborted_estimated() {
    assert_eq!(
        derive_billing(S::Failed, Some("orphan_timeout"), false),
        (Aborted, Estimated)
    );
    assert_eq!(
        derive_billing(S::Failed, Some("orphan_timeout"), true),
        (Aborted, Estimated)
    );
}

#[test]
fn unknown_code_is_failed_estimated() {
    assert_eq!(
        derive_billing(S::Failed, Some("weird_code"), false),
        (Failed, Estimated)
    );
    assert_eq!(
        derive_billing(S::Failed, Some("weird_code"), true),
        (Failed, Estimated)
    );
}

#[test]
fn failed_without_a_code_is_failed_estimated() {
    assert_eq!(derive_billing(S::Failed, None, false), (Failed, Estimated));
}

#[test]
fn outcome_and_method_wire_strings() {
    assert_eq!(Completed.as_str(), "completed");
    assert_eq!(Failed.as_str(), "failed");
    assert_eq!(Aborted.as_str(), "aborted");
    assert_eq!(Actual.as_str(), "actual");
    assert_eq!(Estimated.as_str(), "estimated");
    assert_eq!(Released.as_str(), "released");
}

#[test]
fn terminal_state_converts_from_turn_state() {
    use crate::domain::enums::TurnState;
    assert_eq!(S::try_from(TurnState::Completed), Ok(S::Completed));
    assert_eq!(S::try_from(TurnState::Failed), Ok(S::Failed));
    assert_eq!(S::try_from(TurnState::Cancelled), Ok(S::Cancelled));
    assert!(S::try_from(TurnState::Running).is_err());
}

#[test]
fn known_error_codes_are_the_classified_ones() {
    for code in [
        "provider_error",
        "provider_timeout",
        "rate_limited",
        "web_search_calls_exceeded",
        "code_interpreter_calls_exceeded",
        "agentic_iterations_exceeded",
        "unexpected_tool_use",
        "message_persistence_failed",
        "context_length_exceeded",
        "validation_error",
        "input_too_long",
        "turn_setup_failed",
        "orphan_timeout",
    ] {
        assert!(is_known_error_code(code), "{code}");
    }
    // not settled by the derivation (no reserve exists) and unknown codes
    for code in [
        "quota_exceeded",
        "finalization_failed",
        "",
        "PROVIDER_ERROR",
    ] {
        assert!(!is_known_error_code(code), "{code}");
    }
}
