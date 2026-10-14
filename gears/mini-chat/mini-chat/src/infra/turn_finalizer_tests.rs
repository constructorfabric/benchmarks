//! The stream finalization transaction over in-memory `SQLite`, the real quota service and the
//! shared outbox: the assistant (or partial) message, the CAS, the actual or estimated settlement,
//! exactly one usage event, one audit event and at most one thread-summary message, the
//! downgrade to `message_persistence_failed`, a lost CAS leaving nothing behind and a failed
//! transaction leaving the turn `running`.
//!
//! Slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithms
//! `finalization`, `turn-audit-event`, `summary-trigger` and `message-order` (assistant parts);
//! state `turn-lifecycle`; definitions of done `cpt-cf-mini-chat-dod-message-streaming-finalization`
//! and `cpt-cf-mini-chat-dod-message-streaming-turn-lifecycle`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use mini_chat_sdk::{AuditEvent, UsageEvent, UsageTokens};
use serde_json::json;
use time::Duration as TimeDuration;
use uuid::Uuid;

use crate::domain::error::{DomainError, StreamErrorCode};
use crate::domain::quota::ToolCallCounts;
use crate::domain::streaming::{
    ProviderCallPlan, StreamFinalization, StreamOutcome, StreamTerminal, TurnFinalization,
    TurnOutcome, TurnTarget,
};
use crate::domain::turns::TurnState;
use crate::infra::storage::entity::message;
use crate::infra::turn_finalizer::SummarySettings;
use crate::test_support::chats::{
    a1, chat_uri, fail_outbox_inserts, message_rows, outbox_body_count,
};
use crate::test_support::quota::{FixedClock, all_quota_rows, default_now};
use crate::test_support::raw_exec;
use crate::test_support::send::{
    FailingSummaryOutbox, SendRig, SendRigOptions, chat_model, seed_completed_turn, seed_summary,
    send_rig, send_rig_with, t, turn_rows,
};

const RESPONSE_ID: &str = "resp_0aB1c2D3e4F5g6H7";

fn usage(input: u64, output: u64) -> UsageTokens {
    UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: 5,
        cache_write_input_tokens: 4,
        reasoning_tokens: 3,
    }
}

fn terminal(outcome: StreamOutcome, text: &str, usage: Option<UsageTokens>) -> StreamTerminal {
    StreamTerminal {
        outcome,
        usage,
        provider_response_id: Some(RESPONSE_ID.to_owned()),
        text: text.to_owned(),
        tool_counts: ToolCallCounts::default(),
        incomplete_reason: None,
    }
}

fn completed(text: &str) -> StreamTerminal {
    terminal(StreamOutcome::Completed, text, Some(usage(120, 30)))
}

fn failed(code: StreamErrorCode, text: &str, usage: Option<UsageTokens>) -> StreamTerminal {
    terminal(
        StreamOutcome::Failed {
            code,
            detail: "sanitized provider text".to_owned(),
        },
        text,
        usage,
    )
}

fn cancelled(text: &str) -> StreamTerminal {
    StreamTerminal {
        usage: None,
        ..terminal(StreamOutcome::Cancelled, text, None)
    }
}

async fn setup() -> (
    SendRig,
    crate::infra::storage::repo_impl::ChatRecord,
    ProviderCallPlan,
) {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let plan = rig.plan_for(&chat, "the question").await;
    (rig, chat, plan)
}

fn role_of<'a>(rows: &'a [message::Model], role: &str) -> &'a message::Model {
    let mut found = rows.iter().filter(|row| row.role == role);
    let row = found.next().unwrap_or_else(|| panic!("no {role} message"));
    assert!(found.next().is_none(), "more than one {role} message");
    row
}

async fn committed(
    rig: &SendRig,
    plan: &ProviderCallPlan,
    terminal: &StreamTerminal,
) -> StreamFinalization {
    rig.finalizer
        .finalize_stream(plan, terminal)
        .await
        .expect("the finalization commits")
}

async fn audit_of(rig: &SendRig) -> Vec<AuditEvent> {
    rig.audit_events().await
}

