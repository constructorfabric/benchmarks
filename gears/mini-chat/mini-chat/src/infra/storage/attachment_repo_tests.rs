//! `SQLite` repository tests of the attachment persistence port: the limits-and-insert
//! transaction (live, non-failed rows only), the guarded status writes, the get by uploader, the
//! background heartbeat and guarded `ready` write, and the failure transaction with its cleanup
//! message.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`;
//! algorithms `cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits`,
//! `-provider-file-upload`, `-upload-failure`, `-secondary-copy`, `-background-indexing` and
//! `-cleanup-enqueue` (the `attachment_indexing_failed` message); states
//! `cpt-cf-mini-chat-state-attachments-and-file-search-attachment-status` and
//! `cpt-cf-mini-chat-state-attachments-and-file-search-secondary-copy`; definitions of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-limits-and-provider-upload` and
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-indexing-lifecycle`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::attachments::limits::ChatLimits;
use crate::domain::attachments::store::{
    AttachmentCleanupQueue, AttachmentStore, NewAttachment, SecondaryUpdate,
};
use crate::domain::attachments::{AttachmentKind, AttachmentStatus, Thumbnail};
use crate::domain::error::{AttachmentError, DomainError, ResourceKind};
use crate::infra::outbox::partition_for;
use crate::test_support::attachments::{
    RowSeed, attachment_cleanup_rows, seed_documents, seed_row,
};
use crate::test_support::chats::{at, fail_outbox_inserts};
use crate::test_support::quota::{QuotaRig, STANDARD, quota_rig};
use crate::test_support::{TENANT_A, TENANT_B, USER_A1, USER_A2};

const MIB: i64 = 1_048_576;

fn limits() -> ChatLimits {
    ChatLimits {
        max_documents: 50,
        max_total_bytes: 100 * MIB as u64,
    }
}

fn now() -> OffsetDateTime {
    at(1_000_000) + time::Duration::microseconds(123_000)
}

fn new_row(
    chat: &crate::infra::storage::repo_impl::ChatRecord,
    kind: AttachmentKind,
) -> NewAttachment {
    NewAttachment {
        id: Uuid::new_v4(),
        tenant_id: chat.tenant_id,
        chat_id: chat.id,
        uploaded_by_user_id: chat.user_id,
        filename: "report.pdf".to_owned(),
        content_type: "application/pdf".to_owned(),
        storage_backend: "azure_openai".to_owned(),
        kind,
        for_file_search: kind == AttachmentKind::Document,
        for_code_interpreter: false,
        now: now(),
    }
}

async fn rig() -> (QuotaRig, crate::infra::storage::repo_impl::ChatRecord) {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    (rig, chat)
}

fn store(rig: &QuotaRig) -> &dyn AttachmentStore {
    &*rig.db.provider
}

fn is_limit(
    result: &Result<crate::domain::attachments::AttachmentRecord, DomainError>,
    limit: AttachmentError,
) -> bool {
    matches!(result, Err(DomainError::Attachment(found)) if *found == limit)
}

// ── limits and insert ────────────────────────────────────────────────────────

#[tokio::test]
async fn the_insert_writes_a_pending_row_with_server_derived_ownership() {
    // inst-lim-insert
    let (rig, chat) = rig().await;
    let new = new_row(&chat, AttachmentKind::Document);
    let record = store(&rig).insert_pending(&new, &limits()).await.unwrap();
    assert_eq!(record.id, new.id);
    assert_eq!(record.status, AttachmentStatus::Pending);
    let stores = crate::test_support::attachments::vector_store_rows(&rig.db).await;
    assert!(
        stores.is_empty(),
        "an attachment insert never creates a vector store row"
    );
    let rows = crate::test_support::chats::attachment_rows(&rig.db, chat.id).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(
        (row.tenant_id, row.chat_id, row.uploaded_by_user_id),
        (TENANT_A, chat.id, USER_A1)
    );
    assert_eq!(row.filename, "report.pdf");
    assert_eq!(row.content_type.as_deref(), Some("application/pdf"));
    assert_eq!(row.size_bytes, Some(0));
    assert_eq!(row.storage_backend, "azure_openai");
    assert_eq!(row.status, "pending");
    assert_eq!(row.attachment_kind, "document");
    assert!(row.for_file_search && !row.for_code_interpreter);
    assert_eq!(row.secondary_status, "not_attempted");
    assert_eq!(row.cleanup_status, None);
    assert_eq!(row.cleanup_attempts, 0);
    assert_eq!(row.provider_file_id, None);
    assert_eq!(row.error_code, None);
    assert!(
        row.doc_summary.is_none()
            && row.summary_model.is_none()
            && row.summary_updated_at.is_none()
    );
    assert!(row.img_thumbnail.is_none());
    assert_eq!(row.created_at, now());
    assert_eq!(row.updated_at, now());
    assert_eq!(row.deleted_at, None);
}

#[tokio::test]
async fn the_fifty_first_live_document_is_rejected_with_the_document_limit() {
    // inst-lim-count-docs, inst-lim-doc-limit, inst-lim-return-doc-limit
    let (rig, chat) = rig().await;
    seed_documents(&rig.db, &chat, 49, 10).await;
    let fiftieth = store(&rig)
        .insert_pending(&new_row(&chat, AttachmentKind::Document), &limits())
        .await;
    assert!(fiftieth.is_ok(), "{fiftieth:?}");
    let fifty_first = store(&rig)
        .insert_pending(&new_row(&chat, AttachmentKind::Document), &limits())
        .await;
    assert!(
        is_limit(&fifty_first, AttachmentError::DocumentLimit),
        "{fifty_first:?}"
    );
    assert_eq!(
        crate::test_support::chats::attachment_rows(&rig.db, chat.id)
            .await
            .len(),
        50
    );
}

#[tokio::test]
async fn pending_and_uploaded_documents_count_toward_the_document_limit() {
    let (rig, chat) = rig().await;
    for _ in 0..25 {
        seed_row(&rig.db, &chat, RowSeed::document().status("pending")).await;
        seed_row(&rig.db, &chat, RowSeed::document().status("uploaded")).await;
    }
    let result = store(&rig)
        .insert_pending(&new_row(&chat, AttachmentKind::Document), &limits())
        .await;
    assert!(
        is_limit(&result, AttachmentError::DocumentLimit),
        "{result:?}"
    );
}

#[tokio::test]
async fn failed_and_deleted_documents_do_not_count_toward_the_document_limit() {
    let (rig, chat) = rig().await;
    for _ in 0..30 {
        seed_row(&rig.db, &chat, RowSeed::document().status("failed")).await;
    }
    for _ in 0..30 {
        seed_row(&rig.db, &chat, RowSeed::document().deleted(at(5))).await;
    }
    let result = store(&rig)
        .insert_pending(&new_row(&chat, AttachmentKind::Document), &limits())
        .await;
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn images_never_count_toward_the_document_limit_and_are_never_refused_by_it() {
    // images count toward the storage limit only
    let (rig, chat) = rig().await;
    for _ in 0..50 {
        seed_row(&rig.db, &chat, RowSeed::image().size(10)).await;
    }
    let document = store(&rig)
        .insert_pending(&new_row(&chat, AttachmentKind::Document), &limits())
        .await;
    assert!(
        document.is_ok(),
        "50 images must not block a document: {document:?}"
    );
    seed_documents(&rig.db, &chat, 49, 10).await;
    let image = store(&rig)
        .insert_pending(&new_row(&chat, AttachmentKind::Image), &limits())
        .await;
    assert!(
        image.is_ok(),
        "the document limit refused an image: {image:?}"
    );
}

#[tokio::test]
async fn an_upload_at_the_storage_limit_is_rejected_with_the_storage_limit() {
    // inst-lim-sum-size, inst-lim-storage-limit, inst-lim-return-storage-limit
    let (rig, chat) = rig().await;
    seed_row(&rig.db, &chat, RowSeed::document().size(100 * MIB - 1)).await;
    let under = store(&rig)
        .insert_pending(&new_row(&chat, AttachmentKind::Document), &limits())
        .await;
    assert!(under.is_ok(), "{under:?}");
    seed_row(&rig.db, &chat, RowSeed::document().size(1)).await;
    let at_limit = store(&rig)
        .insert_pending(&new_row(&chat, AttachmentKind::Document), &limits())
        .await;
    assert!(
        is_limit(&at_limit, AttachmentError::StorageLimit),
        "{at_limit:?}"
    );
}

#[tokio::test]
async fn images_count_toward_the_storage_sum_and_an_image_can_hit_the_storage_limit() {
    let (rig, chat) = rig().await;
    seed_row(&rig.db, &chat, RowSeed::image().size(100 * MIB)).await;
    for kind in [AttachmentKind::Document, AttachmentKind::Image] {
        let result = store(&rig)
            .insert_pending(&new_row(&chat, kind), &limits())
            .await;
        assert!(
            is_limit(&result, AttachmentError::StorageLimit),
            "{kind:?}: {result:?}"
        );
    }
}

#[tokio::test]
async fn failed_and_deleted_rows_and_other_chats_do_not_count_toward_the_storage_sum() {
    let (rig, chat) = rig().await;
    let other_chat = rig.chat_a1(STANDARD).await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document().status("failed").size(200 * MIB),
    )
    .await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document().deleted(at(3)).size(200 * MIB),
    )
    .await;
    seed_row(&rig.db, &other_chat, RowSeed::document().size(200 * MIB)).await;
    let result = store(&rig)
        .insert_pending(&new_row(&chat, AttachmentKind::Document), &limits())
        .await;
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn the_document_limit_is_checked_before_the_storage_limit() {
    // inst-lim-is-document precedes inst-lim-sum-size
    let (rig, chat) = rig().await;
    seed_documents(&rig.db, &chat, 50, 3 * MIB).await;
    let result = store(&rig)
        .insert_pending(&new_row(&chat, AttachmentKind::Document), &limits())
        .await;
    assert!(
        is_limit(&result, AttachmentError::DocumentLimit),
        "{result:?}"
    );
}

#[tokio::test]
async fn the_limits_are_the_ones_passed_in() {
    let (rig, chat) = rig().await;
    let tight = ChatLimits {
        max_documents: 2,
        max_total_bytes: 1000,
    };
    seed_documents(&rig.db, &chat, 2, 1).await;
    let result = store(&rig)
        .insert_pending(&new_row(&chat, AttachmentKind::Document), &tight)
        .await;
    assert!(
        is_limit(&result, AttachmentError::DocumentLimit),
        "{result:?}"
    );
    let image = store(&rig)
        .insert_pending(&new_row(&chat, AttachmentKind::Image), &tight)
        .await;
    assert!(image.is_ok(), "{image:?}");
}

#[tokio::test]
async fn a_rejected_insert_leaves_no_row_behind() {
    let (rig, chat) = rig().await;
    seed_row(&rig.db, &chat, RowSeed::document().size(100 * MIB)).await;
    let rejected = store(&rig)
        .insert_pending(&new_row(&chat, AttachmentKind::Document), &limits())
        .await;
    assert!(rejected.is_err());
    assert_eq!(
        crate::test_support::chats::attachment_rows(&rig.db, chat.id)
            .await
            .len(),
        1
    );
}

// ── provider upload writes ───────────────────────────────────────────────────

async fn pending_row(rig: &QuotaRig, chat: &crate::infra::storage::repo_impl::ChatRecord) -> Uuid {
    let new = new_row(chat, AttachmentKind::Document);
    store(rig).insert_pending(&new, &limits()).await.unwrap();
    new.id
}

#[tokio::test]
async fn mark_uploaded_stores_the_provider_file_id_and_the_byte_count() {
    // inst-pfu-set-uploaded
    let (rig, chat) = rig().await;
    let id = pending_row(&rig, &chat).await;
    let later = now() + time::Duration::seconds(2);
    store(&rig)
        .mark_uploaded(TENANT_A, id, "file-abc", 4096, later)
        .await
        .unwrap();
    let row = crate::test_support::chats::attachment_rows(&rig.db, chat.id)
        .await
        .remove(0);
    assert_eq!(row.status, "uploaded");
    assert_eq!(row.provider_file_id.as_deref(), Some("file-abc"));
    assert_eq!(row.size_bytes, Some(4096));
    assert_eq!(row.updated_at, later);
}

#[tokio::test]
async fn mark_uploaded_is_guarded_by_the_pending_status_and_the_tenant() {
    let (rig, chat) = rig().await;
    let id = pending_row(&rig, &chat).await;
    let wrong_tenant = store(&rig)
        .mark_uploaded(TENANT_B, id, "file-x", 1, now())
        .await;
    assert!(wrong_tenant.is_err());
    let row = crate::test_support::chats::attachment_rows(&rig.db, chat.id)
        .await
        .remove(0);
    assert_eq!(row.status, "pending");
    assert_eq!(row.provider_file_id, None);
    store(&rig)
        .mark_uploaded(TENANT_A, id, "file-1", 1, now())
        .await
        .unwrap();
    let second = store(&rig)
        .mark_uploaded(TENANT_A, id, "file-2", 2, now())
        .await;
    assert!(second.is_err(), "a row that left pending was updated again");
    let row = crate::test_support::chats::attachment_rows(&rig.db, chat.id)
        .await
        .remove(0);
    assert_eq!(row.provider_file_id.as_deref(), Some("file-1"));
}

#[tokio::test]
async fn the_recheck_counts_the_other_live_rows_plus_the_final_byte_count() {
    // inst-lim-recheck-sum
    let (rig, chat) = rig().await;
    let id = pending_row(&rig, &chat).await;
    seed_row(&rig.db, &chat, RowSeed::document().size(60 * MIB)).await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document().status("failed").size(500 * MIB),
    )
    .await;
    seed_row(
        &rig.db,
        &chat,
        RowSeed::document().deleted(at(1)).size(500 * MIB),
    )
    .await;
    let ok = store(&rig)
        .storage_recheck(TENANT_A, chat.id, id, 40 * MIB, &limits())
        .await
        .unwrap();
    assert!(ok, "60 MiB + 40 MiB is within the limit");
    let over = store(&rig)
        .storage_recheck(TENANT_A, chat.id, id, 40 * MIB + 1, &limits())
        .await
        .unwrap();
    assert!(!over, "one byte above the limit passed");
}

#[tokio::test]
async fn the_recheck_does_not_count_the_row_being_checked() {
    let (rig, chat) = rig().await;
    let id = pending_row(&rig, &chat).await;
    store(&rig)
        .mark_uploaded(TENANT_A, id, "file-1", 90 * MIB, now())
        .await
        .unwrap();
    let ok = store(&rig)
        .storage_recheck(TENANT_A, chat.id, id, 90 * MIB, &limits())
        .await
        .unwrap();
    assert!(ok, "the row counted itself");
}

// ── failure, secondary, ready ────────────────────────────────────────────────

#[tokio::test]
async fn failing_a_pending_row_reports_the_pending_status() {
    // inst-uf-mark-failed: a row that was pending leaves pending here
    let (rig, chat) = rig().await;
    let id = pending_row(&rig, &chat).await;
    let prior = store(&rig)
        .mark_failed(TENANT_A, id, "file_too_large", now())
        .await
        .unwrap();
    assert_eq!(prior, Some(AttachmentStatus::Pending));
    let row = crate::test_support::chats::attachment_rows(&rig.db, chat.id)
        .await
        .remove(0);
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("file_too_large"));
    assert_eq!(row.updated_at, now());
}

#[tokio::test]
async fn failing_an_uploaded_row_reports_the_uploaded_status() {
    // a row that already left pending at inst-pfu-set-uploaded is not decremented again
    let (rig, chat) = rig().await;
    let id = pending_row(&rig, &chat).await;
    store(&rig)
        .mark_uploaded(TENANT_A, id, "file-1", 10, now())
        .await
        .unwrap();
    let prior = store(&rig)
        .mark_failed(TENANT_A, id, "indexing_failed", now())
        .await
        .unwrap();
    assert_eq!(prior, Some(AttachmentStatus::Uploaded));
    let row = crate::test_support::chats::attachment_rows(&rig.db, chat.id)
        .await
        .remove(0);
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(
        row.provider_file_id.as_deref(),
        Some("file-1"),
        "the file id stays for cleanup"
    );
}

#[tokio::test]
async fn failing_is_guarded_by_pending_or_uploaded_and_the_tenant() {
    let (rig, chat) = rig().await;
    let ready = seed_row(&rig.db, &chat, RowSeed::document().status("ready")).await;
    let failed = seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .status("failed")
            .error("upload_abandoned"),
    )
    .await;
    let pending = pending_row(&rig, &chat).await;
    assert_eq!(
        store(&rig)
            .mark_failed(TENANT_A, ready, "x", now())
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store(&rig)
            .mark_failed(TENANT_A, failed, "x", now())
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store(&rig)
            .mark_failed(TENANT_B, pending, "x", now())
            .await
            .unwrap(),
        None
    );
    let rows = crate::test_support::chats::attachment_rows(&rig.db, chat.id).await;
    let by_id = |id| rows.iter().find(|row| row.id == id).unwrap();
    assert_eq!(by_id(ready).status, "ready");
    assert_eq!(
        by_id(failed).error_code.as_deref(),
        Some("upload_abandoned")
    );
    assert_eq!(by_id(pending).status, "pending");
}

#[tokio::test]
async fn the_secondary_copy_follows_not_attempted_pending_uploaded_or_failed() {
    // inst-sec-pending, inst-sec-set-uploaded, inst-sec-set-failed
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::image().status("uploaded")).await;
    let row = || async {
        crate::test_support::chats::attachment_rows(&rig.db, chat.id)
            .await
            .remove(0)
    };
    assert_eq!(row().await.secondary_status, "not_attempted");
    store(&rig)
        .set_secondary(TENANT_A, id, &SecondaryUpdate::Pending, now())
        .await
        .unwrap();
    assert_eq!(row().await.secondary_status, "pending");
    store(&rig)
        .set_secondary(
            TENANT_A,
            id,
            &SecondaryUpdate::Uploaded {
                file_id: "file_ant_7".to_owned(),
            },
            now(),
        )
        .await
        .unwrap();
    let uploaded = row().await;
    assert_eq!(uploaded.secondary_status, "uploaded");
    assert_eq!(uploaded.secondary_file_id.as_deref(), Some("file_ant_7"));
    assert_eq!(
        uploaded.secondary_provider_kind.as_deref(),
        Some("anthropic")
    );
    // a failed copy never touches the attachment status
    assert_eq!(uploaded.status, "uploaded");
}

#[tokio::test]
async fn a_failed_secondary_upload_marks_only_the_secondary_status() {
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::image().status("uploaded")).await;
    store(&rig)
        .set_secondary(TENANT_A, id, &SecondaryUpdate::Pending, now())
        .await
        .unwrap();
    store(&rig)
        .set_secondary(TENANT_A, id, &SecondaryUpdate::Failed, now())
        .await
        .unwrap();
    let row = crate::test_support::chats::attachment_rows(&rig.db, chat.id)
        .await
        .remove(0);
    assert_eq!(row.secondary_status, "failed");
    assert_eq!(row.secondary_file_id, None);
    assert_eq!(row.status, "uploaded");
}

#[tokio::test]
async fn mark_ready_stores_the_thumbnail_columns() {
    // inst-img-set-ready
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::image().status("uploaded")).await;
    let thumbnail = Thumbnail {
        bytes: vec![9, 8, 7, 6],
        width: 128,
        height: 64,
    };
    store(&rig)
        .mark_ready(TENANT_A, id, Some(&thumbnail), now())
        .await
        .unwrap();
    let row = crate::test_support::chats::attachment_rows(&rig.db, chat.id)
        .await
        .remove(0);
    assert_eq!(row.status, "ready");
    assert_eq!(row.img_thumbnail, Some(vec![9, 8, 7, 6]));
    assert_eq!(
        (row.img_thumbnail_width, row.img_thumbnail_height),
        (Some(128), Some(64))
    );
    assert_eq!(row.error_code, None);
    assert_eq!(row.updated_at, now());
}

#[tokio::test]
async fn mark_ready_without_a_thumbnail_leaves_the_columns_null() {
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::image().status("uploaded")).await;
    store(&rig)
        .mark_ready(TENANT_A, id, None, now())
        .await
        .unwrap();
    let row = crate::test_support::chats::attachment_rows(&rig.db, chat.id)
        .await
        .remove(0);
    assert_eq!(row.status, "ready");
    assert!(row.img_thumbnail.is_none());
    assert!(row.img_thumbnail_width.is_none() && row.img_thumbnail_height.is_none());
}

#[tokio::test]
async fn mark_ready_never_touches_another_tenants_row() {
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().status("uploaded")).await;
    drop(store(&rig).mark_ready(TENANT_B, id, None, now()).await);
    let row = crate::test_support::chats::attachment_rows(&rig.db, chat.id)
        .await
        .remove(0);
    assert_eq!(row.status, "uploaded");
}

// ── get ──────────────────────────────────────────────────────────────────────

fn not_found(result: &Result<crate::domain::attachments::AttachmentRecord, DomainError>) -> bool {
    matches!(
        result,
        Err(DomainError::NotFound {
            resource: ResourceKind::Attachment,
            ..
        })
    )
}

#[tokio::test]
async fn get_returns_the_row_of_the_uploader() {
    // inst-get-select
    let (rig, chat) = rig().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::image().thumbnail(128, 64, &[1, 2, 3]).size(777),
    )
    .await;
    let record = store(&rig)
        .get(TENANT_A, chat.id, id, USER_A1)
        .await
        .unwrap();
    assert_eq!(record.id, id);
    assert_eq!(record.kind, AttachmentKind::Image);
    assert_eq!(record.status, AttachmentStatus::Ready);
    assert_eq!(record.size_bytes, 777);
    assert_eq!(record.content_type, "image/png");
    let thumbnail = record.thumbnail.expect("thumbnail");
    assert_eq!(
        (thumbnail.width, thumbnail.height, thumbnail.bytes),
        (128, 64, vec![1, 2, 3])
    );
}

#[tokio::test]
async fn get_hides_unknown_deleted_foreign_chat_and_foreign_uploader_rows_identically() {
    // inst-get-missing: the same 404 for all four cases (plus the foreign tenant)
    let (rig, chat) = rig().await;
    let other_chat = rig.chat_a1(STANDARD).await;
    let mine = seed_row(&rig.db, &chat, RowSeed::document()).await;
    let deleted = seed_row(&rig.db, &chat, RowSeed::document().deleted(at(2))).await;
    let colleague = seed_row(&rig.db, &chat, RowSeed::document().uploader(USER_A2)).await;
    assert!(not_found(
        &store(&rig)
            .get(TENANT_A, chat.id, Uuid::new_v4(), USER_A1)
            .await
    ));
    assert!(not_found(
        &store(&rig).get(TENANT_A, chat.id, deleted, USER_A1).await
    ));
    assert!(not_found(
        &store(&rig)
            .get(TENANT_A, other_chat.id, mine, USER_A1)
            .await
    ));
    assert!(not_found(
        &store(&rig).get(TENANT_A, chat.id, colleague, USER_A1).await
    ));
    assert!(not_found(
        &store(&rig).get(TENANT_B, chat.id, mine, USER_A1).await
    ));
}

#[tokio::test]
async fn get_returns_a_failed_row_with_its_error_code() {
    let (rig, chat) = rig().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .status("failed")
            .error("indexing_failed"),
    )
    .await;
    let record = store(&rig)
        .get(TENANT_A, chat.id, id, USER_A1)
        .await
        .unwrap();
    assert_eq!(record.status, AttachmentStatus::Failed);
    assert_eq!(record.error_code.as_deref(), Some("indexing_failed"));
}

// ── heartbeat and guarded ready ──────────────────────────────────────────────

#[tokio::test]
async fn the_heartbeat_refreshes_updated_at_of_an_uploaded_live_row() {
    // inst-bg-heartbeat
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().status("uploaded")).await;
    assert!(store(&rig).heartbeat(TENANT_A, id, now()).await.unwrap());
    let row = crate::test_support::chats::attachment_rows(&rig.db, chat.id)
        .await
        .remove(0);
    assert_eq!(row.updated_at, now());
    assert_eq!(row.status, "uploaded");
}

#[tokio::test]
async fn the_heartbeat_keeps_sub_second_precision_for_the_reaper_cutoff() {
    // implementation note 1: SQLite timestamps are variable-length text
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().status("uploaded")).await;
    for micros in [0_i64, 120_000, 123_000, 999_999] {
        let stamp = at(2_000_000) + time::Duration::microseconds(micros);
        assert!(store(&rig).heartbeat(TENANT_A, id, stamp).await.unwrap());
        let row = crate::test_support::chats::attachment_rows(&rig.db, chat.id)
            .await
            .remove(0);
        assert_eq!(row.updated_at, stamp, "{micros} microseconds");
    }
}

async fn guard_cases(
    rig: &QuotaRig,
    chat: &crate::infra::storage::repo_impl::ChatRecord,
) -> Vec<(&'static str, Uuid)> {
    vec![
        (
            "pending",
            seed_row(&rig.db, chat, RowSeed::document().status("pending")).await,
        ),
        (
            "ready",
            seed_row(&rig.db, chat, RowSeed::document().status("ready")).await,
        ),
        (
            "failed",
            seed_row(&rig.db, chat, RowSeed::document().status("failed")).await,
        ),
        (
            "deleted",
            seed_row(
                &rig.db,
                chat,
                RowSeed::document().status("uploaded").deleted(at(1)),
            )
            .await,
        ),
        (
            "cleanup pending",
            seed_row(
                &rig.db,
                chat,
                RowSeed::document().status("uploaded").cleanup("pending"),
            )
            .await,
        ),
        (
            "cleanup done",
            seed_row(
                &rig.db,
                chat,
                RowSeed::document().status("uploaded").cleanup("done"),
            )
            .await,
        ),
    ]
}

#[tokio::test]
async fn the_heartbeat_affects_no_row_outside_the_guard() {
    // inst-bg-row-gone: deleted attachment, chat deletion (cleanup_status set), not uploaded
    let (rig, chat) = rig().await;
    for (label, id) in guard_cases(&rig, &chat).await {
        assert!(
            !store(&rig).heartbeat(TENANT_A, id, now()).await.unwrap(),
            "{label}"
        );
        let row = crate::test_support::chats::attachment_rows(&rig.db, chat.id)
            .await
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        assert_ne!(row.updated_at, now(), "{label}: updated_at was refreshed");
    }
    let uploaded = seed_row(&rig.db, &chat, RowSeed::document().status("uploaded")).await;
    assert!(
        !store(&rig)
            .heartbeat(TENANT_B, uploaded, now())
            .await
            .unwrap(),
        "foreign tenant"
    );
    assert!(
        !store(&rig)
            .heartbeat(TENANT_A, Uuid::new_v4(), now())
            .await
            .unwrap(),
        "unknown id"
    );
}

#[tokio::test]
async fn the_guarded_ready_write_sets_ready_and_updated_at() {
    // inst-bg-case-ready
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().status("uploaded")).await;
    assert!(
        store(&rig)
            .set_ready_guarded(TENANT_A, id, now())
            .await
            .unwrap()
    );
    let row = crate::test_support::chats::attachment_rows(&rig.db, chat.id)
        .await
        .remove(0);
    assert_eq!(row.status, "ready");
    assert_eq!(row.updated_at, now());
}

#[tokio::test]
async fn the_guarded_ready_write_affects_no_row_outside_the_guard() {
    let (rig, chat) = rig().await;
    for (label, id) in guard_cases(&rig, &chat).await {
        assert!(
            !store(&rig)
                .set_ready_guarded(TENANT_A, id, now())
                .await
                .unwrap(),
            "{label}"
        );
    }
    let rows = crate::test_support::chats::attachment_rows(&rig.db, chat.id).await;
    // the deleted and the chat-deleted rows never become ready
    assert!(
        rows.iter()
            .filter(|row| row.status == "uploaded")
            .all(|row| row.status != "ready")
    );
    assert_eq!(
        rows.iter().filter(|row| row.status == "ready").count(),
        1,
        "only the seeded ready row"
    );
}

// ── the failure transaction ──────────────────────────────────────────────────

fn cleanup(rig: &QuotaRig) -> Arc<dyn AttachmentCleanupQueue> {
    rig.enqueuer.clone()
}

#[tokio::test]
async fn the_failure_transaction_fails_the_row_and_enqueues_one_message() {
    // inst-bg-case-failed: status failed, indexing_failed, cleanup pending, one message
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().status("uploaded")).await;
    let committed = store(&rig)
        .fail_indexing(TENANT_A, chat.id, id, now(), cleanup(&rig))
        .await
        .unwrap();
    assert!(committed);
    let row = crate::test_support::chats::attachment_rows(&rig.db, chat.id)
        .await
        .remove(0);
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(row.cleanup_updated_at, Some(now()));
    assert_eq!(row.updated_at, now());
    assert_eq!(row.cleanup_attempts, 0);
    assert!(
        row.deleted_at.is_none(),
        "an indexing failure does not soft-delete the row"
    );
    assert_eq!(attachment_cleanup_rows(&rig.db, &rig.outbox).await.len(), 1);
}

#[tokio::test]
async fn the_cleanup_message_carries_the_payload_of_the_design_table() {
    // inst-enq-payload, inst-enq-transactional: partitioned by tenant
    let (rig, chat) = rig().await;
    let seed = RowSeed::document().status("uploaded");
    let provider_file_id = seed.provider_file_id.clone().unwrap();
    let id = seed_row(&rig.db, &chat, seed).await;
    store(&rig)
        .fail_indexing(TENANT_A, chat.id, id, now(), cleanup(&rig))
        .await
        .unwrap();
    let rows = attachment_cleanup_rows(&rig.db, &rig.outbox).await;
    assert_eq!(rows.len(), 1);
    let payload = &rows[0].payload;
    assert_eq!(payload["event_type"], "attachment_indexing_failed");
    assert_eq!(payload["tenant_id"], TENANT_A.to_string());
    assert_eq!(payload["chat_id"], chat.id.to_string());
    assert_eq!(payload["attachment_id"], id.to_string());
    assert_eq!(payload["provider_file_id"], provider_file_id);
    assert!(payload["vector_store_id"].is_null(), "{payload}");
    assert_eq!(payload["storage_backend"], "azure_openai");
    assert_eq!(payload["attachment_kind"], "document");
    assert!(payload["secondary_ref"].is_null(), "{payload}");
    let deleted_at = payload["deleted_at"].as_str().expect("enqueue time");
    let parsed =
        OffsetDateTime::parse(deleted_at, &time::format_description::well_known::Rfc3339).unwrap();
    assert_eq!(parsed.unix_timestamp(), now().unix_timestamp());
    assert_eq!(
        rows[0].partition,
        i64::from(partition_for(TENANT_A, rig.outbox.num_partitions))
    );
}

#[tokio::test]
async fn a_row_without_a_provider_file_id_enqueues_a_null_file_id() {
    let (rig, chat) = rig().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document().status("uploaded").no_provider_file(),
    )
    .await;
    store(&rig)
        .fail_indexing(TENANT_A, chat.id, id, now(), cleanup(&rig))
        .await
        .unwrap();
    let rows = attachment_cleanup_rows(&rig.db, &rig.outbox).await;
    assert!(rows[0].payload["provider_file_id"].is_null());
}

#[tokio::test]
async fn a_lost_guard_changes_nothing_and_enqueues_nothing() {
    // inst-bg-guard-lost: roll back and enqueue nothing
    let (rig, chat) = rig().await;
    for (label, id) in guard_cases(&rig, &chat).await {
        let committed = store(&rig)
            .fail_indexing(TENANT_A, chat.id, id, now(), cleanup(&rig))
            .await
            .unwrap();
        assert!(!committed, "{label}");
    }
    assert!(
        attachment_cleanup_rows(&rig.db, &rig.outbox)
            .await
            .is_empty()
    );
    let rows = crate::test_support::chats::attachment_rows(&rig.db, chat.id).await;
    assert!(
        rows.iter()
            .all(|row| row.error_code.as_deref() != Some("indexing_failed"))
    );
}

#[tokio::test]
async fn a_failing_enqueue_rolls_the_row_update_back() {
    // every domain write that needs a cleanup message enqueues it in the same transaction
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().status("uploaded")).await;
    fail_outbox_inserts(&rig.db).await;
    let result = store(&rig)
        .fail_indexing(TENANT_A, chat.id, id, now(), cleanup(&rig))
        .await;
    assert!(result.is_err());
    let row = crate::test_support::chats::attachment_rows(&rig.db, chat.id)
        .await
        .remove(0);
    assert_eq!(row.status, "uploaded");
    assert_eq!(row.error_code, None);
    assert_eq!(row.cleanup_status, None);
}

#[tokio::test]
async fn the_failure_transaction_never_touches_another_tenants_row() {
    let (rig, chat) = rig().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().status("uploaded")).await;
    let committed = store(&rig)
        .fail_indexing(TENANT_B, chat.id, id, now(), cleanup(&rig))
        .await
        .unwrap();
    assert!(!committed);
    assert!(
        attachment_cleanup_rows(&rig.db, &rig.outbox)
            .await
            .is_empty()
    );
}
