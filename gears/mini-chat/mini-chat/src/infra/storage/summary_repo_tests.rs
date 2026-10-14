//! The thread-summary repository over in-memory `SQLite`: the frontier pre-check (chat gone, base
//! missing, moved frontier, deleted target, ready, database error), the range selection (exact
//! `(created_at, id)` order with equal timestamps and fractional seconds `.12`, `.123` and `.1215`,
//! soft-deleted and compressed messages, messages after the target, base exclusive and target
//! inclusive, tenant and chat isolation), and the compare-and-set commit (first insert, update with
//! a matching base, insert conflict, moved frontier, missing base, deleted or unknown target, the
//! exact `is_compressed` range, exactly one system-task usage row, rollback on a forced enqueue
//! failure, one commit per frozen range, no `chat_turns` or `quota_usage` change).
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithms
//! `cpt-cf-mini-chat-algo-background-maintenance-summary-frontier-precheck` (`inst-pc-load-chat`,
//! `inst-pc-chat-gone`, `inst-pc-return-chat-gone`, `inst-pc-load-summary`, `inst-pc-base-missing`,
//! `inst-pc-return-base-missing`, `inst-pc-moved`, `inst-pc-return-moved`, `inst-pc-load-target`,
//! `inst-pc-target-deleted`, `inst-pc-return-target-deleted`, `inst-pc-db-error`,
//! `inst-pc-return-db-error`, `inst-pc-return-ready`), `cpt-cf-mini-chat-algo-background-maintenance-summary-range-selection` (`inst-rs-select`,
//! `inst-rs-order`, `inst-rs-exclude-later`, `inst-rs-return`), `cpt-cf-mini-chat-algo-background-maintenance-summary-commit` (`inst-cm-begin`,
//! `inst-cm-lock-target`, `inst-cm-target-deleted`, `inst-cm-return-target-deleted`,
//! `inst-cm-first`, `inst-cm-insert`, `inst-cm-insert-conflict`, `inst-cm-advance`,
//! `inst-cm-update`, `inst-cm-update-conflict`, `inst-cm-compress`, `inst-cm-usage`,
//! `inst-cm-try`, `inst-cm-commit`, `inst-cm-catch`, `inst-cm-return-error`,
//! `inst-cm-single-commit`, `inst-cm-return`) and `cpt-cf-mini-chat-algo-background-maintenance-system-task-usage` (`inst-su-enqueue`,
//! `inst-su-no-turn`, `inst-su-return`); state
//! `cpt-cf-mini-chat-state-background-maintenance-summary-frontier` (`inst-fr-first-commit`,
//! `inst-fr-advance`, `inst-fr-unchanged`); definitions of done
//! `cpt-cf-mini-chat-dod-background-maintenance-summary-commit`,
//! `cpt-cf-mini-chat-dod-background-maintenance-summary-execution` and
//! `cpt-cf-mini-chat-dod-background-maintenance-system-task-isolation`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::maintenance::summary_store::{CommitOutcome, PreCheck, SummaryStore};
use crate::domain::quota::UsageQueue;
use crate::infra::outbox::partition_for;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::message_rows;
use crate::test_support::maintenance::{
    MaintRig, RecordingUsageQueue, commit_of, compressed_ids, ids_of, maint_rig, mid, table_count,
    work,
};
use crate::test_support::mutations::{soft_delete_chat, summary_row};
use crate::test_support::quota::default_now;
use crate::test_support::send::{MessageSeed, seed_msg, seed_summary, t};
use crate::test_support::{TENANT_A, TENANT_B};

async fn seeded() -> (MaintRig, ChatRecord) {
    let rig = maint_rig().await;
    let chat = rig.chat(crate::test_support::quota::STANDARD).await;
    (rig, chat)
}

/// Seed message `n` (id `mid(n)`) of `role` at `micros`.
async fn msg(rig: &MaintRig, chat: &ChatRecord, n: u128, role: &'static str, micros: i64) {
    seed_msg(
        rig.db(),
        chat,
        MessageSeed::new(role, &format!("c{n}"), t(micros)).id(mid(n)),
    )
    .await;
}

/// Messages 1 to `count` at 10 µs steps, alternating `user` and `assistant`.
async fn conversation(rig: &MaintRig, chat: &ChatRecord, count: u128) {
    for n in 1..=count {
        let role = if n % 2 == 1 { "user" } else { "assistant" };
        msg(rig, chat, n, role, i64::try_from(n).unwrap() * 10).await;
    }
}