async fn message_count(rig: &SendRig, chat_id: Uuid) -> i64 {
    rig.get(&chat_uri(chat_id), &a1()).await.json()["message_count"]
        .as_i64()
        .unwrap()
}

async fn reserved(rig: &SendRig) -> i64 {
    all_quota_rows(&rig.quota.db)
        .await
        .iter()
        .map(|row| row.reserved)
        .sum()
}

async fn spent(rig: &SendRig) -> i64 {
    all_quota_rows(&rig.quota.db)
        .await
        .iter()
        .map(|row| row.spent)
        .sum()
}

fn usage_event(events: &[UsageEvent]) -> &UsageEvent {
    assert_eq!(events.len(), 1, "exactly one usage event: {events:?}");
    &events[0]
}

// ── completed ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_completed_stream_persists_the_assistant_message_and_settles_the_actual_usage() {
    let (rig, chat, plan) = setup().await;
    let reserved_before = reserved(&rig).await;
    assert!(reserved_before > 0);

    let result = committed(&rig, &plan, &completed("the full answer")).await;
    assert_eq!(
        result,
        StreamFinalization::Committed(crate::domain::streaming::CommittedTurn {
            state: TurnState::Completed,
            error_code: None,
            assistant_message_id: Some(plan.assistant_message_id),
        })
    );

    let rows = message_rows(&rig.quota.db, chat.id).await;
    assert_eq!(rows.len(), 2);
    let user = role_of(&rows, "user");
    let assistant = role_of(&rows, "assistant");
    assert_eq!(assistant.id, plan.assistant_message_id);
    assert_eq!(assistant.request_id, Some(plan.request_id));
    assert_eq!(assistant.content, "the full answer");
    assert_eq!(assistant.content_type, "text");
    assert_eq!(assistant.request_kind, "chat");
    assert_eq!(assistant.features_used, json!([]));
    assert_eq!(assistant.token_estimate, 0);
    assert!(!assistant.is_compressed);
    assert_eq!(
        assistant.model.as_deref(),
        Some(plan.decision.effective_model.as_str())
    );
    assert_eq!(
        (
            assistant.input_tokens,
            assistant.output_tokens,
            assistant.cache_read_input_tokens,
            assistant.cache_write_input_tokens,
            assistant.reasoning_tokens
        ),
        (120, 30, 5, 4, 3)
    );
    assert_eq!(assistant.provider_response_id.as_deref(), Some(RESPONSE_ID));
    assert_eq!(assistant.deleted_at, None);
    // the clock has not moved: strictly after the user message by one microsecond
    assert_eq!(
        assistant.created_at,
        user.created_at + TimeDuration::microseconds(1)
    );

    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.assistant_message_id, Some(plan.assistant_message_id));
    assert_eq!(turn.error_code, None);
    assert_eq!(turn.provider_response_id.as_deref(), Some(RESPONSE_ID));
    assert!(turn.completed_at.is_some());

    // one settlement on the actual usage, one usage event, one audit event
    assert_eq!(reserved(&rig).await, 0);
    assert!(spent(&rig).await > 0);
    let events = rig.events().await;
    let event = usage_event(&events);
    assert_eq!(event.terminal_state, "completed");
    assert_eq!(event.billing_outcome, "completed");
    assert_eq!(event.settlement_method, "actual");
    assert_eq!(event.usage, Some(usage(120, 30)));
    let audits = audit_of(&rig).await;
    assert_eq!(audits.len(), 1, "{audits:?}");
    assert!(matches!(&audits[0], AuditEvent::TurnCompleted(event)
        if event.terminal_state == "completed" && event.turn_id == plan.turn_id));
    assert_eq!(message_count(&rig, chat.id).await, 2);
}

#[tokio::test]
async fn the_assistant_message_moves_the_chat_activity() {
    let (rig, chat, plan) = setup().await;
    let before = chat_model(&rig.quota.db, chat.id).await.updated_at;
    tokio::time::sleep(Duration::from_millis(5)).await;
    committed(&rig, &plan, &completed("answer")).await;
    let after = chat_model(&rig.quota.db, chat.id).await.updated_at;
    assert!(after > before, "{after} vs {before}");
}

