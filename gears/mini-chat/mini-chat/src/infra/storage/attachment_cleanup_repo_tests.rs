//! `SQLite` tests of the guarded updates of the attachment cleanup store: the parent chat read
//! without the deleted filter, the row read by tenant, chat and id, the failed attempt of a
//! `pending` row (attempts plus 1, the cause, the timestamp, and `failed` at the budget), the
//! guarded `pending` to `done` and `failed` to `done` writes, the replay failure refresh that
//! never grows the attempts, and the tenant scope of every statement.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`:
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler` (`inst-ch-load-chat`,
//! `inst-ch-load-row`, `inst-ch-record-attempt`, `inst-ch-exhausted`, `inst-ch-mark-done`,
//! `inst-ch-replay-done`, `inst-ch-replay-failed`); state
//! `cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status`; definition of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-cleanup-handler`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::attachments::cleanup::{AttemptRecord, CleanupFrom, CleanupRow, CleanupStore};
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::attachments::{RowSeed, seed_row, soft_delete_chat};
use crate::test_support::chats::{at, attachment_rows, seed_chat};
use crate::test_support::quota::{QuotaRig, STANDARD, quota_rig};
use crate::test_support::{TENANT_A, TENANT_B};

fn now() -> OffsetDateTime {
    at(3_000_000) + time::Duration::microseconds(789_000)
}

async fn rig() -> (QuotaRig, ChatRecord) {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    (rig, chat)
}

fn store(rig: &QuotaRig) -> &dyn CleanupStore {
    &*rig.db.provider
}

async fn row_of(
    rig: &QuotaRig,
    chat: &ChatRecord,
    id: Uuid,
) -> crate::infra::storage::entity::attachment::Model {
    attachment_rows(&rig.db, chat.id)
        .await
        .into_iter()
        .find(|row| row.id == id)
        .expect("attachment row")
}

// ── the chat read ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_live_chat_is_live() {
    // inst-ch-load-chat
    let (rig, chat) = rig().await;
    assert!(store(&rig).chat_is_live(TENANT_A, chat.id).await.unwrap());
}

#[tokio::test]
async fn a_soft_deleted_chat_is_read_without_the_deleted_filter_and_is_not_live() {
    // inst-ch-load-chat, inst-ch-chat-deleted
    let (rig, chat) = rig().await;
    soft_delete_chat(&rig.db, chat.id, at(50)).await;
    assert!(!store(&rig).chat_is_live(TENANT_A, chat.id).await.unwrap());
}

#[tokio::test]
async fn a_missing_chat_is_not_live() {
    let (rig, _chat) = rig().await;
    assert!(
        !store(&rig)
            .chat_is_live(TENANT_A, Uuid::new_v4())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_chat_of_another_tenant_is_not_live_for_the_tenant_asked() {
    // the handler filters every query on `tenant_id`
    let (rig, chat) = rig().await;
    assert!(!store(&rig).chat_is_live(TENANT_B, chat.id).await.unwrap());
}

// ── the row read ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_row_read_returns_the_cleanup_columns_of_a_soft_deleted_row() {
    // inst-ch-load-row: the primary case is a soft-deleted row
    let (rig, chat) = rig().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .deleted(at(10))
            .cleanup("pending")
            .attempts(2),
    )
    .await;
    assert_eq!(
        store(&rig).load_row(TENANT_A, chat.id, id).await.unwrap(),
        Some(CleanupRow {
            cleanup_status: Some("pending".to_owned()),
            cleanup_attempts: 2,
        })
    );
}

#[tokio::test]
async fn a_row_without_a_cleanup_status_reads_as_none() {
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    assert_eq!(
        store(&rig).load_row(TENANT_A, chat.id, id).await.unwrap(),
        Some(CleanupRow {
            cleanup_status: None,
            cleanup_attempts: 0,
        })
    );
}

