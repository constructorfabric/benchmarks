#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::{QuotaDecision, TurnAuditEventType, UsageTokens};
use serde_json::json;
use time::macros::datetime;
use uuid::Uuid;

use super::*;
use crate::domain::ports::TurnState;

fn usage(input: i64, output: i64) -> UsageTokens {
    UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: 3,
        cache_write_input_tokens: 2,
        reasoning_tokens: 1,
    }
}

fn decision(outcome: BillingOutcome, method: SettlementMethod) -> BillingDecision {
    BillingDecision {
        outcome,
        method,
        unknown_error_code: false,
    }
}

const ACTUAL_FAILED: BillingDecision = BillingDecision {
    outcome: BillingOutcome::Failed,
    method: SettlementMethod::Actual,
    unknown_error_code: false,
};
const ESTIMATED_FAILED: BillingDecision = BillingDecision {
    outcome: BillingOutcome::Failed,
    method: SettlementMethod::Estimated,
    unknown_error_code: false,
};
const ABORTED: BillingDecision = BillingDecision {
    outcome: BillingOutcome::Aborted,
    method: SettlementMethod::Estimated,
    unknown_error_code: false,
};
const UNKNOWN: BillingDecision = BillingDecision {
    outcome: BillingOutcome::Failed,
    method: SettlementMethod::Estimated,
    unknown_error_code: true,
};

// ── Authoritative mapping table (one test per row) ───────────────────────

#[test]
fn completed_is_always_actual_even_without_usage() {
    let expected = decision(BillingOutcome::Completed, SettlementMethod::Actual);
    for reported in [None, Some(usage(0, 0)), Some(usage(10, 20))] {
        assert_eq!(
            derive_billing(TerminalState::Completed, None, reported.as_ref()),
            expected
        );
    }
}

#[test]
fn provider_failures_settle_actual_with_usage_and_estimated_without() {
    for code in ["provider_error", "provider_timeout", "rate_limited"] {
        assert_eq!(
            derive_billing(TerminalState::Failed, Some(code), Some(&usage(10, 0))),
            ACTUAL_FAILED,
            "{code} with usage"
        );
        assert_eq!(
            derive_billing(TerminalState::Failed, Some(code), Some(&usage(0, 5))),
            ACTUAL_FAILED,
            "{code} with output only"
        );
        assert_eq!(
            derive_billing(TerminalState::Failed, Some(code), None),
            ESTIMATED_FAILED,
            "{code} without usage"
        );
        assert_eq!(
            derive_billing(TerminalState::Failed, Some(code), Some(&usage(0, 0))),
            ESTIMATED_FAILED,
            "{code} with all-zero usage"
        );
    }
}

#[test]
fn mid_turn_limit_and_persistence_failures_settle_actual_or_estimated() {
    for code in [
        "web_search_calls_exceeded",
        "code_interpreter_calls_exceeded",
        "agentic_iterations_exceeded",
        "unexpected_tool_use",
        "message_persistence_failed",
    ] {
        assert_eq!(
            derive_billing(TerminalState::Failed, Some(code), Some(&usage(7, 1))),
            ACTUAL_FAILED,
            "{code} with usage"
        );
        assert_eq!(
            derive_billing(TerminalState::Failed, Some(code), None),
            ESTIMATED_FAILED,
            "{code} without usage (never released)"
        );
    }
}

#[test]
fn pre_provider_failures_are_released_whatever_the_usage() {
    let expected = decision(BillingOutcome::Failed, SettlementMethod::Released);
    for code in [
        "context_length_exceeded",
        "validation_error",
        "input_too_long",
        "turn_setup_failed",
    ] {
        assert_eq!(
            derive_billing(TerminalState::Failed, Some(code), None),
            expected
        );
        assert_eq!(
            derive_billing(TerminalState::Failed, Some(code), Some(&usage(9, 9))),
            expected
        );
    }
}

#[test]
fn cancelled_is_aborted_and_estimated_even_with_usage() {
    assert_eq!(
        derive_billing(TerminalState::Cancelled, None, None),
        ABORTED
    );
    assert_eq!(
        derive_billing(TerminalState::Cancelled, None, Some(&usage(100, 100))),
        ABORTED
    );
}

