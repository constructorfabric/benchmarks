//! The chat-cleanup handler and the attachment cleanup handler deleting the same provider file:
//! the attachment handler checked the chat before the soft-delete, the chat-cleanup handler runs
//! while the first delete is in flight; both treat 404 as success, the terminal write is guarded by
//! `cleanup_status` `pending`, so exactly one path writes it and neither reports an error. And the
//! ownership transfer: once the chat is soft-deleted the attachment handler acknowledges without
//! action while the chat-cleanup handler deletes the file, and the upload reaper leaves rows with a
//! `cleanup_status` alone.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-cleanup-boundaries` (`inst-cb-active`,
//! `inst-cb-attachment-path`, `inst-cb-deleted`, `inst-cb-transfer`, `inst-cb-no-lock`,
//! `inst-cb-outbox-only`, `inst-cb-return`), `cpt-cf-mini-chat-algo-background-maintenance-chat-attachment-file` (`inst-af-guard-lost`,
//! `inst-af-return-skipped`) and `cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-handler` (`inst-cc-idempotent`); definition of done
//! `cpt-cf-mini-chat-dod-background-maintenance-cleanup-boundaries`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::json;
use toolkit_db::outbox::LeasedMessageHandler;

use crate::domain::maintenance::HandlerOutcome;
use crate::test_support::attachments::{
    RowSeed, cleanup_message, deleted_payload, outcome_of, rig, seed_row, soft_delete_chat,
};
use crate::test_support::chats::{at, attachment_rows};
use crate::test_support::maintenance::{chat_cleanup_payload, cleanup_worker_over_attach_rig};
use crate::test_support::provider::Gate;
use crate::test_support::storage_mock::{Answer, Route};

const FILE: &str = "file-shared-1";

#[tokio::test]
async fn both_paths_deleting_the_same_file_end_with_one_terminal_write_and_no_error() {
    // inst-cb-no-lock, inst-af-guard-lost, inst-af-return-skipped: the two paths are not locked
    // against each other; 404 counts as success on both, the write is guarded by `pending`
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document().cleanup("pending").file_id(FILE),
    )
    .await;
    let gate = Gate::new();
    // the first delete (the attachment handler's, started while the chat was still live) is parked
    rig.storage.queue(
        Route::FilesDelete,
        Answer::gated(&gate, Answer::ok(&json!({ "deleted": true }))),
    );
    // the second one finds the file already gone
    rig.storage.queue(Route::FilesDelete, Answer::status(404));

    let attachment_handler = rig.cleanup();
    let message = cleanup_message(&deleted_payload(&chat, id, Some(FILE)), 0);
    let in_flight =
        tokio::spawn(async move { outcome_of(&attachment_handler.handler.handle(&message).await) });
    rig.storage.wait_for(Route::FilesDelete, 1).await;

    // the chat is soft-deleted while that delete is in flight; chat cleanup takes over the row
    soft_delete_chat(&rig.db, chat.id, at(50)).await;
    let (worker, metrics) = cleanup_worker_over_attach_rig(&rig, 5);
    let payload = serde_json::to_vec(&chat_cleanup_payload(
        chat.tenant_id,
        chat.id,
        uuid::Uuid::new_v4(),
    ))
    .unwrap();
    assert_eq!(worker.handle(&payload, 0).await, HandlerOutcome::Ok);
    let after_chat_cleanup = attachment_rows(&rig.db, chat.id).await.remove(0);
    assert_eq!(after_chat_cleanup.cleanup_status.as_deref(), Some("done"));
    let written_at = after_chat_cleanup.cleanup_updated_at;
    assert!(written_at.is_some());

    // the parked delete now answers; the attachment handler's guarded write matches no row
    gate.open();
    assert_eq!(
        in_flight.await.unwrap(),
        "ok",
        "no error on the losing path"
    );
    let end = attachment_rows(&rig.db, chat.id).await.remove(0);
    assert_eq!(end.cleanup_status.as_deref(), Some("done"));
    assert_eq!(
        end.cleanup_updated_at, written_at,
        "exactly one terminal write"
    );
    assert_eq!(end.cleanup_attempts, 0);
    assert_eq!(
        rig.storage.count_of(Route::FilesDelete),
        2,
        "both paths deleted the same file"
    );
    assert_eq!(
        metrics.completed(),
        vec!["file"],
        "the chat-cleanup path wrote the row"
    );
    assert_eq!(
        rig.metrics.cleanup_completed(),
        0,
        "the losing path counts nothing"
    );
}

