//! Deterministic tests of the insert-race / parallel-guard classification
//! (DESIGN 3.3 "Idempotency" table and "Parallel turn guard"): the lookups
//! run against an in-memory `chat_turns`, so every interleaving of the two
//! racing requests is reproduced exactly.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Mutex;

use async_trait::async_trait;
use time::OffsetDateTime;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::*;
use crate::domain::turns::{ToolKind, TurnFinalization};

/// In-memory turns of one chat; only the lookups the classification uses.
#[derive(Default)]
struct FakeTurns {
    rows: Mutex<Vec<TurnRow>>,
    running_reads: Mutex<usize>,
}

impl FakeTurns {
    fn commit(&self, request_id: Uuid, state: &str) {
        self.rows.lock().unwrap().push(TurnRow {
            id: Uuid::new_v4(),
            request_id,
            state: state.to_owned(),
            assistant_message_id: None,
            error_code: None,
            effective_model: None,
            requester_user_id: None,
            web_search_enabled: false,
            deleted_at: None,
            started_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: None,
        });
    }

    /// The winner leaves `running` (finalization committed).
    fn finalize(&self, request_id: Uuid, state: &str) {
        for row in self.rows.lock().unwrap().iter_mut() {
            if row.request_id == request_id {
                state.clone_into(&mut row.state);
            }
        }
    }

    fn running_reads(&self) -> usize {
        *self.running_reads.lock().unwrap()
    }
}

#[async_trait]
impl TurnRepository for FakeTurns {
    async fn find_turn(
        &self,
        _scope: &AccessScope,
        _chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|row| row.request_id == request_id)
            .cloned())
    }

    async fn find_live_turn(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError> {
        self.find_turn(scope, chat_id, request_id).await
    }

    async fn find_running_turn(
        &self,
        _scope: &AccessScope,
        _chat_id: Uuid,
    ) -> Result<Option<TurnRow>, DomainError> {
        *self.running_reads.lock().unwrap() += 1;
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|row| row.state == "running")
            .cloned())
    }

    async fn insert_turn(&self, _scope: &AccessScope, _turn: NewTurn) -> Result<(), DomainError> {
        unreachable!("the classification never writes")
    }

    async fn touch_turn_progress(
        &self,
        _scope: &AccessScope,
        _turn_id: Uuid,
        _now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        unreachable!("the classification never writes")
    }

    async fn count_tool_done(
        &self,
        _scope: &AccessScope,
        _turn_id: Uuid,
        _tool: ToolKind,
        _now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        unreachable!("the classification never writes")
    }

    async fn finalize_turn(
        &self,
        _scope: &AccessScope,
        _finalization: &TurnFinalization,
    ) -> Result<bool, DomainError> {
        unreachable!("the classification never writes")
    }
}

fn scope() -> AccessScope {
    AccessScope::allow_all()
}

async fn lost(turns: &FakeTurns, request_id: Uuid) -> Contention {
    resolve_lost_insert(turns, &scope(), Uuid::nil(), request_id)
        .await
        .unwrap()
}

async fn guard(turns: &FakeTurns, request_id: Uuid) -> Option<Contention> {
    running_guard(turns, &scope(), Uuid::nil(), request_id)
        .await
        .unwrap()
}

fn same_key_state(contention: &Contention) -> &str {
    match contention {
        Contention::SameKey(turn) => &turn.state,
        Contention::Parallel => panic!("expected a same-key contention"),
    }
}

#[tokio::test]
async fn lost_insert_with_the_same_key_while_the_winner_runs_is_same_key() {
    let turns = FakeTurns::default();
    let rid = Uuid::new_v4();
    turns.commit(rid, "running");
    assert_eq!(same_key_state(&lost(&turns, rid).await), "running");
}

#[tokio::test]
async fn lost_insert_with_the_same_key_after_the_winner_finalized_is_same_key() {
    for state in ["completed", "failed", "cancelled"] {
        let turns = FakeTurns::default();
        let rid = Uuid::new_v4();
        turns.commit(rid, "running");
        turns.finalize(rid, state);
        assert_eq!(same_key_state(&lost(&turns, rid).await), state);
    }
}

#[tokio::test]
async fn lost_insert_with_a_different_key_while_the_winner_runs_is_parallel() {
    let turns = FakeTurns::default();
    turns.commit(Uuid::new_v4(), "running");
    assert!(matches!(
        lost(&turns, Uuid::new_v4()).await,
        Contention::Parallel
    ));
}

#[tokio::test]
async fn lost_insert_with_a_different_key_after_the_winner_finalized_is_still_parallel() {
    // The winner left `running` before the re-read: neither lookup finds a
    // row that explains the loss, which must never become a unique violation.
    for state in ["completed", "failed", "cancelled"] {
        let turns = FakeTurns::default();
        let winner = Uuid::new_v4();
        turns.commit(winner, "running");
        turns.finalize(winner, state);
        assert!(matches!(
            lost(&turns, Uuid::new_v4()).await,
            Contention::Parallel
        ));
        assert_eq!(
            turns.running_reads(),
            0,
            "no dependence on the running read"
        );
    }
}

#[tokio::test]
async fn guard_without_a_running_turn_lets_the_send_proceed() {
    let turns = FakeTurns::default();
    assert!(guard(&turns, Uuid::new_v4()).await.is_none());
    turns.commit(Uuid::new_v4(), "completed");
    assert!(guard(&turns, Uuid::new_v4()).await.is_none());
}

#[tokio::test]
async fn guard_with_another_running_turn_is_parallel() {
    let turns = FakeTurns::default();
    turns.commit(Uuid::new_v4(), "running");
    assert!(matches!(
        guard(&turns, Uuid::new_v4()).await,
        Some(Contention::Parallel)
    ));
}

#[tokio::test]
async fn guard_sees_the_running_turn_of_the_same_request_id_as_same_key() {
    // The key lookup ran before the winner committed, the guard after it.
    let turns = FakeTurns::default();
    let rid = Uuid::new_v4();
    turns.commit(rid, "running");
    let contention = guard(&turns, rid).await.unwrap();
    assert_eq!(same_key_state(&contention), "running");
}

#[tokio::test]
async fn guard_rereads_the_key_when_the_same_key_winner_already_finalized() {
    // A running turn of another request plus a completed turn of this key:
    // the key wins (replay), not the parallel guard.
    let turns = FakeTurns::default();
    let rid = Uuid::new_v4();
    turns.commit(rid, "completed");
    turns.commit(Uuid::new_v4(), "running");
    let contention = guard(&turns, rid).await.unwrap();
    assert_eq!(same_key_state(&contention), "completed");
}