#[test]
fn orphan_timeout_is_aborted_and_estimated() {
    assert_eq!(
        derive_billing(TerminalState::Failed, Some(ORPHAN_TIMEOUT), None),
        ABORTED
    );
    assert_eq!(
        derive_billing(
            TerminalState::Failed,
            Some("orphan_timeout"),
            Some(&usage(4, 4))
        ),
        ABORTED
    );
}

#[test]
fn unknown_or_missing_codes_are_failed_estimated_and_flagged() {
    for code in [Some("something_new"), Some("quota_exceeded"), None] {
        assert_eq!(
            derive_billing(TerminalState::Failed, code, Some(&usage(10, 10))),
            UNKNOWN,
            "{code:?}"
        );
    }
}

// ── Value sets ───────────────────────────────────────────────────────────

#[test]
fn wire_values_are_the_allowed_enum_values() {
    assert_eq!(BillingOutcome::Completed.as_str(), "completed");
    assert_eq!(BillingOutcome::Failed.as_str(), "failed");
    assert_eq!(BillingOutcome::Aborted.as_str(), "aborted");
    assert_eq!(SettlementMethod::Actual.as_str(), "actual");
    assert_eq!(SettlementMethod::Estimated.as_str(), "estimated");
    assert_eq!(SettlementMethod::Released.as_str(), "released");
    assert_eq!(TerminalState::Completed.as_str(), "completed");
    assert_eq!(TerminalState::Failed.as_str(), "failed");
    assert_eq!(TerminalState::Cancelled.as_str(), "cancelled");
}

#[test]
fn terminal_state_maps_turn_states() {
    assert_eq!(TerminalState::from_turn_state(TurnState::Running), None);
    for terminal in [
        TerminalState::Completed,
        TerminalState::Failed,
        TerminalState::Cancelled,
    ] {
        assert_eq!(
            TerminalState::from_turn_state(terminal.turn_state()),
            Some(terminal)
        );
        assert_eq!(terminal.turn_state().as_str(), terminal.as_str());
    }
}

#[test]
fn usage_known_requires_a_non_zero_input_or_output_count() {
    assert!(!usage_known(None));
    assert!(!usage_known(Some(&UsageTokens::default())));
    assert!(!usage_known(Some(&UsageTokens {
        reasoning_tokens: 9,
        ..UsageTokens::default()
    })));
    assert!(usage_known(Some(&usage(1, 0))));
    assert!(usage_known(Some(&usage(0, 1))));
}

// ── Builders ─────────────────────────────────────────────────────────────

fn identity() -> TurnIdentity {
    TurnIdentity {
        tenant_id: Uuid::from_u128(0x7e7a),
        user_id: Uuid::from_u128(0xa11c),
        chat_id: Uuid::from_u128(0xc4a7),
        turn_id: Uuid::from_u128(0x7a2),
        request_id: Uuid::from_u128(0x2e9),
    }
}

fn usage_input(billing: BillingDecision, reported: Option<UsageTokens>) -> TurnUsageInput {
    TurnUsageInput {
        identity: identity(),
        selected_model: "premium".to_owned(),
        effective_model: "standard".to_owned(),
        terminal: TerminalState::Failed,
        billing,
        usage: reported,
        actual_credits_micro: 1234,
        policy_version_applied: 7,
        web_search_calls: 1,
        code_interpreter_calls: 2,
        file_search_calls: 3,
        timestamp: datetime!(2026-10-11 12:00 UTC),
    }
}

#[test]
fn usage_event_carries_the_turn_identities_and_billing_fields() {
    let event = turn_usage_event(usage_input(ACTUAL_FAILED, Some(usage(10, 20))));

    assert_eq!(event.tenant_id, identity().tenant_id);
    assert_eq!(event.user_id, Some(identity().user_id));
    assert_eq!(event.chat_id, Some(identity().chat_id));
    assert_eq!(event.turn_id, Some(identity().turn_id));
    assert_eq!(event.request_id, identity().request_id);
    assert_eq!(
        event.dedupe_key,
        format!(
            "{}/{}/{}",
            identity().tenant_id.simple(),
            identity().turn_id.simple(),
            identity().request_id.simple()
        )
    );
    assert_eq!(event.selected_model, "premium");
    assert_eq!(event.effective_model, "standard");
    assert_eq!(event.terminal_state, "failed");
    assert_eq!(event.billing_outcome, "failed");
    assert_eq!(event.settlement_method, "actual");
    assert_eq!(event.usage, Some(usage(10, 20)));
    assert_eq!(event.actual_credits_micro, 1234);
    assert_eq!(event.policy_version_applied, 7);
    assert_eq!(
        (
            event.web_search_calls,
            event.code_interpreter_calls,
            event.file_search_calls
        ),
        (1, 2, 3)
    );
    assert_eq!(event.requester_type, "user");
    assert_eq!(event.system_task_type, None);
}