#[tokio::test]
async fn a_later_clock_reading_is_the_assistant_created_at() {
    let clock = FixedClock::at(default_now());
    let rig = send_rig_with(SendRigOptions::new().with_clock(clock.clone())).await;
    let chat = rig.std_chat().await;
    let plan = rig.plan_for(&chat, "q").await;
    let later = default_now() + TimeDuration::seconds(7);
    clock.set(later);
    committed(&rig, &plan, &completed("a")).await;
    let rows = message_rows(&rig.quota.db, chat.id).await;
    assert_eq!(role_of(&rows, "assistant").created_at, later);
    assert!(role_of(&rows, "assistant").created_at > role_of(&rows, "user").created_at);
}

#[tokio::test]
async fn a_completion_without_text_is_an_empty_replayable_assistant_message() {
    let (rig, chat, plan) = setup().await;
    let result = committed(&rig, &plan, &completed("")).await;
    assert!(matches!(
        result,
        StreamFinalization::Committed(ref turn) if turn.state == TurnState::Completed
    ));
    let rows = message_rows(&rig.quota.db, chat.id).await;
    assert_eq!(role_of(&rows, "assistant").content, "");
    assert_eq!(message_count(&rig, chat.id).await, 2);
}

#[tokio::test]
async fn an_incomplete_response_is_persisted_and_settled_like_a_completed_one() {
    let (rig, chat, plan) = setup().await;
    let mut truncated = completed("cut off");
    truncated.incomplete_reason = Some("max_output_tokens".to_owned());
    committed(&rig, &plan, &truncated).await;
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert_eq!(turn.state, "completed");
    // the incomplete reason is never stored as an error code
    assert_eq!(turn.error_code, None);
    assert_eq!(turn.error_detail, None);
    let events = rig.events().await;
    assert_eq!(usage_event(&events).settlement_method, "actual");
    let rows = message_rows(&rig.quota.db, chat.id).await;
    assert_eq!(role_of(&rows, "assistant").content, "cut off");
}

#[tokio::test]
async fn the_completed_tool_counts_are_settled_and_reported() {
    let (rig, _chat, plan) = setup().await;
    let mut with_tools = completed("answer");
    with_tools.tool_counts = ToolCallCounts {
        web_search: 2,
        code_interpreter: 1,
        file_search: 3,
    };
    committed(&rig, &plan, &with_tools).await;
    let events = rig.events().await;
    let event = usage_event(&events);
    assert_eq!(
        (
            event.web_search_calls,
            event.code_interpreter_calls,
            event.file_search_calls
        ),
        (2, 1, 3)
    );
    let daily_total: i64 = all_quota_rows(&rig.quota.db)
        .await
        .iter()
        .filter(|row| row.bucket == "total" && row.period == "daily")
        .map(|row| i64::from(row.web_search_calls))
        .sum();
    assert_eq!(daily_total, 2);
    let audits = audit_of(&rig).await;
    let AuditEvent::TurnCompleted(audit) = &audits[0] else {
        panic!("{audits:?}");
    };
    assert_eq!(
        (
            audit.web_search_calls,
            audit.code_interpreter_calls,
            audit.file_search_calls
        ),
        (2, 1, 3)
    );
}

