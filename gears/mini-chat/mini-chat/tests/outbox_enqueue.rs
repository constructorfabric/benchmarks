#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Outbox enqueuer: rows are written inside the caller's transaction (rolled
//! back with it), land in the configured queue, and carry the JSON payload.

mod common;

use std::sync::Arc;

use common::{raw_strings, test_db, test_db_with_raw, ts};
use mini_chat::config::OutboxConfig;
use mini_chat::domain::error::DomainError;
use mini_chat::domain::ports::{OutboxPort, PendingWakes};
use mini_chat::infra::outbox::enqueuer::OutboxEnqueuer;
use mini_chat::infra::outbox::payloads::{
    AttachmentCleanupEventType, AttachmentCleanupPayload, ChatCleanupPayload, ThreadSummaryPayload,
};
use mini_chat::infra::outbox::{partition_for, register_queues};
use mini_chat_sdk::{
    BillingOutcome, MiniChatAuditEvent, RequesterType, SettlementMethod, TerminalState,
    TurnMutationAuditEvent, TurnMutationAuditEventType, UsageEvent,
};
use toolkit_db::outbox::{Outbox, OutboxHandle};
use uuid::Uuid;

fn usage_event(tenant: Uuid, chat: Uuid) -> UsageEvent {
    UsageEvent {
        tenant_id: tenant,
        user_id: Some(Uuid::new_v4()),
        chat_id: chat,
        turn_id: Some(Uuid::new_v4()),
        request_id: Uuid::new_v4(),
        effective_model: "gpt-4.1".to_owned(),
        selected_model: "gpt-4.1".to_owned(),
        terminal_state: TerminalState::Completed,
        billing_outcome: BillingOutcome::Completed,
        usage: None,
        actual_credits_micro: 42,
        settlement_method: SettlementMethod::Actual,
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: ts(1_700_000_000),
        requester_type: RequesterType::User,
        dedupe_key: format!("dedupe-{}", Uuid::new_v4()),
        system_task_type: None,
    }
}

fn mutation_audit(tenant: Uuid, chat: Uuid) -> MiniChatAuditEvent {
    MiniChatAuditEvent::Mutation(TurnMutationAuditEvent {
        event_type: TurnMutationAuditEventType::TurnDelete,
        actor_user_id: Uuid::new_v4(),
        tenant_id: tenant,
        chat_id: chat,
        original_request_id: Some(Uuid::new_v4()),
        new_request_id: None,
        request_id: None,
        timestamp: ts(1_700_000_000),
    })
}

async fn started(
    db: &toolkit_db::DBProvider<DomainError>,
    cfg: &OutboxConfig,
) -> (OutboxHandle, Arc<OutboxEnqueuer>) {
    let handle = register_queues(Outbox::builder(db.db()), cfg)
        .start()
        .await
        .expect("outbox pipeline starts");
    let enqueuer = Arc::new(OutboxEnqueuer::new(cfg.clone()));
    enqueuer
        .set_outbox(Arc::clone(handle.outbox()))
        .expect("first set");
    (handle, enqueuer)
}

/// Payloads of every message currently in the outbox, with queue + partition.
async fn queued(raw: &sea_orm::DatabaseConnection) -> Vec<(String, i64, serde_json::Value)> {
    use sea_orm::{ConnectionTrait, Statement};
    let sql = "SELECT p.queue, p.partition, CAST(b.payload AS TEXT) FROM ( \
                 SELECT partition_id, body_id FROM toolkit_outbox_incoming \
                 UNION ALL SELECT partition_id, body_id FROM toolkit_outbox_outgoing) m \
               JOIN toolkit_outbox_partitions p ON p.id = m.partition_id \
               JOIN toolkit_outbox_body b ON b.id = m.body_id";
    raw.query_all_raw(Statement::from_string(raw.get_database_backend(), sql))
        .await
        .unwrap()
        .iter()
        .map(|r| {
            let queue: String = r.try_get_by_index(0).unwrap();
            let partition: i64 = r.try_get_by_index(1).unwrap();
            let payload: String = r.try_get_by_index(2).unwrap();
            (queue, partition, serde_json::from_str(&payload).unwrap())
        })
        .collect()
}

