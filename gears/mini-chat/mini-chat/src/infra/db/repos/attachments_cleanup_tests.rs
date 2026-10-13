#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slices 5 and 6: the reaper's scan and CAS transaction
//! (`cpt-cf-mini-chat-algo-attachments-reap-abandoned`) and the cleanup-handler
//! store calls (`cpt-cf-mini-chat-algo-attachments-handle-cleanup`).

use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::attachment_test_env::{AttEnv, AttOptions};
use crate::chat_test_env::{BASE_UNIX, ts};
use crate::domain::service::attachments::model::{
    AttachmentKind, AttachmentStatus, CleanupRow, CleanupState, StaleUpload,
};
use crate::domain::service::attachments::ports::AttachmentRepository;
use crate::test_doubles::{OTHER_TENANT, USER_TENANT};

fn at(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(BASE_UNIX + secs).unwrap()
}

async fn stale(env: &AttEnv, cutoff: OffsetDateTime) -> Vec<StaleUpload> {
    env.sea.stale_uploads(cutoff, 100).await.unwrap()
}

// ================================================================================================
// the reaper scan
// ================================================================================================

#[tokio::test]
async fn the_scan_selects_pending_and_uploaded_rows_older_than_the_cutoff() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let pending = env
        .seed(chat, 1, |row| {
            row.set("status", "pending")
                .null("provider_file_id")
                .set("updated_at", ts(10))
        })
        .await;
    let uploaded = env
        .seed(chat, 2, |row| {
            row.set("status", "uploaded")
                .set("updated_at", ts(20))
                .set("secondary_file_id", "file_ant_2")
                .set("attachment_kind", "image")
                .set("storage_backend", "azure")
        })
        .await;

    let rows = stale(&env, at(100)).await;

    assert_eq!(
        rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![pending, uploaded]
    );
    assert_eq!(rows[0].status, AttachmentStatus::Pending);
    assert_eq!(rows[0].provider_file_id, None);
    assert_eq!(rows[1].status, AttachmentStatus::Uploaded);
    assert_eq!(rows[1].provider_file_id.as_deref(), Some("file-prov2"));
    assert_eq!(rows[1].secondary_file_id.as_deref(), Some("file_ant_2"));
    assert_eq!(rows[1].storage_backend, "azure");
    assert_eq!(rows[1].kind, AttachmentKind::Image);
    assert_eq!(rows[1].tenant_id, USER_TENANT);
    assert_eq!(rows[1].chat_id, chat);
}

#[tokio::test]
async fn the_cutoff_is_strict_and_other_rows_are_never_selected() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    env.seed(chat, 1, |row| {
        row.set("status", "uploaded").set("updated_at", ts(50))
    })
    .await;
    env.seed(chat, 2, |row| {
        row.set("status", "uploaded").set("updated_at", ts(60))
    })
    .await;
    env.seed(chat, 3, |row| {
        row.set("status", "ready").set("updated_at", ts(1))
    })
    .await;
    env.seed(chat, 4, |row| {
        row.set("status", "failed").set("updated_at", ts(1))
    })
    .await;
    env.seed(chat, 5, |row| {
        row.set("status", "uploaded")
            .set("updated_at", ts(1))
            .set("deleted_at", ts(2))
    })
    .await;
    for (n, state) in [(6, "pending"), (7, "done"), (8, "failed")] {
        env.seed(chat, n, |row| {
            row.set("status", "uploaded")
                .set("updated_at", ts(1))
                .set("cleanup_status", state)
        })
        .await;
    }

    let rows = stale(&env, at(55)).await;

    assert_eq!(
        rows.len(),
        1,
        "only the live uploaded row older than the cutoff: {rows:?}"
    );
    assert_eq!(rows[0].updated_at, at(50));
    assert!(
        stale(&env, at(50)).await.is_empty(),
        "updated_at < cutoff is strict"
    );
}

