//! Concurrent mutations of one chat over in-memory `SQLite` with a controlled interleaving of the
//! mutation steps (preview, preflight, commit): delete then retry, retry then edit, retry then
//! delete, delete then delete, a send that inserts a `running` turn between the preview and the
//! commit, the running-turn index, and truly parallel mutations. Each case yields exactly one
//! committed mutation and one 409 with the expected reason (`NOT_LATEST_TURN` after a committed
//! soft-delete or a newer turn, `GENERATION_IN_PROGRESS` on the running-turn index), never a 500,
//! and no residue (row, reserve or audit event) of the loser.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: algorithms
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-concurrency-resolution`
//! (`inst-cr-no-early-lock`, `inst-cr-first-commit-wins`, `inst-cr-match`, `inst-cr-case-deleted`,
//! `inst-cr-case-newer`, `inst-cr-case-running-index`, `inst-cr-loser-clean`, `inst-cr-send-race`,
//! `inst-cr-no-server-retry`, `inst-cr-return`) and
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-lock-recheck` (`inst-lr-lock`,
//! `inst-lr-reread`, `inst-lr-deleted`, `inst-lr-latest`, `inst-lr-not-latest`);
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit` (`inst-mc-recheck`,
//! `inst-mc-soft-delete-lost`, `inst-mc-running-race`, `inst-mc-atomic`); definitions of done
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-concurrency` and
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-mutation-transaction`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::AuditEvent;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::domain::chats::ChatDetail;
use crate::domain::error::{ConflictReason, DomainError};
use crate::domain::mutations::{
    CommittedMutation, MutationOp, MutationPreview, PreparedMutation, ValidatedEdit,
};
use crate::domain::streaming::SendOutcome;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::a1;
use crate::test_support::mutations::{
    SeededTurn, mutation_events, seed, snapshot, turn, turn_by_request,
};
use crate::test_support::quota::all_quota_rows;
use crate::test_support::send::{SendRig, send_rig, turn_rows};

/// A chat with two completed turns; the second is the latest.
async fn two_turns(rig: &SendRig) -> (ChatRecord, SeededTurn, SeededTurn) {
    let chat = rig.std_chat().await;
    let first = seed(&rig.quota.db, &chat, turn("first").at(10)).await;
    let second = seed(&rig.quota.db, &chat, turn("second").at(100)).await;
    (chat, first, second)
}

/// One request that passed admission, the preview and (for retry and edit) the preflight, and
/// has not committed yet.
#[allow(
    clippy::large_enum_variant,
    reason = "a test value, never stored in bulk"
)]
enum Pending {
    Replace(PreparedMutation),
    Delete(MutationPreview),
}

async fn pending(
    rig: &SendRig,
    chat: &ChatRecord,
    op: MutationOp,
    target: &SeededTurn,
    content: &str,
) -> (ChatDetail, Pending) {
    let detail = rig.mutations.admit(&a1(), op, chat.id).await.unwrap();
    let preview = rig
        .mutations
        .preview(&a1(), &detail, op, target.request_id)
        .await
        .unwrap();
    let pending = if op.replaces() {
        Pending::Replace(
            rig.mutations
                .preflight(&a1(), &detail, preview, content.to_owned())
                .await
                .unwrap(),
        )
    } else {
        Pending::Delete(preview)
    };
    (detail, pending)
}

async fn commit(
    rig: &SendRig,
    detail: &ChatDetail,
    pending: &Pending,
) -> Result<CommittedMutation, DomainError> {
    match pending {
        Pending::Replace(prepared) => rig.mutations.commit(&a1(), detail, prepared).await,
        Pending::Delete(preview) => rig.mutations.commit_delete(&a1(), detail, preview).await,
    }
}

fn reason_of(result: Result<CommittedMutation, DomainError>) -> ConflictReason {
    match result {
        Err(DomainError::Conflict { reason }) => reason,
        other => panic!("expected a 409 conflict, got {other:?}"),
    }
}

/// The loser left nothing: every row, the chat, the summary, the outbox and the quota are as the
/// winner's commit left them, and the loser took no reserve.
async fn assert_no_residue_of_the_loser(
    rig: &SendRig,
    chat: &ChatRecord,
    after_winner: &crate::test_support::mutations::Snapshot,
) {
    assert_eq!(&snapshot(&rig.quota.db, chat).await, after_winner);
    assert!(
        all_quota_rows(&rig.quota.db).await.is_empty(),
        "no reserve of the loser"
    );
    assert_eq!(rig.provider.call_count(), 0);
}

/// Winner `first_op` commits, then the loser `second_op` (previewed and prepared before the
/// winner committed) is rejected with `NOT_LATEST_TURN`.
async fn winner_then_loser(first_op: MutationOp, second_op: MutationOp) {
    let rig = send_rig().await;
    let (chat, _, second) = two_turns(&rig).await;
    // both requests pass the preview and the preflight concurrently: neither holds a lock or
    // writes a row
    let before_any = snapshot(&rig.quota.db, &chat).await;
    let (detail_a, a) = pending(&rig, &chat, first_op, &second, "second").await;
    let (detail_b, b) = pending(&rig, &chat, second_op, &second, "second, edited").await;
    assert_eq!(
        snapshot(&rig.quota.db, &chat).await,
        before_any,
        "no early write"
    );

    commit(&rig, &detail_a, &a)
        .await
        .expect("the first commit wins");
    let after_winner = snapshot(&rig.quota.db, &chat).await;
    let result = commit(&rig, &detail_b, &b).await;
    assert_eq!(
        reason_of(result),
        ConflictReason::NotLatestTurn,
        "{first_op:?} then {second_op:?}"
    );
    assert_no_residue_of_the_loser(&rig, &chat, &after_winner).await;

    // exactly one audit event: the winner's
    let events = mutation_events(&rig).await;
    assert_eq!(
        events.len(),
        1,
        "{first_op:?} then {second_op:?}: {events:?}"
    );
    match (first_op, &events[0]) {
        (MutationOp::Retry, AuditEvent::TurnRetry(_))
        | (MutationOp::Edit, AuditEvent::TurnEdit(_))
        | (MutationOp::Delete, AuditEvent::TurnDelete(_)) => {}
        (op, event) => panic!("{op:?} left {event:?}"),
    }
    // the target is soft-deleted exactly once and only the winner's replacement exists
    let old = turn_by_request(&rig.quota.db, chat.id, second.request_id).await;
    assert!(old.deleted_at.is_some());
    let replacements = turn_rows(&rig.quota.db, chat.id)
        .await
        .into_iter()
        .filter(|row| Some(row.request_id) == old.replaced_by_request_id)
        .count();
    assert_eq!(replacements, usize::from(first_op.replaces()));
}

#[tokio::test]
async fn delete_then_retry_yields_one_delete_and_a_not_latest_turn() {
    // inst-cr-case-deleted
    winner_then_loser(MutationOp::Delete, MutationOp::Retry).await;
}

#[tokio::test]
async fn retry_then_edit_yields_one_retry_and_a_not_latest_turn() {
    winner_then_loser(MutationOp::Retry, MutationOp::Edit).await;
}

#[tokio::test]
async fn retry_then_delete_yields_one_retry_and_a_not_latest_turn() {
    winner_then_loser(MutationOp::Retry, MutationOp::Delete).await;
}

#[tokio::test]
async fn delete_then_delete_yields_one_delete_and_a_not_latest_turn() {
    winner_then_loser(MutationOp::Delete, MutationOp::Delete).await;
}

#[tokio::test]
async fn edit_then_retry_and_retry_then_retry_yield_one_winner() {
    winner_then_loser(MutationOp::Edit, MutationOp::Retry).await;
    winner_then_loser(MutationOp::Retry, MutationOp::Retry).await;
    winner_then_loser(MutationOp::Edit, MutationOp::Edit).await;
    winner_then_loser(MutationOp::Edit, MutationOp::Delete).await;
}

#[tokio::test]
async fn a_send_that_commits_a_running_turn_between_the_preview_and_the_commit_wins() {
    // inst-cr-case-newer: the newer `running` turn of the send makes the target non-latest
    for op in [MutationOp::Retry, MutationOp::Edit, MutationOp::Delete] {
        let rig = send_rig().await;
        let (chat, _, second) = two_turns(&rig).await;
        let (detail, pending) = pending(&rig, &chat, op, &second, "second").await;

        // the send commits its reserve transaction (a running turn) before the mutation commits
        let plan = rig.plan_for(&chat, "a new message").await;
        let after_send = snapshot(&rig.quota.db, &chat).await;
        let reserved_by_send = all_quota_rows(&rig.quota.db).await;

        let result = commit(&rig, &detail, &pending).await;
        let reason = reason_of(result);
        assert!(
            matches!(
                reason,
                ConflictReason::NotLatestTurn | ConflictReason::GenerationInProgress
            ),
            "{op:?}: {reason:?}"
        );
        assert_eq!(
            reason,
            ConflictReason::NotLatestTurn,
            "{op:?}: a newer running turn"
        );
        assert_eq!(
            snapshot(&rig.quota.db, &chat).await,
            after_send,
            "{op:?}: no residue"
        );
        assert_eq!(all_quota_rows(&rig.quota.db).await, reserved_by_send);
        // the send's turn is untouched and running
        let sent = turn_by_request(&rig.quota.db, chat.id, plan.request_id).await;
        assert_eq!(sent.state, "running");
        assert!(sent.deleted_at.is_none());
        assert!(
            mutation_events(&rig).await.is_empty(),
            "{op:?}: no mutation event"
        );
    }
}

#[tokio::test]
async fn a_send_that_races_with_a_committed_mutation_is_rejected_by_its_own_guards() {
    // inst-cr-send-race: 409 `turn_already_running` through the parallel-turn guard, 409
    // `request_id_conflict` when the replaced turn's request id is reused
    use crate::domain::streaming::request::ValidatedSend;
    let rig = send_rig().await;
    let (chat, _, second) = two_turns(&rig).await;
    let (detail, pending) = pending(&rig, &chat, MutationOp::Retry, &second, "second").await;
    commit(&rig, &detail, &pending)
        .await
        .expect("the retry commits");

    let send = |request_id: Option<Uuid>| ValidatedSend {
        content: "late send".to_owned(),
        request_id,
        attachment_ids: Vec::new(),
        web_search_enabled: false,
    };
    let running = rig
        .service
        .send(&a1(), chat.id, send(None), &CancellationToken::new())
        .await;
    assert!(
        matches!(
            running,
            Err(DomainError::Conflict {
                reason: ConflictReason::TurnAlreadyRunning
            })
        ),
        "{running:?}"
    );
    let reused = rig
        .service
        .send(
            &a1(),
            chat.id,
            send(Some(second.request_id)),
            &CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(
            reused,
            Err(DomainError::Conflict {
                reason: ConflictReason::RequestIdConflict
            })
        ),
        "{reused:?}"
    );
}

#[tokio::test]
async fn losing_the_running_turn_index_is_generation_in_progress_and_leaves_no_residue() {
    // inst-cr-case-running-index, inst-mc-running-race: an older `running` turn holds the index
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    seed(&rig.quota.db, &chat, turn("stuck").at(10).running()).await;
    let target = seed(&rig.quota.db, &chat, turn("target").at(100)).await;
    let (detail, pending) = pending(&rig, &chat, MutationOp::Retry, &target, "target").await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let result = commit(&rig, &detail, &pending).await;
    assert_eq!(reason_of(result), ConflictReason::GenerationInProgress);
    assert_no_residue_of_the_loser(&rig, &chat, &before).await;
    assert!(mutation_events(&rig).await.is_empty());
}

#[tokio::test]
async fn a_lost_race_is_never_retried_by_the_server() {
    // inst-cr-no-server-retry: the loser's second attempt hits the same conflict
    let rig = send_rig().await;
    let (chat, _, second) = two_turns(&rig).await;
    let (detail_a, a) = pending(&rig, &chat, MutationOp::Delete, &second, "").await;
    let (detail_b, b) = pending(&rig, &chat, MutationOp::Retry, &second, "second").await;
    commit(&rig, &detail_a, &a).await.unwrap();
    for _ in 0..3 {
        assert_eq!(
            reason_of(commit(&rig, &detail_b, &b).await),
            ConflictReason::NotLatestTurn
        );
    }
    assert_eq!(mutation_events(&rig).await.len(), 1);
}

#[tokio::test]
async fn two_parallel_retries_yield_exactly_one_stream_and_one_conflict() {
    // inst-cr-first-commit-wins, inst-cr-return: `NOT_LATEST_TURN` or `GENERATION_IN_PROGRESS`
    // by the interleaving, never an internal error
    for round in 0..8 {
        let rig = send_rig().await;
        let (chat, _, second) = two_turns(&rig).await;
        let who = a1();
        let token_a = CancellationToken::new();
        let token_b = CancellationToken::new();
        let (a, b) = tokio::join!(
            rig.mutations
                .retry(&who, chat.id, second.request_id, &token_a),
            rig.mutations
                .retry(&who, chat.id, second.request_id, &token_b),
        );
        let outcomes = [a, b];
        let winners = outcomes
            .iter()
            .filter(|o| matches!(o, Ok(SendOutcome::Streaming(_))))
            .count();
        assert_eq!(winners, 1, "round {round}: {outcomes:?}");
        for outcome in outcomes.iter().filter(|o| o.is_err()) {
            assert!(
                matches!(
                    outcome,
                    Err(DomainError::Conflict {
                        reason: ConflictReason::NotLatestTurn
                            | ConflictReason::GenerationInProgress
                    })
                ),
                "round {round}: {outcome:?}"
            );
        }
        assert_eq!(mutation_events(&rig).await.len(), 1, "round {round}");
        let replacements = turn_rows(&rig.quota.db, chat.id)
            .await
            .into_iter()
            .filter(|row| row.deleted_at.is_none() && row.state == "running")
            .count();
        assert_eq!(replacements, 1, "round {round}: one running replacement");
        assert_eq!(
            rig.provider.call_count(),
            1,
            "round {round}: one provider task"
        );
    }
}

#[tokio::test]
async fn a_parallel_retry_edit_and_delete_leave_exactly_one_committed_mutation() {
    for round in 0..8 {
        let rig = send_rig().await;
        let (chat, _, second) = two_turns(&rig).await;
        let who = a1();
        let (token_a, token_b) = (CancellationToken::new(), CancellationToken::new());
        let (retry, edit, delete) = tokio::join!(
            rig.mutations
                .retry(&who, chat.id, second.request_id, &token_a),
            rig.mutations.edit(
                &who,
                chat.id,
                second.request_id,
                ValidatedEdit {
                    content: "edited".to_owned()
                },
                &token_b
            ),
            rig.mutations.delete(&who, chat.id, second.request_id),
        );
        let committed = [retry.is_ok(), edit.is_ok(), delete.is_ok()]
            .iter()
            .filter(|ok| **ok)
            .count();
        assert_eq!(committed, 1, "round {round}: {retry:?} {edit:?} {delete:?}");
        for error in [retry.err(), edit.err(), delete.err()]
            .into_iter()
            .flatten()
        {
            assert!(
                matches!(
                    error,
                    DomainError::Conflict {
                        reason: ConflictReason::NotLatestTurn
                            | ConflictReason::GenerationInProgress
                    }
                ),
                "round {round}: {error:?}"
            );
        }
        assert_eq!(mutation_events(&rig).await.len(), 1, "round {round}");
        assert!(
            all_quota_rows(&rig.quota.db).await.len() <= 2,
            "at most the winner's reserve"
        );
    }
}

#[tokio::test]
async fn a_parallel_delete_and_retry_never_leave_two_replacement_turns() {
    for round in 0..8 {
        let rig = send_rig().await;
        let (chat, _, second) = two_turns(&rig).await;
        let who = a1();
        let token = CancellationToken::new();
        let (delete, retry) = tokio::join!(
            rig.mutations.delete(&who, chat.id, second.request_id),
            rig.mutations
                .retry(&who, chat.id, second.request_id, &token),
        );
        assert_ne!(
            delete.is_ok(),
            retry.is_ok(),
            "round {round}: exactly one wins"
        );
        let rows = turn_rows(&rig.quota.db, chat.id).await;
        let live = rows.iter().filter(|row| row.deleted_at.is_none()).count();
        let expected = if retry.is_ok() { 2 } else { 1 };
        assert_eq!(live, expected, "round {round}: {rows:?}");
    }
}
