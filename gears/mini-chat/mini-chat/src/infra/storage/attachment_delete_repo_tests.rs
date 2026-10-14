//! `SQLite` tests of the attachment delete transaction over the real outbox: the soft delete with
//! `cleanup_status` `pending` and one `attachment_deleted` message in one transaction, the
//! payload of the DESIGN table (with the `secondary_ref` of an uploaded secondary copy), the
//! partition by tenant, 404 masking (unknown id, other chat, other uploader, other tenant, the
//! uploader check before the idempotency check), the idempotent repeat without a second message,
//! the lock by any `message_attachments` row, the concurrent delete, a set `cleanup_status`
//! keeping its state, and the rollback on an oversized payload and on a database failure.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`:
//! algorithms `cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction`
//! (`inst-dtx-*`) and `-cleanup-enqueue` (`inst-enq-*`); state
//! `cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status` (`inst-cs-pending`);
//! definition of done `cpt-cf-mini-chat-dod-attachments-and-file-search-delete-api`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::attachments::store::{
    AttachmentCleanupQueue, AttachmentStore, DeleteOutcome, DeleteRequest,
};
use crate::domain::error::{DomainError, EnqueueSite};
use crate::infra::outbox::partition_for;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::attachments::{
    ANTHROPIC_ALIAS, RowSeed, attachment_cleanup_rows, seed_row,
};
use crate::test_support::chats::{a1, at, attachment_rows, fail_outbox_inserts, seed_chat};
use crate::test_support::quota::{QuotaRig, STANDARD, quota_rig};
use crate::test_support::send::{MessageSeed, TurnSeedFull, link_att, seed_msg, seed_turn_full, t};
use crate::test_support::{TENANT_A, TENANT_B, USER_A1, USER_A2};

fn now() -> OffsetDateTime {
    at(2_000_000) + time::Duration::microseconds(456_000)
}

async fn rig() -> (QuotaRig, ChatRecord) {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    (rig, chat)
}

fn store(rig: &QuotaRig) -> &dyn AttachmentStore {
    &*rig.db.provider
}

fn cleanup(rig: &QuotaRig) -> Arc<dyn AttachmentCleanupQueue> {
    rig.enqueuer.clone()
}

fn request(chat: &ChatRecord, attachment_id: Uuid) -> DeleteRequest {
    DeleteRequest {
        tenant_id: chat.tenant_id,
        chat_id: chat.id,
        attachment_id,
        uploader_id: USER_A1,
        now: now(),
        secondary_upstream_alias: None,
    }
}

async fn delete(rig: &QuotaRig, request: &DeleteRequest) -> Result<DeleteOutcome, DomainError> {
    store(rig).delete_attachment(request, cleanup(rig)).await
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

async fn messages(rig: &QuotaRig) -> Vec<crate::test_support::chats::OutboxRow> {
    attachment_cleanup_rows(&rig.db, &rig.outbox).await
}

/// Assert the row and the queue are exactly as before a refused delete.
async fn assert_untouched(rig: &QuotaRig, chat: &ChatRecord, id: Uuid) {
    let row = row_of(rig, chat, id).await;
    assert_eq!(row.deleted_at, None, "the attachment stays active");
    assert_eq!(row.cleanup_status, None);
    assert_eq!(row.cleanup_updated_at, None);
    assert_eq!(row.updated_at, at(0));
    assert!(messages(rig).await.is_empty(), "nothing was enqueued");
}

// ── the soft delete and the message ──────────────────────────────────────────

#[tokio::test]
async fn an_unreferenced_attachment_is_soft_deleted_with_cleanup_pending() {
    // inst-dtx-select, inst-dtx-soft-delete, inst-dtx-return-deleted, inst-cs-pending
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    assert_eq!(
        delete(&rig, &request(&chat, id)).await.unwrap(),
        DeleteOutcome::Deleted
    );
    let row = row_of(&rig, &chat, id).await;
    assert_eq!(row.deleted_at, Some(now()));
    assert_eq!(row.updated_at, now());
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(row.cleanup_updated_at, Some(now()));
    assert_eq!(row.cleanup_attempts, 0);
    assert_eq!(
        row.status, "ready",
        "the status is orthogonal to the soft delete"
    );
}

#[tokio::test]
async fn the_delete_enqueues_exactly_one_message_in_the_same_transaction() {
    // inst-dtx-enqueue, inst-dtx-commit, inst-enq-transactional, inst-enq-return
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    delete(&rig, &request(&chat, id)).await.unwrap();
    let rows = messages(&rig).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].queue, rig.outbox.cleanup_queue_name);
    assert_eq!(rows[0].payload_type, "mini_chat.attachment_cleanup");
    assert_eq!(
        rows[0].partition,
        i64::from(partition_for(TENANT_A, rig.outbox.num_partitions)),
        "partitioned by `tenant_id`"
    );
}