fn the_work(
    chat: &ChatRecord,
    base: Option<(i64, u128)>,
    target: (i64, u128),
) -> crate::domain::streaming::summary::ThreadSummaryWork {
    work(
        chat.tenant_id,
        chat.id,
        base.map(|(micros, n)| (t(micros), mid(n))),
        (t(target.0), mid(target.1)),
    )
}

fn queue(rig: &MaintRig) -> Arc<dyn UsageQueue> {
    rig.quota.enqueuer.clone()
}

async fn precheck(
    rig: &MaintRig,
    work: &crate::domain::streaming::summary::ThreadSummaryWork,
) -> PreCheck {
    rig.db().provider.precheck(work).await.expect("pre-check")
}

async fn range(
    rig: &MaintRig,
    work: &crate::domain::streaming::summary::ThreadSummaryWork,
) -> Vec<Uuid> {
    ids_of(&rig.db().provider.load_range(work).await.expect("range"))
}

// ── the pre-check ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_live_chat_with_no_summary_and_an_empty_base_is_ready_with_no_base_text() {
    // inst-pc-load-chat, inst-pc-load-summary, inst-pc-load-target, inst-pc-return-ready
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 4).await;
    assert_eq!(
        precheck(&rig, &the_work(&chat, None, (30, 3))).await,
        PreCheck::Ready { base_summary: None }
    );
}

#[tokio::test]
async fn a_matching_base_is_ready_with_the_stored_summary_text() {
    // inst-pc-return-ready: the text of the base for the prompt
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 4).await;
    seed_summary(rig.db(), &chat, "the stored summary", 12, (t(10), mid(1))).await;
    assert_eq!(
        precheck(&rig, &the_work(&chat, Some((10, 1)), (30, 3))).await,
        PreCheck::Ready {
            base_summary: Some("the stored summary".to_owned())
        }
    );
}

#[tokio::test]
async fn a_missing_chat_is_gone() {
    // inst-pc-load-chat, inst-pc-chat-gone, inst-pc-return-chat-gone
    let rig = maint_rig().await;
    let unknown = ChatRecord {
        id: Uuid::new_v4(),
        tenant_id: TENANT_A,
        user_id: Uuid::new_v4(),
        model: "m".to_owned(),
        title: None,
        created_at: t(0),
        updated_at: t(0),
    };
    assert_eq!(
        precheck(&rig, &the_work(&unknown, None, (30, 3))).await,
        PreCheck::ChatGone
    );
}

#[tokio::test]
async fn a_soft_deleted_chat_is_gone() {
    // inst-pc-chat-gone: the select does not apply the deleted filter; the content of a deleted
    // chat is not sent to the provider again
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 4).await;
    soft_delete_chat(rig.db(), chat.id).await;
    assert_eq!(
        precheck(&rig, &the_work(&chat, None, (30, 3))).await,
        PreCheck::ChatGone
    );
}

#[tokio::test]
async fn another_tenant_never_sees_the_chat() {
    // inst-pc-load-chat: every query filters on the payload's `tenant_id`
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 4).await;
    let foreign = work(TENANT_B, chat.id, None, (t(30), mid(3)));
    assert_eq!(precheck(&rig, &foreign).await, PreCheck::ChatGone);
}

#[tokio::test]
async fn a_base_without_a_summary_row_is_missing() {
    // inst-pc-load-summary, inst-pc-base-missing, inst-pc-return-base-missing
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 4).await;
    assert_eq!(
        precheck(&rig, &the_work(&chat, Some((10, 1)), (30, 3))).await,
        PreCheck::BaseMissing
    );
}

#[tokio::test]
async fn an_empty_base_with_a_row_is_a_moved_frontier() {
    // inst-pc-moved, inst-pc-return-moved: another first summary already committed
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 4).await;
    seed_summary(rig.db(), &chat, "other", 5, (t(20), mid(2))).await;
    assert_eq!(
        precheck(&rig, &the_work(&chat, None, (30, 3))).await,
        PreCheck::FrontierMoved
    );
}

