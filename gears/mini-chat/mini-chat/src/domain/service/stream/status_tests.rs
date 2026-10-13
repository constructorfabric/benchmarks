#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 6: the turn status
//! (`cpt-cf-mini-chat-flow-streaming-turns-get-turn-status`).

use super::*;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::service::stream::test_support::{
    ASSISTANT, CHAT, FakeTurns, REQUEST, TurnsCall, World, ctx, turn,
};
use crate::test_doubles::PdpReply;

fn with_code(mut row: TurnRecord, code: Option<&str>) -> TurnRecord {
    row.error_code = code.map(str::to_owned);
    row
}

#[test]
fn a_running_turn_has_no_code_and_no_message() {
    let mut row = turn(TurnState::Running);
    row.error_code = Some("leftover".to_owned());
    row.assistant_message_id = Some(ASSISTANT);
    let status = project_status(&row);
    assert_eq!(status.state, TurnState::Running);
    assert_eq!(status.state.api_state(), "running");
    assert_eq!(status.error_code, None);
    assert_eq!(status.assistant_message_id, None);
    assert_eq!(status.request_id, REQUEST);
    assert_eq!(status.updated_at, row.updated_at);
}

#[test]
fn a_completed_turn_is_done_with_its_message() {
    let status = project_status(&turn(TurnState::Completed));
    assert_eq!(status.state.api_state(), "done");
    assert_eq!(status.assistant_message_id, Some(ASSISTANT));
    assert_eq!(status.error_code, None);
}

#[test]
fn a_failed_turn_is_an_error_with_its_code_and_no_message() {
    let mut row = with_code(turn(TurnState::Failed), Some("orphan_timeout"));
    row.assistant_message_id = Some(ASSISTANT);
    let status = project_status(&row);
    assert_eq!(status.state.api_state(), "error");
    assert_eq!(status.error_code.as_deref(), Some("orphan_timeout"));
    assert_eq!(status.assistant_message_id, None);
}

#[test]
fn a_failed_turn_without_a_stored_code_has_no_code() {
    let status = project_status(&with_code(turn(TurnState::Failed), None));
    assert_eq!(status.error_code, None);
}

#[test]
fn a_cancelled_turn_carries_the_message_only_when_a_partial_one_was_persisted() {
    let mut row = turn(TurnState::Cancelled);
    row.assistant_message_id = Some(ASSISTANT);
    row.error_code = Some("ignored".to_owned());
    let status = project_status(&row);
    assert_eq!(status.state.api_state(), "cancelled");
    assert_eq!(status.assistant_message_id, Some(ASSISTANT));
    assert_eq!(status.error_code, None);

    row.assistant_message_id = None;
    assert_eq!(project_status(&row).assistant_message_id, None);
}

#[test]
fn the_api_states_map_one_to_one() {
    assert_eq!(TurnState::Running.api_state(), "running");
    assert_eq!(TurnState::Completed.api_state(), "done");
    assert_eq!(TurnState::Failed.api_state(), "error");
    assert_eq!(TurnState::Cancelled.api_state(), "cancelled");
}

// ---- the service --------------------------------------------------------------------------------

fn world_with_turn(row: TurnRecord) -> World {
    let world = World::new();
    world.turns.with(|t| t.by_request.push(row));
    world
}

#[tokio::test]
async fn the_status_of_a_known_turn_is_projected() {
    let world = world_with_turn(turn(TurnState::Completed));
    let status = world
        .service
        .turn_status(&ctx(), CHAT, REQUEST)
        .await
        .unwrap();
    assert_eq!(status.state, TurnState::Completed);
    assert_eq!(status.assistant_message_id, Some(ASSISTANT));
}

#[tokio::test]
async fn read_turn_is_authorized_on_the_chat_and_send_message_is_not() {
    let world = world_with_turn(turn(TurnState::Running));
    world
        .service
        .turn_status(&ctx(), CHAT, REQUEST)
        .await
        .unwrap();
    let requests = world.pdp.requests();
    assert_eq!(requests.len(), 1, "one evaluation");
    assert_eq!(requests[0].action.name, "read_turn");
    assert_eq!(requests[0].resource.id, Some(CHAT));
}

#[tokio::test]
async fn the_turn_is_read_through_the_loaded_chat_and_the_scope() {
    let world = world_with_turn(turn(TurnState::Running));
    world
        .service
        .turn_status(&ctx(), CHAT, REQUEST)
        .await
        .unwrap();
    assert_eq!(
        world.turns.calls(),
        vec![TurnsCall::FindByRequestId(CHAT, REQUEST)]
    );
    let loads = world.chats.loads.lock().unwrap().clone();
    assert_eq!(loads.len(), 1);
    assert_eq!(loads[0].1, CHAT);
    let scopes = world.turns.with(|t| t.scopes.clone());
    assert_eq!(
        scopes,
        vec![loads[0].0.clone()],
        "the scope of the PEP decision"
    );
}

#[tokio::test]
async fn an_unknown_turn_is_a_404_of_the_turn_type() {
    let world = World::new();
    let error = world
        .service
        .turn_status(&ctx(), CHAT, REQUEST)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            DomainError::NotFound {
                resource: ResourceKind::Turn
            }
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_soft_deleted_turn_is_a_404_of_the_turn_type() {
    let mut row = turn(TurnState::Completed);
    row.deleted = true;
    let world = world_with_turn(row);
    let error = world
        .service
        .turn_status(&ctx(), CHAT, REQUEST)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        DomainError::NotFound {
            resource: ResourceKind::Turn
        }
    ));
}

#[tokio::test]
async fn a_missing_or_foreign_chat_is_a_404_of_the_chat_type_before_the_turn_is_read() {
    let world = World::build(
        None,
        PdpReply::AllowScoped,
        crate::domain::service::stream::test_support::settings(),
    );
    let error = world
        .service
        .turn_status(&ctx(), CHAT, REQUEST)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        DomainError::NotFound {
            resource: ResourceKind::Chat
        }
    ));
    assert!(world.turns.calls().is_empty());
}

#[tokio::test]
async fn a_pdp_deny_is_authz_denied_and_nothing_is_read() {
    let world = World::build(
        Some(crate::domain::service::stream::test_support::chat("p")),
        PdpReply::Deny,
        crate::domain::service::stream::test_support::settings(),
    );
    let error = world
        .service
        .turn_status(&ctx(), CHAT, REQUEST)
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::AuthzDenied), "{error:?}");
    assert!(world.turns.calls().is_empty());
    assert!(world.chats.loads.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_pdp_failure_is_unavailable() {
    let world = World::build(
        Some(crate::domain::service::stream::test_support::chat("p")),
        PdpReply::Failure,
        crate::domain::service::stream::test_support::settings(),
    );
    let error = world
        .service
        .turn_status(&ctx(), CHAT, REQUEST)
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::PdpUnavailable), "{error:?}");
}

#[tokio::test]
async fn a_database_error_is_propagated() {
    let world = world_with_turn(turn(TurnState::Running));
    world
        .turns
        .with(|t| t.read_error = Some("db down".to_owned()));
    let error = world
        .service
        .turn_status(&ctx(), CHAT, REQUEST)
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::Database(_)));
}

#[allow(dead_code)]
fn unused(_: &FakeTurns) {}