// ── failed ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_failed_stream_writes_no_message_and_discards_the_partial_text() {
    let (rig, chat, plan) = setup().await;
    let result = committed(
        &rig,
        &plan,
        &failed(
            StreamErrorCode::ProviderError,
            "partial text that is discarded",
            None,
        ),
    )
    .await;
    assert_eq!(
        result,
        StreamFinalization::Committed(crate::domain::streaming::CommittedTurn {
            state: TurnState::Failed,
            error_code: Some("provider_error".to_owned()),
            assistant_message_id: None,
        })
    );
    let rows = message_rows(&rig.quota.db, chat.id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].role, "user");
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
    assert_eq!(
        turn.error_detail.as_deref(),
        Some("sanitized provider text")
    );
    assert_eq!(turn.assistant_message_id, None);
    assert_eq!(turn.provider_response_id.as_deref(), Some(RESPONSE_ID));
    assert!(turn.completed_at.is_some());
    assert_eq!(message_count(&rig, chat.id).await, 1);
    // no usage: settled on the estimated path
    assert_eq!(reserved(&rig).await, 0);
    let events = rig.events().await;
    let event = usage_event(&events);
    assert_eq!(event.terminal_state, "failed");
    assert_eq!(event.settlement_method, "estimated");
    assert_eq!(event.usage, None);
    let audits = audit_of(&rig).await;
    assert!(matches!(&audits[0], AuditEvent::TurnFailed(event)
        if event.error_code.as_deref() == Some("provider_error")));
}

#[tokio::test]
async fn a_failed_stream_with_known_usage_settles_actual() {
    let (rig, _chat, plan) = setup().await;
    committed(
        &rig,
        &plan,
        &failed(StreamErrorCode::ProviderError, "", Some(usage(80, 12))),
    )
    .await;
    let events = rig.events().await;
    let event = usage_event(&events);
    assert_eq!(event.settlement_method, "actual");
    assert_eq!(event.billing_outcome, "failed");
    assert_eq!(event.usage, Some(usage(80, 12)));
}

#[tokio::test]
async fn every_stored_error_code_is_written_and_the_sse_only_codes_are_refused() {
    for code in [
        StreamErrorCode::ProviderError,
        StreamErrorCode::ProviderTimeout,
        StreamErrorCode::RateLimited,
        StreamErrorCode::WebSearchCallsExceeded,
        StreamErrorCode::CodeInterpreterCallsExceeded,
        StreamErrorCode::UnexpectedToolUse,
    ] {
        let (rig, chat, plan) = setup().await;
        committed(&rig, &plan, &failed(code, "", None)).await;
        let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
        assert_eq!(turn.error_code.as_deref(), Some(code.as_str()));
    }
    for code in [
        StreamErrorCode::FinalizationFailed,
        StreamErrorCode::StreamInterrupted,
    ] {
        let (rig, chat, plan) = setup().await;
        let outcome = rig
            .finalizer
            .finalize_stream(&plan, &failed(code, "", None))
            .await;
        assert!(outcome.is_err(), "{code:?}");
        assert_eq!(turn_rows(&rig.quota.db, chat.id).await[0].state, "running");
    }
}

// ── cancelled ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_cancelled_stream_with_text_persists_the_partial_message() {
    let (rig, chat, plan) = setup().await;
    let result = committed(&rig, &plan, &cancelled("half an ans")).await;
    assert_eq!(
        result,
        StreamFinalization::Committed(crate::domain::streaming::CommittedTurn {
            state: TurnState::Cancelled,
            error_code: None,
            assistant_message_id: Some(plan.assistant_message_id),
        })
    );
    let rows = message_rows(&rig.quota.db, chat.id).await;
    let assistant = role_of(&rows, "assistant");
    assert_eq!(assistant.id, plan.assistant_message_id);
    assert_eq!(assistant.content, "half an ans");
    assert_eq!(
        (
            assistant.input_tokens,
            assistant.output_tokens,
            assistant.cache_read_input_tokens
        ),
        (0, 0, 0)
    );
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert_eq!(turn.state, "cancelled");
    assert_eq!(turn.assistant_message_id, Some(plan.assistant_message_id));
    assert_eq!(turn.error_code, None);
    assert_eq!(message_count(&rig, chat.id).await, 2);
    // estimated settlement and the `aborted` usage event
    assert_eq!(reserved(&rig).await, 0);
    let events = rig.events().await;
    let event = usage_event(&events);
    assert_eq!(event.terminal_state, "cancelled");
    assert_eq!(event.billing_outcome, "aborted");
    assert_eq!(event.settlement_method, "estimated");
    assert_eq!(event.usage, None);
    // there is no `turn_cancelled` audit type: `turn_failed` with the cancelled state
    let audits = audit_of(&rig).await;
    assert_eq!(audits.len(), 1);
    assert!(
        matches!(&audits[0], AuditEvent::TurnFailed(event) if event.terminal_state == "cancelled")
    );
}

