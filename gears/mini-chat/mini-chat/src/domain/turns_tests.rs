//! The turn status projection (slice S1-setup-path of
//! `cpt-cf-mini-chat-feature-message-streaming`): the state mapping, the `error_code` and
//! `assistant_message_id` rules and the omission of absent fields.
//!
//! Algorithm `cpt-cf-mini-chat-algo-message-streaming-turn-status`; definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-turn-status`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use time::OffsetDateTime;
use uuid::Uuid;

use super::{TurnRow, TurnState, TurnStatusState, project_turn_status};

fn instant(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).unwrap()
}

fn row(state: TurnState, error_code: Option<&str>, assistant: Option<Uuid>) -> TurnRow {
    TurnRow {
        id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
        requester_user_id: Some(Uuid::new_v4()),
        state,
        assistant_message_id: assistant,
        error_code: error_code.map(ToOwned::to_owned),
        effective_model: Some("gpt".to_owned()),
        deleted_at: None,
        started_at: instant(1),
        updated_at: instant(9),
    }
}

#[test]
fn the_four_internal_states_map_to_running_done_error_and_cancelled() {
    let cases = [
        (TurnState::Running, TurnStatusState::Running, "running"),
        (TurnState::Completed, TurnStatusState::Done, "done"),
        (TurnState::Failed, TurnStatusState::Error, "error"),
        (
            TurnState::Cancelled,
            TurnStatusState::Cancelled,
            "cancelled",
        ),
    ];
    for (internal, expected, wire) in cases {
        let view = project_turn_status(&row(internal, None, None));
        assert_eq!(view.state, expected, "{internal:?}");
        assert_eq!(view.state.as_str(), wire);
    }
}

#[test]
fn request_id_and_updated_at_are_carried() {
    let turn = row(TurnState::Running, None, None);
    let view = project_turn_status(&turn);
    assert_eq!(view.request_id, turn.request_id);
    assert_eq!(view.updated_at, instant(9));
}

#[test]
fn a_completed_turn_reports_its_assistant_message_and_no_error_code() {
    let message = Uuid::new_v4();
    let view = project_turn_status(&row(TurnState::Completed, None, Some(message)));
    assert_eq!(view.assistant_message_id, Some(message));
    assert_eq!(view.error_code, None);
}

#[test]
fn only_a_failed_turn_reports_its_error_code() {
    for code in [
        "provider_error",
        "orphan_timeout",
        "context_length_exceeded",
        "turn_setup_failed",
    ] {
        let view = project_turn_status(&row(TurnState::Failed, Some(code), None));
        assert_eq!(view.error_code.as_deref(), Some(code));
    }
    // a stray code on any other state is never reported
    for state in [
        TurnState::Running,
        TurnState::Completed,
        TurnState::Cancelled,
    ] {
        let view = project_turn_status(&row(state, Some("provider_error"), None));
        assert_eq!(view.error_code, None, "{state:?}");
    }
}

#[test]
fn a_failed_turn_never_reports_an_assistant_message() {
    let view = project_turn_status(&row(
        TurnState::Failed,
        Some("provider_error"),
        Some(Uuid::new_v4()),
    ));
    assert_eq!(view.assistant_message_id, None);
}

#[test]
fn a_running_turn_never_reports_an_assistant_message() {
    let view = project_turn_status(&row(TurnState::Running, None, Some(Uuid::new_v4())));
    assert_eq!(view.assistant_message_id, None);
}

#[test]
fn a_cancelled_turn_reports_its_partial_message_only_when_one_was_persisted() {
    let message = Uuid::new_v4();
    let with = project_turn_status(&row(TurnState::Cancelled, None, Some(message)));
    assert_eq!(with.assistant_message_id, Some(message));
    let without = project_turn_status(&row(TurnState::Cancelled, None, None));
    assert_eq!(without.assistant_message_id, None);
}
