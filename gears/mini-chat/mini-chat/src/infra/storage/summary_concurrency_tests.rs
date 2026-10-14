//! The summary commit against concurrent work over in-memory `SQLite`: the mutation transaction of
//! `cpt-cf-mini-chat-feature-turn-mutations-and-reactions` that soft-deletes the target frontier
//! message (both orders: the worker skips with `frontier_deleted`, or the mutation invalidates the
//! committed summary, and a truly parallel run), two deliveries of work items with the same base
//! (one commit, one CAS conflict, one usage event) and two concurrent deliveries through the worker
//! with the mock provider.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-summary-commit` (`inst-cm-begin`,
//! `inst-cm-lock-target`, `inst-cm-target-deleted`, `inst-cm-insert-conflict`,
//! `inst-cm-update-conflict`, `inst-cm-single-commit`); algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-summary-handler`
//! (`inst-sh-case-commit-target-deleted`, `inst-sh-case-commit-conflict`,
//! `inst-sh-duplicates-accepted`); state
//! `cpt-cf-mini-chat-state-background-maintenance-summary-frontier` (`inst-fr-invalidated`,
//! `inst-fr-unchanged`); flow `cpt-cf-mini-chat-flow-background-maintenance-long-conversation`
//! (`inst-lc-handle`); definitions of done
//! `cpt-cf-mini-chat-dod-background-maintenance-summary-commit` and
//! `cpt-cf-mini-chat-dod-background-maintenance-summary-failure-retry`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::maintenance::HandlerOutcome;
use crate::domain::maintenance::summary_store::{CommitOutcome, SummaryStore};
use crate::domain::quota::UsageQueue;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::outbox_rows;
use crate::test_support::maintenance::{
    SUMMARY_MODEL, commit_of, compressed_ids, maint_rig, responses_answer, seed_range,
    settings_for, summary_payload, summary_text, work,
};
use crate::test_support::mutations::{SeededTurn, seed, summary_row, turn};
use crate::test_support::provider::Reply;
use crate::test_support::quota::default_now;
use crate::test_support::send::{SendRig, send_rig, t};

async fn usage_count(rig: &SendRig) -> usize {
    let queue = rig.quota.outbox.queue_name.clone();
    outbox_rows(&rig.quota.db)
        .await
        .into_iter()
        .filter(|row| row.queue == queue)
        .count()
}

/// A chat with two completed turns; the first turn's assistant message is the frozen target (the
/// second turn is the one that caused the trigger).
async fn two_turns(rig: &SendRig) -> (ChatRecord, SeededTurn, SeededTurn) {
    let chat = rig.std_chat().await;
    let first = seed(&rig.quota.db, &chat, turn("first").at(10)).await;
    let second = seed(&rig.quota.db, &chat, turn("second").at(100)).await;
    (chat, first, second)
}

fn work_of(
    chat: &ChatRecord,
    first: &SeededTurn,
) -> crate::domain::streaming::summary::ThreadSummaryWork {
    work(
        chat.tenant_id,
        chat.id,
        None,
        (t(11), first.assistant_message_id.unwrap()),
    )
}

fn queue(rig: &SendRig) -> Arc<dyn UsageQueue> {
    rig.quota.enqueuer.clone()
}

async fn delete(rig: &SendRig, chat: &ChatRecord, turn: &SeededTurn) {
    let reply = rig.delete_turn(chat.id, turn.request_id).await;
    assert_eq!(reply.status.as_u16(), 204, "{}", reply.text);
}

// ── a mutation that soft-deletes the target ──────────────────────────────────

#[tokio::test]
async fn a_mutation_that_commits_first_makes_the_worker_skip_with_a_deleted_target() {
    // inst-cm-lock-target, inst-cm-target-deleted, inst-sh-case-commit-target-deleted: whichever
    // commits second sees the other's result
    let rig = send_rig().await;
    let (chat, first, second) = two_turns(&rig).await;
    let w = work_of(&chat, &first);
    delete(&rig, &chat, &second).await;
    delete(&rig, &chat, &first).await;
    let outcome = rig
        .quota
        .db
        .provider
        .commit(&commit_of(&w, "S", 5, default_now()), queue(&rig))
        .await
        .unwrap();
    assert!(
        matches!(outcome, CommitOutcome::TargetDeleted),
        "{outcome:?}"
    );
    assert!(summary_row(&rig.quota.db, chat.id).await.is_none());
    assert!(compressed_ids(&rig.quota.db, chat.id).await.is_empty());
    assert_eq!(usage_count(&rig).await, 0, "no usage event was written");
}