#[tokio::test]
async fn a_stored_frontier_that_differs_in_time_or_id_is_a_moved_frontier() {
    // inst-pc-moved: compared on `created_at` and on `id`
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 4).await;
    seed_summary(rig.db(), &chat, "s", 5, (t(20), mid(2))).await;
    assert_eq!(
        precheck(&rig, &the_work(&chat, Some((10, 1)), (40, 4))).await,
        PreCheck::FrontierMoved,
        "different time and id"
    );
    assert_eq!(
        precheck(&rig, &the_work(&chat, Some((20, 1)), (40, 4))).await,
        PreCheck::FrontierMoved,
        "same time, different id"
    );
    assert_eq!(
        precheck(&rig, &the_work(&chat, Some((10, 2)), (40, 4))).await,
        PreCheck::FrontierMoved,
        "same id, different time"
    );
    assert_eq!(
        precheck(&rig, &the_work(&chat, Some((20, 2)), (40, 4))).await,
        PreCheck::Ready {
            base_summary: Some("s".to_owned())
        }
    );
}

#[tokio::test]
async fn frontiers_with_fractional_seconds_of_different_lengths_compare_by_instant() {
    // inst-pc-moved: `.12` and `.123` are different instants, `.12` equals `.120`
    let (rig, chat) = seeded().await;
    msg(&rig, &chat, 1, "user", 120_000).await;
    msg(&rig, &chat, 2, "assistant", 123_000).await;
    msg(&rig, &chat, 3, "user", 130_000).await;
    seed_summary(rig.db(), &chat, "s", 5, (t(120_000), mid(1))).await;
    assert_eq!(
        precheck(&rig, &the_work(&chat, Some((120_000, 1)), (130_000, 3))).await,
        PreCheck::Ready {
            base_summary: Some("s".to_owned())
        }
    );
    assert_eq!(
        precheck(&rig, &the_work(&chat, Some((123_000, 1)), (130_000, 3))).await,
        PreCheck::FrontierMoved
    );
}

#[tokio::test]
async fn a_missing_target_is_deleted() {
    // inst-pc-load-target, inst-pc-target-deleted, inst-pc-return-target-deleted
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 2).await;
    assert_eq!(
        precheck(&rig, &the_work(&chat, None, (99, 77))).await,
        PreCheck::TargetDeleted
    );
}

#[tokio::test]
async fn a_soft_deleted_target_is_deleted() {
    // inst-pc-target-deleted: a retry, edit or delete removed the target frontier message
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 3).await;
    seed_msg(
        rig.db(),
        &chat,
        MessageSeed::new("assistant", "gone", t(35))
            .id(mid(9))
            .deleted(t(36)),
    )
    .await;
    assert_eq!(
        precheck(&rig, &the_work(&chat, None, (35, 9))).await,
        PreCheck::TargetDeleted
    );
}

#[tokio::test]
async fn a_database_error_is_reported_as_an_error() {
    // inst-pc-db-error, inst-pc-return-db-error
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 3).await;
    crate::test_support::raw_exec(
        &rig.db().raw,
        "ALTER TABLE thread_summaries RENAME TO thread_summaries_gone",
    )
    .await
    .unwrap();
    let result = rig
        .db()
        .provider
        .precheck(&the_work(&chat, None, (30, 3)))
        .await;
    assert!(result.is_err(), "{result:?}");
}

// ── the range selection ──────────────────────────────────────────────────────

#[tokio::test]
async fn the_range_is_every_live_uncompressed_message_up_to_the_target_in_order() {
    // inst-rs-select, inst-rs-return: an empty base starts at the beginning, the target is
    // inclusive
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 6).await;
    let messages = rig
        .db()
        .provider
        .load_range(&the_work(&chat, None, (40, 4)))
        .await
        .unwrap();
    assert_eq!(ids_of(&messages), vec![mid(1), mid(2), mid(3), mid(4)]);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[0].content, "c1");
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(messages[0].created_at, t(10));
}

#[tokio::test]
async fn the_base_frontier_is_exclusive() {
    // inst-rs-select: strictly after the base
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 6).await;
    assert_eq!(
        range(&rig, &the_work(&chat, Some((20, 2)), (50, 5))).await,
        vec![mid(3), mid(4), mid(5)]
    );
}

#[tokio::test]
async fn messages_after_the_frozen_target_are_excluded() {
    // inst-rs-exclude-later: they neither cancel, widen nor invalidate the run
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 3).await;
    let target = the_work(&chat, None, (30, 3));
    let before = range(&rig, &target).await;
    msg(&rig, &chat, 4, "user", 40).await;
    msg(&rig, &chat, 5, "assistant", 50).await;
    assert_eq!(range(&rig, &target).await, before);
    assert_eq!(before, vec![mid(1), mid(2), mid(3)]);
}