#[tokio::test]
async fn the_message_carries_the_payload_of_the_design_table() {
    // inst-enq-payload: no secondary copy, so `secondary_ref` is null
    let (rig, chat) = rig().await;
    let seed = RowSeed::document();
    let file = seed.provider_file_id.clone().unwrap();
    let id = seed_row(&rig.db, &chat, seed).await;
    delete(&rig, &request(&chat, id)).await.unwrap();
    let payload = messages(&rig).await.remove(0).payload;
    assert_eq!(payload["event_type"], "attachment_deleted");
    assert_eq!(payload["tenant_id"], TENANT_A.to_string());
    assert_eq!(payload["chat_id"], chat.id.to_string());
    assert_eq!(payload["attachment_id"], id.to_string());
    assert_eq!(payload["provider_file_id"], file);
    assert!(payload["vector_store_id"].is_null(), "{payload}");
    assert_eq!(payload["storage_backend"], "azure_openai");
    assert_eq!(payload["attachment_kind"], "document");
    assert!(payload["secondary_ref"].is_null(), "{payload}");
    let deleted_at = OffsetDateTime::parse(
        payload["deleted_at"].as_str().expect("the enqueue time"),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    assert_eq!(deleted_at, now(), "`deleted_at` equals the enqueue time");
}

#[tokio::test]
async fn an_image_row_is_enqueued_with_its_kind() {
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::image()).await;
    delete(&rig, &request(&chat, id)).await.unwrap();
    assert_eq!(messages(&rig).await[0].payload["attachment_kind"], "image");
}

#[tokio::test]
async fn a_row_that_never_reached_the_provider_enqueues_a_null_file_id() {
    // inst-enq-payload: `provider_file_id` null when the upload never reached the provider
    let (rig, chat) = rig().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document().status("pending").no_provider_file(),
    )
    .await;
    assert_eq!(
        delete(&rig, &request(&chat, id)).await.unwrap(),
        DeleteOutcome::Deleted
    );
    let payload = messages(&rig).await.remove(0).payload;
    assert!(payload["provider_file_id"].is_null(), "{payload}");
}

#[tokio::test]
async fn an_uploaded_secondary_copy_is_carried_as_the_secondary_ref() {
    // inst-enq-secondary, inst-enq-secondary-ref: `{file_id, provider_kind, upstream_alias}`
    let (rig, chat) = rig().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::image().secondary("file_ant_7", "uploaded"),
    )
    .await;
    let mut request = request(&chat, id);
    request.secondary_upstream_alias = Some(ANTHROPIC_ALIAS.to_owned());
    assert_eq!(
        delete(&rig, &request).await.unwrap(),
        DeleteOutcome::Deleted
    );
    let payload = messages(&rig).await.remove(0).payload;
    assert_eq!(
        payload["secondary_ref"],
        serde_json::json!({
            "file_id": "file_ant_7",
            "provider_kind": "anthropic",
            "upstream_alias": ANTHROPIC_ALIAS,
        })
    );
}

