//! The attachment cleanup enqueue of the shared outbox: one message of the configured queue in
//! the caller's transaction, partitioned by `tenant_id`, with the payload of the DESIGN table
//! (`event_type`, identifiers, `provider_file_id` or null, `vector_store_id` always null,
//! `storage_backend`, `attachment_kind`, `deleted_at` equal to the enqueue time and the
//! `secondary_ref` object or null), the rollback with the caller's transaction and the payload
//! size limit.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`:
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue` (`inst-enq-*`);
//! definition of done `cpt-cf-mini-chat-dod-attachments-and-file-search-delete-api`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::attachments::AttachmentKind;
use crate::domain::attachments::store::{
    AttachmentCleanupMessage, AttachmentCleanupQueue, CleanupEvent, SecondaryRef,
};
use crate::domain::error::{DomainError, EnqueueSite};
use crate::infra::outbox::partition_for;
use crate::test_support::attachments::{ANTHROPIC_ALIAS, attachment_cleanup_rows};
use crate::test_support::chats::{at, outbox_body_count};
use crate::test_support::quota::{QuotaRig, quota_rig};
use crate::test_support::{TENANT_A, TENANT_B};

fn message(event: CleanupEvent) -> AttachmentCleanupMessage {
    AttachmentCleanupMessage {
        event_type: event,
        tenant_id: TENANT_A,
        chat_id: Uuid::from_u128(0xC1),
        attachment_id: Uuid::from_u128(0xA7),
        provider_file_id: Some("file-1".to_owned()),
        storage_backend: "azure_openai".to_owned(),
        attachment_kind: AttachmentKind::Document,
        enqueued_at: at(4_000_000) + time::Duration::milliseconds(250),
        secondary_ref: None,
    }
}

async fn enqueue(rig: &QuotaRig, message: AttachmentCleanupMessage) -> Result<(), DomainError> {
    let enqueuer = Arc::clone(&rig.enqueuer);
    let wake = rig
        .db
        .provider
        .transaction(move |tx| {
            let enqueuer = Arc::clone(&enqueuer);
            Box::pin(async move { enqueuer.enqueue_attachment_cleanup(tx, &message).await })
        })
        .await?;
    wake.fire();
    Ok(())
}

#[tokio::test]
async fn one_message_lands_on_the_configured_queue_partitioned_by_tenant() {
    // inst-enq-transactional, inst-enq-return
    let rig = quota_rig().await;
    enqueue(&rig, message(CleanupEvent::AttachmentDeleted))
        .await
        .unwrap();
    let rows = attachment_cleanup_rows(&rig.db, &rig.outbox).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].queue, rig.outbox.cleanup_queue_name);
    assert_eq!(
        rows[0].partition,
        i64::from(partition_for(TENANT_A, rig.outbox.num_partitions))
    );
    let mut other = message(CleanupEvent::AttachmentDeleted);
    other.tenant_id = TENANT_B;
    enqueue(&rig, other).await.unwrap();
    let rows = attachment_cleanup_rows(&rig.db, &rig.outbox).await;
    assert_eq!(
        rows[1].partition,
        i64::from(partition_for(TENANT_B, rig.outbox.num_partitions))
    );
}

#[tokio::test]
async fn the_payload_is_the_design_table() {
    // inst-enq-payload
    let rig = quota_rig().await;
    let message = message(CleanupEvent::AttachmentDeleted);
    enqueue(&rig, message.clone()).await.unwrap();
    let payload = attachment_cleanup_rows(&rig.db, &rig.outbox)
        .await
        .remove(0)
        .payload;
    assert_eq!(payload["event_type"], "attachment_deleted");
    assert_eq!(payload["tenant_id"], TENANT_A.to_string());
    assert_eq!(payload["chat_id"], message.chat_id.to_string());
    assert_eq!(payload["attachment_id"], message.attachment_id.to_string());
    assert_eq!(payload["provider_file_id"], "file-1");
    assert!(payload["vector_store_id"].is_null());
    assert_eq!(payload["storage_backend"], "azure_openai");
    assert_eq!(payload["attachment_kind"], "document");
    assert!(payload["secondary_ref"].is_null());
    let deleted_at = OffsetDateTime::parse(
        payload["deleted_at"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    assert_eq!(deleted_at, message.enqueued_at, "equal to the enqueue time");
}

#[tokio::test]
async fn the_three_event_types_use_their_wire_names() {
    for (event, name) in [
        (CleanupEvent::AttachmentDeleted, "attachment_deleted"),
        (
            CleanupEvent::AttachmentUploadAbandoned,
            "attachment_upload_abandoned",
        ),
        (
            CleanupEvent::AttachmentIndexingFailed,
            "attachment_indexing_failed",
        ),
    ] {
        let rig = quota_rig().await;
        enqueue(&rig, message(event)).await.unwrap();
        let payload = attachment_cleanup_rows(&rig.db, &rig.outbox)
            .await
            .remove(0)
            .payload;
        assert_eq!(payload["event_type"], name);
    }
}

#[tokio::test]
async fn a_null_provider_file_id_and_an_image_kind_are_carried() {
    let rig = quota_rig().await;
    let mut message = message(CleanupEvent::AttachmentDeleted);
    message.provider_file_id = None;
    message.attachment_kind = AttachmentKind::Image;
    enqueue(&rig, message).await.unwrap();
    let payload = attachment_cleanup_rows(&rig.db, &rig.outbox)
        .await
        .remove(0)
        .payload;
    assert!(payload["provider_file_id"].is_null());
    assert_eq!(payload["attachment_kind"], "image");
}

#[tokio::test]
async fn the_secondary_ref_of_a_deleted_event_is_the_design_object() {
    // inst-enq-secondary, inst-enq-secondary-ref: `{file_id, provider_kind, upstream_alias}`
    let rig = quota_rig().await;
    let mut message = message(CleanupEvent::AttachmentDeleted);
    message.secondary_ref = Some(SecondaryRef {
        file_id: "file_ant_5".to_owned(),
        provider_kind: "anthropic".to_owned(),
        upstream_alias: ANTHROPIC_ALIAS.to_owned(),
    });
    enqueue(&rig, message).await.unwrap();
    let payload = attachment_cleanup_rows(&rig.db, &rig.outbox)
        .await
        .remove(0)
        .payload;
    assert_eq!(
        payload["secondary_ref"],
        json!({
            "file_id": "file_ant_5",
            "provider_kind": "anthropic",
            "upstream_alias": ANTHROPIC_ALIAS,
        })
    );
}

#[tokio::test]
async fn the_message_commits_or_rolls_back_with_the_callers_transaction() {
    // inst-enq-transactional
    let rig = quota_rig().await;
    let enqueuer = Arc::clone(&rig.enqueuer);
    let before = outbox_body_count(&rig.db).await;
    let outcome: Result<(), DomainError> = rig
        .db
        .provider
        .transaction(move |tx| {
            let enqueuer = Arc::clone(&enqueuer);
            Box::pin(async move {
                let wake = enqueuer
                    .enqueue_attachment_cleanup(tx, &message(CleanupEvent::AttachmentDeleted))
                    .await?;
                drop(wake);
                Err(DomainError::internal("the caller's transaction fails"))
            })
        })
        .await;
    assert!(outcome.is_err());
    assert_eq!(
        outbox_body_count(&rig.db).await,
        before,
        "rolled back with the caller"
    );
}

#[tokio::test]
async fn an_oversized_payload_is_an_enqueue_error_for_the_caller() {
    // inst-enq-too-large, inst-enq-return-error: the outbox payload limit is 64 KiB
    let rig = quota_rig().await;
    let mut message = message(CleanupEvent::AttachmentDeleted);
    message.provider_file_id = Some("f".repeat(70_000));
    let result = enqueue(&rig, message).await;
    assert!(
        matches!(
            result,
            Err(DomainError::OutboxPayloadTooLarge {
                site: EnqueueSite::AttachmentDelete
            })
        ),
        "{result:?}"
    );
    assert!(
        attachment_cleanup_rows(&rig.db, &rig.outbox)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn the_payload_holds_no_credential_or_file_content() {
    let rig = quota_rig().await;
    enqueue(&rig, message(CleanupEvent::AttachmentDeleted))
        .await
        .unwrap();
    let payload = attachment_cleanup_rows(&rig.db, &rig.outbox)
        .await
        .remove(0)
        .payload;
    let rendered = payload.to_string().to_lowercase();
    for needle in ["secret", "bearer", "api-key", "password", "content"] {
        assert!(!rendered.contains(needle), "{needle} in {rendered}");
    }
}