#[tokio::test]
async fn soft_deleted_and_compressed_messages_are_excluded() {
    // inst-rs-select: `deleted_at IS NULL` and `is_compressed` false
    let (rig, chat) = seeded().await;
    msg(&rig, &chat, 1, "user", 10).await;
    seed_msg(
        rig.db(),
        &chat,
        MessageSeed::new("assistant", "x", t(20))
            .id(mid(2))
            .deleted(t(21)),
    )
    .await;
    seed_msg(
        rig.db(),
        &chat,
        MessageSeed::new("user", "x", t(30)).id(mid(3)).compressed(),
    )
    .await;
    msg(&rig, &chat, 4, "assistant", 40).await;
    assert_eq!(
        range(&rig, &the_work(&chat, None, (40, 4))).await,
        vec![mid(1), mid(4)]
    );
}

#[tokio::test]
async fn equal_timestamps_are_ordered_by_id() {
    // inst-rs-order: `id` is the deterministic tie-break
    let (rig, chat) = seeded().await;
    // inserted out of id order, all at the same instant
    for n in [5, 2, 9, 1, 7] {
        msg(&rig, &chat, n, "user", 100).await;
    }
    assert_eq!(
        range(&rig, &the_work(&chat, None, (100, 9))).await,
        vec![mid(1), mid(2), mid(5), mid(7), mid(9)]
    );
}

#[tokio::test]
async fn the_frontier_comparison_uses_the_id_for_equal_timestamps() {
    // inst-rs-select: `(created_at, id) > base` and `<= target` with the same `created_at`
    let (rig, chat) = seeded().await;
    for n in 1..=6 {
        msg(&rig, &chat, n, "user", 100).await;
    }
    assert_eq!(
        range(&rig, &the_work(&chat, Some((100, 2)), (100, 5))).await,
        vec![mid(3), mid(4), mid(5)]
    );
}

#[tokio::test]
async fn fractional_seconds_of_different_lengths_keep_their_chronological_order() {
    // inst-rs-order: `.12`, `.1215` and `.123` sort chronologically, not as raw text (raw text
    // puts `.12Z` after `.1215Z` and `.123Z`)
    let (rig, chat) = seeded().await;
    msg(&rig, &chat, 1, "user", 120_000).await;
    msg(&rig, &chat, 2, "assistant", 121_500).await;
    msg(&rig, &chat, 3, "user", 123_000).await;
    msg(&rig, &chat, 4, "assistant", 1_000_000).await;
    msg(&rig, &chat, 5, "user", 0).await;
    assert_eq!(
        range(&rig, &the_work(&chat, None, (1_000_000, 4))).await,
        vec![mid(5), mid(1), mid(2), mid(3), mid(4)]
    );
}

#[tokio::test]
async fn a_base_and_a_target_with_different_fraction_lengths_bound_the_range_chronologically() {
    // inst-rs-order: after `.12`, up to and including `.123`
    let (rig, chat) = seeded().await;
    msg(&rig, &chat, 1, "user", 120_000).await;
    msg(&rig, &chat, 2, "assistant", 121_500).await;
    msg(&rig, &chat, 3, "user", 123_000).await;
    msg(&rig, &chat, 4, "assistant", 123_500).await;
    assert_eq!(
        range(&rig, &the_work(&chat, Some((120_000, 1)), (123_000, 3))).await,
        vec![mid(2), mid(3)]
    );
}

#[tokio::test]
async fn only_the_messages_of_the_chat_of_the_tenant_are_loaded() {
    // inst-rs-select: filter `tenant_id` and `chat_id`
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 3).await;
    let other = rig.chat(crate::test_support::quota::STANDARD).await;
    seed_msg(
        rig.db(),
        &other,
        MessageSeed::new("user", "foreign", t(15)).id(mid(50)),
    )
    .await;
    assert_eq!(
        range(&rig, &the_work(&chat, None, (30, 3))).await,
        vec![mid(1), mid(2), mid(3)]
    );
    let foreign_tenant = work(TENANT_B, chat.id, None, (t(30), mid(3)));
    assert!(range(&rig, &foreign_tenant).await.is_empty());
}

#[tokio::test]
async fn a_target_at_or_before_the_base_yields_an_empty_range() {
    // inst-rs-select
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 4).await;
    assert!(
        range(&rig, &the_work(&chat, Some((30, 3)), (30, 3)))
            .await
            .is_empty()
    );
    assert!(
        range(&rig, &the_work(&chat, Some((30, 3)), (20, 2)))
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_range_database_error_is_reported() {
    // inst-sh-range-db at the repository: the query error surfaces
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 2).await;
    crate::test_support::raw_exec(
        &rig.db().raw,
        "ALTER TABLE messages RENAME TO messages_gone",
    )
    .await
    .unwrap();
    assert!(
        rig.db()
            .provider
            .load_range(&the_work(&chat, None, (20, 2)))
            .await
            .is_err()
    );
}

