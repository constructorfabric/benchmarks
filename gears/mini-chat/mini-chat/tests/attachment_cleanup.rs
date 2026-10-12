//! Provider-side cleanup: the attachment-cleanup handler (queue `mini-chat.attachment_cleanup`) and
//! the chat-cleanup handler (queue `mini-chat.chat_cleanup`), their outcomes (`Ok` / `Retry` /
//! `Reject`), the attachment cleanup state machine and the vector store ordering.
//!
//! Source of truth: DESIGN "Cleanup on Chat Deletion" (sequence, "Cleanup responsibility
//! boundaries", "Attachment cleanup state machine", "Outbox payload and execution semantics",
//! "Vector store cleanup ordering", "Additional invariants"), "Attachment Deletion" Phase 2, B.9.2
//! and B.9.3 (`cleanup_worker.max_attempts`, partitioning by chat). The handlers are driven two
//! ways: end to end through the real outbox (the API enqueues, the handler calls the fake provider
//! storage and updates the rows) and directly with crafted `OutboxMessage`s, which pins the
//! delivery counting (`attempts`) without waiting for backoff.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod attachment_support;
mod chat_support;
mod common;
mod quota_support;
mod stream_support;

use std::time::Duration;

use attachment_support::{
    ATTACHMENT_CLEANUP_QUEUE, AttRow, AttachmentHarness, AttachmentOptions, CHAT_CLEANUP_QUEUE,
    Cleanup, StorageCall, StorageOp, StorageReply, TENANT_A, chat_cleanup_payload, eventually,
};
use serde_json::{Value, json};
use toolkit_db::outbox::MessageResult;
use uuid::Uuid;

fn is_ok(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Ok)
}

fn is_retry(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Retry)
}

fn rejected(result: &MessageResult) -> Option<&str> {
    match result {
        MessageResult::Reject(reason) => Some(reason.as_str()),
        _ => None,
    }
}

/// Both cleanup queues acknowledge and record: the test drives the handlers itself.
async fn manual(config: Value) -> AttachmentHarness {
    AttachmentHarness::with(AttachmentOptions {
        config,
        attachment_cleanup: Cleanup::Recording,
        chat_cleanup: Cleanup::Recording,
        ..Default::default()
    })
    .await
}

fn max_attempts(n: u32) -> Value {
    json!({"cleanup_worker": {"max_attempts": n}})
}

// ═════════════════════════════════════════════════════════════════════════════
// attachment cleanup handler: end to end
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn deleting_an_attachment_deletes_its_provider_file_through_the_outbox() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let id = h.doc(chat, "a.pdf").await;
    let provider_file = h.provider_file_of(id).await;

    assert_eq!(h.delete(chat, id).await.status.as_u16(), 204);
    let row = h.wait_cleanup(id, "done").await;
    assert!(
        row.cleanup_updated_at.is_some(),
        "cleanup_updated_at is set on completion"
    );
    assert_eq!(row.cleanup_attempts, 0, "only failed attempts are counted");
    assert!(row.last_cleanup_error.is_none());
    assert!(row.deleted_at.is_some());

    let deletes = h.storage.calls_to(StorageOp::DeleteFile);
    assert_eq!(deletes.len(), 1);
    assert_eq!(deletes[0].method, http::Method::DELETE);
    assert_eq!(
        deletes[0].path(),
        format!("/provider-alias/v1/files/{provider_file}")
    );
    assert_eq!(h.storage.count(StorageOp::DeleteVectorStore), 0);
}

#[tokio::test]
async fn a_provider_error_is_retried_by_the_outbox_until_the_delete_succeeds() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let id = h.doc(chat, "a.pdf").await;
    h.storage.push(
        StorageOp::DeleteFile,
        StorageReply::error(500, "provider-secret-detail"),
    );
    h.storage.push(
        StorageOp::DeleteFile,
        StorageReply::error(503, "provider-secret-detail"),
    );

    assert_eq!(h.delete(chat, id).await.status.as_u16(), 204);
    let row = h.wait_cleanup(id, "done").await;
    assert_eq!(
        row.cleanup_attempts, 2,
        "two failed attempts were recorded before the success"
    );
    assert_eq!(h.storage.count(StorageOp::DeleteFile), 3);
}

