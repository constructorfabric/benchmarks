#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use super::*;
use crate::infra::db::repo::tests::TestStore;

const TENANT: Uuid = Uuid::from_u128(0x7e7a);
const CHAT: Uuid = Uuid::from_u128(0xc4a7);

fn ts(rfc3339: &str) -> OffsetDateTime {
    OffsetDateTime::parse(rfc3339, &Rfc3339).unwrap()
}

#[test]
fn queues_follow_the_configuration() {
    let queues = OutboxQueues::default();

    assert_eq!(
        queues.name(MiniChatQueue::Usage),
        "mini-chat.usage_snapshot"
    );
    assert_eq!(
        queues.name(MiniChatQueue::AttachmentCleanup),
        "mini-chat.attachment_cleanup"
    );
    assert_eq!(
        queues.name(MiniChatQueue::ChatCleanup),
        "mini-chat.chat_cleanup"
    );
    assert_eq!(
        queues.name(MiniChatQueue::ThreadSummary),
        "mini-chat.thread_summary"
    );
    assert_eq!(queues.name(MiniChatQueue::Audit), "mini-chat.audit");
    assert_eq!(queues.partitions().count(), 4);
}

#[test]
fn partitions_are_stable_and_in_range() {
    let queues = OutboxQueues::default();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..256 {
        let key = Uuid::new_v4();
        let partition = queues.partition_for(key);
        assert!(partition < 4);
        assert_eq!(partition, queues.partition_for(key));
        seen.insert(partition);
    }
    assert_eq!(seen.len(), 4, "keys spread over every partition");

    let single = OutboxQueues {
        num_partitions: 1,
        ..OutboxQueues::default()
    };
    assert_eq!(single.partition_for(Uuid::new_v4()), 0);
}

#[test]
fn chat_cleanup_payload_wire_shape() {
    let payload = ChatCleanupPayload {
        tenant_id: TENANT,
        chat_id: CHAT,
        system_request_id: Uuid::from_u128(5),
        reason: ChatCleanupReason::ChatSoftDelete,
        chat_deleted_at: ts("2026-10-11T12:00:00.5Z"),
    };

    let value = serde_json::to_value(&payload).unwrap();

    assert_eq!(
        value,
        json!({
            "tenant_id": TENANT.to_string(),
            "chat_id": CHAT.to_string(),
            "system_request_id": Uuid::from_u128(5).to_string(),
            "reason": "chat_soft_delete",
            "chat_deleted_at": "2026-10-11T12:00:00.5Z",
        })
    );
    assert_eq!(
        serde_json::from_value::<ChatCleanupPayload>(value).unwrap(),
        payload
    );
}

#[test]
fn attachment_cleanup_payload_keeps_nulls() {
    let payload = AttachmentCleanupPayload {
        event_type: AttachmentCleanupEvent::Deleted,
        tenant_id: TENANT,
        chat_id: CHAT,
        attachment_id: Uuid::from_u128(9),
        provider_file_id: None,
        vector_store_id: None,
        storage_backend: "openai".to_owned(),
        attachment_kind: "image".to_owned(),
        deleted_at: ts("2026-10-11T12:00:00Z"),
        secondary_ref: Some(SecondaryFileRef {
            file_id: "file_2".to_owned(),
            provider_kind: "anthropic".to_owned(),
            upstream_alias: "api.anthropic.com".to_owned(),
        }),
    };

    let value = serde_json::to_value(&payload).unwrap();

    assert_eq!(value["event_type"], "attachment_deleted");
    assert!(value["provider_file_id"].is_null());
    assert!(value["vector_store_id"].is_null());
    assert_eq!(value["secondary_ref"]["provider_kind"], "anthropic");
    assert_eq!(value["deleted_at"], "2026-10-11T12:00:00Z");
    assert_eq!(
        serde_json::to_value(AttachmentCleanupEvent::UploadAbandoned).unwrap(),
        "attachment_upload_abandoned"
    );
    assert_eq!(
        serde_json::to_value(AttachmentCleanupEvent::IndexingFailed).unwrap(),
        "attachment_indexing_failed"
    );
}