// ── the commit ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_first_commit_inserts_the_row_and_marks_exactly_the_frozen_range() {
    // inst-cm-begin, inst-cm-lock-target, inst-cm-first, inst-cm-insert, inst-cm-compress,
    // inst-cm-commit, inst-cm-return, inst-fr-first-commit
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 6).await;
    let w = the_work(&chat, None, (40, 4));
    let outcome = rig
        .db()
        .provider
        .commit(
            &commit_of(&w, "FIRST SUMMARY", 33, default_now()),
            queue(&rig),
        )
        .await
        .unwrap();
    let CommitOutcome::Committed(wake) = outcome else {
        panic!("{outcome:?}");
    };
    wake.fire();
    let row = summary_row(rig.db(), chat.id)
        .await
        .expect("the row exists");
    assert_eq!(row.summary_text, "FIRST SUMMARY");
    assert_eq!(row.token_estimate, 33);
    assert_eq!(row.summarized_up_to_message_id, mid(4));
    assert_eq!(row.summarized_up_to_created_at, t(40));
    assert_eq!(row.tenant_id, TENANT_A);
    assert_eq!(row.created_at, default_now());
    assert_eq!(row.updated_at, default_now());
    assert_eq!(
        compressed_ids(rig.db(), chat.id).await,
        vec![mid(1), mid(2), mid(3), mid(4)],
        "exactly the frozen range; messages 5 and 6 are untouched"
    );
}

#[tokio::test]
async fn a_commit_with_a_matching_base_advances_the_frontier_and_marks_the_new_range() {
    // inst-cm-advance, inst-cm-update, inst-cm-compress, inst-fr-advance
    let (rig, chat) = seeded().await;
    for n in 1..=2u128 {
        let role = if n % 2 == 1 { "user" } else { "assistant" };
        seed_msg(
            rig.db(),
            &chat,
            MessageSeed::new(role, "old", t(i64::try_from(n).unwrap() * 10))
                .id(mid(n))
                .compressed(),
        )
        .await;
    }
    for n in 3..=7u128 {
        let role = if n % 2 == 1 { "user" } else { "assistant" };
        msg(&rig, &chat, n, role, i64::try_from(n).unwrap() * 10).await;
    }
    seed_summary(rig.db(), &chat, "OLD SUMMARY", 10, (t(20), mid(2))).await;
    let before = summary_row(rig.db(), chat.id).await.unwrap();
    let w = the_work(&chat, Some((20, 2)), (50, 5));
    let outcome = rig
        .db()
        .provider
        .commit(
            &commit_of(&w, "NEW SUMMARY", 44, default_now()),
            queue(&rig),
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, CommitOutcome::Committed(_)),
        "{outcome:?}"
    );
    let after = summary_row(rig.db(), chat.id).await.unwrap();
    assert_eq!(after.id, before.id, "the same row is advanced");
    assert_eq!(after.summary_text, "NEW SUMMARY");
    assert_eq!(after.token_estimate, 44);
    assert_eq!(after.summarized_up_to_message_id, mid(5));
    assert_eq!(after.summarized_up_to_created_at, t(50));
    assert_eq!(
        after.created_at, before.created_at,
        "created_at never changes"
    );
    assert_eq!(after.updated_at, default_now());
    assert_eq!(
        compressed_ids(rig.db(), chat.id).await,
        vec![mid(1), mid(2), mid(3), mid(4), mid(5)],
        "the new range (3, 4, 5) joined the compressed 1 and 2; 6 and 7 are untouched"
    );
}

#[tokio::test]
async fn a_commit_enqueues_exactly_one_system_task_usage_event() {
    // inst-cm-usage, inst-su-enqueue, inst-su-return: in the commit transaction, partitioned by
    // the tenant
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 4).await;
    let w = the_work(&chat, None, (40, 4));
    rig.db()
        .provider
        .commit(&commit_of(&w, "S", 5, default_now()), queue(&rig))
        .await
        .unwrap();
    let rows = rig.usage_rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].payload_type, "mini_chat.usage_event");
    assert_eq!(rows[0].partition, i64::from(partition_for(TENANT_A, 4)));
    let payload = &rows[0].payload;
    assert_eq!(payload["billing_outcome"], "system_task");
    assert_eq!(payload["requester_type"], "system");
    assert_eq!(payload["system_task_type"], "thread_summary_update");
    assert_eq!(payload["request_id"], w.system_request_id.to_string());
    assert_eq!(
        payload["dedupe_key"],
        format!(
            "{}/thread_summary_update/{}",
            TENANT_A.simple(),
            w.system_request_id.simple()
        )
    );
    assert!(
        payload.get("user_id").is_none() && payload.get("turn_id").is_none(),
        "{payload}"
    );
}