#[tokio::test]
async fn a_secondary_copy_that_is_not_uploaded_gives_a_null_secondary_ref() {
    // inst-enq-secondary-ref: otherwise `secondary_ref` is null
    for status in ["pending", "failed", "not_attempted"] {
        let (rig, chat) = rig().await;
        let seed = if status == "not_attempted" {
            RowSeed::image()
        } else {
            RowSeed::image().secondary("file_ant_7", status_static(status))
        };
        let id = seed_row(&rig.db, &chat, seed).await;
        let mut request = request(&chat, id);
        request.secondary_upstream_alias = Some(ANTHROPIC_ALIAS.to_owned());
        delete(&rig, &request).await.unwrap();
        let payload = messages(&rig).await.remove(0).payload;
        assert!(payload["secondary_ref"].is_null(), "{status}: {payload}");
    }
}

fn status_static(status: &str) -> &'static str {
    match status {
        "pending" => "pending",
        "failed" => "failed",
        _ => "not_attempted",
    }
}

#[tokio::test]
async fn the_message_holds_no_credential_or_file_content() {
    // constraint no-credentials: only identifiers and labels travel
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    delete(&rig, &request(&chat, id)).await.unwrap();
    let payload = messages(&rig).await.remove(0).payload;
    let keys: std::collections::BTreeSet<_> =
        payload.as_object().unwrap().keys().cloned().collect();
    let expected: std::collections::BTreeSet<_> = [
        "event_type",
        "tenant_id",
        "chat_id",
        "attachment_id",
        "provider_file_id",
        "vector_store_id",
        "storage_backend",
        "attachment_kind",
        "deleted_at",
        "secondary_ref",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(keys, expected, "{payload}");
}

// ── not found ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_unknown_attachment_is_not_found() {
    // inst-dtx-not-found, inst-dtx-return-not-found
    let (rig, chat) = rig().await;
    let outcome = delete(&rig, &request(&chat, Uuid::new_v4())).await.unwrap();
    assert_eq!(outcome, DeleteOutcome::NotFound);
    assert!(messages(&rig).await.is_empty());
}

#[tokio::test]
async fn an_attachment_of_another_chat_is_not_found() {
    let (rig, chat) = rig().await;
    let other = seed_chat(
        &rig.db,
        &a1(),
        Uuid::new_v4(),
        Some("other"),
        STANDARD,
        at(0),
    )
    .await;
    let id = seed_row(&rig.db, &other, RowSeed::document()).await;
    assert_eq!(
        delete(&rig, &request(&chat, id)).await.unwrap(),
        DeleteOutcome::NotFound
    );
    assert_untouched(&rig, &other, id).await;
}

#[tokio::test]
async fn an_attachment_uploaded_by_another_user_is_not_found() {
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().uploader(USER_A2)).await;
    assert_eq!(
        delete(&rig, &request(&chat, id)).await.unwrap(),
        DeleteOutcome::NotFound
    );
    assert_untouched(&rig, &chat, id).await;
}

#[tokio::test]
async fn an_attachment_of_another_tenant_is_not_found() {
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    let mut request = request(&chat, id);
    request.tenant_id = TENANT_B;
    assert_eq!(
        delete(&rig, &request).await.unwrap(),
        DeleteOutcome::NotFound
    );
    assert_untouched(&rig, &chat, id).await;
}

#[tokio::test]
async fn the_uploader_check_runs_before_the_idempotency_check() {
    // inst-dtx-not-found: "checked before the idempotency check" — a repeated delete by another
    // user of an already deleted attachment is also not found
    let (rig, chat) = rig().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document().uploader(USER_A2).deleted(at(10)),
    )
    .await;
    assert_eq!(
        delete(&rig, &request(&chat, id)).await.unwrap(),
        DeleteOutcome::NotFound
    );
    assert!(messages(&rig).await.is_empty());
}

