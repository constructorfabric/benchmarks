//! The turn outbox over the shared outbox pipeline: the transactional audit enqueue (queue,
//! payload type, payload, tenant partition) and the thread-summary work enqueue (queue, payload
//! fields, chat partition, stable `system_request_id`), both committed or rolled back with the
//! caller's transaction.
//!
//! Slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithms
//! `finalization`, `turn-audit-event` and `summary-trigger`; definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-finalization`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mini_chat_sdk::AuditEvent;
use serde_json::Value;
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::DomainError;
use crate::domain::streaming::TurnOutbox;
use crate::domain::streaming::summary::{Frontier, THREAD_SUMMARY_UPDATE, ThreadSummaryWork};
use crate::infra::audit_gateway::AUDIT_PAYLOAD_TYPE;
use crate::infra::outbox::{
    OutboxEnqueuer, THREAD_SUMMARY_PAYLOAD_TYPE, partition_for, queue_specs,
};
use crate::test_support::TENANT_A;
use crate::test_support::chats::{outbox_body_count, outbox_rows};
use crate::test_support::quota::{QuotaRig, quota_rig};
use crate::test_support::s2::all_audit_events;
use crate::test_support::send::t;

async fn enqueue_audit(rig: &QuotaRig, event: AuditEvent) -> Result<(), DomainError> {
    let enqueuer = Arc::clone(&rig.enqueuer);
    let wake = rig
        .db
        .provider
        .transaction(move |tx| {
            let enqueuer = Arc::clone(&enqueuer);
            Box::pin(async move { enqueuer.enqueue_turn_audit(tx, &event).await })
        })
        .await?;
    wake.fire();
    Ok(())
}

fn work(chat_id: Uuid, with_base: bool) -> ThreadSummaryWork {
    ThreadSummaryWork {
        tenant_id: TENANT_A,
        chat_id,
        system_request_id: Uuid::new_v4(),
        base_frontier: with_base.then(|| Frontier {
            created_at: t(5),
            message_id: Uuid::from_u128(0xB0),
        }),
        frozen_target: Frontier {
            created_at: t(50),
            message_id: Uuid::from_u128(0xF7),
        },
    }
}

async fn enqueue_summary(rig: &QuotaRig, work: ThreadSummaryWork) -> Result<(), DomainError> {
    let enqueuer = Arc::clone(&rig.enqueuer);
    let wake = rig
        .db
        .provider
        .transaction(move |tx| {
            let enqueuer = Arc::clone(&enqueuer);
            Box::pin(async move { enqueuer.enqueue_thread_summary(tx, &work).await })
        })
        .await?;
    wake.fire();
    Ok(())
}

#[tokio::test]
async fn an_audit_event_lands_on_the_audit_queue_as_its_json() {
    let rig = quota_rig().await;
    let chat_id = Uuid::from_u128(0xC4A7);
    for event in all_audit_events(chat_id).into_iter().take(2) {
        enqueue_audit(&rig, event.clone()).await.unwrap();
        let rows = outbox_rows(&rig.db).await;
        let row = rows.last().unwrap();
        assert_eq!(row.queue, rig.outbox.audit_queue_name);
        assert_eq!(row.payload_type, AUDIT_PAYLOAD_TYPE);
        let back: AuditEvent = serde_json::from_value(row.payload.clone()).unwrap();
        assert_eq!(back, event);
    }
}

#[tokio::test]
async fn the_audit_queue_is_partitioned_by_the_tenant() {
    let rig = quota_rig().await;
    // a chat id that routes to another partition than the tenant: the key is the tenant
    let chat_id = Uuid::from_u128(0xC4A7);
    let partitions = rig.outbox.num_partitions;
    assert_ne!(
        partition_for(chat_id, partitions),
        partition_for(TENANT_A, partitions)
    );
    let event = all_audit_events(chat_id).remove(0);
    enqueue_audit(&rig, event).await.unwrap();
    let rows = outbox_rows(&rig.db).await;
    assert_eq!(
        rows[0].partition,
        i64::from(partition_for(TENANT_A, partitions))
    );
}

