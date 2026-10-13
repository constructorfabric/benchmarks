#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatAuditEvent, TurnMutationAuditEvent, UsageEvent};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use uuid::Uuid;

use super::payloads::{AttachmentCleanupPayload, ChatCleanupPayload, ThreadSummaryPayload};
use super::{DeferredHandler, MiniChatOutbox, OutboxHandlers, partition_for};
use crate::config::OutboxConfig;
use crate::domain::error::DomainError;
use crate::infra::db::all_migrations;

#[test]
fn partition_uses_last_two_bytes_modulo_partitions() {
    let mut bytes = [0u8; 16];
    bytes[14] = 0x01;
    bytes[15] = 0x03; // 0x0103 = 259
    let id = Uuid::from_bytes(bytes);
    assert_eq!(partition_for(id, 4), 3);
    assert_eq!(partition_for(id, 8), 3);
    assert_eq!(partition_for(id, 64), 3);
    assert_eq!(partition_for(id, 1), 0);
    bytes[14] = 0xFF;
    bytes[15] = 0xFE; // 65534
    assert_eq!(partition_for(Uuid::from_bytes(bytes), 16), 14);
    // byte 13 is ignored
    bytes[13] = 0xAA;
    assert_eq!(partition_for(Uuid::from_bytes(bytes), 16), 14);
}

/// Forwards `(payload_type, payload)` of every message to a channel.
struct Recorder(mpsc::UnboundedSender<(String, serde_json::Value)>);

#[async_trait]
impl LeasedMessageHandler for Recorder {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let body = serde_json::from_slice(&msg.payload).unwrap();
        self.0.send((msg.payload_type.clone(), body)).unwrap();
        MessageResult::Ok
    }
}

