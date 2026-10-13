#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 1: idempotency and the parallel-turn guard
//! (`cpt-cf-mini-chat-algo-streaming-turns-resolve-idempotency`).

use toolkit_security::AccessScope;

use super::*;
use crate::domain::service::stream::test_support::TurnsCall;
use crate::domain::service::stream::test_support::{CHAT, FakeTurns, REQUEST, other_running, turn};
use crate::domain::service::stream::types::TurnState;
use crate::test_doubles::{LogCapture, USER_TENANT};

fn scope() -> AccessScope {
    AccessScope::for_tenant(USER_TENANT)
}

async fn resolve(turns: &FakeTurns) -> Result<IdempotencyOutcome, DomainError> {
    resolve_idempotency(turns, &scope(), CHAT, REQUEST).await
}

#[tokio::test]
async fn nothing_in_the_way_proceeds_after_both_reads() {
    let turns = FakeTurns::new();
    assert_eq!(resolve(&turns).await.unwrap(), IdempotencyOutcome::Proceed);
    assert_eq!(
        turns.calls(),
        vec![
            TurnsCall::FindByRequestId(CHAT, REQUEST),
            TurnsCall::FindRunning(CHAT)
        ]
    );
}

#[tokio::test]
async fn a_completed_turn_replays_and_the_guard_is_not_consulted() {
    let turns = FakeTurns::new();
    let completed = turn(TurnState::Completed);
    turns.with(|t| {
        t.by_request.push(completed.clone());
        // Another turn of the chat is running: replay still wins.
        t.running = Some(other_running());
    });
    assert_eq!(
        resolve(&turns).await.unwrap(),
        IdempotencyOutcome::Replay(completed)
    );
    assert_eq!(
        turns.calls(),
        vec![TurnsCall::FindByRequestId(CHAT, REQUEST)],
        "replay is decided before the parallel-turn guard"
    );
}

#[tokio::test]
async fn a_soft_deleted_completed_turn_is_a_request_id_conflict() {
    let turns = FakeTurns::new();
    let mut deleted = turn(TurnState::Completed);
    deleted.deleted = true;
    turns.with(|t| t.by_request.push(deleted));
    assert_eq!(
        resolve(&turns).await.unwrap(),
        IdempotencyOutcome::RequestIdConflict
    );
    assert_eq!(turns.count(|c| matches!(c, TurnsCall::FindRunning(_))), 0);
}

#[tokio::test]
async fn running_failed_and_cancelled_turns_conflict_on_their_request_id() {
    for state in [TurnState::Running, TurnState::Failed, TurnState::Cancelled] {
        let turns = FakeTurns::new();
        turns.with(|t| t.by_request.push(turn(state)));
        assert_eq!(
            resolve(&turns).await.unwrap(),
            IdempotencyOutcome::RequestIdConflict,
            "{state:?}"
        );
    }
}

#[tokio::test]
async fn every_soft_deleted_state_conflicts() {
    for state in [
        TurnState::Running,
        TurnState::Completed,
        TurnState::Failed,
        TurnState::Cancelled,
    ] {
        let turns = FakeTurns::new();
        let mut row = turn(state);
        row.deleted = true;
        turns.with(|t| t.by_request.push(row));
        assert_eq!(
            resolve(&turns).await.unwrap(),
            IdempotencyOutcome::RequestIdConflict,
            "{state:?}"
        );
    }
}

#[tokio::test]
async fn another_running_turn_blocks_a_new_request_id() {
    let turns = FakeTurns::new();
    turns.with(|t| t.running = Some(other_running()));
    assert_eq!(
        resolve(&turns).await.unwrap(),
        IdempotencyOutcome::TurnAlreadyRunning
    );
}

#[tokio::test]
async fn a_repository_error_is_propagated() {
    let turns = FakeTurns::new();
    turns.with(|t| t.read_error = Some("boom".to_owned()));
    assert!(matches!(
        resolve(&turns).await,
        Err(DomainError::Database(_))
    ));
}

// ---- the insert race ----------------------------------------------------------------------------

async fn race(turns: &FakeTurns) -> Result<IdempotencyOutcome, DomainError> {
    resolve_unique_violation(turns, &scope(), CHAT, REQUEST).await
}

#[tokio::test]
async fn a_race_won_by_a_completed_turn_replays() {
    let turns = FakeTurns::new();
    let completed = turn(TurnState::Completed);
    turns.with(|t| t.by_request.push(completed.clone()));
    assert_eq!(
        race(&turns).await.unwrap(),
        IdempotencyOutcome::Replay(completed)
    );
}

#[tokio::test]
async fn a_race_won_by_a_non_completed_turn_is_a_request_id_conflict() {
    for state in [TurnState::Running, TurnState::Failed, TurnState::Cancelled] {
        let turns = FakeTurns::new();
        turns.with(|t| t.by_request.push(turn(state)));
        assert_eq!(
            race(&turns).await.unwrap(),
            IdempotencyOutcome::RequestIdConflict,
            "{state:?}"
        );
    }
}

#[tokio::test]
async fn a_race_lost_to_another_request_is_turn_already_running() {
    let turns = FakeTurns::new();
    turns.with(|t| t.running = Some(other_running()));
    assert_eq!(
        race(&turns).await.unwrap(),
        IdempotencyOutcome::TurnAlreadyRunning
    );
    assert_eq!(
        turns.calls(),
        vec![
            TurnsCall::FindByRequestId(CHAT, REQUEST),
            TurnsCall::FindRunning(CHAT)
        ],
        "the key is re-read first, then the running turn"
    );
}

#[tokio::test]
async fn a_violation_without_any_conflicting_row_is_internal_and_logged() {
    let logs = LogCapture::start();
    let turns = FakeTurns::new();
    let error = race(&turns).await.unwrap_err();
    assert!(matches!(error, DomainError::Internal(_)), "{error:?}");
    let errors = logs.at(tracing::Level::ERROR);
    assert_eq!(errors.len(), 1, "one error record");
    let text = errors[0].text();
    assert!(text.contains(&CHAT.to_string()), "{text}");
    assert!(text.contains(&REQUEST.to_string()), "{text}");
}