#[tokio::test]
async fn the_attachment_handler_acknowledges_a_deleted_chat_while_chat_cleanup_deletes_the_file() {
    // inst-cb-deleted, inst-cb-transfer: exclusive ownership from the soft-delete commit on
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document().cleanup("pending").file_id(FILE),
    )
    .await;
    soft_delete_chat(&rig.db, chat.id, at(50)).await;

    let attachment_handler = rig.cleanup();
    let message = cleanup_message(&deleted_payload(&chat, id, Some(FILE)), 0);
    assert_eq!(
        outcome_of(&attachment_handler.handler.handle(&message).await),
        "ok"
    );
    assert_eq!(
        rig.storage.count_of(Route::FilesDelete),
        0,
        "no action on a deleted chat"
    );
    assert_eq!(
        attachment_rows(&rig.db, chat.id)
            .await
            .remove(0)
            .cleanup_status
            .as_deref(),
        Some("pending")
    );

    let (worker, _metrics) = cleanup_worker_over_attach_rig(&rig, 5);
    let payload = serde_json::to_vec(&chat_cleanup_payload(
        chat.tenant_id,
        chat.id,
        uuid::Uuid::new_v4(),
    ))
    .unwrap();
    assert_eq!(worker.handle(&payload, 0).await, HandlerOutcome::Ok);
    assert_eq!(rig.storage.count_of(Route::FilesDelete), 1);
    assert_eq!(
        attachment_rows(&rig.db, chat.id)
            .await
            .remove(0)
            .cleanup_status
            .as_deref(),
        Some("done")
    );
}

#[tokio::test]
async fn chat_cleanup_leaves_the_attachments_of_an_active_chat_to_the_attachment_path() {
    // inst-cb-active, inst-cb-attachment-path: a message for an active chat is rejected without
    // any provider call and no row changes
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .cleanup("pending")
            .file_id(FILE)
            .deleted(at(5)),
    )
    .await;
    let (worker, metrics) = cleanup_worker_over_attach_rig(&rig, 5);
    let payload = serde_json::to_vec(&chat_cleanup_payload(
        chat.tenant_id,
        chat.id,
        uuid::Uuid::new_v4(),
    ))
    .unwrap();
    let outcome = worker.handle(&payload, 0).await;
    assert!(matches!(outcome, HandlerOutcome::Reject(_)), "{outcome:?}");
    assert_eq!(rig.storage.call_count(), 0);
    assert_eq!(
        attachment_rows(&rig.db, chat.id)
            .await
            .iter()
            .find(|r| r.id == id)
            .unwrap()
            .cleanup_status
            .as_deref(),
        Some("pending")
    );
    assert!(metrics.is_silent());
}

#[tokio::test]
async fn the_upload_reaper_leaves_rows_with_a_cleanup_status_alone() {
    // inst-cb-outbox-only: no reaper or background task deletes a deleted chat's provider
    // resources; the rows with a `cleanup_status` belong to the outbox-driven path
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .status("uploaded")
            .cleanup("pending")
            .file_id(FILE)
            .updated(at(0)),
    )
    .await;
    soft_delete_chat(&rig.db, chat.id, at(50)).await;
    let reaper = rig.reaper();
    reaper.scan_once().await.expect("a scan");
    let row = attachment_rows(&rig.db, chat.id)
        .await
        .into_iter()
        .find(|r| r.id == id)
        .unwrap();
    assert_eq!(row.status, "uploaded", "the reaper did not fail the row");
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(
        rig.storage.call_count(),
        0,
        "the reaper made no provider call"
    );
}

#[tokio::test]
async fn background_indexing_leaves_rows_with_a_cleanup_status_alone() {
    // inst-cb-outbox-only: the heartbeat, the guarded `ready` write and the indexing failure of
    // the background task all require `cleanup_status IS NULL`, so a row of a deleted chat is
    // never touched by them and the chat-cleanup handler stays its only owner
    use crate::domain::attachments::store::{AttachmentCleanupQueue, AttachmentStore};
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .status("uploaded")
            .cleanup("pending")
            .file_id(FILE),
    )
    .await;
    soft_delete_chat(&rig.db, chat.id, at(50)).await;
    let store = rig.quota.db.provider.clone();
    let outbox_before = crate::test_support::chats::outbox_rows(&rig.db).await.len();
    assert!(!store.heartbeat(chat.tenant_id, id, at(60)).await.unwrap());
    assert!(
        !store
            .set_ready_guarded(chat.tenant_id, id, at(60))
            .await
            .unwrap()
    );
    let cleanup: std::sync::Arc<dyn AttachmentCleanupQueue> = rig.quota.enqueuer.clone();
    assert!(
        !store
            .fail_indexing(chat.tenant_id, chat.id, id, at(60), cleanup)
            .await
            .unwrap()
    );
    let row = attachment_rows(&rig.db, chat.id)
        .await
        .into_iter()
        .find(|r| r.id == id)
        .unwrap();
    assert_eq!(row.status, "uploaded");
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(
        crate::test_support::chats::outbox_rows(&rig.db).await.len(),
        outbox_before,
        "no cleanup message was enqueued"
    );
}