#[tokio::test]
async fn an_audit_enqueue_rolls_back_with_the_caller_transaction() {
    let rig = quota_rig().await;
    let enqueuer = Arc::clone(&rig.enqueuer);
    let event = all_audit_events(Uuid::from_u128(1)).remove(0);
    let outcome: Result<(), DomainError> = rig
        .db
        .provider
        .transaction(move |tx| {
            let enqueuer = Arc::clone(&enqueuer);
            Box::pin(async move {
                let wake = enqueuer.enqueue_turn_audit(tx, &event).await?;
                drop(wake);
                Err(DomainError::internal(
                    "the finalization failed after the enqueue",
                ))
            })
        })
        .await;
    assert!(outcome.is_err());
    assert_eq!(outbox_body_count(&rig.db).await, 0);
}

#[tokio::test]
async fn an_unbound_outbox_fails_the_enqueue_internally() {
    let rig = quota_rig().await;
    let unbound = Arc::new(OutboxEnqueuer::new(queue_specs(
        &OutboxConfig::default(),
        300,
    )));
    let event = all_audit_events(Uuid::from_u128(1)).remove(0);
    let outcome = rig
        .db
        .provider
        .transaction(move |tx| {
            let unbound = Arc::clone(&unbound);
            Box::pin(async move { unbound.enqueue_turn_audit(tx, &event).await })
        })
        .await;
    assert!(
        matches!(outcome, Err(DomainError::Internal { .. })),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn thread_summary_work_lands_on_its_queue_with_the_frozen_range() {
    let rig = quota_rig().await;
    let chat_id = Uuid::from_u128(0xC4A7);
    let item = work(chat_id, true);
    enqueue_summary(&rig, item.clone()).await.unwrap();
    let rows = outbox_rows(&rig.db).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.queue, rig.outbox.thread_summary_queue_name);
    assert_eq!(row.payload_type, THREAD_SUMMARY_PAYLOAD_TYPE);
    // partitioned by the chat
    assert_eq!(
        row.partition,
        i64::from(partition_for(chat_id, rig.outbox.num_partitions))
    );
    let payload = &row.payload;
    assert_eq!(payload["tenant_id"], TENANT_A.to_string());
    assert_eq!(payload["chat_id"], chat_id.to_string());
    assert_eq!(
        payload["system_request_id"],
        item.system_request_id.to_string()
    );
    assert_eq!(payload["system_task_type"], THREAD_SUMMARY_UPDATE);
    assert_eq!(
        payload["frozen_target_message_id"],
        Uuid::from_u128(0xF7).to_string()
    );
    assert_eq!(
        payload["base_frontier_message_id"],
        Uuid::from_u128(0xB0).to_string()
    );
    for key in ["frozen_target_created_at", "base_frontier_created_at"] {
        let text = payload[key]
            .as_str()
            .unwrap_or_else(|| panic!("`{key}` in {payload}"));
        assert!(
            time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
                .is_ok(),
            "{key}: {text}"
        );
    }
}

#[tokio::test]
async fn an_empty_base_frontier_is_null_in_the_payload() {
    let rig = quota_rig().await;
    enqueue_summary(&rig, work(Uuid::from_u128(1), false))
        .await
        .unwrap();
    let payload: Value = outbox_rows(&rig.db).await.remove(0).payload;
    assert!(payload["base_frontier_message_id"].is_null(), "{payload}");
    assert!(payload["base_frontier_created_at"].is_null(), "{payload}");
    assert!(payload["frozen_target_message_id"].is_string());
}

#[tokio::test]
async fn thread_summary_work_rolls_back_with_the_caller_transaction() {
    let rig = quota_rig().await;
    let enqueuer = Arc::clone(&rig.enqueuer);
    let item = work(Uuid::from_u128(1), true);
    let outcome: Result<(), DomainError> = rig
        .db
        .provider
        .transaction(move |tx| {
            let enqueuer = Arc::clone(&enqueuer);
            Box::pin(async move {
                let wake = enqueuer.enqueue_thread_summary(tx, &item).await?;
                drop(wake);
                Err(DomainError::internal("rolled back"))
            })
        })
        .await;
    assert!(outcome.is_err());
    assert_eq!(outbox_body_count(&rig.db).await, 0);
}