#[tokio::test]
async fn the_row_read_is_scoped_by_tenant_and_chat() {
    let (rig, chat) = rig().await;
    let other = seed_chat(
        &rig.db,
        &crate::test_support::chats::a1(),
        Uuid::new_v4(),
        Some("other"),
        STANDARD,
        at(0),
    )
    .await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().cleanup("pending")).await;
    assert_eq!(
        store(&rig).load_row(TENANT_B, chat.id, id).await.unwrap(),
        None
    );
    assert_eq!(
        store(&rig).load_row(TENANT_A, other.id, id).await.unwrap(),
        None
    );
    assert_eq!(
        store(&rig)
            .load_row(TENANT_A, chat.id, Uuid::new_v4())
            .await
            .unwrap(),
        None
    );
}

// ── the failed attempt ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_failed_attempt_increments_records_the_cause_and_keeps_the_row_pending() {
    // inst-ch-record-attempt, inst-cs-retry
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().cleanup("pending")).await;
    let record = store(&rig)
        .record_failed_attempt(TENANT_A, id, "provider answered 500", 5, now())
        .await
        .unwrap();
    assert_eq!(
        record,
        Some(AttemptRecord {
            attempts: 1,
            exhausted: false
        })
    );
    let row = row_of(&rig, &chat, id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(row.cleanup_attempts, 1);
    assert_eq!(
        row.last_cleanup_error.as_deref(),
        Some("provider answered 500")
    );
    assert_eq!(row.cleanup_updated_at, Some(now()));
}

#[tokio::test]
async fn the_attempt_that_reaches_the_budget_sets_the_row_failed() {
    // inst-ch-exhausted, inst-cs-failed
    let (rig, chat) = rig().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document().cleanup("pending").attempts(4),
    )
    .await;
    let record = store(&rig)
        .record_failed_attempt(TENANT_A, id, "boom", 5, now())
        .await
        .unwrap();
    assert_eq!(
        record,
        Some(AttemptRecord {
            attempts: 5,
            exhausted: true
        })
    );
    let row = row_of(&rig, &chat, id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(row.cleanup_attempts, 5);
}

#[tokio::test]
async fn the_budget_is_the_value_passed_in() {
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().cleanup("pending")).await;
    let first = store(&rig)
        .record_failed_attempt(TENANT_A, id, "x", 2, now())
        .await
        .unwrap();
    assert_eq!(first.map(|r| r.exhausted), Some(false));
    let second = store(&rig)
        .record_failed_attempt(TENANT_A, id, "x", 2, now())
        .await
        .unwrap();
    assert_eq!(second.map(|r| r.exhausted), Some(true));
    assert_eq!(
        row_of(&rig, &chat, id).await.cleanup_status.as_deref(),
        Some("failed")
    );
}

#[tokio::test]
async fn a_failed_attempt_is_guarded_by_the_pending_status() {
    // inst-ch-record-attempt: guarded by `cleanup_status` `pending`
    for status in [None, Some("done"), Some("failed")] {
        let (rig, chat) = rig().await;
        let mut seed = RowSeed::document().attempts(1);
        if let Some(status) = status {
            seed = seed.cleanup(match status {
                "done" => "done",
                _ => "failed",
            });
        }
        let id = seed_row(&rig.db, &chat, seed).await;
        let record = store(&rig)
            .record_failed_attempt(TENANT_A, id, "boom", 5, now())
            .await
            .unwrap();
        assert_eq!(record, None, "{status:?}");
        let row = row_of(&rig, &chat, id).await;
        assert_eq!(row.cleanup_attempts, 1, "{status:?}");
        assert_eq!(row.last_cleanup_error, None, "{status:?}");
        assert_eq!(row.cleanup_status.as_deref(), status, "{status:?}");
    }
}

#[tokio::test]
async fn a_failed_attempt_never_touches_another_tenants_row() {
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().cleanup("pending")).await;
    let record = store(&rig)
        .record_failed_attempt(TENANT_B, id, "boom", 5, now())
        .await
        .unwrap();
    assert_eq!(record, None);
    assert_eq!(row_of(&rig, &chat, id).await.cleanup_attempts, 0);
}

// ── the done write ───────────────────────────────────────────────────────────

