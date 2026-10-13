#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;
use uuid::Uuid;

use super::enqueuer::MAX_PAYLOAD_BYTES;
use super::payloads::{CHAT_CLEANUP_PAYLOAD_TYPE, ChatCleanupPayload};
use super::{OutboxEnqueuer, OutboxHandlers, QueueKind, partition_for, start_outbox};
use crate::config::{MiniChatConfig, OutboxConfig};
use crate::domain::error::DomainError;
use crate::infra::db::test_db;

#[test]
fn partition_uses_first_four_bytes_big_endian() {
    let key = Uuid::parse_str("00000007-ffff-ffff-ffff-ffffffffffff").unwrap();
    assert_eq!(partition_for(key, 4), 3);
    assert_eq!(partition_for(key, 1), 0);
    let key = Uuid::parse_str("01000000-0000-0000-0000-000000000000").unwrap();
    assert_eq!(partition_for(key, 64), (1u32 << 24) % 64);
    for _ in 0..100 {
        assert!(partition_for(Uuid::new_v4(), 8) < 8);
    }
}

#[test]
fn queue_kinds_map_to_configured_names() {
    let cfg = OutboxConfig::default();
    let names: Vec<&str> = QueueKind::ALL.iter().map(|q| q.queue_name(&cfg)).collect();
    assert_eq!(
        names,
        [
            "mini-chat.usage_snapshot",
            "mini-chat.attachment_cleanup",
            "mini-chat.chat_cleanup",
            "mini-chat.thread_summary",
            "mini-chat.audit",
        ]
    );
}

#[test]
fn chat_cleanup_payload_wire_shape() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-04T12:00:00.5Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let p = ChatCleanupPayload::soft_delete(Uuid::nil(), Uuid::max(), now);
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v["reason"], "chat_soft_delete");
    assert_eq!(v["chat_deleted_at"], "2026-10-04T12:00:00.500Z");
    assert_eq!(v["tenant_id"], Uuid::nil().to_string());
    assert_eq!(v["chat_id"], Uuid::max().to_string());
    assert_eq!(v.as_object().unwrap().len(), 5);
    assert_ne!(
        p.system_request_id,
        ChatCleanupPayload::soft_delete(Uuid::nil(), Uuid::max(), now).system_request_id
    );
}

#[tokio::test]
async fn enqueue_before_start_is_internal() {
    let db = test_db().await;
    let enq = OutboxEnqueuer::new(OutboxConfig::default());
    let conn = db.conn().unwrap();
    let err = enq
        .enqueue_json(&conn, QueueKind::Usage, Uuid::new_v4(), "t", &json!({}))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DomainError::Internal(ref m) if m == "outbox not started"),
        "{err:?}"
    );
}

#[tokio::test]
async fn oversized_payload_is_rejected_before_enqueue() {
    let db = test_db().await;
    let enq = OutboxEnqueuer::new(OutboxConfig::default());
    let conn = db.conn().unwrap();
    let big = json!({ "x": "a".repeat(MAX_PAYLOAD_BYTES) });
    let err = enq
        .enqueue_json(&conn, QueueKind::ChatCleanup, Uuid::new_v4(), "t", &big)
        .await
        .unwrap_err();
    assert!(
        matches!(err, DomainError::OutboxPayloadTooLarge(_)),
        "{err:?}"
    );
}

#[tokio::test]
async fn enqueue_after_start_succeeds_on_every_queue() {
    let db = test_db().await;
    let cfg = MiniChatConfig::default();
    let handle = start_outbox(db.clone(), &cfg, OutboxHandlers::placeholders())
        .await
        .unwrap();
    let enq = OutboxEnqueuer::new(cfg.outbox.clone());
    enq.set_outbox(handle.outbox().clone());
    let payload = ChatCleanupPayload::soft_delete(
        Uuid::new_v4(),
        Uuid::new_v4(),
        crate::domain::clock::now_utc(),
    );
    for q in QueueKind::ALL {
        let conn = db.conn().unwrap();
        let wake = enq
            .enqueue_json(
                &conn,
                q,
                Uuid::new_v4(),
                CHAT_CLEANUP_PAYLOAD_TYPE,
                &payload,
            )
            .await
            .unwrap();
        wake.fire();
    }
    handle.stop().await;
}