#[test]
fn usage_event_usage_follows_the_settlement_method() {
    let actual_without = turn_usage_event(usage_input(ACTUAL_FAILED, None));
    assert_eq!(actual_without.usage, None);

    let estimated = turn_usage_event(usage_input(ESTIMATED_FAILED, Some(usage(0, 0))));
    assert_eq!(estimated.usage, None);
    let wire = serde_json::to_value(&estimated).unwrap();
    assert_eq!(wire["usage"], serde_json::Value::Null);

    let released = turn_usage_event(usage_input(
        decision(BillingOutcome::Failed, SettlementMethod::Released),
        Some(usage(5, 5)),
    ));
    assert_eq!(released.usage, Some(UsageTokens::default()));
    assert_eq!(released.settlement_method, "released");
}

fn audit_input(terminal: TerminalState, error_code: Option<&str>) -> TurnAuditInput {
    TurnAuditInput {
        identity: identity(),
        selected_model: "premium".to_owned(),
        effective_model: "premium".to_owned(),
        terminal,
        error_code: error_code.map(str::to_owned),
        usage: Some(usage(10, 20)),
        latency_ms: 1500,
        web_search_calls: 2,
        file_search_calls: 1,
        quota_decision: QuotaDecision {
            decision: QuotaDecision::ALLOW.to_owned(),
            downgrade_from: None,
            downgrade_reason: None,
        },
        trace_id: Some("trace-1".to_owned()),
        timestamp: datetime!(2026-10-11 12:00 UTC),
    }
}

#[test]
fn audit_event_type_is_completed_only_for_completed_turns() {
    assert_eq!(
        turn_audit_event(audit_input(TerminalState::Completed, None)).event_type,
        TurnAuditEventType::Completed
    );
    assert_eq!(
        turn_audit_event(audit_input(TerminalState::Failed, Some("provider_error"))).event_type,
        TurnAuditEventType::Failed
    );
    assert_eq!(
        turn_audit_event(audit_input(TerminalState::Cancelled, None)).event_type,
        TurnAuditEventType::Failed
    );
}

#[test]
fn audit_event_carries_metadata_and_no_content() {
    let event = turn_audit_event(audit_input(TerminalState::Failed, Some("orphan_timeout")));

    assert_eq!(event.tenant_id, identity().tenant_id);
    assert_eq!(event.user_id, identity().user_id);
    assert_eq!(event.chat_id, identity().chat_id);
    assert_eq!(event.turn_id, identity().turn_id);
    assert_eq!(event.request_id, identity().request_id);
    assert_eq!(event.requester_type, "user");
    assert_eq!(event.latency_ms, 1500);
    assert_eq!(event.tool_calls.web_search_calls, 2);
    assert_eq!(event.tool_calls.file_search_calls, 1);
    assert_eq!(event.policy_decisions.quota.decision, "allow");
    assert_eq!(event.error_code.as_deref(), Some("orphan_timeout"));
    assert_eq!(event.trace_id.as_deref(), Some("trace-1"));
    let wire = serde_json::to_value(mini_chat_sdk::AuditEvent::from(event)).unwrap();
    assert_eq!(wire["prompt"], json!(""));
    assert_eq!(wire["response"], json!(""));
    assert_eq!(wire["attachments"], json!([]));
    assert_eq!(wire["license"], serde_json::Value::Null);
    assert_eq!(wire["quota_scope"], serde_json::Value::Null);
}

#[test]
fn unknown_quota_decision_has_no_downgrade() {
    let decision = unknown_quota_decision();
    assert_eq!(decision.decision, "unknown");
    assert_eq!(decision.downgrade_from, None);
    assert_eq!(decision.downgrade_reason, None);
}
