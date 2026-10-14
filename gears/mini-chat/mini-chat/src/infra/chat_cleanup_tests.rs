//! The chat-cleanup enqueue contract: one durable message on the configured chat-cleanup queue,
//! partitioned by `chat_id`, with the DESIGN payload fields, written in the caller's transaction.
//!
//! Feature `cpt-cf-mini-chat-feature-chat-management`: algorithm
//! `cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue`; definition of done
//! `cpt-cf-mini-chat-dod-chat-management-delete` (the message contract).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{CHAT_CLEANUP_PAYLOAD_TYPE, OutboxEnqueuer, partition_for, validate_dedupe_key};
use crate::config::OutboxConfig;
use crate::domain::chats::{CHAT_CLEANUP_REASON, ChatCleanupMessage, ChatCleanupQueue};
use crate::domain::error::DomainError;
use crate::test_support::chats::{
    ChatRig, Queue, RigOptions, at, outbox_body_count, parse_ts, rig, rig_with,
};
use crate::test_support::{Pdp, TENANT_A};

fn message(chat_id: Uuid, deleted_at: OffsetDateTime) -> ChatCleanupMessage {
    ChatCleanupMessage {
        tenant_id: TENANT_A,
        chat_id,
        chat_deleted_at: deleted_at,
    }
}

/// Enqueue `message` in its own committed transaction through the rig's real enqueuer.
async fn enqueue(rig: &ChatRig, message: ChatCleanupMessage) -> Result<(), DomainError> {
    let enqueuer: Arc<OutboxEnqueuer> = Arc::clone(&rig.enqueuer);
    let wake = rig
        .db
        .provider
        .transaction(move |tx| {
            Box::pin(async move { enqueuer.enqueue_chat_cleanup(tx, &message).await })
        })
        .await?;
    wake.fire();
    Ok(())
}

#[test]
fn the_reason_of_the_payload_is_chat_soft_delete() {
    assert_eq!(CHAT_CLEANUP_REASON, "chat_soft_delete");
}

#[tokio::test]
async fn one_message_is_enqueued_on_the_chat_cleanup_queue_with_the_design_payload() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat_id = Uuid::from_u128(0xC4A7_0000_0000_0000_0000_0000_0000_0002);
    enqueue(&rig, message(chat_id, at(5))).await.unwrap();

    let rows = rig.outbox_rows().await;
    assert_eq!(rows.len(), 1, "exactly one message per chat deletion");
    let row = &rows[0];
    assert_eq!(row.queue, "mini-chat.chat_cleanup");
    assert_eq!(row.queue, rig.outbox.chat_cleanup_queue_name);
    assert_eq!(row.payload_type, CHAT_CLEANUP_PAYLOAD_TYPE);

    let payload = &row.payload;
    assert_eq!(
        Uuid::parse_str(payload["tenant_id"].as_str().unwrap()).unwrap(),
        TENANT_A
    );
    assert_eq!(
        Uuid::parse_str(payload["chat_id"].as_str().unwrap()).unwrap(),
        chat_id
    );
    assert_eq!(payload["reason"], "chat_soft_delete");
    assert_eq!(parse_ts(&payload["chat_deleted_at"]), at(5));
    let request_id = Uuid::parse_str(payload["system_request_id"].as_str().unwrap()).unwrap();
    assert_eq!(
        request_id.get_version_num(),
        4,
        "a server-generated UUID v4"
    );
}

#[tokio::test]
async fn the_payload_carries_no_content_of_the_chat() {
    let rig = rig(Pdp::TenantOnly).await;
    enqueue(&rig, message(Uuid::new_v4(), at(1))).await.unwrap();
    let payload = rig.outbox_rows().await.remove(0).payload;
    let keys: BTreeSet<&str> = payload
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    for required in [
        "tenant_id",
        "chat_id",
        "system_request_id",
        "reason",
        "chat_deleted_at",
    ] {
        assert!(keys.contains(required), "missing `{required}` in {payload}");
    }
    for forbidden in ["title", "model", "content", "user_id"] {
        assert!(!keys.contains(forbidden), "`{forbidden}` in {payload}");
    }
}

#[tokio::test]
async fn a_dedupe_key_when_present_starts_with_the_32_hex_tenant_id_and_a_slash() {
    let rig = rig(Pdp::TenantOnly).await;
    enqueue(&rig, message(Uuid::new_v4(), at(1))).await.unwrap();
    let payload = rig.outbox_rows().await.remove(0).payload;
    if let Some(key) = payload.get("dedupe_key") {
        let key = key.as_str().expect("dedupe_key is a string");
        validate_dedupe_key(TENANT_A, key).expect("the foundation dedupe prefix rule");
    }
}