#[tokio::test]
async fn a_cancelled_stream_without_text_persists_nothing() {
    let (rig, chat, plan) = setup().await;
    let result = committed(&rig, &plan, &cancelled("")).await;
    assert!(matches!(
        result,
        StreamFinalization::Committed(ref turn)
            if turn.state == TurnState::Cancelled && turn.assistant_message_id.is_none()
    ));
    assert_eq!(message_rows(&rig.quota.db, chat.id).await.len(), 1);
    assert_eq!(
        turn_rows(&rig.quota.db, chat.id).await[0].assistant_message_id,
        None
    );
    assert_eq!(message_count(&rig, chat.id).await, 1);
}

#[tokio::test]
async fn a_failed_partial_insert_still_ends_cancelled_without_a_message_id() {
    let (rig, chat, plan) = setup().await;
    raw_exec(
        &rig.quota.db.raw,
        "CREATE TRIGGER fail_assistant_insert BEFORE INSERT ON messages \
         WHEN NEW.role = 'assistant' BEGIN SELECT RAISE(ABORT, 'forced insert failure'); END",
    )
    .await
    .unwrap();
    let result = committed(&rig, &plan, &cancelled("partial")).await;
    assert!(matches!(
        result,
        StreamFinalization::Committed(ref turn)
            if turn.state == TurnState::Cancelled && turn.assistant_message_id.is_none()
    ));
    assert_eq!(message_rows(&rig.quota.db, chat.id).await.len(), 1);
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert_eq!(turn.state, "cancelled");
    assert_eq!(turn.assistant_message_id, None);
    // the finalization still settled and enqueued once
    assert_eq!(reserved(&rig).await, 0);
    assert_eq!(rig.events().await.len(), 1);
    assert_eq!(audit_of(&rig).await.len(), 1);
}

// ── persistence failure and CAS loss ─────────────────────────────────────────

#[tokio::test]
async fn a_forced_assistant_insert_failure_is_downgraded_to_message_persistence_failed() {
    let (rig, chat, plan) = setup().await;
    raw_exec(
        &rig.quota.db.raw,
        "CREATE TRIGGER fail_assistant_insert BEFORE INSERT ON messages \
         WHEN NEW.role = 'assistant' BEGIN SELECT RAISE(ABORT, 'forced insert failure'); END",
    )
    .await
    .unwrap();
    let result = committed(&rig, &plan, &completed("the answer")).await;
    assert_eq!(
        result,
        StreamFinalization::Committed(crate::domain::streaming::CommittedTurn {
            state: TurnState::Failed,
            error_code: Some("message_persistence_failed".to_owned()),
            assistant_message_id: None,
        })
    );
    // `completed` is never observable without its message
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("message_persistence_failed")
    );
    assert_eq!(turn.assistant_message_id, None);
    assert_eq!(message_rows(&rig.quota.db, chat.id).await.len(), 1);
    // the usage is known: settled actual under the failed outcome, one event, one audit
    let events = rig.events().await;
    let event = usage_event(&events);
    assert_eq!(event.terminal_state, "failed");
    assert_eq!(event.settlement_method, "actual");
    assert_eq!(event.usage, Some(usage(120, 30)));
    let audits = audit_of(&rig).await;
    assert_eq!(audits.len(), 1);
    assert!(matches!(&audits[0], AuditEvent::TurnFailed(event)
        if event.error_code.as_deref() == Some("message_persistence_failed")));
}