#[tokio::test]
async fn pending_to_done_is_guarded_and_stamps_the_time() {
    // inst-ch-mark-done, inst-cs-done
    let (rig, chat) = rig().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document().cleanup("pending").attempts(2),
    )
    .await;
    assert!(
        store(&rig)
            .mark_done(TENANT_A, id, CleanupFrom::Pending, now())
            .await
            .unwrap()
    );
    let row = row_of(&rig, &chat, id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("done"));
    assert_eq!(row.cleanup_updated_at, Some(now()));
    assert_eq!(row.cleanup_attempts, 2, "the attempts are not reset");
    // the second write finds no `pending` row
    assert!(
        !store(&rig)
            .mark_done(TENANT_A, id, CleanupFrom::Pending, now())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn failed_to_done_is_the_guarded_replay_transition() {
    // inst-ch-replay-done, inst-cs-replay-done: guarded by `cleanup_status` `failed`
    let (rig, chat) = rig().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document().cleanup("failed").attempts(5),
    )
    .await;
    assert!(
        !store(&rig)
            .mark_done(TENANT_A, id, CleanupFrom::Pending, now())
            .await
            .unwrap(),
        "the pending guard does not match a failed row"
    );
    assert_eq!(
        row_of(&rig, &chat, id).await.cleanup_status.as_deref(),
        Some("failed")
    );
    assert!(
        store(&rig)
            .mark_done(TENANT_A, id, CleanupFrom::Failed, now())
            .await
            .unwrap()
    );
    let row = row_of(&rig, &chat, id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("done"));
    assert_eq!(row.cleanup_attempts, 5);
}

#[tokio::test]
async fn the_done_writes_never_touch_a_row_in_another_state_or_tenant() {
    for (seed, from) in [
        (RowSeed::document().cleanup("done"), CleanupFrom::Pending),
        (RowSeed::document().cleanup("done"), CleanupFrom::Failed),
        (RowSeed::document(), CleanupFrom::Pending),
        (RowSeed::document(), CleanupFrom::Failed),
    ] {
        let (rig, chat) = rig().await;
        let status = seed.cleanup_status;
        let id = seed_row(&rig.db, &chat, seed).await;
        assert!(
            !store(&rig)
                .mark_done(TENANT_A, id, from, now())
                .await
                .unwrap()
        );
        assert_eq!(
            row_of(&rig, &chat, id).await.cleanup_status.as_deref(),
            status
        );
    }
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().cleanup("pending")).await;
    assert!(
        !store(&rig)
            .mark_done(TENANT_B, id, CleanupFrom::Pending, now())
            .await
            .unwrap()
    );
}

// ── the replay failure ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_replay_failure_refreshes_the_cause_and_the_time_but_never_the_attempts() {
    // inst-ch-replay-failed, inst-cs-replay-failed
    let (rig, chat) = rig().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .cleanup("failed")
            .attempts(5)
            .last_error("old cause"),
    )
    .await;
    assert!(
        store(&rig)
            .record_replay_failure(TENANT_A, id, "new cause", now())
            .await
            .unwrap()
    );
    let row = row_of(&rig, &chat, id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(row.cleanup_attempts, 5, "it never grows beyond the budget");
    assert_eq!(row.last_cleanup_error.as_deref(), Some("new cause"));
    assert_eq!(row.cleanup_updated_at, Some(now()));
}

#[tokio::test]
async fn a_replay_failure_is_guarded_by_the_failed_status_and_the_tenant() {
    for status in [None, Some("pending"), Some("done")] {
        let (rig, chat) = rig().await;
        let mut seed = RowSeed::document();
        if let Some(status) = status {
            seed = seed.cleanup(if status == "pending" {
                "pending"
            } else {
                "done"
            });
        }
        let id = seed_row(&rig.db, &chat, seed).await;
        assert!(
            !store(&rig)
                .record_replay_failure(TENANT_A, id, "cause", now())
                .await
                .unwrap(),
            "{status:?}"
        );
        assert_eq!(
            row_of(&rig, &chat, id).await.last_cleanup_error,
            None,
            "{status:?}"
        );
    }
    let (rig, chat) = rig().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document().cleanup("failed").attempts(5),
    )
    .await;
    assert!(
        !store(&rig)
            .record_replay_failure(TENANT_B, id, "cause", now())
            .await
            .unwrap()
    );
}