#[tokio::test]
async fn a_summary_that_commits_first_is_invalidated_by_the_mutation_of_a_covered_turn() {
    // inst-fr-invalidated: the mutation deletes the row and clears `is_compressed`
    let rig = send_rig().await;
    let (chat, first, second) = two_turns(&rig).await;
    let w = work_of(&chat, &first);
    let outcome = rig
        .quota
        .db
        .provider
        .commit(&commit_of(&w, "S", 5, default_now()), queue(&rig))
        .await
        .unwrap();
    assert!(
        matches!(outcome, CommitOutcome::Committed(_)),
        "{outcome:?}"
    );
    assert_eq!(compressed_ids(&rig.quota.db, chat.id).await.len(), 2);
    // the second turn is not covered by the frontier: the summary stays
    delete(&rig, &chat, &second).await;
    assert!(summary_row(&rig.quota.db, chat.id).await.is_some());
    // the first turn is covered: the summary is deleted and nothing stays compressed
    delete(&rig, &chat, &first).await;
    assert!(summary_row(&rig.quota.db, chat.id).await.is_none());
    assert!(compressed_ids(&rig.quota.db, chat.id).await.is_empty());
    assert_eq!(
        usage_count(&rig).await,
        1,
        "the usage event of the committed call stays"
    );
}

#[tokio::test]
async fn a_parallel_commit_and_mutation_leave_no_summary_in_either_order() {
    // inst-cm-begin, inst-cm-lock-target: the commit and the soft-delete of its target never leave
    // a summary over a deleted message
    for round in 0..8 {
        let rig = send_rig().await;
        let (chat, first, second) = two_turns(&rig).await;
        let w = work_of(&chat, &first);
        delete(&rig, &chat, &second).await;
        let store = rig.quota.db.provider.clone();
        let usage = queue(&rig);
        let commit = async {
            store
                .commit(&commit_of(&w, "S", 5, default_now()), usage)
                .await
                .unwrap()
        };
        let mutation = async {
            let reply = rig.delete_turn(chat.id, first.request_id).await;
            assert_eq!(reply.status.as_u16(), 204, "round {round}: {}", reply.text);
        };
        let (outcome, ()) = tokio::join!(commit, mutation);
        assert!(
            matches!(
                outcome,
                CommitOutcome::Committed(_) | CommitOutcome::TargetDeleted
            ),
            "round {round}: {outcome:?}"
        );
        assert!(
            summary_row(&rig.quota.db, chat.id).await.is_none(),
            "round {round}: a summary over a deleted frontier message remains"
        );
        assert!(
            compressed_ids(&rig.quota.db, chat.id).await.is_empty(),
            "round {round}: a compressed message remains"
        );
        let usage = usage_count(&rig).await;
        match outcome {
            CommitOutcome::Committed(_) => assert_eq!(usage, 1, "round {round}"),
            _ => assert_eq!(usage, 0, "round {round}"),
        }
    }
}

// ── two deliveries of the same range ─────────────────────────────────────────

#[tokio::test]
async fn two_concurrent_commits_with_the_same_base_yield_one_commit_and_one_conflict() {
    // inst-cm-insert-conflict, inst-cm-update-conflict, inst-cm-single-commit: for an empty base
    // and for a stored base
    for with_base in [false, true] {
        for round in 0..4 {
            let rig = maint_rig().await;
            let chat = rig.chat(crate::test_support::quota::STANDARD).await;
            seed_range(rig.db(), &chat, 6).await;
            let base = with_base.then_some((t(20), mid_of(&rig, &chat, 2).await));
            if with_base {
                crate::test_support::send::seed_summary(rig.db(), &chat, "BASE", 9, base.unwrap())
                    .await;
            }
            let ids = crate::test_support::chats::message_rows(rig.db(), chat.id).await;
            let target = ids.iter().find(|m| m.created_at == t(50)).unwrap().id;
            let w = work(chat.tenant_id, chat.id, base, (t(50), target));
            let usage: Arc<dyn UsageQueue> = rig.quota.enqueuer.clone();
            let store = rig.db().provider.clone();
            let (commit_a, commit_b) = (
                commit_of(&w, "A", 5, default_now()),
                commit_of(&w, "B", 6, default_now()),
            );
            let (a, b) = tokio::join!(
                store.commit(&commit_a, usage.clone()),
                store.commit(&commit_b, usage.clone()),
            );
            let (a, b) = (a.unwrap(), b.unwrap());
            let committed = [&a, &b]
                .iter()
                .filter(|o| matches!(o, CommitOutcome::Committed(_)))
                .count();
            let moved = [&a, &b]
                .iter()
                .filter(|o| matches!(o, CommitOutcome::FrontierMoved))
                .count();
            assert_eq!(
                (committed, moved),
                (1, 1),
                "base {with_base} round {round}: {a:?} {b:?}"
            );
            assert_eq!(rig.usage_rows().await.len(), 1);
            let row = summary_row(rig.db(), chat.id).await.unwrap();
            assert_eq!(row.summarized_up_to_message_id, target);
            assert!(["A", "B"].contains(&row.summary_text.as_str()));
        }
    }
}