// ── idempotency ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_already_deleted_attachment_is_reported_without_a_second_message() {
    // inst-dtx-already, inst-dtx-return-already
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    assert_eq!(
        delete(&rig, &request(&chat, id)).await.unwrap(),
        DeleteOutcome::Deleted
    );
    let first = row_of(&rig, &chat, id).await;
    let mut later = request(&chat, id);
    later.now = now() + time::Duration::seconds(30);
    assert_eq!(
        delete(&rig, &later).await.unwrap(),
        DeleteOutcome::AlreadyDeleted
    );
    assert_eq!(messages(&rig).await.len(), 1, "no second message");
    let second = row_of(&rig, &chat, id).await;
    assert_eq!(
        second.deleted_at, first.deleted_at,
        "the first delete time stays"
    );
    assert_eq!(second.updated_at, first.updated_at);
}

#[tokio::test]
async fn a_soft_deleted_row_of_a_deleted_chat_flow_is_already_deleted() {
    // a row soft-deleted by anything else (for example chat cleanup) is already deleted
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().deleted(at(10))).await;
    assert_eq!(
        delete(&rig, &request(&chat, id)).await.unwrap(),
        DeleteOutcome::AlreadyDeleted
    );
    assert!(messages(&rig).await.is_empty());
}

#[tokio::test]
async fn two_concurrent_deletes_commit_one_message() {
    // inst-dtx-race, inst-dtx-return-race: the loser reports already deleted
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    let first = request(&chat, id);
    let second = request(&chat, id);
    let (a, b) = tokio::join!(delete(&rig, &first), delete(&rig, &second));
    let mut outcomes = vec![a.unwrap(), b.unwrap()];
    outcomes.sort_by_key(|outcome| format!("{outcome:?}"));
    assert_eq!(
        outcomes,
        vec![DeleteOutcome::AlreadyDeleted, DeleteOutcome::Deleted]
    );
    assert_eq!(messages(&rig).await.len(), 1);
}

// ── the lock ─────────────────────────────────────────────────────────────────

async fn linked(rig: &QuotaRig, chat: &ChatRecord, id: Uuid) {
    let message = seed_msg(
        &rig.db,
        chat,
        MessageSeed::new("user", "see the file", t(1)),
    )
    .await;
    link_att(&rig.db, chat, message, id).await;
}

#[tokio::test]
async fn an_attachment_referenced_by_a_message_is_locked() {
    // inst-dtx-begin, inst-dtx-lock-check, inst-dtx-locked, inst-dtx-return-locked
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    linked(&rig, &chat, id).await;
    assert_eq!(
        delete(&rig, &request(&chat, id)).await.unwrap(),
        DeleteOutcome::Locked
    );
    assert_untouched(&rig, &chat, id).await;
}

#[tokio::test]
async fn a_reference_from_a_soft_deleted_turn_still_locks_the_attachment() {
    // feature-local deviation: any `message_attachments` row locks, links are never removed
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    let message = seed_msg(
        &rig.db,
        &chat,
        MessageSeed::new("user", "x", t(1)).deleted(t(5)),
    )
    .await;
    link_att(&rig.db, &chat, message, id).await;
    seed_turn_full(&rig.db, &chat, TurnSeedFull::new("completed").deleted(t(6))).await;
    assert_eq!(
        delete(&rig, &request(&chat, id)).await.unwrap(),
        DeleteOutcome::Locked
    );
    assert_untouched(&rig, &chat, id).await;
}

#[tokio::test]
async fn only_the_referenced_attachment_is_locked() {
    let (rig, chat) = rig().await;
    let locked = seed_row(&rig.db, &chat, RowSeed::document()).await;
    let free = seed_row(&rig.db, &chat, RowSeed::document()).await;
    linked(&rig, &chat, locked).await;
    assert_eq!(
        delete(&rig, &request(&chat, free)).await.unwrap(),
        DeleteOutcome::Deleted
    );
    assert_eq!(
        delete(&rig, &request(&chat, locked)).await.unwrap(),
        DeleteOutcome::Locked
    );
    assert_eq!(messages(&rig).await.len(), 1);
}