#[tokio::test]
async fn a_lost_cas_is_a_complete_no_op() {
    let (rig, chat, plan) = setup().await;
    // another finalizer (the cancellation, the watchdog) won first
    let target = TurnTarget::of_plan(&plan);
    assert_eq!(
        rig.finalizer
            .finalize(&target, TurnOutcome::Cancelled)
            .await
            .unwrap(),
        TurnFinalization::Finalized
    );
    let usage_before = rig.events().await.len();
    let audit_before = audit_of(&rig).await.len();
    let spent_before = spent(&rig).await;

    let result = committed(&rig, &plan, &completed("late answer")).await;
    assert_eq!(result, StreamFinalization::AlreadyFinalized);

    // no assistant message of the loser, no state change, no settlement, no event
    assert_eq!(message_rows(&rig.quota.db, chat.id).await.len(), 1);
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert_eq!(turn.state, "cancelled");
    assert_eq!(turn.assistant_message_id, None);
    assert_eq!(rig.events().await.len(), usage_before);
    assert_eq!(audit_of(&rig).await.len(), audit_before);
    assert_eq!(spent(&rig).await, spent_before);
}

#[tokio::test]
async fn a_second_stream_finalization_of_the_same_turn_is_a_no_op() {
    let (rig, chat, plan) = setup().await;
    committed(&rig, &plan, &completed("first")).await;
    let result = committed(&rig, &plan, &completed("second")).await;
    assert_eq!(result, StreamFinalization::AlreadyFinalized);
    let rows = message_rows(&rig.quota.db, chat.id).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(role_of(&rows, "assistant").content, "first");
    assert_eq!(rig.events().await.len(), 1);
    assert_eq!(audit_of(&rig).await.len(), 1);
}

#[tokio::test]
async fn terminal_states_are_immutable_whatever_signal_arrives_later() {
    let firsts = [
        ("completed", completed("first")),
        ("failed", failed(StreamErrorCode::ProviderError, "", None)),
        ("cancelled", cancelled("")),
    ];
    for (state, first) in firsts {
        let (rig, chat, plan) = setup().await;
        committed(&rig, &plan, &first).await;
        let before = turn_rows(&rig.quota.db, chat.id).await.remove(0);
        let later = [
            completed("late"),
            failed(StreamErrorCode::RateLimited, "late", Some(usage(1, 1))),
            cancelled("late partial"),
        ];
        for signal in later {
            let result = committed(&rig, &plan, &signal).await;
            assert_eq!(result, StreamFinalization::AlreadyFinalized, "{state}");
        }
        let after = turn_rows(&rig.quota.db, chat.id).await.remove(0);
        assert_eq!(after.state, state);
        assert_eq!(after.error_code, before.error_code);
        assert_eq!(after.assistant_message_id, before.assistant_message_id);
        assert_eq!(rig.events().await.len(), 1, "{state}");
        assert_eq!(audit_of(&rig).await.len(), 1, "{state}");
    }
}

// ── a failed transaction ─────────────────────────────────────────────────────

#[tokio::test]
async fn a_forced_enqueue_failure_leaves_the_turn_running_with_nothing_committed() {
    let (rig, chat, plan) = setup().await;
    let outbox_before = outbox_body_count(&rig.quota.db).await;
    let reserved_before = reserved(&rig).await;
    fail_outbox_inserts(&rig.quota.db).await;
    let result = rig
        .finalizer
        .finalize_stream(&plan, &completed("the answer"))
        .await;
    assert!(result.is_err(), "{result:?}");
    // one transaction: no message, no state change, no settlement, no outbox message
    assert_eq!(message_rows(&rig.quota.db, chat.id).await.len(), 1);
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert_eq!(turn.state, "running");
    assert_eq!(turn.assistant_message_id, None);
    assert_eq!(turn.completed_at, None);
    assert_eq!(reserved(&rig).await, reserved_before);
    assert_eq!(outbox_body_count(&rig.quota.db).await, outbox_before);
}

