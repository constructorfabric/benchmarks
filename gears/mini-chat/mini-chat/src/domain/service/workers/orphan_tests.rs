#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 2: the orphan inputs
//! (`cpt-cf-mini-chat-algo-background-workers-finalize-orphan`): the stale
//! cutoff and the `turn_failed` audit event built from the candidate row.

use mini_chat_sdk::TurnAuditEventType;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{OrphanCandidate, orphan_audit_event, stale_cutoff};

fn at(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).unwrap()
}

fn candidate() -> OrphanCandidate {
    OrphanCandidate {
        id: Uuid::from_u128(0xA1),
        tenant_id: Uuid::from_u128(0x7E),
        chat_id: Uuid::from_u128(0xC1),
        request_id: Uuid::from_u128(0xE1),
        requester_user_id: Some(Uuid::from_u128(0x55)),
        effective_model: Some("gpt-5.2".to_owned()),
        policy_version_applied: Some(7),
        started_at: at(1_800_000_000),
        web_search_completed_count: 2,
        file_search_completed_count: 3,
    }
}

#[test]
fn the_cutoff_is_now_minus_the_timeout() {
    assert_eq!(stale_cutoff(at(1_800_000_300), 300), at(1_800_000_000));
    assert_eq!(stale_cutoff(at(1_800_000_000), 90), at(1_799_999_910));
}

#[test]
fn the_audit_event_is_a_turn_failed_event_of_the_candidate() {
    let now = at(1_800_000_000) + time::Duration::milliseconds(2_500);

    let event = orphan_audit_event(&candidate(), now).expect("an event");

    assert_eq!(event.event_type, TurnAuditEventType::TurnFailed);
    assert_eq!(event.tenant_id, Uuid::from_u128(0x7E));
    assert_eq!(event.requester_user_id, Uuid::from_u128(0x55));
    assert_eq!(event.chat_id, Uuid::from_u128(0xC1));
    assert_eq!(event.turn_id, Uuid::from_u128(0xA1));
    assert_eq!(event.request_id, Uuid::from_u128(0xE1));
    assert_eq!(event.timestamp, now);
}

#[test]
fn the_selected_model_is_the_persisted_effective_model_and_the_quota_decision_is_unknown() {
    let event = orphan_audit_event(&candidate(), at(1_800_000_001)).unwrap();

    assert_eq!(event.selected_model, "gpt-5.2");
    assert_eq!(event.effective_model, "gpt-5.2");
    assert_eq!(event.policy_decisions.quota.decision, "unknown");
    assert_eq!(
        (event.usage.input_tokens, event.usage.output_tokens),
        (0, 0)
    );
}

#[test]
fn the_models_are_empty_when_the_turn_never_reserved() {
    let mut row = candidate();
    row.effective_model = None;

    let event = orphan_audit_event(&row, at(1_800_000_001)).unwrap();

    assert_eq!(event.selected_model, "");
    assert_eq!(event.effective_model, "");
}

#[test]
fn the_latency_runs_from_started_at_to_now_in_milliseconds_and_never_goes_negative() {
    let started = candidate().started_at;

    let late =
        orphan_audit_event(&candidate(), started + time::Duration::milliseconds(1_234)).unwrap();
    let early = orphan_audit_event(&candidate(), started - time::Duration::seconds(5)).unwrap();

    assert_eq!(late.latency_ms, 1_234);
    assert_eq!(early.latency_ms, 0);
}

#[test]
fn the_tool_calls_come_from_the_two_persisted_counters() {
    let event = orphan_audit_event(&candidate(), at(1_800_000_001)).unwrap();

    assert_eq!(event.tool_calls.web_search, 2);
    assert_eq!(event.tool_calls.file_search, 3);
}

#[test]
fn the_content_fields_are_empty() {
    let event = orphan_audit_event(&candidate(), at(1_800_000_001)).unwrap();

    assert_eq!(event.prompt, "");
    assert_eq!(event.response, "");
    assert!(event.attachments.is_empty());
    assert!(event.license.is_none() && event.quota_scope.is_none());
}

#[test]
fn a_candidate_without_a_requester_has_no_audit_event() {
    let mut row = candidate();
    row.requester_user_id = None;

    assert!(orphan_audit_event(&row, at(1_800_000_001)).is_none());
}
