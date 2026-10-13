#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::let_underscore_must_use
)]
//! The mutation preview matrix: unknown turn, foreign requester, soft-deleted
//! target, running target, newer live turn, missing user message, and the
//! read-only order of the checks.

use std::sync::Arc;

use toolkit_security::AccessScope;
use tracing::Level;
use uuid::Uuid;

use super::preview;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::service::mutations::test_support::{
    FakeMutations, MutationCall, ORIGINAL, TARGET_USER_MESSAGE, latest_of, other_turn, target,
    target_message,
};
use crate::domain::service::mutations::types::{LatestTurn, MutationTarget};
use crate::domain::service::stream::test_support::{CHAT, REQUEST, TURN, at};
use crate::domain::service::stream::types::TurnState;
use crate::test_doubles::{LogCapture, OTHER_USER, USER, USER_TENANT};

fn scope() -> AccessScope {
    AccessScope::for_tenant(USER_TENANT)
}

/// A store with the default completed latest target turn and its user message.
fn store() -> Arc<FakeMutations> {
    let store = FakeMutations::new();
    let row = target(TurnState::Completed);
    store.with(|inner| {
        inner.latest = Some(latest_of(&row));
        inner.targets.push(row);
        inner.user_message = Some(target_message());
    });
    store
}

async fn run(store: &FakeMutations) -> Result<MutationTarget, DomainError> {
    preview(store, &scope(), CHAT, USER, REQUEST).await
}

fn refusal(result: Result<MutationTarget, DomainError>) -> DomainError {
    match result {
        Err(error) => error,
        Ok(target) => panic!("the preview was expected to refuse, got {target:?}"),
    }
}

#[tokio::test]
async fn the_latest_completed_turn_of_the_caller_is_returned_with_its_user_message() {
    let store = store();

    let previewed = run(&store).await.unwrap();

    assert_eq!(previewed.turn.id, TURN);
    assert_eq!(previewed.turn.request_id, REQUEST);
    assert!(!previewed.web_search_enabled);
    assert_eq!(previewed.user_message.id, TARGET_USER_MESSAGE);
    assert_eq!(previewed.user_message.content, ORIGINAL);
    assert_eq!(previewed.user_message.created_at, at(-61));
}

#[tokio::test]
async fn the_web_search_flag_of_the_target_is_returned() {
    let store = store();
    store.with(|inner| inner.targets[0].web_search_enabled = true);

    assert!(run(&store).await.unwrap().web_search_enabled);
}

#[tokio::test]
async fn a_failed_and_a_cancelled_turn_are_accepted_like_a_completed_one() {
    for state in [TurnState::Failed, TurnState::Cancelled] {
        let store = store();
        let row = target(state);
        store.with(|inner| {
            inner.latest = Some(latest_of(&row));
            inner.targets = vec![row];
        });

        assert!(run(&store).await.is_ok(), "{state:?}");
    }
}

#[tokio::test]
async fn the_reads_are_the_target_the_latest_turn_and_the_user_message_in_that_order() {
    let store = store();

    run(&store).await.unwrap();

    assert_eq!(
        store.calls(),
        vec![
            MutationCall::FindTarget(CHAT, REQUEST),
            MutationCall::Latest(CHAT),
            MutationCall::UserMessage(CHAT, REQUEST),
        ]
    );
}

#[tokio::test]
async fn every_read_runs_with_the_scope_of_the_request() {
    let store = store();

    run(&store).await.unwrap();

    assert_eq!(store.with(|inner| inner.scopes.clone()), vec![scope(); 3]);
}

#[tokio::test]
async fn an_unknown_request_id_is_a_missing_turn_and_nothing_else_is_read() {
    let store = store();

    let error = refusal(preview(&*store, &scope(), CHAT, USER, Uuid::from_u128(0xDEAD)).await);

    assert!(
        matches!(
            error,
            DomainError::NotFound {
                resource: ResourceKind::Turn
            }
        ),
        "{error:?}"
    );
    assert_eq!(store.calls().len(), 1, "only the lookup of the target ran");
}

#[tokio::test]
async fn a_turn_of_another_chat_is_a_missing_turn() {
    let store = store();
    store.with(|inner| inner.targets[0].turn.chat_id = Uuid::from_u128(0xC2));

    let error = refusal(run(&store).await);

    assert!(matches!(
        error,
        DomainError::NotFound {
            resource: ResourceKind::Turn
        }
    ));
}

#[tokio::test]
async fn a_foreign_requester_is_authz_denied() {
    let store = store();
    store.with(|inner| inner.targets[0].turn.requester_user_id = Some(OTHER_USER));

    let error = refusal(run(&store).await);

    assert!(matches!(error, DomainError::AuthzDenied), "{error:?}");
}

#[tokio::test]
async fn a_turn_without_a_requester_is_authz_denied() {
    let store = store();
    store.with(|inner| inner.targets[0].turn.requester_user_id = None);

    let error = refusal(run(&store).await);

    assert!(matches!(error, DomainError::AuthzDenied), "{error:?}");
}