#[tokio::test]
async fn a_failed_settlement_leaves_the_turn_running_with_no_message() {
    let (rig, chat, plan) = setup().await;
    rig.quota
        .gateway
        .fail_version
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let result = rig
        .finalizer
        .finalize_stream(&plan, &completed("the answer"))
        .await;
    assert!(
        matches!(result, Err(DomainError::PolicyUnavailable { .. })),
        "{result:?}"
    );
    assert_eq!(message_rows(&rig.quota.db, chat.id).await.len(), 1);
    assert_eq!(turn_rows(&rig.quota.db, chat.id).await[0].state, "running");
    assert!(reserved(&rig).await > 0);
    assert!(rig.events().await.is_empty());
}

#[tokio::test]
async fn a_failed_error_path_finalization_also_leaves_the_turn_running() {
    let (rig, chat, plan) = setup().await;
    fail_outbox_inserts(&rig.quota.db).await;
    let result = rig
        .finalizer
        .finalize_stream(&plan, &failed(StreamErrorCode::ProviderError, "", None))
        .await;
    assert!(result.is_err());
    assert_eq!(turn_rows(&rig.quota.db, chat.id).await[0].state, "running");
}

// ── the S1 outcomes enqueue the audit event too ──────────────────────────────

#[tokio::test]
async fn the_pre_provider_cancellation_enqueues_the_turn_failed_audit_event() {
    let (rig, _chat, plan) = setup().await;
    let target = TurnTarget::of_plan(&plan);
    rig.finalizer
        .finalize(&target, TurnOutcome::Cancelled)
        .await
        .unwrap();
    let events = rig.events().await;
    let event = usage_event(&events);
    assert_eq!(event.billing_outcome, "aborted");
    let audits = audit_of(&rig).await;
    assert_eq!(audits.len(), 1, "{audits:?}");
    let AuditEvent::TurnFailed(audit) = &audits[0] else {
        panic!("{audits:?}");
    };
    assert_eq!(audit.terminal_state, "cancelled");
    assert_eq!(audit.turn_id, plan.turn_id);
    assert_eq!(audit.usage, None);
}

#[tokio::test]
async fn a_provider_task_that_could_not_start_enqueues_the_failed_audit_event() {
    let (rig, _chat, plan) = setup().await;
    let target = TurnTarget::of_plan(&plan);
    rig.finalizer
        .finalize(
            &target,
            TurnOutcome::Failed {
                code: StreamErrorCode::ProviderError,
            },
        )
        .await
        .unwrap();
    let audits = audit_of(&rig).await;
    assert_eq!(audits.len(), 1, "{audits:?}");
    assert!(matches!(&audits[0], AuditEvent::TurnFailed(event)
        if event.error_code.as_deref() == Some("provider_error")));
}

// ── the thread-summary trigger ───────────────────────────────────────────────

/// A chat with one earlier completed turn, then a new committed turn whose context facts say
/// `tokens` of `budget` were assembled.
async fn with_history(
    tokens: i64,
    budget: i64,
    truncated: bool,
) -> (
    SendRig,
    crate::infra::storage::repo_impl::ChatRecord,
    ProviderCallPlan,
    Uuid,
) {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (_, old_assistant) =
        seed_completed_turn(&rig.quota.db, &chat, "old question", "old answer", t(10)).await;
    let mut plan = rig.plan_for(&chat, "new question").await;
    plan.context.assembled_context_tokens = tokens;
    plan.context.effective_budget = budget;
    plan.context.messages_truncated = truncated;
    (rig, chat, plan, old_assistant)
}

#[tokio::test]
async fn reaching_the_threshold_enqueues_one_summary_work_item_with_the_frozen_range() {
    let (rig, chat, plan, old_assistant) = with_history(900, 1000, false).await;
    committed(&rig, &plan, &completed("answer")).await;
    let rows = rig.summary_rows().await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let payload = &rows[0].payload;
    assert_eq!(payload["tenant_id"], chat.tenant_id.to_string());
    assert_eq!(payload["chat_id"], chat.id.to_string());
    assert_eq!(payload["system_task_type"], "thread_summary_update");
    // the causing turn's messages are never in the frozen range: the target is the latest
    // message of the previous turn
    assert_eq!(
        payload["frozen_target_message_id"],
        old_assistant.to_string()
    );
    assert!(payload["base_frontier_message_id"].is_null(), "{payload}");
    let system_request_id = payload["system_request_id"].as_str().unwrap();
    let parsed = Uuid::parse_str(system_request_id).unwrap();
    assert_eq!(parsed.get_version_num(), 4);
}