#[tokio::test]
async fn the_scan_is_oldest_first_and_bounded_by_the_limit() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let mut ids = Vec::new();
    for (n, secs) in [(1, 50), (2, 10), (3, 40), (4, 20), (5, 30)] {
        ids.push((
            secs,
            env.seed(chat, n, |row| {
                row.set("status", "uploaded").set("updated_at", ts(secs))
            })
            .await,
        ));
    }
    ids.sort_by_key(|(secs, _)| *secs);

    let rows = env.sea.stale_uploads(at(1000), 3).await.unwrap();

    assert_eq!(
        rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        ids.iter().take(3).map(|(_, id)| *id).collect::<Vec<_>>()
    );
}

// ================================================================================================
// the reaper CAS transaction
// ================================================================================================

async fn selected(env: &AttEnv, id: Uuid) -> StaleUpload {
    stale(env, at(1000))
        .await
        .into_iter()
        .find(|r| r.id == id)
        .expect("selected")
}

#[tokio::test]
async fn a_pending_row_without_a_file_is_failed_without_cleanup() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("status", "pending")
                .null("provider_file_id")
                .set("updated_at", ts(10))
        })
        .await;
    let row = selected(&env, id).await;

    let won = env.sea.abandon(&row, at(1000), env.now()).await.unwrap();

    let snap = env.att(id).await;
    assert!(won);
    assert_eq!(snap.status, "failed");
    assert_eq!(snap.error_code.as_deref(), Some("upload_abandoned"));
    assert_eq!(snap.updated_at, AttEnv::stamp(env.now()));
    assert_eq!(snap.cleanup_status, None);
    assert_eq!(snap.deleted_at, None, "the row is never soft-deleted");
    assert_eq!(env.stream.quota.outbox_message_count().await, 0);
}

#[tokio::test]
async fn an_uploaded_row_with_a_file_gets_cleanup_and_its_message_in_one_transaction() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("status", "uploaded")
                .set("updated_at", ts(10))
                .set("secondary_file_id", "file_ant_1")
                .set("secondary_status", "uploaded")
                .set("storage_backend", "azure")
                .set("attachment_kind", "image")
        })
        .await;
    let row = selected(&env, id).await;

    let won = env.sea.abandon(&row, at(1000), env.now()).await.unwrap();

    let snap = env.att(id).await;
    assert!(won);
    assert_eq!(snap.status, "failed");
    assert_eq!(snap.error_code.as_deref(), Some("upload_abandoned"));
    assert_eq!(snap.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(
        snap.cleanup_updated_at.as_deref(),
        Some(AttEnv::stamp(env.now()).as_str())
    );
    assert_eq!(snap.deleted_at, None);
    assert_eq!(
        env.cleanup_messages().await,
        vec![json!({
            "event_type": "attachment_upload_abandoned",
            "tenant_id": USER_TENANT.to_string(),
            "chat_id": chat.to_string(),
            "attachment_id": id.to_string(),
            "provider_file_id": "file-prov1",
            "vector_store_id": null,
            "storage_backend": "azure",
            "attachment_kind": "image",
            "deleted_at": AttEnv::stamp(env.now()),
            "secondary_ref": null,
        })],
        "no secondary_ref: the copy is not deleted"
    );
    assert_eq!(
        env.cleanup_partitions().await,
        vec![i64::from(env.partition_of(USER_TENANT, chat))]
    );
}