#[tokio::test]
async fn each_message_gets_its_own_system_request_id() {
    let rig = rig(Pdp::TenantOnly).await;
    enqueue(&rig, message(Uuid::new_v4(), at(1))).await.unwrap();
    enqueue(&rig, message(Uuid::new_v4(), at(2))).await.unwrap();
    let rows = rig.outbox_rows().await;
    assert_eq!(rows.len(), 2);
    let ids: BTreeSet<&str> = rows
        .iter()
        .map(|r| r.payload["system_request_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
}

#[tokio::test]
async fn the_message_is_partitioned_by_chat_id_not_by_tenant() {
    let rig = rig(Pdp::TenantOnly).await;
    let partitions = i64::from(rig.outbox.num_partitions);
    // TENANT_A routes to partition 1; these chats route to 2, 3 and 0
    let chats = [
        Uuid::from_u128(0xC4A7_0000_0000_0000_0000_0000_0000_0002),
        Uuid::from_u128(0xC4A7_0000_0000_0000_0000_0000_0000_0003),
        Uuid::from_u128(0xC4A7_0000_0000_0000_0000_0000_0000_0004),
    ];
    assert_ne!(partition_for(TENANT_A, rig.outbox.num_partitions), 2);
    for chat in chats {
        enqueue(&rig, message(chat, at(1))).await.unwrap();
    }
    let rows = rig.outbox_rows().await;
    assert_eq!(rows.len(), 3);
    for (chat, row) in chats.iter().zip(&rows) {
        let expected = i64::from(partition_for(*chat, rig.outbox.num_partitions));
        assert_eq!(row.partition, expected, "{chat}");
        assert!(row.partition < partitions);
    }
    let distinct: BTreeSet<i64> = rows.iter().map(|r| r.partition).collect();
    assert_eq!(distinct.len(), 3, "three chats, three partitions: {rows:?}");
}

#[tokio::test]
async fn the_queue_name_comes_from_the_outbox_config() {
    let mut options = RigOptions::new(Pdp::TenantOnly);
    options.outbox = OutboxConfig {
        chat_cleanup_queue_name: "acme.chat_cleanup".to_owned(),
        ..OutboxConfig::default()
    };
    let rig = rig_with(options).await;
    enqueue(&rig, message(Uuid::new_v4(), at(1))).await.unwrap();
    let rows = rig.outbox_rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].queue, "acme.chat_cleanup");
}

#[tokio::test]
async fn the_message_commits_and_rolls_back_with_the_callers_transaction() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = Uuid::new_v4();

    let enqueuer = Arc::clone(&rig.enqueuer);
    let msg = message(chat, at(1));
    let outcome: Result<(), DomainError> = rig
        .db
        .provider
        .transaction(move |tx| {
            Box::pin(async move {
                let wake = enqueuer.enqueue_chat_cleanup(tx, &msg).await?;
                drop(wake);
                Err(DomainError::internal("forced failure after the enqueue"))
            })
        })
        .await;
    assert!(outcome.is_err());
    assert_eq!(
        outbox_body_count(&rig.db).await,
        0,
        "rolled back with the transaction"
    );

    enqueue(&rig, message(chat, at(1))).await.unwrap();
    assert_eq!(
        outbox_body_count(&rig.db).await,
        1,
        "committed with the transaction"
    );
}

#[tokio::test]
async fn an_enqueue_without_a_running_outbox_is_an_internal_error_and_writes_nothing() {
    let mut options = RigOptions::new(Pdp::TenantOnly);
    options.queue = Queue::Unbound;
    let rig = rig_with(options).await;
    let outcome = enqueue(&rig, message(Uuid::new_v4(), at(1))).await;
    assert!(
        matches!(outcome, Err(DomainError::Internal { .. })),
        "{outcome:?}"
    );
    assert_eq!(outbox_body_count(&rig.db).await, 0);
}

#[tokio::test]
async fn the_payload_is_valid_json_that_round_trips_through_the_stored_bytes() {
    let rig = rig(Pdp::TenantOnly).await;
    enqueue(&rig, message(Uuid::new_v4(), at(9))).await.unwrap();
    let first: Value = rig.outbox_rows().await.remove(0).payload;
    let second: Value = rig.outbox_rows().await.remove(0).payload;
    assert_eq!(
        first, second,
        "the persisted payload, and its system_request_id, is stable"
    );
}
