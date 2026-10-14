//! The replay stream builder (slice S1-setup-path of
//! `cpt-cf-mini-chat-feature-message-streaming`): the three events of a side-effect-free replay,
//! with and without a downgrade, and the persisted-row guards.
//!
//! Algorithm `cpt-cf-mini-chat-algo-message-streaming-replay`; definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-idempotency`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::QuotaDecision;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{DeltaKind, StreamEvent, build_replay};
use crate::domain::error::DomainError;
use crate::domain::messages::MessageRow;
use crate::domain::turns::{TurnRow, TurnState};

fn when() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

struct Fixture {
    turn: TurnRow,
    assistant: MessageRow,
}

fn fixture(content: &str, effective: Option<&str>) -> Fixture {
    let request_id = Uuid::new_v4();
    let assistant_id = Uuid::new_v4();
    Fixture {
        turn: TurnRow {
            id: Uuid::new_v4(),
            chat_id: Uuid::new_v4(),
            request_id,
            requester_user_id: Some(Uuid::new_v4()),
            state: TurnState::Completed,
            assistant_message_id: Some(assistant_id),
            error_code: None,
            effective_model: effective.map(ToOwned::to_owned),
            deleted_at: None,
            started_at: when(),
            updated_at: when(),
        },
        assistant: MessageRow {
            id: assistant_id,
            request_id: Some(request_id),
            role: "assistant".to_owned(),
            content: content.to_owned(),
            model: effective.map(ToOwned::to_owned),
            input_tokens: 500,
            output_tokens: 120,
            created_at: when(),
        },
    }
}

fn events(f: &Fixture, selected: &str) -> Vec<StreamEvent> {
    build_replay(&f.turn, Some(&f.assistant), selected).unwrap()
}

#[test]
fn a_replay_is_exactly_stream_started_one_delta_and_done() {
    let f = fixture("the stored answer", Some("gpt-a"));
    let events = events(&f, "gpt-a");
    assert_eq!(events.len(), 3, "{events:?}");
    assert!(matches!(events[0], StreamEvent::StreamStarted(_)));
    assert!(matches!(events[1], StreamEvent::Delta(_)));
    assert!(matches!(events[2], StreamEvent::Done(_)));
}

#[test]
fn stream_started_carries_the_persisted_message_and_is_not_a_new_turn() {
    let f = fixture("answer", Some("gpt-a"));
    let StreamEvent::StreamStarted(started) = &events(&f, "gpt-a")[0] else {
        panic!("first event is not stream_started");
    };
    assert_eq!(started.request_id, f.turn.request_id);
    assert_eq!(started.message_id, f.assistant.id);
    assert!(!started.is_new_turn);
    assert_eq!(started.thread_summary_token_estimate, None);
}

#[test]
fn the_delta_is_the_full_persisted_text() {
    let text = "line one\nline two \u{2713} with   spacing ";
    let f = fixture(text, Some("gpt-a"));
    let StreamEvent::Delta(delta) = &events(&f, "gpt-a")[1] else {
        panic!("second event is not a delta");
    };
    assert_eq!(delta.kind, DeltaKind::Text);
    assert_eq!(delta.content, text);
}

#[test]
fn an_empty_persisted_text_is_still_one_empty_delta() {
    let f = fixture("", Some("gpt-a"));
    let events = events(&f, "gpt-a");
    assert_eq!(events.len(), 3);
    let StreamEvent::Delta(delta) = &events[1] else {
        panic!("second event is not a delta");
    };
    assert_eq!(delta.content, "");
}

#[test]
fn done_without_a_downgrade_is_allow() {
    let f = fixture("answer", Some("gpt-a"));
    let StreamEvent::Done(done) = &events(&f, "gpt-a")[2] else {
        panic!("third event is not done");
    };
    assert_eq!(done.input_tokens, 500);
    assert_eq!(done.output_tokens, 120);
    assert_eq!(done.effective_model, "gpt-a");
    assert_eq!(done.selected_model, "gpt-a");
    assert_eq!(done.quota_decision, QuotaDecision::Allow);
    assert_eq!(done.downgrade_from, None);
}

#[test]
fn done_after_a_downgrade_names_the_selected_model_as_the_origin() {
    let f = fixture("answer", Some("gpt-standard"));
    let StreamEvent::Done(done) = &events(&f, "gpt-premium")[2] else {
        panic!("third event is not done");
    };
    assert_eq!(done.effective_model, "gpt-standard");
    assert_eq!(done.selected_model, "gpt-premium");
    assert_eq!(done.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(done.downgrade_from.as_deref(), Some("gpt-premium"));
}

#[test]
fn a_replay_never_carries_a_downgrade_reason_or_quota_warnings() {
    for (effective, selected) in [("a", "a"), ("a", "b")] {
        let f = fixture("answer", Some(effective));
        let StreamEvent::Done(done) = &events(&f, selected)[2] else {
            panic!("third event is not done");
        };
        assert_eq!(done.downgrade_reason, None);
        assert_eq!(done.quota_warnings, None);
    }
}

#[test]
fn a_missing_assistant_message_is_an_internal_error() {
    let f = fixture("answer", Some("gpt-a"));
    let result = build_replay(&f.turn, None, "gpt-a");
    assert!(
        matches!(result, Err(DomainError::Internal { .. })),
        "{result:?}"
    );
}

#[test]
fn an_assistant_message_without_a_request_id_is_an_internal_error() {
    let mut f = fixture("answer", Some("gpt-a"));
    f.assistant.request_id = None;
    let result = build_replay(&f.turn, Some(&f.assistant), "gpt-a");
    assert!(
        matches!(result, Err(DomainError::Internal { .. })),
        "{result:?}"
    );
}