#[test]
fn thread_summary_payload_carries_both_frontiers() {
    let payload = ThreadSummaryPayload {
        tenant_id: TENANT,
        chat_id: CHAT,
        system_request_id: Uuid::from_u128(7),
        system_task_type: SystemTaskType::ThreadSummaryUpdate,
        base_frontier_created_at: None,
        base_frontier_message_id: None,
        frozen_target_created_at: ts("2026-10-11T12:00:00Z"),
        frozen_target_message_id: Uuid::from_u128(8),
    };

    let value = serde_json::to_value(&payload).unwrap();

    assert_eq!(value["system_task_type"], "thread_summary_update");
    assert!(value["base_frontier_created_at"].is_null());
    assert!(value["base_frontier_message_id"].is_null());
    assert_eq!(
        value["frozen_target_message_id"],
        Uuid::from_u128(8).to_string()
    );
    assert_eq!(
        serde_json::from_value::<ThreadSummaryPayload>(value).unwrap(),
        payload
    );
}

#[test]
fn oversized_payloads_are_classified() {
    let err = EnqueueError::from(OutboxError::PayloadTooLarge { size: 10, max: 5 });
    assert!(matches!(
        err,
        EnqueueError::PayloadTooLarge { size: 10, max: 5 }
    ));
    assert!(matches!(
        DomainError::from(EnqueueError::NotStarted),
        DomainError::Internal(_)
    ));
}

#[tokio::test]
async fn enqueue_before_attach_is_not_started() {
    let store = TestStore::without_outbox().await;
    assert!(!store.enqueuer.is_attached());

    let err = store
        .enqueuer
        .enqueue_in_tx(&store.conn(), MiniChatQueue::ChatCleanup, CHAT, &json!({}))
        .await
        .unwrap_err();

    assert!(matches!(err, EnqueueError::NotStarted));
}

#[tokio::test]
async fn enqueue_in_tx_delivers_json_on_the_named_queue_after_commit() {
    let store = TestStore::new().await;
    assert!(!store.enqueuer.attach(std::sync::Arc::clone(store.outbox())));
    let payload = ThreadSummaryPayload {
        tenant_id: TENANT,
        chat_id: CHAT,
        system_request_id: Uuid::new_v4(),
        system_task_type: SystemTaskType::ThreadSummaryUpdate,
        base_frontier_created_at: None,
        base_frontier_message_id: None,
        frozen_target_created_at: ts("2026-10-11T12:00:00Z"),
        frozen_target_message_id: Uuid::new_v4(),
    };
    let enqueuer = std::sync::Arc::clone(&store.enqueuer);
    let sent = payload.clone();

    toolkit_db::outbox::in_transaction(&store.db, move |tx| {
        Box::pin(async move {
            let wake = enqueuer
                .enqueue_thread_summary(tx, &sent)
                .await
                .map_err(DomainError::from)?;
            Ok::<_, DomainError>(((), wake))
        })
    })
    .await
    .unwrap();

    let delivered = store.wait_delivered("mini-chat.thread_summary", 1).await;
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].payload_type, JSON_PAYLOAD_TYPE);
    assert_eq!(
        delivered[0].payload,
        serde_json::to_value(&payload).unwrap()
    );
    assert!(store.delivered("mini-chat.chat_cleanup").is_empty());
}

#[tokio::test]
async fn too_large_payloads_are_rejected_before_any_write() {
    let store = TestStore::new().await;
    let huge = "x".repeat(70 * 1024);

    let err = store
        .enqueuer
        .enqueue_in_tx(
            &store.conn(),
            MiniChatQueue::Audit,
            TENANT,
            &json!({ "blob": huge }),
        )
        .await
        .unwrap_err();

    assert!(
        matches!(err, EnqueueError::PayloadTooLarge { .. }),
        "{err:?}"
    );
}