#[tokio::test]
async fn an_insert_conflict_is_a_moved_frontier_and_writes_nothing() {
    // inst-cm-insert-conflict: another first summary committed
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 5).await;
    seed_summary(rig.db(), &chat, "THE WINNER", 7, (t(20), mid(2))).await;
    let w = the_work(&chat, None, (40, 4));
    let outcome = rig
        .db()
        .provider
        .commit(&commit_of(&w, "LOSER", 5, default_now()), queue(&rig))
        .await
        .unwrap();
    assert!(
        matches!(outcome, CommitOutcome::FrontierMoved),
        "{outcome:?}"
    );
    let row = summary_row(rig.db(), chat.id).await.unwrap();
    assert_eq!(row.summary_text, "THE WINNER");
    assert_eq!(row.summarized_up_to_message_id, mid(2));
    assert!(compressed_ids(rig.db(), chat.id).await.is_empty());
    assert!(rig.usage_rows().await.is_empty());
}

#[tokio::test]
async fn a_moved_frontier_is_not_overwritten() {
    // inst-cm-update-conflict, inst-fr-unchanged: the stored frontier no longer equals the base
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 6).await;
    seed_summary(rig.db(), &chat, "ALREADY ADVANCED", 7, (t(40), mid(4))).await;
    let w = the_work(&chat, Some((20, 2)), (50, 5));
    let outcome = rig
        .db()
        .provider
        .commit(&commit_of(&w, "STALE", 5, default_now()), queue(&rig))
        .await
        .unwrap();
    assert!(
        matches!(outcome, CommitOutcome::FrontierMoved),
        "{outcome:?}"
    );
    let row = summary_row(rig.db(), chat.id).await.unwrap();
    assert_eq!(row.summary_text, "ALREADY ADVANCED");
    assert_eq!(row.summarized_up_to_message_id, mid(4));
    assert!(compressed_ids(rig.db(), chat.id).await.is_empty());
    assert!(rig.usage_rows().await.is_empty());
}

#[tokio::test]
async fn a_base_without_a_summary_row_is_a_missing_base() {
    // inst-cm-update-conflict: a retry, edit or delete deleted the summary after the enqueue
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 6).await;
    let w = the_work(&chat, Some((20, 2)), (50, 5));
    let outcome = rig
        .db()
        .provider
        .commit(&commit_of(&w, "S", 5, default_now()), queue(&rig))
        .await
        .unwrap();
    assert!(matches!(outcome, CommitOutcome::BaseMissing), "{outcome:?}");
    assert!(summary_row(rig.db(), chat.id).await.is_none());
    assert!(compressed_ids(rig.db(), chat.id).await.is_empty());
    assert!(rig.usage_rows().await.is_empty());
}

#[tokio::test]
async fn a_soft_deleted_target_commits_nothing() {
    // inst-cm-target-deleted, inst-cm-return-target-deleted: no summary, no `is_compressed`
    // change, no usage event
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 3).await;
    seed_msg(
        rig.db(),
        &chat,
        MessageSeed::new("assistant", "gone", t(35))
            .id(mid(9))
            .deleted(t(36)),
    )
    .await;
    let w = the_work(&chat, None, (35, 9));
    let outcome = rig
        .db()
        .provider
        .commit(&commit_of(&w, "S", 5, default_now()), queue(&rig))
        .await
        .unwrap();
    assert!(
        matches!(outcome, CommitOutcome::TargetDeleted),
        "{outcome:?}"
    );
    assert!(summary_row(rig.db(), chat.id).await.is_none());
    assert!(compressed_ids(rig.db(), chat.id).await.is_empty());
    assert!(rig.usage_rows().await.is_empty());
}

