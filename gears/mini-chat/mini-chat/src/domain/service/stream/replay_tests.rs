#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 1: completed turn replay
//! (`cpt-cf-mini-chat-algo-streaming-turns-replay-completed`).

use toolkit_security::AccessScope;

use super::*;
use crate::domain::service::quota::preflight::QuotaDecision;
use crate::domain::service::stream::ports::StoredAssistantMessage;
use crate::domain::service::stream::test_support::{
    ASSISTANT, CHAT, FakeTurns, REQUEST, TURN, TurnsCall, chat, turn,
};
use crate::domain::service::stream::types::{DeltaKind, DoneEvent, StreamStarted, TurnState};
use crate::test_doubles::{LogCapture, USER_TENANT};

fn stored(content: &str) -> StoredAssistantMessage {
    StoredAssistantMessage {
        id: ASSISTANT,
        content: content.to_owned(),
        input_tokens: 120,
        output_tokens: 34,
    }
}

#[test]
fn replay_is_started_one_delta_and_done_exactly() {
    let events = build_replay(
        &chat("p"),
        &turn(TurnState::Completed),
        &stored("The answer."),
    );
    assert_eq!(
        events,
        vec![
            StreamEvent::StreamStarted(StreamStarted {
                request_id: REQUEST,
                message_id: ASSISTANT,
                is_new_turn: false,
                thread_summary_applied: None,
            }),
            StreamEvent::Delta {
                kind: DeltaKind::Text,
                content: "The answer.".to_owned(),
            },
            StreamEvent::Done(DoneEvent {
                usage: (120, 34),
                effective_model: "p".to_owned(),
                selected_model: "p".to_owned(),
                quota_decision: QuotaDecision::Allow,
                downgrade_from: None,
                downgrade_reason: None,
                quota_warnings: None,
            }),
        ]
    );
}

#[test]
fn replay_has_no_ping_and_no_citations() {
    let events = build_replay(&chat("p"), &turn(TurnState::Completed), &stored("x"));
    let names: Vec<_> = events.iter().map(StreamEvent::name).collect();
    assert_eq!(names, ["stream_started", "delta", "done"]);
}

#[test]
fn an_empty_persisted_text_is_still_one_delta() {
    let events = build_replay(&chat("p"), &turn(TurnState::Completed), &stored(""));
    assert_eq!(
        events[1],
        StreamEvent::Delta {
            kind: DeltaKind::Text,
            content: String::new(),
        }
    );
    assert_eq!(events.len(), 3);
}

#[test]
fn a_downgraded_turn_rebuilds_the_decision_without_a_reason() {
    let mut row = turn(TurnState::Completed);
    row.effective_model = Some("s".to_owned());
    let events = build_replay(&chat("p"), &row, &stored("x"));
    let StreamEvent::Done(done) = &events[2] else {
        panic!("done expected");
    };
    assert_eq!(done.effective_model, "s");
    assert_eq!(done.selected_model, "p");
    assert_eq!(done.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(done.downgrade_from.as_deref(), Some("p"));
    assert_eq!(done.downgrade_reason, None);
    assert_eq!(done.quota_warnings, None);
}

#[test]
fn a_turn_without_an_effective_model_counts_as_the_selected_model() {
    let mut row = turn(TurnState::Completed);
    row.effective_model = None;
    let events = build_replay(&chat("p"), &row, &stored("x"));
    let StreamEvent::Done(done) = &events[2] else {
        panic!("done expected");
    };
    assert_eq!(done.effective_model, "p");
    assert_eq!(done.quota_decision, QuotaDecision::Allow);
}

#[tokio::test]
async fn replay_loads_only_the_assistant_message_of_the_chat() {
    let turns = FakeTurns::new();
    turns.with(|t| t.assistant = Some(stored("saved")));
    let scope = AccessScope::for_tenant(USER_TENANT);
    let events = replay_completed(&*turns, &scope, &chat("p"), &turn(TurnState::Completed))
        .await
        .unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(
        turns.calls(),
        vec![TurnsCall::LoadAssistant(CHAT, ASSISTANT)]
    );
}

#[tokio::test]
async fn a_completed_turn_without_its_message_is_internal_and_logged_at_error() {
    let logs = LogCapture::start();
    let turns = FakeTurns::new();
    let scope = AccessScope::for_tenant(USER_TENANT);
    let error = replay_completed(&*turns, &scope, &chat("p"), &turn(TurnState::Completed))
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::Internal(_)), "{error:?}");
    let records = logs.at(tracing::Level::ERROR);
    assert_eq!(records.len(), 1);
    let text = records[0].text();
    for id in [CHAT, TURN, REQUEST] {
        assert!(text.contains(&id.to_string()), "{text}");
    }
}
