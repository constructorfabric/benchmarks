//! The turn audit event on both paths: the event type and terminal state per outcome, the
//! fields of the stream path (models, decision, usage, counts, latency, empty content fields) and
//! of the orphan path (`selected_model` equal to the effective model, decision `unknown`).
//!
//! Slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithm
//! `turn-audit-event`; definition of done `cpt-cf-mini-chat-dod-message-streaming-finalization`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::{AuditEvent, QuotaDecision, TurnAuditEvent, UsageTokens};
use serde_json::{Value, json};
use uuid::Uuid;

use super::audit::{OrphanTurnFacts, build_orphan_audit, build_stream_audit};
use super::finalizer::{StreamOutcome, StreamTerminal};
use crate::domain::error::StreamErrorCode;
use crate::domain::quota::ToolCallCounts;
use crate::test_support::send::t;
use crate::test_support::streaming::{PLAN_TENANT, PLAN_USER, PlanBuilder, counts};

fn terminal(outcome: StreamOutcome, usage: Option<UsageTokens>) -> StreamTerminal {
    StreamTerminal {
        outcome,
        usage,
        provider_response_id: Some("resp_0aB1c2D3e4F5g6H7".to_owned()),
        text: "the answer text".to_owned(),
        tool_counts: counts(2, 1, 3),
        incomplete_reason: None,
    }
}

fn usage() -> UsageTokens {
    UsageTokens {
        input_tokens: 500,
        output_tokens: 120,
        cache_read_input_tokens: 20,
        cache_write_input_tokens: 10,
        reasoning_tokens: 5,
    }
}

fn parts(event: AuditEvent) -> (&'static str, TurnAuditEvent) {
    match event {
        AuditEvent::TurnCompleted(inner) => ("turn_completed", inner),
        AuditEvent::TurnFailed(inner) => ("turn_failed", inner),
        other => panic!("not a turn finalization event: {other:?}"),
    }
}

#[test]
fn a_completed_turn_is_turn_completed_with_the_stream_path_fields() {
    let plan = PlanBuilder::new().build();
    let finished = t(100) + time::Duration::milliseconds(2500);
    let event = build_stream_audit(
        &plan,
        &terminal(StreamOutcome::Completed, Some(usage())),
        finished,
        Some("0af7651916cd43dd8448eb211c80319c".to_owned()),
    );
    let (kind, audit) = parts(event);
    assert_eq!(kind, "turn_completed");
    assert_eq!(audit.tenant_id, PLAN_TENANT);
    assert_eq!(audit.user_id, PLAN_USER);
    assert_eq!(audit.chat_id, plan.chat_id);
    assert_eq!(audit.turn_id, plan.turn_id);
    assert_eq!(audit.request_id, plan.request_id);
    assert_eq!(audit.requester_type, "user");
    assert_eq!(audit.terminal_state, "completed");
    assert_eq!(audit.error_code, None);
    assert_eq!(audit.selected_model, plan.selected_model);
    assert_eq!(audit.effective_model, plan.decision.effective_model);
    assert_eq!(audit.quota_decision, QuotaDecision::Allow);
    assert_eq!(audit.downgrade_from, None);
    assert_eq!(audit.downgrade_reason, None);
    assert_eq!(audit.usage, Some(usage()));
    assert_eq!(audit.latency_ms, 2500);
    assert_eq!(
        (
            audit.web_search_calls,
            audit.code_interpreter_calls,
            audit.file_search_calls
        ),
        (2, 1, 3)
    );
    assert_eq!(
        audit.trace_id.as_deref(),
        Some("0af7651916cd43dd8448eb211c80319c")
    );
    assert_eq!(audit.timestamp, finished);
}

#[test]
fn the_content_fields_stay_empty_and_no_provider_identifier_is_present() {
    let plan = PlanBuilder::new().build();
    let event = build_stream_audit(
        &plan,
        &terminal(StreamOutcome::Completed, Some(usage())),
        t(200),
        None,
    );
    let json: Value = serde_json::to_value(&event).unwrap();
    assert_eq!(json["prompt"], "");
    assert_eq!(json["response"], "");
    assert_eq!(json["attachments"], json!([]));
    assert!(json["license"].is_null());
    assert!(json["quota_scope"].is_null());
    assert!(json["trace_id"].is_null());
    let text = json.to_string();
    for hidden in ["resp_0aB1c2D3e4F5g6H7", "the answer text"] {
        assert!(!text.contains(hidden), "`{hidden}` in {text}");
    }
}