#[tokio::test]
async fn an_unknown_target_commits_nothing() {
    // inst-cm-target-deleted: missing is deleted
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 3).await;
    let w = the_work(&chat, None, (35, 99));
    let outcome = rig
        .db()
        .provider
        .commit(&commit_of(&w, "S", 5, default_now()), queue(&rig))
        .await
        .unwrap();
    assert!(
        matches!(outcome, CommitOutcome::TargetDeleted),
        "{outcome:?}"
    );
    assert!(summary_row(rig.db(), chat.id).await.is_none());
}

#[tokio::test]
async fn a_foreign_tenant_cannot_commit_into_the_chat() {
    // every query filters on the payload's tenant: the target is not found
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 3).await;
    let foreign = work(TENANT_B, chat.id, None, (t(30), mid(3)));
    let outcome = rig
        .db()
        .provider
        .commit(&commit_of(&foreign, "S", 5, default_now()), queue(&rig))
        .await
        .unwrap();
    assert!(
        matches!(outcome, CommitOutcome::TargetDeleted),
        "{outcome:?}"
    );
    assert!(summary_row(rig.db(), chat.id).await.is_none());
    assert!(compressed_ids(rig.db(), chat.id).await.is_empty());
}

#[tokio::test]
async fn soft_deleted_messages_of_the_range_are_not_marked() {
    // inst-cm-compress: `deleted_at IS NULL` and `is_compressed` false, with the predicate of the
    // range selection
    let (rig, chat) = seeded().await;
    msg(&rig, &chat, 1, "user", 10).await;
    seed_msg(
        rig.db(),
        &chat,
        MessageSeed::new("assistant", "x", t(20))
            .id(mid(2))
            .deleted(t(21)),
    )
    .await;
    msg(&rig, &chat, 3, "user", 30).await;
    let w = the_work(&chat, None, (30, 3));
    rig.db()
        .provider
        .commit(&commit_of(&w, "S", 5, default_now()), queue(&rig))
        .await
        .unwrap();
    assert_eq!(
        compressed_ids(rig.db(), chat.id).await,
        vec![mid(1), mid(3)]
    );
}

#[tokio::test]
async fn the_marked_range_is_the_range_the_selection_loaded() {
    // inst-cm-compress, inst-rs-order: same predicate and sort key, fractional seconds included
    let (rig, chat) = seeded().await;
    msg(&rig, &chat, 1, "user", 120_000).await;
    msg(&rig, &chat, 2, "assistant", 121_500).await;
    msg(&rig, &chat, 3, "user", 123_000).await;
    msg(&rig, &chat, 4, "assistant", 123_000).await;
    msg(&rig, &chat, 5, "user", 500_000).await;
    seed_summary(rig.db(), &chat, "s", 3, (t(120_000), mid(1))).await;
    seed_msg(
        rig.db(),
        &chat,
        MessageSeed::new("user", "old", t(100_000))
            .id(mid(20))
            .compressed(),
    )
    .await;
    let w = the_work(&chat, Some((120_000, 1)), (123_000, 4));
    let loaded = range(&rig, &w).await;
    assert_eq!(loaded, vec![mid(2), mid(3), mid(4)]);
    rig.db()
        .provider
        .commit(&commit_of(&w, "S2", 5, default_now()), queue(&rig))
        .await
        .unwrap();
    let mut compressed = compressed_ids(rig.db(), chat.id).await;
    compressed.retain(|id| *id != mid(20));
    assert_eq!(compressed, loaded);
}

#[tokio::test]
async fn messages_after_the_target_are_neither_summarized_nor_marked() {
    // inst-cm-compress, inst-rs-exclude-later
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 3).await;
    let w = the_work(&chat, None, (30, 3));
    msg(&rig, &chat, 4, "user", 40).await;
    rig.db()
        .provider
        .commit(&commit_of(&w, "S", 5, default_now()), queue(&rig))
        .await
        .unwrap();
    assert_eq!(
        compressed_ids(rig.db(), chat.id).await,
        vec![mid(1), mid(2), mid(3)]
    );
    let rows = message_rows(rig.db(), chat.id).await;
    assert!(
        rows.iter()
            .find(|r| r.id == mid(4))
            .is_some_and(|r| !r.is_compressed)
    );
}