#[tokio::test]
async fn the_lock_check_is_made_before_the_idempotency_of_a_live_row_only() {
    // a locked, live row stays locked on every repeat: the message links are never modified
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    linked(&rig, &chat, id).await;
    for _ in 0..2 {
        assert_eq!(
            delete(&rig, &request(&chat, id)).await.unwrap(),
            DeleteOutcome::Locked
        );
    }
    let links = crate::test_support::send::link_rows(&rig.db, chat.id).await;
    assert_eq!(
        links.len(),
        1,
        "the message's attachments array is never modified"
    );
}

// ── an existing cleanup status keeps its state ───────────────────────────────

#[tokio::test]
async fn a_set_cleanup_status_keeps_its_state_on_delete() {
    // inst-dtx-soft-delete: `cleanup_status` is set to `pending` only when it is NULL
    for status in ["pending", "done", "failed"] {
        let (rig, chat) = rig().await;
        let seed = RowSeed::document()
            .status("failed")
            .error("upload_abandoned")
            .cleanup(static_cleanup(status))
            .attempts(2);
        let id = seed_row(&rig.db, &chat, seed).await;
        assert_eq!(
            delete(&rig, &request(&chat, id)).await.unwrap(),
            DeleteOutcome::Deleted
        );
        let row = row_of(&rig, &chat, id).await;
        assert_eq!(row.cleanup_status.as_deref(), Some(status), "{status}");
        assert_eq!(row.cleanup_attempts, 2, "{status}");
        assert_eq!(row.deleted_at, Some(now()), "{status}");
        assert_eq!(
            messages(&rig).await.len(),
            1,
            "{status}: the message is still enqueued"
        );
    }
}

fn static_cleanup(status: &str) -> &'static str {
    match status {
        "pending" => "pending",
        "done" => "done",
        _ => "failed",
    }
}

#[tokio::test]
async fn a_failed_upload_row_can_be_deleted_like_any_row() {
    // the delete flow does not depend on the attachment status
    for status in ["pending", "uploaded", "ready", "failed"] {
        let (rig, chat) = rig().await;
        let id = seed_row(
            &rig.db,
            &chat,
            RowSeed::document().status(static_status(status)),
        )
        .await;
        assert_eq!(
            delete(&rig, &request(&chat, id)).await.unwrap(),
            DeleteOutcome::Deleted,
            "{status}"
        );
    }
}

fn static_status(status: &str) -> &'static str {
    match status {
        "pending" => "pending",
        "uploaded" => "uploaded",
        "ready" => "ready",
        _ => "failed",
    }
}

// ── rollback ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_oversized_payload_rolls_back_and_the_attachment_stays_active() {
    // inst-dtx-too-large, inst-dtx-return-too-large, inst-enq-too-large, inst-enq-return-error
    let (rig, chat) = rig().await;
    let huge = "f".repeat(70_000);
    let id = seed_row(&rig.db, &chat, RowSeed::document().file_id(&huge)).await;
    let result = delete(&rig, &request(&chat, id)).await;
    assert!(
        matches!(
            result,
            Err(DomainError::OutboxPayloadTooLarge {
                site: EnqueueSite::AttachmentDelete
            })
        ),
        "{result:?}"
    );
    assert_untouched(&rig, &chat, id).await;
}

#[tokio::test]
async fn a_database_failure_rolls_back_the_soft_delete() {
    // inst-dtx-catch, inst-dtx-return-db-failure: nothing is changed
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    fail_outbox_inserts(&rig.db).await;
    let result = delete(&rig, &request(&chat, id)).await;
    assert!(
        matches!(result, Err(DomainError::Internal { .. })),
        "{result:?}"
    );
    assert_untouched(&rig, &chat, id).await;
}

#[tokio::test]
async fn the_delete_makes_no_row_change_other_than_the_one_attachment() {
    let (rig, chat) = rig().await;
    let target = seed_row(&rig.db, &chat, RowSeed::document()).await;
    let bystander = seed_row(&rig.db, &chat, RowSeed::document()).await;
    delete(&rig, &request(&chat, target)).await.unwrap();
    let row = row_of(&rig, &chat, bystander).await;
    assert_eq!(row.deleted_at, None);
    assert_eq!(row.cleanup_status, None);
    assert_eq!(row.updated_at, at(0));
}
