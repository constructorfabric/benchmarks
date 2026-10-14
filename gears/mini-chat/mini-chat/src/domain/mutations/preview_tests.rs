//! The pure rules of the mutation preview and of the lock re-check: the check order on the target
//! turn (unknown, foreign requester, soft-deleted, running, non-latest), the latest-turn
//! selection with equal `started_at` and the `id` tie-break, every state counted, soft-deleted
//! turns never counted, and the turn visibility transitions.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: algorithms
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview` (`inst-mp-missing`,
//! `inst-mp-return-404`, `inst-mp-not-owner`, `inst-mp-return-403`, `inst-mp-deleted`,
//! `inst-mp-return-deleted`, `inst-mp-running`, `inst-mp-return-running`, `inst-mp-not-latest`,
//! `inst-mp-return-not-latest`, `inst-mp-generic-detail`, `inst-mp-return`),
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-latest-turn` (`inst-lt-select`,
//! `inst-lt-sort-key`, `inst-lt-every-state`, `inst-lt-skip-deleted`, `inst-lt-return`) and
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck` (`inst-lr-deleted`,
//! `inst-lr-not-terminal`, `inst-lr-not-latest`, `inst-lr-return`); state
//! `cpt-cf-mini-chat-state-turn-mutations-and-reactions-turn-visibility` (`inst-tv-final`);
//! definition of done `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-latest-terminal-turn`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::{check_latest, check_target, select_latest};
use crate::domain::error::{ConflictReason, DomainError, ResourceKind};
use crate::domain::turns::{TurnRow, TurnState};

const CALLER: Uuid = Uuid::from_u128(0x1111);
const OTHER: Uuid = Uuid::from_u128(0x2222);

fn instant(micros: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap() + Duration::microseconds(micros)
}

fn turn(state: TurnState, started: i64) -> TurnRow {
    TurnRow {
        id: Uuid::new_v4(),
        chat_id: Uuid::from_u128(0xC4A7),
        request_id: Uuid::new_v4(),
        requester_user_id: Some(CALLER),
        state,
        assistant_message_id: None,
        error_code: None,
        effective_model: None,
        deleted_at: None,
        started_at: instant(started),
        updated_at: instant(started),
    }
}

fn deleted(mut row: TurnRow) -> TurnRow {
    row.deleted_at = Some(instant(1_000_000));
    row
}

fn reason_of(result: Result<(), DomainError>) -> ConflictReason {
    match result {
        Err(DomainError::Conflict { reason }) => reason,
        other => panic!("expected a conflict, got {other:?}"),
    }
}

// ── the check order on the target ────────────────────────────────────────────

#[test]
fn an_unknown_turn_is_a_not_found_of_the_turn_type() {
    // inst-mp-missing, inst-mp-return-404
    let error = check_target(CALLER, None).unwrap_err();
    assert!(
        matches!(
            error,
            DomainError::NotFound {
                resource: ResourceKind::Turn,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_terminal_turn_of_the_caller_passes_in_every_terminal_state() {
    // inst-mp-return: completed, failed and cancelled are mutable
    for state in [
        TurnState::Completed,
        TurnState::Failed,
        TurnState::Cancelled,
    ] {
        check_target(CALLER, Some(&turn(state, 10))).unwrap_or_else(|e| panic!("{state:?}: {e:?}"));
    }
}

#[test]
fn a_turn_of_another_requester_is_an_authz_denied() {
    // inst-mp-not-owner, inst-mp-return-403
    let mut row = turn(TurnState::Completed, 10);
    row.requester_user_id = Some(OTHER);
    assert!(matches!(
        check_target(CALLER, Some(&row)),
        Err(DomainError::AuthzDenied)
    ));
}

#[test]
fn a_turn_without_a_requester_is_an_authz_denied() {
    // inst-mp-not-owner: a NULL requester included
    let mut row = turn(TurnState::Completed, 10);
    row.requester_user_id = None;
    assert!(matches!(
        check_target(CALLER, Some(&row)),
        Err(DomainError::AuthzDenied)
    ));
}

#[test]
fn a_soft_deleted_turn_is_not_latest_whatever_its_state() {
    // inst-mp-deleted, inst-mp-return-deleted, inst-tv-final: replaced or deleted turns
    for state in [
        TurnState::Completed,
        TurnState::Failed,
        TurnState::Cancelled,
    ] {
        let row = deleted(turn(state, 10));
        assert_eq!(
            reason_of(check_target(CALLER, Some(&row))),
            ConflictReason::NotLatestTurn,
            "{state:?}"
        );
    }
}

#[test]
fn a_replaced_turn_is_not_latest() {
    // inst-tv-final: Replaced stays Replaced
    let mut row = deleted(turn(TurnState::Completed, 10));
    row.updated_at = instant(2_000_000);
    assert_eq!(
        reason_of(check_target(CALLER, Some(&row))),
        ConflictReason::NotLatestTurn
    );
}

#[test]
fn a_running_turn_is_a_turn_state_precondition() {
    // inst-mp-running, inst-mp-return-running, inst-lr-not-terminal
    assert!(matches!(
        check_target(CALLER, Some(&turn(TurnState::Running, 10))),
        Err(DomainError::TurnNotTerminal)
    ));
}

#[test]
fn the_check_order_is_missing_requester_deleted_running() {
    // the feature-local order of the preview: 404, 403, soft-deleted 409, running 400
    let mut foreign_deleted = deleted(turn(TurnState::Completed, 10));
    foreign_deleted.requester_user_id = Some(OTHER);
    assert!(
        matches!(
            check_target(CALLER, Some(&foreign_deleted)),
            Err(DomainError::AuthzDenied)
        ),
        "the requester check precedes the soft-delete check"
    );

    let mut foreign_running = turn(TurnState::Running, 10);
    foreign_running.requester_user_id = Some(OTHER);
    assert!(
        matches!(
            check_target(CALLER, Some(&foreign_running)),
            Err(DomainError::AuthzDenied)
        ),
        "the requester check precedes the running check"
    );

    let running_deleted = deleted(turn(TurnState::Running, 10));
    assert_eq!(
        reason_of(check_target(CALLER, Some(&running_deleted))),
        ConflictReason::NotLatestTurn,
        "the soft-delete check precedes the running check"
    );
}

#[test]
fn the_rejections_carry_no_turn_identifier_in_their_text() {
    // inst-mp-generic-detail: the display text of the errors is generic
    check_target(CALLER, Some(&turn(TurnState::Completed, 10))).unwrap();
    let row = deleted(turn(TurnState::Completed, 10));
    let error = check_target(CALLER, Some(&row)).unwrap_err();
    let text = error.to_string();
    assert!(!text.contains(&row.request_id.to_string()), "{text}");
    assert!(!text.contains(&row.id.to_string()), "{text}");
}

// ── the latest-turn selection ────────────────────────────────────────────────

#[test]
fn a_chat_without_a_non_deleted_turn_has_no_latest_turn() {
    // inst-lt-return
    assert!(select_latest(&[]).is_none());
    let all_deleted = [
        deleted(turn(TurnState::Completed, 1)),
        deleted(turn(TurnState::Failed, 2)),
    ];
    assert!(select_latest(&all_deleted).is_none());
}

#[test]
fn the_latest_turn_is_the_one_with_the_greatest_started_at() {
    // inst-lt-select
    let turns = [
        turn(TurnState::Completed, 10),
        turn(TurnState::Completed, 30),
        turn(TurnState::Completed, 20),
    ];
    assert_eq!(select_latest(&turns).unwrap().id, turns[1].id);
}

#[test]
fn equal_started_at_values_are_broken_by_the_greatest_id() {
    // inst-lt-sort-key: `id` is the deterministic tie-break
    let mut low = turn(TurnState::Completed, 10);
    let mut high = turn(TurnState::Completed, 10);
    low.id = Uuid::from_u128(0x01);
    high.id = Uuid::from_u128(0xFF);
    let forward = [low.clone(), high.clone()];
    let backward = [high.clone(), low];
    assert_eq!(select_latest(&forward).unwrap().id, high.id);
    assert_eq!(select_latest(&backward).unwrap().id, high.id);
}

#[test]
fn started_at_values_that_differ_by_a_microsecond_are_ordered_chronologically() {
    // `.123000` against `.123001`: the later instant wins whatever the textual form
    let earlier = turn(TurnState::Completed, 123_000);
    let later = turn(TurnState::Completed, 123_001);
    let turns = [later.clone(), earlier];
    assert_eq!(select_latest(&turns).unwrap().id, later.id);
}

#[test]
fn every_state_counts_a_newer_running_turn_is_the_latest_turn() {
    // inst-lt-every-state
    for newer in [
        TurnState::Running,
        TurnState::Completed,
        TurnState::Failed,
        TurnState::Cancelled,
    ] {
        let old = turn(TurnState::Completed, 10);
        let new = turn(newer, 20);
        let turns = [old, new.clone()];
        assert_eq!(select_latest(&turns).unwrap().id, new.id, "{newer:?}");
    }
}

#[test]
fn a_soft_deleted_turn_is_never_the_latest_turn() {
    // inst-lt-skip-deleted: a replaced or deleted newer turn is skipped
    let old = turn(TurnState::Completed, 10);
    let newer_deleted = deleted(turn(TurnState::Completed, 20));
    let turns = [old.clone(), newer_deleted];
    assert_eq!(select_latest(&turns).unwrap().id, old.id);
}

#[test]
fn after_a_delete_the_previous_turn_becomes_the_latest_turn() {
    // acceptance: after deleting T2, T1 is the latest turn and can be retried
    let t1 = turn(TurnState::Completed, 10);
    let t2 = turn(TurnState::Completed, 20);
    assert_eq!(select_latest(&[t1.clone(), t2.clone()]).unwrap().id, t2.id);
    let after_delete = [t1.clone(), deleted(t2)];
    let latest = select_latest(&after_delete).unwrap();
    assert_eq!(latest.id, t1.id);
    check_latest(&t1, Some(latest)).unwrap();
}

// ── the latest-turn check ────────────────────────────────────────────────────

#[test]
fn the_target_that_is_the_latest_turn_passes_the_latest_check() {
    // inst-lr-return
    let target = turn(TurnState::Completed, 20);
    check_latest(&target, Some(&target.clone())).unwrap();
}

#[test]
fn a_newer_turn_makes_the_target_not_latest_even_when_it_is_running() {
    // inst-mp-not-latest, inst-mp-return-not-latest, inst-lr-not-latest
    let target = turn(TurnState::Completed, 10);
    for state in [TurnState::Completed, TurnState::Running, TurnState::Failed] {
        let newer = turn(state, 20);
        assert_eq!(
            reason_of(check_latest(&target, Some(&newer))),
            ConflictReason::NotLatestTurn,
            "{state:?}"
        );
    }
}

#[test]
fn a_chat_without_a_latest_turn_makes_the_target_not_latest() {
    // the target is not in the non-deleted set (it was soft-deleted by a concurrent mutation)
    let target = turn(TurnState::Completed, 10);
    assert_eq!(
        reason_of(check_latest(&target, None)),
        ConflictReason::NotLatestTurn
    );
}

#[test]
fn the_latest_check_compares_the_turn_identity_not_the_timestamps() {
    // a different turn with the very same `started_at` is not the target
    let target = turn(TurnState::Completed, 10);
    let mut sibling = turn(TurnState::Completed, 10);
    sibling.id = Uuid::from_u128(u128::MAX);
    assert_eq!(
        reason_of(check_latest(&target, Some(&sibling))),
        ConflictReason::NotLatestTurn
    );
}