#[tokio::test]
async fn the_requester_check_runs_before_the_deleted_and_the_state_checks() {
    let store = store();
    store.with(|inner| {
        inner.targets[0].turn.requester_user_id = Some(OTHER_USER);
        inner.targets[0].turn.deleted = true;
        inner.targets[0].turn.state = TurnState::Running;
    });

    let error = refusal(run(&store).await);

    assert!(matches!(error, DomainError::AuthzDenied), "{error:?}");
    assert_eq!(store.calls().len(), 1);
}

#[tokio::test]
async fn a_soft_deleted_target_is_not_the_latest_turn() {
    let store = store();
    store.with(|inner| inner.targets[0].turn.deleted = true);

    let error = refusal(run(&store).await);

    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
    assert_eq!(store.calls().len(), 1, "no latest-turn read");
}

#[tokio::test]
async fn a_soft_deleted_running_target_is_not_the_latest_turn_rather_than_not_terminal() {
    let store = store();
    store.with(|inner| {
        inner.targets[0].turn.deleted = true;
        inner.targets[0].turn.state = TurnState::Running;
    });

    let error = refusal(run(&store).await);

    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
}

#[tokio::test]
async fn a_running_target_is_not_terminal() {
    let store = store();
    store.with(|inner| inner.targets[0].turn.state = TurnState::Running);

    let error = refusal(run(&store).await);

    assert!(matches!(error, DomainError::TurnNotTerminal), "{error:?}");
}

#[tokio::test]
async fn the_state_check_runs_before_the_latest_turn_check() {
    // A running target that is not the latest turn is still `turn_state`.
    let store = store();
    let newer = other_turn(1, TurnState::Completed, 10);
    store.with(|inner| {
        inner.targets[0].turn.state = TurnState::Running;
        inner.latest = Some(LatestTurn {
            id: newer.id,
            started_at: newer.started_at,
        });
    });

    let error = refusal(run(&store).await);

    assert!(matches!(error, DomainError::TurnNotTerminal), "{error:?}");
    assert_eq!(store.calls().len(), 1, "no latest-turn read");
}

#[tokio::test]
async fn a_newer_live_turn_makes_the_target_not_the_latest() {
    let store = store();
    let newer = other_turn(1, TurnState::Completed, 10);
    store.with(|inner| {
        inner.latest = Some(LatestTurn {
            id: newer.id,
            started_at: newer.started_at,
        });
    });

    let error = refusal(run(&store).await);

    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
    assert_eq!(store.calls().len(), 2, "the user message is not read");
}

#[tokio::test]
async fn a_newer_running_turn_makes_the_target_not_the_latest() {
    let store = store();
    let newer = other_turn(1, TurnState::Running, 10);
    store.with(|inner| {
        inner.latest = Some(LatestTurn {
            id: newer.id,
            started_at: newer.started_at,
        });
    });

    let error = refusal(run(&store).await);

    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
}

#[tokio::test]
async fn an_equal_start_time_is_decided_by_the_id_of_the_latest_row() {
    // The store orders by (started_at DESC, id DESC); the preview compares ids.
    let store = store();
    store.with(|inner| {
        inner.latest = Some(LatestTurn {
            id: Uuid::from_u128(u128::MAX),
            started_at: inner.targets[0].turn.started_at,
        });
    });

    let error = refusal(run(&store).await);

    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
}

#[tokio::test]
async fn a_chat_without_a_live_turn_cannot_have_a_latest_target() {
    let store = store();
    store.with(|inner| inner.latest = None);

    let error = refusal(run(&store).await);

    assert!(matches!(error, DomainError::NotLatestTurn), "{error:?}");
}

#[tokio::test]
async fn a_turn_without_its_user_message_is_an_internal_error_logged_with_the_ids() {
    let logs = LogCapture::start();
    let store = store();
    store.with(|inner| inner.user_message = None);

    let error = refusal(run(&store).await);

    assert!(matches!(error, DomainError::Internal(_)), "{error:?}");
    let errors = logs.at(Level::ERROR);
    assert_eq!(errors.len(), 1, "{errors:?}");
    let text = errors[0].text();
    for id in [CHAT, TURN, REQUEST] {
        assert!(text.contains(&id.to_string()), "`{text}` names {id}");
    }
}

#[tokio::test]
async fn a_read_failure_is_a_database_error() {
    let store = store();
    store.with(|inner| inner.read_error = Some("down".to_owned()));

    let error = refusal(run(&store).await);

    assert!(matches!(error, DomainError::Database(_)), "{error:?}");
}

#[tokio::test]
async fn the_preview_never_writes() {
    let store = store();

    assert!(run(&store).await.is_ok(), "the preview itself works");

    assert_eq!(
        store.count(|call| matches!(
            call,
            MutationCall::Replace(_) | MutationCall::Delete(_) | MutationCall::Mark { .. }
        )),
        0
    );
}