#[tokio::test]
async fn a_forced_enqueue_failure_rolls_the_whole_commit_back() {
    // inst-cm-try, inst-cm-catch, inst-cm-return-error, inst-cm-usage: the summary, the frontier,
    // the compressed range and the usage event commit together or not at all
    for with_base in [false, true] {
        let (rig, chat) = seeded().await;
        conversation(&rig, &chat, 6).await;
        let base = with_base.then_some((20, 2));
        if with_base {
            seed_summary(rig.db(), &chat, "KEPT", 9, (t(20), mid(2))).await;
        }
        let w = the_work(&chat, base, (50, 5));
        let failing: Arc<dyn UsageQueue> = RecordingUsageQueue::failing();
        let result = rig
            .db()
            .provider
            .commit(&commit_of(&w, "LOST", 5, default_now()), failing)
            .await;
        assert!(result.is_err(), "{with_base}: {result:?}");
        let row = summary_row(rig.db(), chat.id).await;
        if with_base {
            let row = row.expect("the previous summary stays");
            assert_eq!(row.summary_text, "KEPT");
            assert_eq!(row.summarized_up_to_message_id, mid(2));
        } else {
            assert!(row.is_none(), "no row was inserted");
        }
        assert!(
            compressed_ids(rig.db(), chat.id).await.is_empty(),
            "{with_base}"
        );
        assert!(rig.usage_rows().await.is_empty(), "{with_base}");
    }
}

#[tokio::test]
async fn at_most_one_commit_wins_for_a_frozen_range() {
    // inst-cm-single-commit: a second commit of the same range finds the frontier moved
    for with_base in [false, true] {
        let (rig, chat) = seeded().await;
        conversation(&rig, &chat, 6).await;
        let base = with_base.then_some((20, 2));
        if with_base {
            seed_summary(rig.db(), &chat, "BASE", 9, (t(20), mid(2))).await;
        }
        let w = the_work(&chat, base, (50, 5));
        let first = rig
            .db()
            .provider
            .commit(&commit_of(&w, "ONE", 5, default_now()), queue(&rig))
            .await
            .unwrap();
        assert!(matches!(first, CommitOutcome::Committed(_)), "{first:?}");
        let second = rig
            .db()
            .provider
            .commit(&commit_of(&w, "TWO", 6, default_now()), queue(&rig))
            .await
            .unwrap();
        assert!(
            matches!(second, CommitOutcome::FrontierMoved),
            "{with_base}: {second:?}"
        );
        assert_eq!(
            summary_row(rig.db(), chat.id).await.unwrap().summary_text,
            "ONE"
        );
        assert_eq!(rig.usage_rows().await.len(), 1, "exactly one usage event");
    }
}

#[tokio::test]
async fn a_summary_commit_writes_no_turn_and_no_quota_row() {
    // inst-su-no-turn: no `chat_turns` row, no `quota_usage` change
    let (rig, chat) = seeded().await;
    conversation(&rig, &chat, 4).await;
    let turns = table_count(rig.db(), "chat_turns").await;
    let quota = table_count(rig.db(), "quota_usage").await;
    let w = the_work(&chat, None, (40, 4));
    rig.db()
        .provider
        .commit(&commit_of(&w, "S", 5, default_now()), queue(&rig))
        .await
        .unwrap();
    assert_eq!(table_count(rig.db(), "chat_turns").await, turns);
    assert_eq!(table_count(rig.db(), "quota_usage").await, quota);
    assert_eq!(table_count(rig.db(), "chat_turns").await, 0);
}

#[tokio::test]
async fn the_frontier_equality_holds_for_fractional_seconds() {
    // inst-cm-update: the stored frontier equals the base through the sort key
    let (rig, chat) = seeded().await;
    msg(&rig, &chat, 1, "user", 120_000).await;
    msg(&rig, &chat, 2, "assistant", 121_500).await;
    msg(&rig, &chat, 3, "user", 123_000).await;
    seed_summary(rig.db(), &chat, "s", 3, (t(120_000), mid(1))).await;
    let ok = rig
        .db()
        .provider
        .commit(
            &commit_of(
                &the_work(&chat, Some((120_000, 1)), (123_000, 3)),
                "S",
                5,
                default_now(),
            ),
            queue(&rig),
        )
        .await
        .unwrap();
    assert!(matches!(ok, CommitOutcome::Committed(_)), "{ok:?}");
    let row = summary_row(rig.db(), chat.id).await.unwrap();
    assert_eq!(row.summarized_up_to_created_at, t(123_000));
    let stale = rig
        .db()
        .provider
        .commit(
            &commit_of(
                &the_work(&chat, Some((120_000, 1)), (123_000, 3)),
                "S",
                5,
                default_now(),
            ),
            queue(&rig),
        )
        .await
        .unwrap();
    assert!(matches!(stale, CommitOutcome::FrontierMoved), "{stale:?}");
}