#[tokio::test]
async fn usage_row_written_in_tx_and_rolled_back() {
    let (db, raw) = test_db_with_raw().await;
    let cfg = OutboxConfig::default();
    let (handle, enqueuer) = started(&db, &cfg).await;
    let ev = usage_event(Uuid::new_v4(), Uuid::new_v4());

    // Err from the transaction body → rollback → no body row.
    let enq = Arc::clone(&enqueuer);
    let ev1 = ev.clone();
    let rolled_back: Result<(), DomainError> = db
        .transaction(move |tx| {
            Box::pin(async move {
                let mut wakes = PendingWakes::new();
                enq.enqueue_usage(tx, &ev1, &mut wakes).await?;
                assert_eq!(wakes.len(), 1);
                Err(DomainError::Internal("business failure".to_owned()))
            })
        })
        .await;
    assert!(rolled_back.is_err());

    // Ok → commit → exactly one row, fired after commit.
    let enq = Arc::clone(&enqueuer);
    let ev2 = ev.clone();
    let wakes = db
        .transaction(move |tx| {
            Box::pin(async move {
                let mut wakes = PendingWakes::new();
                enq.enqueue_usage(tx, &ev2, &mut wakes).await?;
                Ok(wakes)
            })
        })
        .await
        .unwrap();
    wakes.fire_all();
    handle.stop().await;

    let bodies = raw_strings(
        &raw,
        "SELECT CAST(payload AS TEXT) FROM toolkit_outbox_body",
    )
    .await;
    assert_eq!(bodies.len(), 1, "rolled-back enqueue must leave no row");
    let payload: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    assert_eq!(payload["dedupe_key"], serde_json::json!(ev.dedupe_key));
    assert_eq!(payload["tenant_id"], serde_json::json!(ev.tenant_id));
}

#[tokio::test]
async fn each_kind_goes_to_its_queue_and_partition() {
    let (db, raw) = test_db_with_raw().await;
    let cfg = OutboxConfig::default();
    let (handle, enqueuer) = started(&db, &cfg).await;
    let (tenant, chat) = (Uuid::new_v4(), Uuid::new_v4());

    let usage = usage_event(tenant, chat);
    let audit = mutation_audit(tenant, chat);
    let attachment = AttachmentCleanupPayload {
        event_type: AttachmentCleanupEventType::AttachmentDeleted,
        tenant_id: tenant,
        chat_id: chat,
        attachment_id: Uuid::new_v4(),
        provider_file_id: Some("file-abc".to_owned()),
        vector_store_id: None,
        storage_backend: "azure".to_owned(),
        attachment_kind: "document".to_owned(),
        deleted_at: ts(1_700_000_100),
        secondary_ref: None,
    };
    let chat_cleanup = ChatCleanupPayload::new(tenant, chat, ts(1_700_000_200));
    let summary =
        ThreadSummaryPayload::new(tenant, chat, None, (ts(1_700_000_300), Uuid::new_v4()));

    let enq = Arc::clone(&enqueuer);
    let (usage_ev, audit_ev, att_p, chat_p, summary_p) = (
        usage.clone(),
        audit.clone(),
        attachment.clone(),
        chat_cleanup.clone(),
        summary.clone(),
    );
    let wakes = db
        .transaction(move |tx| {
            Box::pin(async move {
                let mut wakes = PendingWakes::new();
                enq.enqueue_usage(tx, &usage_ev, &mut wakes).await?;
                enq.enqueue_audit(tx, &audit_ev, &mut wakes).await?;
                enq.enqueue_attachment_cleanup(tx, &att_p, &mut wakes)
                    .await?;
                enq.enqueue_chat_cleanup(tx, &chat_p, &mut wakes).await?;
                enq.enqueue_thread_summary(tx, &summary_p, &mut wakes)
                    .await?;
                Ok(wakes)
            })
        })
        .await
        .unwrap();
    assert_eq!(wakes.len(), 5);
    wakes.fire_all();
    handle.stop().await;

    let rows = queued(&raw).await;
    assert_eq!(rows.len(), 5, "rows: {rows:?}");
    let parts = cfg.num_partitions;
    let by_tenant = i64::from(partition_for(tenant, parts));
    let by_chat = i64::from(partition_for(chat, parts));
    let find = |queue: &str| {
        rows.iter()
            .find(|(q, _, _)| q == queue)
            .unwrap_or_else(|| panic!("no message on {queue}: {rows:?}"))
            .clone()
    };

    let (_, p, body) = find(&cfg.queue_name);
    assert_eq!(p, by_tenant);
    assert_eq!(body["dedupe_key"], serde_json::json!(usage.dedupe_key));

    let (_, p, body) = find(&cfg.audit_queue_name);
    assert_eq!(p, by_tenant);
    assert_eq!(body["event_type"], serde_json::json!("turn_delete"));

    let (_, p, body) = find(&cfg.cleanup_queue_name);
    assert_eq!(p, by_tenant);
    assert_eq!(body["event_type"], serde_json::json!("attachment_deleted"));
    assert_eq!(body["vector_store_id"], serde_json::Value::Null);

    let (_, p, body) = find(&cfg.chat_cleanup_queue_name);
    assert_eq!(p, by_chat);
    assert_eq!(body["reason"], serde_json::json!("chat_soft_delete"));
    assert_eq!(
        body["system_request_id"],
        serde_json::json!(chat_cleanup.system_request_id)
    );

    let (_, p, body) = find(&cfg.thread_summary_queue_name);
    assert_eq!(p, by_chat);
    assert_eq!(
        body["system_task_type"],
        serde_json::json!("thread_summary_update")
    );
    assert_eq!(body["base_frontier_message_id"], serde_json::Value::Null);
}