#[tokio::test]
async fn the_cas_skips_a_row_that_changed_since_the_scan() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let refreshed = env
        .seed(chat, 1, |row| {
            row.set("status", "uploaded").set("updated_at", ts(10))
        })
        .await;
    let advanced = env
        .seed(chat, 2, |row| {
            row.set("status", "pending")
                .null("provider_file_id")
                .set("updated_at", ts(11))
        })
        .await;
    let deleted = env
        .seed(chat, 3, |row| {
            row.set("status", "uploaded").set("updated_at", ts(12))
        })
        .await;
    let claimed = env
        .seed(chat, 4, |row| {
            row.set("status", "uploaded").set("updated_at", ts(13))
        })
        .await;
    let rows = stale(&env, at(1000)).await;
    // The upload finishes, the heartbeat refreshes, the user deletes, the chat is deleted.
    env.exec(&format!(
        "UPDATE attachments SET updated_at = '{}' WHERE id = {}",
        AttEnv::stamp(at(2000)),
        crate::chat_test_env::blob(refreshed)
    ))
    .await;
    env.exec(&format!(
        "UPDATE attachments SET status = 'uploaded', provider_file_id = 'file-new' WHERE id = {}",
        crate::chat_test_env::blob(advanced)
    ))
    .await;
    env.exec(&format!(
        "UPDATE attachments SET deleted_at = '{}' WHERE id = {}",
        ts(20),
        crate::chat_test_env::blob(deleted)
    ))
    .await;
    env.exec(&format!(
        "UPDATE attachments SET cleanup_status = 'pending' WHERE id = {}",
        crate::chat_test_env::blob(claimed)
    ))
    .await;
    let before = env.attachments_dump().await;

    for row in &rows {
        assert!(
            !env.sea.abandon(row, at(1000), env.now()).await.unwrap(),
            "{}",
            row.id
        );
    }

    assert_eq!(env.attachments_dump().await, before);
    assert_eq!(env.stream.quota.outbox_message_count().await, 0);
}

#[tokio::test]
async fn a_failed_enqueue_rolls_the_abandon_back() {
    let env = AttEnv::with(AttOptions {
        enqueue_fails: true,
        ..AttOptions::default()
    })
    .await;
    let chat = env.chat("m-oa").await;
    let id = env
        .seed(chat, 1, |row| {
            row.set("status", "uploaded").set("updated_at", ts(10))
        })
        .await;
    let row = selected(&env, id).await;
    let before = env.attachments_dump().await;

    let result = env.sea.abandon(&row, at(1000), env.now()).await;

    assert!(result.is_err(), "{result:?}");
    assert_eq!(
        env.attachments_dump().await,
        before,
        "the row stays uploaded for the next scan"
    );
}

// ================================================================================================
// handler store calls
// ================================================================================================