#[tokio::test]
async fn truncated_history_enqueues_urgently_even_far_below_the_threshold() {
    let (rig, _chat, plan, _) = with_history(10, 1000, true).await;
    committed(&rig, &plan, &completed("answer")).await;
    assert_eq!(rig.summary_rows().await.len(), 1);
}

#[tokio::test]
async fn below_the_threshold_nothing_is_enqueued() {
    let (rig, _chat, plan, _) = with_history(100, 1000, false).await;
    committed(&rig, &plan, &completed("answer")).await;
    assert!(rig.summary_rows().await.is_empty());
}

#[tokio::test]
async fn a_summary_that_already_covers_the_target_is_not_needed() {
    let (rig, chat, mut plan, old_assistant) = with_history(10, 1000, true).await;
    seed_summary(&rig.quota.db, &chat, "summary", 12, (t(11), old_assistant)).await;
    plan.context.messages_truncated = true;
    committed(&rig, &plan, &completed("answer")).await;
    assert!(rig.summary_rows().await.is_empty());
}

#[tokio::test]
async fn the_first_turn_of_an_empty_chat_has_no_target_and_enqueues_nothing() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let mut plan = rig.plan_for(&chat, "first").await;
    plan.context.assembled_context_tokens = 950;
    plan.context.effective_budget = 1000;
    committed(&rig, &plan, &completed("answer")).await;
    assert!(rig.summary_rows().await.is_empty());
}

#[tokio::test]
async fn failed_and_cancelled_turns_evaluate_no_trigger() {
    for terminal in [
        failed(StreamErrorCode::ProviderError, "", None),
        cancelled("partial"),
    ] {
        let (rig, _chat, plan, _) = with_history(990, 1000, true).await;
        committed(&rig, &plan, &terminal).await;
        assert!(rig.summary_rows().await.is_empty(), "{terminal:?}");
    }
}

#[tokio::test]
async fn a_disabled_summary_worker_enqueues_nothing() {
    let rig = send_rig_with(SendRigOptions::new().with_summary(SummarySettings {
        enabled: false,
        compression_threshold_pct: 80,
    }))
    .await;
    let chat = rig.std_chat().await;
    seed_completed_turn(&rig.quota.db, &chat, "q", "a", t(10)).await;
    let mut plan = rig.plan_for(&chat, "new").await;
    plan.context.assembled_context_tokens = 990;
    plan.context.effective_budget = 1000;
    committed(&rig, &plan, &completed("answer")).await;
    assert!(rig.summary_rows().await.is_empty());
}

#[tokio::test]
async fn a_failed_summary_enqueue_rolls_the_whole_finalization_back() {
    let failing = FailingSummaryOutbox::new();
    let rig = send_rig_with(SendRigOptions::new().with_turn_outbox(failing.clone())).await;
    failing.bind(rig.quota.enqueuer.clone());
    let chat = rig.std_chat().await;
    seed_completed_turn(&rig.quota.db, &chat, "q", "a", t(10)).await;
    let mut plan = rig.plan_for(&chat, "new").await;
    plan.context.assembled_context_tokens = 900;
    plan.context.effective_budget = 1000;
    let result = rig
        .finalizer
        .finalize_stream(&plan, &completed("answer"))
        .await;
    assert!(result.is_err(), "{result:?}");
    // the state change, the message, the settlement and the audit event rolled back together
    let turn = turn_rows(&rig.quota.db, chat.id)
        .await
        .into_iter()
        .find(|turn| turn.id == plan.turn_id)
        .unwrap();
    assert_eq!(turn.state, "running");
    assert_eq!(message_rows(&rig.quota.db, chat.id).await.len(), 3);
    assert!(rig.events().await.is_empty());
    assert!(audit_of(&rig).await.is_empty());
    assert!(reserved(&rig).await > 0);
}
