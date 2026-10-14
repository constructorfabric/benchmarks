//! Turn admission checks (slice S1-setup-path of
//! `cpt-cf-mini-chat-feature-message-streaming`): the check order (idempotency first, then the
//! parallel-turn guard) for every existing state, soft-deleted rows and replay while another turn
//! runs.
//!
//! Algorithm `cpt-cf-mini-chat-algo-message-streaming-turn-admission-checks`; definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-idempotency`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use time::OffsetDateTime;
use uuid::Uuid;

use super::{AdmissionDecision, decide_admission};
use crate::domain::error::{ConflictReason, DomainError};
use crate::domain::turns::{TurnRow, TurnState};

fn turn(state: TurnState, deleted: bool) -> TurnRow {
    let when = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    TurnRow {
        id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
        requester_user_id: Some(Uuid::new_v4()),
        state,
        assistant_message_id: matches!(state, TurnState::Completed).then(Uuid::new_v4),
        error_code: None,
        effective_model: Some("gpt".to_owned()),
        deleted_at: deleted.then_some(when),
        started_at: when,
        updated_at: when,
    }
}

fn conflict(result: Result<AdmissionDecision, DomainError>) -> ConflictReason {
    match result {
        Err(DomainError::Conflict { reason }) => reason,
        other => panic!("expected a conflict, got {other:?}"),
    }
}

#[test]
fn no_row_and_no_running_turn_proceeds() {
    let decision = decide_admission(true, None, false).unwrap();
    assert_eq!(decision, AdmissionDecision::Proceed);
}

#[test]
fn a_completed_live_turn_is_replayed() {
    let completed = turn(TurnState::Completed, false);
    let decision = decide_admission(true, Some(&completed), false).unwrap();
    assert_eq!(decision, AdmissionDecision::Replay(Box::new(completed)));
}

#[test]
fn replay_is_decided_before_the_parallel_turn_guard() {
    let completed = turn(TurnState::Completed, false);
    let decision = decide_admission(true, Some(&completed), true).unwrap();
    assert_eq!(decision, AdmissionDecision::Replay(Box::new(completed)));
}

#[test]
fn a_running_row_for_the_key_is_a_request_id_conflict() {
    let running = turn(TurnState::Running, false);
    for running_exists in [true, false] {
        let reason = conflict(decide_admission(true, Some(&running), running_exists));
        assert_eq!(reason, ConflictReason::RequestIdConflict);
    }
}

#[test]
fn a_failed_or_cancelled_row_for_the_key_is_a_request_id_conflict() {
    for state in [TurnState::Failed, TurnState::Cancelled] {
        let row = turn(state, false);
        for running_exists in [true, false] {
            let reason = conflict(decide_admission(true, Some(&row), running_exists));
            assert_eq!(reason, ConflictReason::RequestIdConflict, "{state:?}");
        }
    }
}

#[test]
fn a_soft_deleted_row_of_any_state_is_a_request_id_conflict() {
    for state in [
        TurnState::Running,
        TurnState::Completed,
        TurnState::Failed,
        TurnState::Cancelled,
    ] {
        let row = turn(state, true);
        for running_exists in [true, false] {
            let reason = conflict(decide_admission(true, Some(&row), running_exists));
            assert_eq!(
                reason,
                ConflictReason::RequestIdConflict,
                "{state:?} deleted"
            );
        }
    }
}

#[test]
fn no_row_but_another_running_turn_is_turn_already_running() {
    let reason = conflict(decide_admission(true, None, true));
    assert_eq!(reason, ConflictReason::TurnAlreadyRunning);
}

#[test]
fn a_server_generated_key_skips_the_key_lookup_result() {
    // a generated request id cannot collide, so only the parallel-turn guard applies
    let completed = turn(TurnState::Completed, false);
    assert_eq!(
        decide_admission(false, Some(&completed), false).unwrap(),
        AdmissionDecision::Proceed
    );
    assert_eq!(
        conflict(decide_admission(false, Some(&completed), true)),
        ConflictReason::TurnAlreadyRunning
    );
    assert_eq!(
        conflict(decide_admission(false, None, true)),
        ConflictReason::TurnAlreadyRunning
    );
    assert_eq!(
        decide_admission(false, None, false).unwrap(),
        AdmissionDecision::Proceed
    );
}

#[test]
fn the_conflict_reasons_have_their_wire_values() {
    assert_eq!(
        ConflictReason::RequestIdConflict.as_str(),
        "request_id_conflict"
    );
    assert_eq!(
        ConflictReason::TurnAlreadyRunning.as_str(),
        "turn_already_running"
    );
}
