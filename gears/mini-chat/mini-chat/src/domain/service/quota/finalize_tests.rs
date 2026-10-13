#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 4: the billing outcome table of
//! `cpt-cf-mini-chat-algo-quota-billing-billing-outcome` and the request
//! constructors of the finalization contract
//! (`cpt-cf-mini-chat-algo-quota-billing-finalize-turn`). The transactional
//! behaviour is tested on `SQLite` in `infra::db::repos::quota_finalize_tests`.

use mini_chat_sdk::{BillingOutcome, SettlementMethod, TerminalState, UsageTokens};
use uuid::Uuid;

use super::{
    BillingDerivation, FinalizeRequest, FinalizeVariant, ORPHAN_TIMEOUT, TerminalClassification,
    derive_billing_outcome,
};
use crate::domain::service::quota::policy::SettlementSnapshot;
use crate::domain::service::quota::settle::ToolCallCounts;
use crate::domain::service::quota::test_support::{noon, utc};

fn usage(input: u64, output: u64, cache: u64, reasoning: u64) -> UsageTokens {
    UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cache,
        cache_write_input_tokens: cache,
        reasoning_tokens: reasoning,
    }
}

fn failed(code: &str) -> TerminalClassification {
    TerminalClassification::Failed {
        error_code: code.to_owned(),
    }
}

/// The usage objects a provider can report, with whether a count above 0 is
/// among `input_tokens` and `output_tokens`.
fn usages() -> Vec<(Option<UsageTokens>, bool)> {
    vec![
        (None, false),
        (Some(usage(0, 0, 0, 0)), false),
        (Some(usage(0, 0, 500, 500)), false),
        (Some(usage(10, 0, 0, 0)), true),
        (Some(usage(0, 10, 0, 0)), true),
        (Some(usage(900, 300, 1, 1)), true),
    ]
}

const POST_PROVIDER: [&str; 8] = [
    "provider_error",
    "provider_timeout",
    "rate_limited",
    "web_search_calls_exceeded",
    "code_interpreter_calls_exceeded",
    "agentic_iterations_exceeded",
    "unexpected_tool_use",
    "message_persistence_failed",
];

const PRE_PROVIDER: [&str; 4] = [
    "context_length_exceeded",
    "validation_error",
    "input_too_long",
    "turn_setup_failed",
];

#[test]
fn a_completed_turn_settles_actual_whatever_the_usage() {
    for (usage, _) in usages() {
        let derived = derive_billing_outcome(&TerminalClassification::Completed, usage.as_ref());

        assert_eq!(
            derived,
            BillingDerivation {
                terminal_state: TerminalState::Completed,
                billing_outcome: BillingOutcome::Completed,
                settlement_method: SettlementMethod::Actual,
                unknown_error_code: false,
            },
            "{usage:?}"
        );
    }
}

#[test]
fn a_post_provider_failure_settles_actual_only_with_a_nonzero_token_count() {
    for code in POST_PROVIDER {
        for (usage, has_tokens) in usages() {
            let derived = derive_billing_outcome(&failed(code), usage.as_ref());

            assert_eq!(derived.terminal_state, TerminalState::Failed, "{code}");
            assert_eq!(derived.billing_outcome, BillingOutcome::Failed, "{code}");
            assert_eq!(
                derived.settlement_method,
                if has_tokens {
                    SettlementMethod::Actual
                } else {
                    SettlementMethod::Estimated
                },
                "{code} {usage:?}"
            );
            assert!(!derived.unknown_error_code, "{code}");
        }
    }
}

#[test]
fn a_post_provider_failure_is_never_released() {
    for code in POST_PROVIDER {
        for (usage, _) in usages() {
            assert_ne!(
                derive_billing_outcome(&failed(code), usage.as_ref()).settlement_method,
                SettlementMethod::Released,
                "{code}: the provider was called"
            );
        }
    }
}

#[test]
fn a_pre_provider_failure_releases_the_reserve_whatever_the_usage() {
    for code in PRE_PROVIDER {
        for (usage, _) in usages() {
            let derived = derive_billing_outcome(&failed(code), usage.as_ref());

            assert_eq!(
                derived,
                BillingDerivation {
                    terminal_state: TerminalState::Failed,
                    billing_outcome: BillingOutcome::Failed,
                    settlement_method: SettlementMethod::Released,
                    unknown_error_code: false,
                },
                "{code} {usage:?}"
            );
        }
    }
}

#[test]
fn a_client_disconnect_is_aborted_and_estimated_ignoring_any_usage() {
    for (usage, _) in usages() {
        let derived = derive_billing_outcome(&TerminalClassification::Cancelled, usage.as_ref());

        assert_eq!(
            derived,
            BillingDerivation {
                terminal_state: TerminalState::Cancelled,
                billing_outcome: BillingOutcome::Aborted,
                settlement_method: SettlementMethod::Estimated,
                unknown_error_code: false,
            },
            "{usage:?}"
        );
    }
}

#[test]
fn an_orphan_timeout_is_failed_state_with_an_aborted_estimated_billing_outcome() {
    for (usage, _) in usages() {
        let derived = derive_billing_outcome(&failed(ORPHAN_TIMEOUT), usage.as_ref());

        assert_eq!(
            derived,
            BillingDerivation {
                terminal_state: TerminalState::Failed,
                billing_outcome: BillingOutcome::Aborted,
                settlement_method: SettlementMethod::Estimated,
                unknown_error_code: false,
            },
            "{usage:?}"
        );
    }
}

#[test]
fn any_other_error_code_is_failed_estimated_and_flagged_as_unknown() {
    // `quota_exceeded` never reaches settlement because no reserve exists.
    for code in ["quota_exceeded", "something_new", "", "PROVIDER_ERROR"] {
        for (usage, _) in usages() {
            let derived = derive_billing_outcome(&failed(code), usage.as_ref());

            assert_eq!(
                derived,
                BillingDerivation {
                    terminal_state: TerminalState::Failed,
                    billing_outcome: BillingOutcome::Failed,
                    settlement_method: SettlementMethod::Estimated,
                    unknown_error_code: true,
                },
                "{code:?} {usage:?}"
            );
        }
    }
}

#[test]
fn the_orphan_timeout_constant_is_the_persisted_error_code() {
    assert_eq!(ORPHAN_TIMEOUT, "orphan_timeout");
}

#[test]
fn the_orphan_request_is_a_failed_orphan_timeout_finalization_without_usage() {
    let turn = Uuid::from_u128(0x77);
    let cutoff = utc(2026, 3, 10, 11, 55, 0);

    let request = FinalizeRequest::orphan(turn, cutoff, SettlementSnapshot::Unavailable, noon());

    assert_eq!(request.turn_id, turn);
    assert_eq!(
        request.variant,
        FinalizeVariant::Orphan {
            stale_cutoff: cutoff
        }
    );
    assert_eq!(request.classification, failed(ORPHAN_TIMEOUT));
    assert_eq!(request.columns.state, TerminalState::Failed);
    assert_eq!(request.columns.error_code.as_deref(), Some(ORPHAN_TIMEOUT));
    assert_eq!(request.columns.assistant_message_id, None);
    assert_eq!(request.columns.provider_response_id, None);
    assert_eq!(request.usage, None);
    assert_eq!(request.tool_calls, ToolCallCounts::default());
    assert_eq!(request.snapshot, SettlementSnapshot::Unavailable);
    assert_eq!(request.now, noon());
}