#[test]
fn a_downgrade_carries_the_origin_and_the_reason() {
    let plan = PlanBuilder::new().downgraded_from("premium-1").build();
    let (_, audit) = parts(build_stream_audit(
        &plan,
        &terminal(StreamOutcome::Completed, Some(usage())),
        t(200),
        None,
    ));
    assert_eq!(audit.selected_model, "premium-1");
    assert_eq!(audit.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(audit.downgrade_from.as_deref(), Some("premium-1"));
    assert_eq!(
        audit.downgrade_reason.as_deref(),
        Some("premium_quota_exhausted")
    );
}

#[test]
fn a_failed_turn_is_turn_failed_with_its_error_code_and_known_usage() {
    let plan = PlanBuilder::new().build();
    let (kind, audit) = parts(build_stream_audit(
        &plan,
        &terminal(
            StreamOutcome::Failed {
                code: StreamErrorCode::RateLimited,
                detail: "slow down".to_owned(),
            },
            None,
        ),
        t(200),
        None,
    ));
    assert_eq!(kind, "turn_failed");
    assert_eq!(audit.terminal_state, "failed");
    assert_eq!(audit.error_code.as_deref(), Some("rate_limited"));
    assert_eq!(audit.usage, None);
}

#[test]
fn a_cancelled_turn_is_turn_failed_with_the_cancelled_state_there_is_no_turn_cancelled() {
    let plan = PlanBuilder::new().build();
    let event = build_stream_audit(
        &plan,
        &terminal(StreamOutcome::Cancelled, None),
        t(200),
        None,
    );
    assert!(matches!(event, AuditEvent::TurnFailed(_)), "{event:?}");
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["event_type"], "turn_failed");
    assert_eq!(json["terminal_state"], "cancelled");
    assert!(json["error_code"].is_null());
}

fn orphan(effective_model: &str) -> OrphanTurnFacts {
    OrphanTurnFacts {
        tenant_id: PLAN_TENANT,
        user_id: PLAN_USER,
        chat_id: Uuid::from_u128(0xC4A7),
        turn_id: Uuid::from_u128(0x7071),
        request_id: Uuid::from_u128(0x7E57),
        effective_model: effective_model.to_owned(),
        started_at: t(0),
        tool_counts: ToolCallCounts {
            web_search: 1,
            code_interpreter: 0,
            file_search: 0,
        },
    }
}

#[test]
fn the_orphan_path_reports_the_effective_model_as_selected_and_the_unknown_decision() {
    let event = build_orphan_audit(
        &orphan("standard-1"),
        t(0) + time::Duration::seconds(301),
        None,
    );
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["event_type"], "turn_failed");
    assert_eq!(json["terminal_state"], "failed");
    assert_eq!(json["error_code"], "orphan_timeout");
    assert_eq!(json["selected_model"], "standard-1");
    assert_eq!(json["effective_model"], "standard-1");
    assert_eq!(json["quota_decision"], "unknown");
    assert!(json["usage"].is_null());
    assert_eq!(json["latency_ms"], 301_000);
    assert_eq!(json["web_search_calls"], 1);
    assert_eq!(json["prompt"], "");
    assert_eq!(json["response"], "");
    assert!(json["downgrade_from"].is_null());
}

#[test]
fn an_orphan_turn_without_reserve_columns_has_an_empty_effective_model() {
    let event = build_orphan_audit(&orphan(""), t(0) + time::Duration::seconds(10), None);
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["selected_model"], "");
    assert_eq!(json["effective_model"], "");
    assert_eq!(json["quota_decision"], "unknown");
}

#[test]
fn the_orphan_event_can_be_read_back_by_the_audit_delivery() {
    // the payload type must be able to carry `unknown`
    let event = build_orphan_audit(
        &orphan("standard-1"),
        t(0) + time::Duration::seconds(10),
        None,
    );
    let json = serde_json::to_value(&event).unwrap();
    let back: AuditEvent = serde_json::from_value(json).expect("the audit payload deserializes");
    assert_eq!(back, event);
}