async fn mid_of(
    rig: &crate::test_support::maintenance::MaintRig,
    chat: &ChatRecord,
    n: i64,
) -> Uuid {
    crate::test_support::chats::message_rows(rig.db(), chat.id)
        .await
        .into_iter()
        .find(|m| m.created_at == t(n * 10))
        .unwrap()
        .id
}

#[tokio::test]
async fn work_items_with_the_same_base_and_different_targets_commit_once() {
    // inst-cm-single-commit: a wider target for the same base loses the CAS once the first
    // committed; the compressed range is the winner's
    let rig = maint_rig().await;
    let chat = rig.chat(crate::test_support::quota::STANDARD).await;
    seed_range(rig.db(), &chat, 6).await;
    let narrow = work(
        chat.tenant_id,
        chat.id,
        None,
        (t(30), mid_of(&rig, &chat, 3).await),
    );
    let wide = work(
        chat.tenant_id,
        chat.id,
        None,
        (t(50), mid_of(&rig, &chat, 5).await),
    );
    let usage: Arc<dyn UsageQueue> = rig.quota.enqueuer.clone();
    let first = rig
        .db()
        .provider
        .commit(
            &commit_of(&narrow, "NARROW", 5, default_now()),
            usage.clone(),
        )
        .await
        .unwrap();
    assert!(matches!(first, CommitOutcome::Committed(_)), "{first:?}");
    let second = rig
        .db()
        .provider
        .commit(&commit_of(&wide, "WIDE", 6, default_now()), usage)
        .await
        .unwrap();
    assert!(matches!(second, CommitOutcome::FrontierMoved), "{second:?}");
    assert_eq!(
        summary_row(rig.db(), chat.id).await.unwrap().summary_text,
        "NARROW"
    );
    assert_eq!(compressed_ids(rig.db(), chat.id).await.len(), 3);
}

#[tokio::test]
async fn two_concurrent_deliveries_of_one_work_item_commit_one_summary_and_one_usage_event() {
    // inst-sh-duplicates-accepted, inst-sh-case-commit-conflict, inst-cm-single-commit: duplicate
    // provider calls are accepted, the CAS decides; the loser records a CAS conflict and no
    // execution result
    for round in 0..4 {
        let rig = maint_rig().await;
        let chat = rig.chat(crate::test_support::quota::STANDARD).await;
        seed_range(rig.db(), &chat, 6).await;
        rig.upstream.set_default(Reply::http(
            200,
            &responses_answer(&summary_text("ONE SUMMARY"), (10, 5)),
        ));
        let target = mid_of(&rig, &chat, 5).await;
        let payload = summary_payload(
            chat.tenant_id,
            chat.id,
            None,
            (t(50), target),
            Uuid::new_v4(),
        );
        let bytes = serde_json::to_vec(&payload).unwrap();
        let worker = rig.summary_worker(settings_for(SUMMARY_MODEL));
        let (a, b) = tokio::join!(worker.handle(&bytes, 0), worker.handle(&bytes, 0));
        assert_eq!(
            (a, b),
            (HandlerOutcome::Ok, HandlerOutcome::Ok),
            "round {round}"
        );
        assert_eq!(rig.recorder.executions(), vec!["success"], "round {round}");
        assert_eq!(rig.recorder.cas_conflicts(), 1, "round {round}");
        assert_eq!(
            rig.usage_rows().await.len(),
            1,
            "round {round}: one usage event"
        );
        assert_eq!(compressed_ids(rig.db(), chat.id).await.len(), 5);
        assert_eq!(
            summary_row(rig.db(), chat.id).await.unwrap().summary_text,
            "ONE SUMMARY"
        );
        assert!(
            (1..=2).contains(&rig.upstream.call_count()),
            "round {round}"
        );
    }
}

#[tokio::test]
async fn a_replay_after_a_commit_is_a_cas_conflict_without_a_provider_call() {
    // inst-sh-case-conflict through the real store: the replay of a committed work item
    let rig = maint_rig().await;
    let chat = rig.chat(crate::test_support::quota::STANDARD).await;
    seed_range(rig.db(), &chat, 6).await;
    rig.upstream.set_default(Reply::http(
        200,
        &responses_answer(&summary_text("S"), (10, 5)),
    ));
    let target = mid_of(&rig, &chat, 4).await;
    let payload = summary_payload(
        chat.tenant_id,
        chat.id,
        None,
        (t(40), target),
        Uuid::new_v4(),
    );
    let bytes = serde_json::to_vec(&payload).unwrap();
    let worker = rig.summary_worker(settings_for(SUMMARY_MODEL));
    assert_eq!(worker.handle(&bytes, 0).await, HandlerOutcome::Ok);
    assert_eq!(rig.upstream.call_count(), 1);
    assert_eq!(worker.handle(&bytes, 1).await, HandlerOutcome::Ok);
    assert_eq!(
        rig.upstream.call_count(),
        1,
        "the pre-check saves the provider call"
    );
    assert_eq!(rig.recorder.cas_conflicts(), 1);
    assert_eq!(rig.usage_rows().await.len(), 1);
}