#[tokio::test]
async fn the_chat_deleted_flag_covers_soft_deleted_chats() {
    let env = AttEnv::new().await;
    let live = env.chat("m-oa").await;
    let gone = env.chat("m-oa").await;
    env.delete_chat(gone).await;

    assert_eq!(
        env.sea.chat_deleted(USER_TENANT, live).await.unwrap(),
        Some(false)
    );
    assert_eq!(
        env.sea.chat_deleted(USER_TENANT, gone).await.unwrap(),
        Some(true)
    );
    assert_eq!(
        env.sea
            .chat_deleted(USER_TENANT, Uuid::new_v4())
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        env.sea.chat_deleted(OTHER_TENANT, live).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn the_cleanup_row_reports_state_and_attempts_soft_deleted_rows_included() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let none = env.seed(chat, 1, |row| row).await;
    let pending = env
        .seed(chat, 2, |row| {
            row.set("cleanup_status", "pending")
                .set("cleanup_attempts", 2_i32)
                .set("deleted_at", ts(5))
        })
        .await;
    let done = env
        .seed(chat, 3, |row| row.set("cleanup_status", "done"))
        .await;
    let failed = env
        .seed(chat, 4, |row| {
            row.set("cleanup_status", "failed")
                .set("cleanup_attempts", 5_i32)
        })
        .await;

    let read = |id| env.sea.cleanup_row(USER_TENANT, chat, id);

    assert_eq!(
        read(none).await.unwrap(),
        Some(CleanupRow {
            state: None,
            attempts: 0
        })
    );
    assert_eq!(
        read(pending).await.unwrap(),
        Some(CleanupRow {
            state: Some(CleanupState::Pending),
            attempts: 2
        })
    );
    assert_eq!(
        read(done).await.unwrap().unwrap().state,
        Some(CleanupState::Done)
    );
    assert_eq!(
        read(failed).await.unwrap(),
        Some(CleanupRow {
            state: Some(CleanupState::Failed),
            attempts: 5
        })
    );
    assert_eq!(read(Uuid::new_v4()).await.unwrap(), None);
    assert_eq!(
        env.sea
            .cleanup_row(USER_TENANT, Uuid::new_v4(), none)
            .await
            .unwrap(),
        None,
        "bound to the chat"
    );
    assert_eq!(
        env.sea.cleanup_row(OTHER_TENANT, chat, none).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn an_attempt_is_counted_only_while_the_row_is_pending() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let id = env
        .seed(chat, 1, |row| row.set("cleanup_status", "pending"))
        .await;

    let first = env
        .sea
        .record_cleanup_attempt(USER_TENANT, id, "http 500", env.now())
        .await
        .unwrap();
    let second = env
        .sea
        .record_cleanup_attempt(USER_TENANT, id, "timeout", env.now())
        .await
        .unwrap();

    let snap = env.att(id).await;
    assert_eq!((first, second), (Some(1), Some(2)));
    assert_eq!(snap.cleanup_attempts, 2);
    assert_eq!(snap.last_cleanup_error.as_deref(), Some("timeout"));
    assert_eq!(
        snap.cleanup_updated_at.as_deref(),
        Some(AttEnv::stamp(env.now()).as_str())
    );
    assert_eq!(snap.cleanup_status.as_deref(), Some("pending"));
}

#[tokio::test]
async fn an_attempt_on_a_row_that_is_not_pending_changes_nothing() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let cases = [
        env.seed(chat, 1, |row| row).await,
        env.seed(chat, 2, |row| row.set("cleanup_status", "done"))
            .await,
        env.seed(chat, 3, |row| row.set("cleanup_status", "failed"))
            .await,
    ];
    let pending = env
        .seed(chat, 4, |row| row.set("cleanup_status", "pending"))
        .await;
    let before = env.attachments_dump().await;

    for id in cases {
        assert_eq!(
            env.sea
                .record_cleanup_attempt(USER_TENANT, id, "http 500", env.now())
                .await
                .unwrap(),
            None
        );
    }
    assert_eq!(
        env.sea
            .record_cleanup_attempt(OTHER_TENANT, pending, "x", env.now())
            .await
            .unwrap(),
        None
    );

    assert_eq!(env.attachments_dump().await, before);
}

#[tokio::test]
async fn the_cleanup_finishes_as_done_or_failed_only_from_pending() {
    let env = AttEnv::new().await;
    let chat = env.chat("m-oa").await;
    let to_done = env
        .seed(chat, 1, |row| row.set("cleanup_status", "pending"))
        .await;
    let to_failed = env
        .seed(chat, 2, |row| row.set("cleanup_status", "pending"))
        .await;
    let already_done = env
        .seed(chat, 3, |row| row.set("cleanup_status", "done"))
        .await;
    let none = env.seed(chat, 4, |row| row).await;

    assert!(
        env.sea
            .finish_cleanup(USER_TENANT, to_done, CleanupState::Done, env.now())
            .await
            .unwrap()
    );
    assert!(
        env.sea
            .finish_cleanup(USER_TENANT, to_failed, CleanupState::Failed, env.now())
            .await
            .unwrap()
    );
    assert!(
        !env.sea
            .finish_cleanup(USER_TENANT, already_done, CleanupState::Failed, env.now())
            .await
            .unwrap()
    );
    assert!(
        !env.sea
            .finish_cleanup(USER_TENANT, none, CleanupState::Done, env.now())
            .await
            .unwrap()
    );
    assert!(
        !env.sea
            .finish_cleanup(OTHER_TENANT, to_done, CleanupState::Failed, env.now())
            .await
            .unwrap()
    );

    assert_eq!(
        env.att(to_done).await.cleanup_status.as_deref(),
        Some("done")
    );
    assert_eq!(
        env.att(to_done).await.cleanup_updated_at.as_deref(),
        Some(AttEnv::stamp(env.now()).as_str())
    );
    assert_eq!(
        env.att(to_failed).await.cleanup_status.as_deref(),
        Some("failed")
    );
    assert_eq!(
        env.att(already_done).await.cleanup_status.as_deref(),
        Some("done")
    );
    assert_eq!(env.att(none).await.cleanup_status, None);
}
