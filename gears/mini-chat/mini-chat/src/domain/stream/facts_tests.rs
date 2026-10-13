#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::{
    BillingOutcome, SettlementMethod, TokenUsage, TurnAuditEventType, TurnLatency,
    UsageTerminalState,
};
use time::macros::datetime;
use uuid::Uuid;

use super::{TerminalFacts, TurnFacts, build_events, dedupe_key};
use crate::domain::billing::{Settlement, SettlementBasis};
use crate::domain::stream::model::ToolCounts;

fn facts() -> TurnFacts {
    TurnFacts {
        turn_id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        user_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
        assistant_message_id: Uuid::new_v4(),
        selected_model: "premium".to_owned(),
        effective_model: "standard".to_owned(),
        quota_decision: "downgrade",
        downgrade_from: Some("premium".to_owned()),
        downgrade_reason: Some("premium_quota_exhausted"),
        policy_version: 1,
        started_at: datetime!(2026-02-28 15:30 UTC),
        basis: SettlementBasis {
            reserve_tokens: 10,
            max_output_tokens_applied: 5,
            reserved_credits_micro: 20,
            minimal_generation_floor_applied: 2,
            in_mult: 1,
            out_mult: 1,
        },
        premium: false,
        thread_summary_tokens: None,
        trace_id: None,
        summary_trigger: None,
    }
}

fn settlement(method: SettlementMethod, credits: i64) -> Settlement {
    Settlement {
        method,
        committed_credits_micro: credits,
        telemetry_input_tokens: 0,
        telemetry_output_tokens: 0,
        overshoot: false,
    }
}

fn terminal(state: UsageTerminalState, outcome: BillingOutcome) -> TerminalFacts<'static> {
    TerminalFacts {
        state,
        error_code: None,
        usage: Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..TokenUsage::default()
        }),
        billing_outcome: outcome,
        tool_counts: ToolCounts {
            web_search: 1,
            code_interpreter: 2,
            file_search: 3,
        },
        latency: TurnLatency {
            time_to_first_token_ms: Some(12),
            total_ms: 34,
        },
        now: datetime!(2026-02-28 15:31 UTC),
    }
}

#[test]
fn the_dedupe_key_uses_the_simple_uuid_forms() {
    let (tenant, turn, request) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());

    let key = dedupe_key(tenant, turn, request);

    assert_eq!(
        key,
        format!("{}/{}/{}", tenant.simple(), turn.simple(), request.simple())
    );
    assert!(!key.contains('-'));
}

#[test]
fn an_actual_settlement_reports_the_usage_and_the_decision_goes_to_the_audit_event_only() {
    let facts = facts();

    let (usage, audit) = build_events(
        &facts,
        &terminal(UsageTerminalState::Completed, BillingOutcome::Completed),
        &settlement(SettlementMethod::Actual, 25),
    );

    assert_eq!(usage.usage.unwrap().input_tokens, 10);
    assert_eq!(usage.actual_credits_micro, 25);
    assert_eq!(
        usage.dedupe_key,
        dedupe_key(facts.tenant_id, facts.turn_id, facts.request_id)
    );
    assert_eq!(
        (
            usage.web_search_calls,
            usage.code_interpreter_calls,
            usage.file_search_calls
        ),
        (1, 2, 3)
    );
    assert_eq!(audit.event_type, TurnAuditEventType::Completed);
    assert_eq!(audit.policy_decisions.quota.decision, "downgrade");
    assert_eq!(
        audit.policy_decisions.quota.downgrade_reason.as_deref(),
        Some("premium_quota_exhausted")
    );
    assert!(usage.system_task_type.is_none());
}

#[test]
fn an_estimated_settlement_reports_no_usage_and_a_cancel_is_a_failed_audit_event() {
    let (usage, audit) = build_events(
        &facts(),
        &terminal(UsageTerminalState::Cancelled, BillingOutcome::Aborted),
        &settlement(SettlementMethod::Estimated, 263),
    );

    assert!(usage.usage.is_none());
    assert_eq!(usage.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(audit.event_type, TurnAuditEventType::Failed);
    assert_eq!(audit.terminal_state, UsageTerminalState::Cancelled);
}

#[test]
fn the_audit_event_carries_the_trace_id_of_the_request_and_omits_it_without_one() {
    let mut with_trace = facts();
    with_trace.trace_id = Some("4bf92f3577b34da6a3ce929d0e0e4736".to_owned());
    let without_trace = facts();
    let terminal = terminal(UsageTerminalState::Completed, BillingOutcome::Completed);
    let settlement = settlement(SettlementMethod::Actual, 25);

    let (_, traced) = build_events(&with_trace, &terminal, &settlement);
    let (_, untraced) = build_events(&without_trace, &terminal, &settlement);

    assert_eq!(
        serde_json::to_value(&traced).unwrap()["trace_id"],
        "4bf92f3577b34da6a3ce929d0e0e4736"
    );
    assert!(
        serde_json::to_value(&untraced)
            .unwrap()
            .get("trace_id")
            .is_none(),
        "the field is omitted when no span was active"
    );
}