#[tokio::test]
async fn a_message_that_never_succeeds_is_dead_lettered_and_the_row_becomes_failed() {
    let h = AttachmentHarness::with_config(max_attempts(2)).await;
    let chat = h.seed_chat().await;
    let id = h.doc(chat, "a.pdf").await;
    h.storage
        .set_default(StorageOp::DeleteFile, StorageReply::error(500, "boom"));

    assert_eq!(h.delete(chat, id).await.status.as_u16(), 204);
    let row = h.wait_cleanup(id, "failed").await;
    assert_eq!(row.cleanup_attempts, 2, "cleanup_worker.max_attempts");
    assert!(row.last_cleanup_error.is_some());
    eventually("the message is in the dead-letter store", || async {
        h.dead_letters(ATTACHMENT_CLEANUP_QUEUE).await == 1
    })
    .await;
    assert_eq!(
        h.storage.count(StorageOp::DeleteFile),
        2,
        "no attempt after the limit"
    );
    assert_eq!(
        h.get(chat, id).await.status.as_u16(),
        404,
        "the attachment stays deleted for the API"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// attachment cleanup handler: direct deliveries
// ═════════════════════════════════════════════════════════════════════════════

async fn pending_attachment(h: &AttachmentHarness, chat: Uuid) -> (Uuid, Value) {
    let id = h
        .seed(
            AttRow::document(chat, "a.pdf")
                .deleted()
                .cleanup("pending", 0),
        )
        .await;
    let payload = h.attachment_cleanup_payload(id, "attachment_deleted").await;
    (id, payload)
}

#[tokio::test]
async fn a_successful_delete_marks_the_cleanup_done() {
    let h = manual(json!({})).await;
    let chat = h.seed_chat().await;
    let (id, payload) = pending_attachment(&h, chat).await;
    let provider_file = h.provider_file_of(id).await;

    let result = h.deliver_attachment_cleanup(&payload, 0).await;
    assert!(is_ok(&result), "{result:?}");
    let row = h.row(id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("done"));
    assert!(row.cleanup_updated_at.is_some());
    assert_eq!(
        h.storage.calls_to(StorageOp::DeleteFile)[0].last_segment(),
        provider_file
    );
}

#[tokio::test]
async fn the_three_event_types_are_handled_alike() {
    let h = manual(json!({})).await;
    let chat = h.seed_chat().await;
    for event in [
        "attachment_deleted",
        "attachment_upload_abandoned",
        "attachment_indexing_failed",
    ] {
        let id = h
            .seed(
                AttRow::document(chat, "a.pdf")
                    .status("failed")
                    .cleanup("pending", 0),
            )
            .await;
        let payload = h.attachment_cleanup_payload(id, event).await;
        let result = h.deliver_attachment_cleanup(&payload, 0).await;
        assert!(is_ok(&result), "{event}: {result:?}");
        assert_eq!(
            h.row(id).await.cleanup_status.as_deref(),
            Some("done"),
            "{event}"
        );
    }
    assert_eq!(h.storage.count(StorageOp::DeleteFile), 3);
}

#[tokio::test]
async fn a_provider_404_counts_as_success() {
    let h = manual(json!({})).await;
    let chat = h.seed_chat().await;
    let (id, payload) = pending_attachment(&h, chat).await;
    h.storage
        .push(StorageOp::DeleteFile, StorageReply::not_found());

    let result = h.deliver_attachment_cleanup(&payload, 0).await;
    assert!(is_ok(&result), "{result:?}");
    let row = h.row(id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("done"));
    assert_eq!(row.cleanup_attempts, 0);
}

#[tokio::test]
async fn any_other_status_is_a_failed_attempt_that_stays_pending_and_asks_for_a_retry() {
    for status in [400_u16, 403, 429, 500, 502] {
        let h = manual(max_attempts(5)).await;
        let chat = h.seed_chat().await;
        let (id, payload) = pending_attachment(&h, chat).await;
        h.storage.push(
            StorageOp::DeleteFile,
            StorageReply::error(status, "provider-secret-detail"),
        );

        let result = h.deliver_attachment_cleanup(&payload, 0).await;
        assert!(is_retry(&result), "{status}: {result:?}");
        let row = h.row(id).await;
        assert_eq!(row.cleanup_status.as_deref(), Some("pending"), "{status}");
        assert_eq!(row.cleanup_attempts, 1, "{status}");
        assert!(row.last_cleanup_error.is_some(), "{status}");
        assert!(row.cleanup_updated_at.is_some(), "{status}");
    }
}

#[tokio::test]
async fn a_gateway_failure_is_a_retry_and_the_row_stays_pending() {
    let h = manual(max_attempts(5)).await;
    let chat = h.seed_chat().await;
    let (id, payload) = pending_attachment(&h, chat).await;
    h.storage
        .push(StorageOp::DeleteFile, StorageReply::Unavailable);

    let result = h.deliver_attachment_cleanup(&payload, 0).await;
    assert!(is_retry(&result), "{result:?}");
    assert_eq!(h.row(id).await.cleanup_status.as_deref(), Some("pending"));
}

#[tokio::test]
async fn failed_attempts_accumulate_and_the_last_one_is_terminal() {
    let h = manual(max_attempts(3)).await;
    let chat = h.seed_chat().await;
    let (id, payload) = pending_attachment(&h, chat).await;
    h.storage
        .set_default(StorageOp::DeleteFile, StorageReply::error(500, "boom"));

    let first = h.deliver_attachment_cleanup(&payload, 0).await;
    assert!(is_retry(&first), "{first:?}");
    assert_eq!(h.row(id).await.cleanup_attempts, 1);
    let second = h.deliver_attachment_cleanup(&payload, 1).await;
    assert!(is_retry(&second), "{second:?}");
    assert_eq!(h.row(id).await.cleanup_attempts, 2);

    let third = h.deliver_attachment_cleanup(&payload, 2).await;
    assert!(
        rejected(&third).is_some(),
        "the delivery that reaches the limit is rejected: {third:?}"
    );
    let row = h.row(id).await;
    assert_eq!(
        row.cleanup_status.as_deref(),
        Some("failed"),
        "terminal unresolved provider-file debt"
    );
    assert_eq!(row.cleanup_attempts, 3);
    assert!(row.last_cleanup_error.is_some());
    assert_eq!(h.storage.count(StorageOp::DeleteFile), 3);
}

#[tokio::test]
async fn a_success_after_failed_attempts_ends_in_done() {
    let h = manual(max_attempts(5)).await;
    let chat = h.seed_chat().await;
    let (id, payload) = pending_attachment(&h, chat).await;
    h.storage
        .push(StorageOp::DeleteFile, StorageReply::error(500, "boom"));

    assert!(is_retry(&h.deliver_attachment_cleanup(&payload, 0).await));
    assert!(is_ok(&h.deliver_attachment_cleanup(&payload, 1).await));
    let row = h.row(id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("done"));
    assert_eq!(row.cleanup_attempts, 1);
}

#[tokio::test]
async fn an_attachment_that_never_reached_the_provider_is_done_without_a_call() {
    let h = manual(json!({})).await;
    let chat = h.seed_chat().await;
    let id = h
        .seed(
            AttRow::document(chat, "a.pdf")
                .status("failed")
                .provider_file(None)
                .cleanup("pending", 0),
        )
        .await;
    let payload = h.attachment_cleanup_payload(id, "attachment_deleted").await;
    assert!(payload["provider_file_id"].is_null());

    let result = h.deliver_attachment_cleanup(&payload, 0).await;
    assert!(is_ok(&result), "{result:?}");
    assert_eq!(h.row(id).await.cleanup_status.as_deref(), Some("done"));
    h.storage.assert_no_calls();
}

#[tokio::test]
async fn the_message_of_a_soft_deleted_chat_is_acknowledged_without_action() {
    let h = manual(json!({})).await;
    let chat = h.seed_deleted_chat().await;
    let (id, payload) = pending_attachment(&h, chat).await;

    let result = h.deliver_attachment_cleanup(&payload, 0).await;
    assert!(
        is_ok(&result),
        "acked: the chat cleanup owns the files now: {result:?}"
    );
    h.storage.assert_no_calls();
    assert_eq!(
        h.row(id).await.cleanup_status.as_deref(),
        Some("pending"),
        "left for the chat cleanup"
    );
}

#[tokio::test]
async fn a_malformed_message_is_rejected_at_once_and_touches_nothing() {
    let h = manual(json!({})).await;
    let chat = h.seed_chat().await;
    let (id, mut payload) = pending_attachment(&h, chat).await;

    let mut bad_id = payload.clone();
    bad_id["attachment_id"] = json!("not-a-uuid");
    payload["event_type"] = json!("something_else");
    let candidates: Vec<Vec<u8>> = vec![
        b"this is not json".to_vec(),
        b"{}".to_vec(),
        b"[]".to_vec(),
        bad_id.to_string().into_bytes(),
        payload.to_string().into_bytes(),
    ];
    for raw in candidates {
        let result = h.deliver_attachment_cleanup_raw(&raw, 0).await;
        assert!(
            rejected(&result).is_some(),
            "{} must be rejected, got {result:?}",
            String::from_utf8_lossy(&raw)
        );
    }
    h.storage.assert_no_calls();
    let row = h.row(id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(
        row.cleanup_attempts, 0,
        "a malformed message is not an attempt"
    );
}

#[tokio::test]
async fn the_file_is_deleted_through_the_storage_backend_of_the_row() {
    let h = manual(json!({"providers": {"rag": {
        "kind": "openai_responses",
        "host": "rag.provider.test",
        "upstream_alias": "rag-alias",
        "storage_kind": "openai"
    }}}))
    .await;
    let chat = h.seed_chat().await;
    let id = h
        .seed(
            AttRow::document(chat, "a.pdf")
                .backend("rag")
                .deleted()
                .cleanup("pending", 0),
        )
        .await;
    let provider_file = h.provider_file_of(id).await;
    let payload = h.attachment_cleanup_payload(id, "attachment_deleted").await;
    assert_eq!(payload["storage_backend"], json!("rag"));

    assert!(is_ok(&h.deliver_attachment_cleanup(&payload, 0).await));
    let deletes = h.storage.calls_to(StorageOp::DeleteFile);
    assert_eq!(
        deletes[0].path(),
        format!("/rag-alias/v1/files/{provider_file}")
    );
}

#[tokio::test]
async fn the_secondary_copy_is_deleted_best_effort() {
    let h = manual(json!({})).await;
    let chat = h.seed_chat().await;
    let id = h
        .seed(
            AttRow::image(chat, "a.png")
                .secondary("file-anthropic-secondary-9")
                .deleted()
                .cleanup("pending", 0),
        )
        .await;
    let primary = h.provider_file_of(id).await;
    let payload = h.attachment_cleanup_payload(id, "attachment_deleted").await;

    // the secondary delete fails: the cleanup is still done
    h.storage.push(
        StorageOp::DeleteFile,
        StorageReply::Http(200, json!({"deleted": true})),
    );
    h.storage.push(
        StorageOp::DeleteFile,
        StorageReply::error(500, "secondary boom"),
    );
    let result = h.deliver_attachment_cleanup(&payload, 0).await;
    assert!(is_ok(&result), "{result:?}");
    let deletes = h.storage.calls_to(StorageOp::DeleteFile);
    assert_eq!(deletes.len(), 2, "primary then secondary");
    assert_eq!(deletes[0].last_segment(), primary);
    assert_eq!(deletes[1].last_segment(), "file-anthropic-secondary-9");
    let row = h.row(id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("done"));
    assert_eq!(
        row.cleanup_attempts, 0,
        "a secondary failure is not an attempt"
    );
}

#[tokio::test]
async fn a_failed_primary_delete_does_not_depend_on_the_secondary() {
    let h = manual(max_attempts(5)).await;
    let chat = h.seed_chat().await;
    let id = h
        .seed(
            AttRow::image(chat, "a.png")
                .secondary("file-anthropic-secondary-9")
                .deleted()
                .cleanup("pending", 0),
        )
        .await;
    let payload = h.attachment_cleanup_payload(id, "attachment_deleted").await;
    h.storage.push(
        StorageOp::DeleteFile,
        StorageReply::error(500, "primary boom"),
    );

    let result = h.deliver_attachment_cleanup(&payload, 0).await;
    assert!(is_retry(&result), "{result:?}");
    assert_eq!(h.row(id).await.cleanup_status.as_deref(), Some("pending"));
}

#[tokio::test]
async fn a_repeated_delivery_after_completion_is_harmless() {
    let h = manual(json!({})).await;
    let chat = h.seed_chat().await;
    let (id, payload) = pending_attachment(&h, chat).await;
    assert!(is_ok(&h.deliver_attachment_cleanup(&payload, 0).await));
    let done = h.row(id).await;
    assert!(
        is_ok(&h.deliver_attachment_cleanup(&payload, 0).await),
        "idempotent"
    );
    let again = h.row(id).await;
    assert_eq!(again.cleanup_status.as_deref(), Some("done"));
    assert_eq!(again.cleanup_attempts, done.cleanup_attempts);
}

// ═════════════════════════════════════════════════════════════════════════════
// chat cleanup: end to end
// ═════════════════════════════════════════════════════════════════════════════

/// A chat with two documents, a spreadsheet and an image (all ready, uploaded through the API).
async fn full_chat(h: &AttachmentHarness) -> (Uuid, Vec<Uuid>) {
    let chat = h.seed_chat().await;
    let ids = vec![
        h.doc(chat, "one.pdf").await,
        h.doc(chat, "two.pdf").await,
        h.xlsx(chat, "sheet.xlsx").await,
        h.image(chat, "photo.png").await,
    ];
    (chat, ids)
}

#[tokio::test]
async fn deleting_a_chat_deletes_every_provider_file_then_the_vector_store() {
    let h = AttachmentHarness::new().await;
    let (chat, ids) = full_chat(&h).await;
    let mut files = Vec::new();
    for id in &ids {
        files.push(h.provider_file_of(*id).await);
    }
    let store = h.vector_store_of(chat).await;
    let messages_before = h.messages_in(chat).await.len();

    assert_eq!(h.delete_chat(chat).await.status.as_u16(), 204);
    for id in &ids {
        let row = h.wait_cleanup(*id, "done").await;
        assert!(row.cleanup_updated_at.is_some());
        assert!(
            row.deleted_at.is_none(),
            "chat cleanup does not soft-delete attachment rows"
        );
    }
    eventually("the chat_vector_stores row is gone", || async {
        h.stores_of(chat).await.is_empty()
    })
    .await;

    let deletes = h.storage.calls_to(StorageOp::DeleteFile);
    let mut deleted: Vec<_> = deletes.iter().map(StorageCall::last_segment).collect();
    deleted.sort();
    files.sort();
    assert_eq!(deleted, files, "one delete per provider file");
    let store_deletes = h.storage.calls_to(StorageOp::DeleteVectorStore);
    assert_eq!(store_deletes.len(), 1);
    assert_eq!(
        store_deletes[0].path(),
        format!("/provider-alias/v1/vector_stores/{store}")
    );
    assert_eq!(
        h.storage.ops().last(),
        Some(&StorageOp::DeleteVectorStore),
        "the store is deleted after every file: {:?}",
        h.storage.ops()
    );
    assert_eq!(
        h.messages_in(chat).await.len(),
        messages_before,
        "soft-deleted rows are never purged"
    );
    assert!(h.chat.chat_row(chat).await.unwrap().deleted_at.is_some());
}

#[tokio::test]
async fn a_chat_without_provider_resources_needs_no_provider_call() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.say(chat, "hello").await;
    assert_eq!(h.delete_chat(chat).await.status.as_u16(), 204);
    tokio::time::sleep(Duration::from_millis(700)).await;
    h.storage.assert_no_calls();
    assert_eq!(h.dead_letters(CHAT_CLEANUP_QUEUE).await, 0);
}

#[tokio::test]
async fn a_failing_provider_is_retried_until_the_chat_is_fully_cleaned() {
    let h = AttachmentHarness::new().await;
    let (chat, ids) = full_chat(&h).await;
    // the first file delete and the first store delete fail
    h.storage
        .push(StorageOp::DeleteFile, StorageReply::error(500, "boom"));
    h.storage.push(
        StorageOp::DeleteVectorStore,
        StorageReply::error(500, "boom"),
    );

    assert_eq!(h.delete_chat(chat).await.status.as_u16(), 204);
    for id in &ids {
        h.wait_cleanup(*id, "done").await;
    }
    eventually("the store is deleted after the retries", || async {
        h.stores_of(chat).await.is_empty()
    })
    .await;
    assert!(
        h.storage.count(StorageOp::DeleteVectorStore) >= 2,
        "the failed store delete was retried"
    );
    assert_eq!(h.dead_letters(CHAT_CLEANUP_QUEUE).await, 0);
}

#[tokio::test]
async fn a_store_that_cannot_be_deleted_ends_in_the_dead_letter_store_with_its_row_kept() {
    let h = AttachmentHarness::with_config(max_attempts(2)).await;
    let (chat, ids) = full_chat(&h).await;
    h.storage.set_default(
        StorageOp::DeleteVectorStore,
        StorageReply::error(500, "boom"),
    );

    assert_eq!(h.delete_chat(chat).await.status.as_u16(), 204);
    for id in &ids {
        h.wait_cleanup(*id, "done").await;
    }
    eventually("the chat cleanup message is dead-lettered", || async {
        h.dead_letters(CHAT_CLEANUP_QUEUE).await == 1
    })
    .await;
    assert_eq!(
        h.stores_of(chat).await.len(),
        1,
        "the row is the durable marker of the open work"
    );
}

#[tokio::test]
async fn an_attachment_deleted_before_its_chat_is_cleaned_by_the_chat_cleanup() {
    let h = manual(json!({})).await;
    let chat = h.seed_chat().await;
    let id = h.doc(chat, "a.pdf").await;
    let provider_file = h.provider_file_of(id).await;
    assert_eq!(h.delete(chat, id).await.status.as_u16(), 204);
    let queued = h.attachment_log.wait_for(1).await.remove(0);
    assert_eq!(h.delete_chat(chat).await.status.as_u16(), 204);

    // the attachment message arrives after the chat was deleted: acked, no action
    let result = h.deliver_attachment_cleanup(&queued, 0).await;
    assert!(is_ok(&result), "{result:?}");
    assert_eq!(h.storage.count(StorageOp::DeleteFile), 0);
    assert_eq!(h.row(id).await.cleanup_status.as_deref(), Some("pending"));

    // the chat cleanup now owns the file
    let payload = h.chat_log.wait_for(1).await.remove(0);
    let result = h.deliver_chat_cleanup(&payload, 0).await;
    assert!(is_ok(&result), "{result:?}");
    assert_eq!(h.row(id).await.cleanup_status.as_deref(), Some("done"));
    assert_eq!(
        h.storage.calls_to(StorageOp::DeleteFile)[0].last_segment(),
        provider_file
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// chat cleanup handler: direct deliveries
// ═════════════════════════════════════════════════════════════════════════════

/// A soft-deleted chat with pending attachments and a vector store.
async fn deleted_chat_with(h: &AttachmentHarness, files: usize) -> (Uuid, Vec<Uuid>, Value) {
    let chat = h.seed_deleted_chat().await;
    h.seed_store(chat, "vs_seeded_store_0001", "openai").await;
    let mut ids = Vec::new();
    for i in 0..files {
        ids.push(
            h.seed(AttRow::document(chat, &format!("f{i}.pdf")).cleanup("pending", 0))
                .await,
        );
    }
    let payload = chat_cleanup_payload(chat, TENANT_A);
    (chat, ids, payload)
}

#[tokio::test]
async fn the_chat_cleanup_deletes_files_then_the_store_and_removes_the_store_row() {
    let h = manual(json!({})).await;
    let (chat, ids, payload) = deleted_chat_with(&h, 3).await;

    let result = h.deliver_chat_cleanup(&payload, 0).await;
    assert!(is_ok(&result), "{result:?}");
    for id in &ids {
        assert_eq!(h.row(*id).await.cleanup_status.as_deref(), Some("done"));
    }
    assert!(h.stores_of(chat).await.is_empty());
    assert_eq!(h.storage.count(StorageOp::DeleteFile), 3);
    assert_eq!(h.storage.ops().last(), Some(&StorageOp::DeleteVectorStore));
    assert_eq!(
        h.storage.calls_to(StorageOp::DeleteVectorStore)[0].path(),
        "/provider-alias/v1/vector_stores/vs_seeded_store_0001"
    );
}

#[tokio::test]
async fn provider_404s_count_as_success_for_files_and_store() {
    let h = manual(json!({})).await;
    let (chat, ids, payload) = deleted_chat_with(&h, 2).await;
    h.storage
        .set_default(StorageOp::DeleteFile, StorageReply::not_found());
    h.storage
        .set_default(StorageOp::DeleteVectorStore, StorageReply::not_found());

    let result = h.deliver_chat_cleanup(&payload, 0).await;
    assert!(is_ok(&result), "{result:?}");
    for id in &ids {
        assert_eq!(h.row(*id).await.cleanup_status.as_deref(), Some("done"));
    }
    assert!(
        h.stores_of(chat).await.is_empty(),
        "the row is deleted on a 404 as well"
    );
}

#[tokio::test]
async fn a_pending_attachment_blocks_the_vector_store_delete_and_asks_for_a_retry() {
    let h = manual(max_attempts(5)).await;
    let (chat, ids, payload) = deleted_chat_with(&h, 2).await;
    h.storage
        .push(StorageOp::DeleteFile, StorageReply::error(500, "boom"));

    let result = h.deliver_chat_cleanup(&payload, 0).await;
    assert!(is_retry(&result), "{result:?}");
    let statuses: Vec<_> = {
        let mut out = Vec::new();
        for id in &ids {
            let row = h.row(*id).await;
            out.push((row.cleanup_status.clone(), row.cleanup_attempts));
        }
        out
    };
    assert_eq!(
        statuses
            .iter()
            .filter(|(s, _)| s.as_deref() == Some("done"))
            .count(),
        1,
        "the other attachment is still cleaned: {statuses:?}"
    );
    let pending = statuses
        .iter()
        .find(|(s, _)| s.as_deref() == Some("pending"))
        .unwrap();
    assert_eq!(pending.1, 1, "one failed attempt recorded");
    assert_eq!(
        h.storage.count(StorageOp::DeleteVectorStore),
        0,
        "never while an attachment is pending"
    );
    assert_eq!(h.stores_of(chat).await.len(), 1);

    // the next delivery finishes the job
    let result = h.deliver_chat_cleanup(&payload, 1).await;
    assert!(is_ok(&result), "{result:?}");
    assert!(h.stores_of(chat).await.is_empty());
    assert_eq!(
        h.storage.count(StorageOp::DeleteFile),
        3,
        "the done attachment is not deleted twice"
    );
}

#[tokio::test]
async fn a_failed_attachment_does_not_block_the_store_delete() {
    let h = manual(max_attempts(2)).await;
    let (chat, ids, payload) = deleted_chat_with(&h, 1).await;
    h.storage
        .set_default(StorageOp::DeleteFile, StorageReply::error(500, "boom"));

    assert!(is_retry(&h.deliver_chat_cleanup(&payload, 0).await));
    assert_eq!(
        h.row(ids[0]).await.cleanup_status.as_deref(),
        Some("pending")
    );
    assert_eq!(h.storage.count(StorageOp::DeleteVectorStore), 0);

    // the second attempt reaches cleanup_worker.max_attempts: the attachment is terminally failed,
    // the handler goes on and deletes the store
    let result = h.deliver_chat_cleanup(&payload, 1).await;
    assert!(is_ok(&result), "{result:?}");
    let row = h.row(ids[0]).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(row.cleanup_attempts, 2);
    assert!(row.last_cleanup_error.is_some());
    assert!(
        h.stores_of(chat).await.is_empty(),
        "the store is no longer retained as a blocking resource"
    );
}

#[tokio::test]
async fn a_failed_store_delete_keeps_the_row_and_is_retried() {
    let h = manual(max_attempts(5)).await;
    let (chat, ids, payload) = deleted_chat_with(&h, 1).await;
    h.storage.push(
        StorageOp::DeleteVectorStore,
        StorageReply::error(500, "boom"),
    );

    let result = h.deliver_chat_cleanup(&payload, 0).await;
    assert!(is_retry(&result), "{result:?}");
    assert_eq!(
        h.stores_of(chat).await.len(),
        1,
        "the row stays: it marks the open work"
    );
    assert_eq!(h.row(ids[0]).await.cleanup_status.as_deref(), Some("done"));

    let result = h.deliver_chat_cleanup(&payload, 1).await;
    assert!(is_ok(&result), "{result:?}");
    assert!(h.stores_of(chat).await.is_empty());
    assert_eq!(
        h.storage.count(StorageOp::DeleteFile),
        1,
        "the file is not deleted again"
    );
}

#[tokio::test]
async fn the_delivery_that_reaches_the_limit_rejects_and_keeps_the_store_row() {
    let h = manual(max_attempts(3)).await;
    let (chat, _ids, payload) = deleted_chat_with(&h, 0).await;
    h.storage.set_default(
        StorageOp::DeleteVectorStore,
        StorageReply::error(500, "boom"),
    );

    assert!(is_retry(&h.deliver_chat_cleanup(&payload, 0).await));
    assert!(is_retry(&h.deliver_chat_cleanup(&payload, 1).await));
    let third = h.deliver_chat_cleanup(&payload, 2).await;
    let reason =
        rejected(&third).unwrap_or_else(|| panic!("the third delivery is rejected: {third:?}"));
    assert!(reason.contains("max attempts"), "{reason}");
    assert_eq!(
        h.stores_of(chat).await.len(),
        1,
        "a dead-letter replay retries the delete"
    );
}

#[tokio::test]
async fn every_delivery_counts_toward_the_limit_including_those_that_waited_for_attachments() {
    let h = manual(max_attempts(3)).await;
    let (chat, ids, payload) = deleted_chat_with(&h, 1).await;
    h.storage
        .set_default(StorageOp::DeleteFile, StorageReply::error(500, "boom"));
    h.storage.set_default(
        StorageOp::DeleteVectorStore,
        StorageReply::error(500, "boom"),
    );

    assert!(
        is_retry(&h.deliver_chat_cleanup(&payload, 0).await),
        "attachment attempt 1"
    );
    assert!(
        is_retry(&h.deliver_chat_cleanup(&payload, 1).await),
        "attachment attempt 2"
    );
    // delivery 3: the attachment reaches its limit (failed), the store delete fails, 3 = max_attempts
    let result = h.deliver_chat_cleanup(&payload, 2).await;
    assert!(rejected(&result).is_some(), "{result:?}");
    assert_eq!(
        h.row(ids[0]).await.cleanup_status.as_deref(),
        Some("failed")
    );
    assert_eq!(h.stores_of(chat).await.len(), 1);
}

#[tokio::test]
async fn an_attachment_without_a_provider_file_needs_no_delete() {
    let h = manual(json!({})).await;
    let chat = h.seed_deleted_chat().await;
    let id = h
        .seed(
            AttRow::document(chat, "a.pdf")
                .status("pending")
                .provider_file(None)
                .cleanup("pending", 0),
        )
        .await;
    let result = h
        .deliver_chat_cleanup(&chat_cleanup_payload(chat, TENANT_A), 0)
        .await;
    assert!(is_ok(&result), "{result:?}");
    assert_eq!(h.row(id).await.cleanup_status.as_deref(), Some("done"));
    h.storage.assert_no_calls();
}

#[tokio::test]
async fn a_user_deleted_attachment_pending_when_the_chat_is_deleted_is_handled_too() {
    let h = manual(json!({})).await;
    let chat = h.seed_deleted_chat().await;
    let id = h
        .seed(
            AttRow::document(chat, "a.pdf")
                .deleted()
                .cleanup("pending", 1),
        )
        .await;
    let result = h
        .deliver_chat_cleanup(&chat_cleanup_payload(chat, TENANT_A), 0)
        .await;
    assert!(is_ok(&result), "{result:?}");
    assert_eq!(h.row(id).await.cleanup_status.as_deref(), Some("done"));
    assert_eq!(h.storage.count(StorageOp::DeleteFile), 1);
}

#[tokio::test]
async fn the_secondary_copy_of_an_image_is_deleted_with_the_chat() {
    let h = manual(json!({})).await;
    let chat = h.seed_deleted_chat().await;
    let id = h
        .seed(
            AttRow::image(chat, "a.png")
                .secondary("file-anthropic-secondary-7")
                .cleanup("pending", 0),
        )
        .await;
    let primary = h.provider_file_of(id).await;
    assert!(is_ok(
        &h.deliver_chat_cleanup(&chat_cleanup_payload(chat, TENANT_A), 0)
            .await
    ));
    let deleted: Vec<_> = h
        .storage
        .calls_to(StorageOp::DeleteFile)
        .iter()
        .map(StorageCall::last_segment)
        .collect();
    assert_eq!(
        deleted,
        vec![primary, "file-anthropic-secondary-7".to_owned()]
    );
}

#[tokio::test]
async fn done_and_failed_attachments_are_left_alone_and_other_chats_are_untouched() {
    let h = manual(json!({})).await;
    let chat = h.seed_deleted_chat().await;
    let other = h.seed_chat().await;
    let done = h
        .seed(AttRow::document(chat, "done.pdf").cleanup("done", 0))
        .await;
    let failed = h
        .seed(AttRow::document(chat, "failed.pdf").cleanup("failed", 5))
        .await;
    let foreign = h.seed(AttRow::document(other, "other.pdf")).await;
    let foreign_pending = h
        .seed(AttRow::document(other, "x.pdf").cleanup("pending", 0))
        .await;

    let result = h
        .deliver_chat_cleanup(&chat_cleanup_payload(chat, TENANT_A), 0)
        .await;
    assert!(is_ok(&result), "{result:?}");
    h.storage.assert_no_calls();
    assert_eq!(h.row(done).await.cleanup_status.as_deref(), Some("done"));
    assert_eq!(
        h.row(failed).await.cleanup_status.as_deref(),
        Some("failed")
    );
    assert!(h.row(foreign).await.cleanup_status.is_none());
    assert_eq!(
        h.row(foreign_pending).await.cleanup_status.as_deref(),
        Some("pending")
    );
}

#[tokio::test]
async fn the_cleanup_of_one_chat_never_touches_the_store_of_another() {
    let h = manual(json!({})).await;
    let (_chat, _ids, payload) = deleted_chat_with(&h, 1).await;
    let other = h.seed_chat().await;
    h.seed_store(other, "vs_other_chat_store_01", "openai")
        .await;

    assert!(is_ok(&h.deliver_chat_cleanup(&payload, 0).await));
    assert_eq!(h.stores_of(other).await.len(), 1);
    let deleted: Vec<_> = h
        .storage
        .calls_to(StorageOp::DeleteVectorStore)
        .iter()
        .map(StorageCall::last_segment)
        .collect();
    assert_eq!(deleted, vec!["vs_seeded_store_0001".to_owned()]);
}

#[tokio::test]
async fn an_active_chat_is_rejected_and_nothing_is_deleted() {
    let h = manual(json!({})).await;
    let chat = h.seed_chat().await;
    h.seed_store(chat, "vs_active_chat_store_1", "openai").await;
    let id = h
        .seed(AttRow::document(chat, "a.pdf").cleanup("pending", 0))
        .await;

    let result = h
        .deliver_chat_cleanup(&chat_cleanup_payload(chat, TENANT_A), 0)
        .await;
    let reason = rejected(&result).unwrap_or_else(|| panic!("rejected: {result:?}"));
    assert!(reason.contains("not soft-deleted"), "{reason}");
    h.storage.assert_no_calls();
    assert_eq!(h.stores_of(chat).await.len(), 1);
    assert_eq!(h.row(id).await.cleanup_status.as_deref(), Some("pending"));
}

#[tokio::test]
async fn a_malformed_chat_cleanup_message_is_rejected_at_once() {
    let h = manual(json!({})).await;
    let (chat, _ids, mut payload) = deleted_chat_with(&h, 1).await;
    payload["chat_id"] = json!("not-a-uuid");
    for raw in [
        b"not json".to_vec(),
        b"{}".to_vec(),
        payload.to_string().into_bytes(),
    ] {
        let result = h.deliver_chat_cleanup_raw(&raw, 0).await;
        assert!(
            rejected(&result).is_some(),
            "{} -> {result:?}",
            String::from_utf8_lossy(&raw)
        );
    }
    h.storage.assert_no_calls();
    assert_eq!(h.stores_of(chat).await.len(), 1);
}

#[tokio::test]
async fn a_repeated_chat_cleanup_delivery_after_completion_is_harmless() {
    let h = manual(json!({})).await;
    let (chat, ids, payload) = deleted_chat_with(&h, 2).await;
    assert!(is_ok(&h.deliver_chat_cleanup(&payload, 0).await));
    let calls = h.storage.calls().len();

    let again = h.deliver_chat_cleanup(&payload, 1).await;
    assert!(is_ok(&again), "{again:?}");
    assert_eq!(h.storage.calls().len(), calls, "nothing is deleted twice");
    for id in &ids {
        assert_eq!(h.row(*id).await.cleanup_status.as_deref(), Some("done"));
    }
    assert!(h.stores_of(chat).await.is_empty());
}

#[tokio::test]
async fn the_chat_delete_marks_the_attachments_that_the_chat_cleanup_then_processes() {
    let h = manual(json!({})).await;
    let chat = h.seed_chat().await;
    let doc = h.doc(chat, "a.pdf").await;
    let image = h.image(chat, "p.png").await;
    let sheet = h.xlsx(chat, "s.xlsx").await;

    assert_eq!(h.delete_chat(chat).await.status.as_u16(), 204);
    for id in [doc, image, sheet] {
        assert_eq!(h.row(id).await.cleanup_status.as_deref(), Some("pending"));
    }
    let payload = h.chat_log.wait_for(1).await.remove(0);
    assert_eq!(payload["chat_id"], json!(chat.to_string()));
    assert_eq!(payload["reason"], json!("chat_soft_delete"));
    assert!(is_ok(&h.deliver_chat_cleanup(&payload, 0).await));
    for id in [doc, image, sheet] {
        assert_eq!(h.row(id).await.cleanup_status.as_deref(), Some("done"));
    }
    assert!(h.stores_of(chat).await.is_empty());
}