#[tokio::test]
async fn enqueue_before_pipeline_is_internal_error() {
    let db = test_db().await;
    let enqueuer = Arc::new(OutboxEnqueuer::new(OutboxConfig::default()));
    let ev = usage_event(Uuid::new_v4(), Uuid::new_v4());
    let enq = Arc::clone(&enqueuer);
    let res: Result<(), DomainError> = db
        .transaction(move |tx| {
            Box::pin(async move {
                let mut wakes = PendingWakes::new();
                enq.enqueue_usage(tx, &ev, &mut wakes).await
            })
        })
        .await;
    assert!(
        matches!(res, Err(DomainError::Internal(_))),
        "unexpected: {res:?}"
    );
}

#[tokio::test]
async fn outbox_can_be_set_only_once() {
    let db = test_db().await;
    let cfg = OutboxConfig::default();
    let (handle, enqueuer) = started(&db, &cfg).await;
    assert!(enqueuer.set_outbox(Arc::clone(handle.outbox())).is_err());
    handle.stop().await;
}

#[tokio::test]
async fn oversized_payload_is_outbox_payload_too_large() {
    let db = test_db().await;
    let cfg = OutboxConfig::default();
    let (handle, enqueuer) = started(&db, &cfg).await;
    let (tenant, chat) = (Uuid::new_v4(), Uuid::new_v4());
    // 64 KiB is the platform maximum payload size.
    let p = AttachmentCleanupPayload {
        event_type: AttachmentCleanupEventType::AttachmentDeleted,
        tenant_id: tenant,
        chat_id: chat,
        attachment_id: Uuid::new_v4(),
        provider_file_id: Some("f".repeat(70 * 1024)),
        vector_store_id: None,
        storage_backend: "azure".to_owned(),
        attachment_kind: "document".to_owned(),
        deleted_at: ts(1_700_000_100),
        secondary_ref: None,
    };
    let enq = Arc::clone(&enqueuer);
    let res: Result<(), DomainError> = db
        .transaction(move |tx| {
            Box::pin(async move {
                let mut wakes = PendingWakes::new();
                enq.enqueue_attachment_cleanup(tx, &p, &mut wakes).await
            })
        })
        .await;
    handle.stop().await;
    assert!(
        matches!(res, Err(DomainError::OutboxPayloadTooLarge(_))),
        "unexpected: {res:?}"
    );
}