/// File-backed WAL database (the server's setup): shared-cache in-memory
/// `SQLite` uses table locks and deadlocks the concurrent outbox workers.
async fn outbox_db() -> (tempfile::TempDir, toolkit_db::Db) {
    let dir = tempfile::tempdir().unwrap();
    let dsn = format!(
        "sqlite://{}?mode=rwc&journal_mode=wal",
        dir.path().join("outbox.db").display()
    );
    let db = connect_db(
        &dsn,
        ConnectOpts {
            max_conns: Some(5),
            min_conns: Some(1),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    run_migrations_for_testing(&db, all_migrations())
        .await
        .unwrap();
    (dir, db)
}

fn ts() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

fn usage_event(tenant: Uuid) -> UsageEvent {
    UsageEvent {
        tenant_id: tenant,
        user_id: Some(Uuid::new_v4()),
        chat_id: Uuid::new_v4(),
        turn_id: Some(Uuid::new_v4()),
        request_id: Uuid::new_v4(),
        effective_model: "m".to_owned(),
        selected_model: "m".to_owned(),
        terminal_state: "completed".to_owned(),
        billing_outcome: "completed".to_owned(),
        usage: None,
        actual_credits_micro: 7,
        settlement_method: "actual".to_owned(),
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: ts(),
        requester_type: "user".to_owned(),
        dedupe_key: "k".to_owned(),
        system_task_type: None,
    }
}

#[tokio::test]
async fn each_queue_delivers_its_payload_type_after_commit() {
    let (_dir, db) = outbox_db().await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let rec = || -> Arc<dyn LeasedMessageHandler> { Arc::new(Recorder(tx.clone())) };
    let handlers = OutboxHandlers {
        usage: rec(),
        audit: rec(),
        attachment_cleanup: rec(),
        chat_cleanup: rec(),
        thread_summary: rec(),
    };
    let outbox = Arc::new(
        MiniChatOutbox::start(
            db.clone(),
            &OutboxConfig::default(),
            Duration::from_secs(300),
            handlers,
        )
        .await
        .unwrap(),
    );

    let tenant = Uuid::new_v4();
    let chat = Uuid::new_v4();
    let usage = usage_event(tenant);
    let audit = MiniChatAuditEvent::Mutation(TurnMutationAuditEvent {
        event_type: "turn_delete".to_owned(),
        tenant_id: tenant,
        chat_id: chat,
        actor_user_id: Uuid::new_v4(),
        original_request_id: None,
        new_request_id: None,
        request_id: Some(Uuid::new_v4()),
        timestamp: ts(),
    });
    let attachment = AttachmentCleanupPayload {
        event_type: "attachment_deleted".to_owned(),
        tenant_id: tenant,
        chat_id: chat,
        attachment_id: Uuid::new_v4(),
        provider_file_id: Some("file-1".to_owned()),
        vector_store_id: None,
        storage_backend: "openai".to_owned(),
        attachment_kind: "document".to_owned(),
        deleted_at: ts(),
        secondary_ref: None,
    };
    let chat_cleanup = ChatCleanupPayload {
        tenant_id: tenant,
        chat_id: chat,
        system_request_id: Uuid::new_v4(),
        reason: "chat_soft_delete".to_owned(),
        chat_deleted_at: ts(),
    };
    let summary = ThreadSummaryPayload {
        tenant_id: tenant,
        chat_id: chat,
        system_request_id: Uuid::new_v4(),
        base_frontier_created_at: None,
        base_frontier_message_id: None,
        frozen_target_created_at: ts(),
        frozen_target_message_id: Uuid::new_v4(),
        system_task_type: "thread_summary_update".to_owned(),
    };

    let provider = DBProvider::<DomainError>::new(db);
    let ob = Arc::clone(&outbox);
    let (u, a, at, cc, s) = (
        usage.clone(),
        audit.clone(),
        attachment.clone(),
        chat_cleanup.clone(),
        summary.clone(),
    );
    let wake = provider
        .transaction(move |txn| {
            Box::pin(async move {
                let mut wake = ob.enqueue_usage(txn, &u).await?;
                wake += ob.enqueue_audit(txn, tenant, &a).await?;
                wake += ob.enqueue_attachment_cleanup(txn, &at).await?;
                wake += ob.enqueue_chat_cleanup(txn, &cc).await?;
                wake += ob.enqueue_thread_summary(txn, &s).await?;
                Ok(wake)
            })
        })
        .await
        .unwrap();
    wake.fire();

    let mut got = std::collections::BTreeMap::new();
    while got.len() < 5 {
        let (ty, body) = tokio::time::timeout(Duration::from_secs(20), rx.recv())
            .await
            .expect("all five messages delivered")
            .unwrap();
        got.insert(ty, body);
    }
    assert_eq!(
        got["mini-chat.usage.v1"],
        serde_json::to_value(&usage).unwrap()
    );
    assert_eq!(
        got["mini-chat.audit.v1"],
        serde_json::to_value(&audit).unwrap()
    );
    assert_eq!(
        got["mini-chat.attachment_cleanup.v1"],
        serde_json::to_value(&attachment).unwrap()
    );
    assert_eq!(
        got["mini-chat.chat_cleanup.v1"],
        serde_json::to_value(&chat_cleanup).unwrap()
    );
    assert_eq!(
        got["mini-chat.thread_summary.v1"],
        serde_json::to_value(&summary).unwrap()
    );

    outbox.stop().await;
}

#[test]
fn payloads_serialize_design_fields() {
    let p = AttachmentCleanupPayload {
        event_type: "attachment_deleted".to_owned(),
        tenant_id: Uuid::nil(),
        chat_id: Uuid::nil(),
        attachment_id: Uuid::nil(),
        provider_file_id: None,
        vector_store_id: None,
        storage_backend: "openai".to_owned(),
        attachment_kind: "image".to_owned(),
        deleted_at: ts(),
        secondary_ref: Some(super::payloads::SecondaryRef {
            file_id: "f".to_owned(),
            provider_kind: "anthropic".to_owned(),
            upstream_alias: "api.anthropic.com".to_owned(),
        }),
    };
    let json = serde_json::to_value(&p).unwrap();
    assert_eq!(json["provider_file_id"], serde_json::Value::Null);
    assert_eq!(json["vector_store_id"], serde_json::Value::Null);
    assert_eq!(json["deleted_at"], "2026-09-21T14:13:20Z");
    assert_eq!(json["secondary_ref"]["upstream_alias"], "api.anthropic.com");
    let back: AttachmentCleanupPayload = serde_json::from_value(json).unwrap();
    assert_eq!(back, p);
}

// ── Deferred handler ─────────────────────────────────────────────────────────

struct Fixed(MessageResult);

#[async_trait]
impl LeasedMessageHandler for Fixed {
    async fn handle(&self, _msg: &OutboxMessage) -> MessageResult {
        match &self.0 {
            MessageResult::Ok => MessageResult::Ok,
            MessageResult::Retry => MessageResult::Retry,
            MessageResult::Reject(r) => MessageResult::Reject(r.clone()),
        }
    }
}

fn any_msg() -> OutboxMessage {
    OutboxMessage {
        partition_id: 0,
        seq: 1,
        payload: Vec::new(),
        payload_type: "t".to_owned(),
        created_at: chrono::Utc::now(),
        attempts: 0,
    }
}

#[tokio::test(start_paused = true)]
async fn deferred_handler_waits_for_install() {
    let deferred = Arc::new(DeferredHandler::default());
    let pending = {
        let d = Arc::clone(&deferred);
        tokio::spawn(async move { d.handle(&any_msg()).await })
    };
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(!pending.is_finished());
    deferred.install(Arc::new(Fixed(MessageResult::Reject("x".to_owned()))));
    assert!(matches!(pending.await.unwrap(), MessageResult::Reject(r) if r == "x"));
    // Installed: delegates immediately.
    assert!(matches!(
        deferred.handle(&any_msg()).await,
        MessageResult::Reject(_)
    ));
}

#[tokio::test(start_paused = true)]
async fn deferred_handler_retries_when_never_installed() {
    let deferred = DeferredHandler::default();
    let started = tokio::time::Instant::now();
    assert!(matches!(
        deferred.handle(&any_msg()).await,
        MessageResult::Retry
    ));
    assert!(started.elapsed() >= Duration::from_secs(30));
}
