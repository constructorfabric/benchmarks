//! Outbox queue tests: the five queue registrations, partition routing, dedupe keys, the
//! pipeline start on SQLite and the transactional enqueuer.
//!
//! Slice S1, algorithm `outbox-queues`, definition of done `outbox` (the outbox part of `gear-start`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::UsageEvent;
use time::OffsetDateTime;
use toolkit_db::outbox::{OutboxError, Record, outbox_migrations};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::{
    AUDIT_LEASE, DedupeKeyError, OutboxEnqueuer, PartitionBy, QueueKind, QueueSpec,
    normalize_uuid_for_dedupe_key, partition_for, queue_specs, start_pipeline,
    system_task_dedupe_key, usage_dedupe_key, validate_dedupe_key,
};
use crate::config::OutboxConfig;
use crate::domain::error::{DomainError, EnqueueSite};
use crate::infra::storage::migrations::all_migrations;
use crate::infra::storage::repo_impl::{ChatRecord, ChatRepo, NewChat, NewMessage};
use crate::test_support::{TENANT_A, TENANT_B, USER_A1, ctx, raw_i64, sqlite_db};

fn cfg(partitions: u32) -> OutboxConfig {
    OutboxConfig {
        queue_name: "mini-chat.usage_snapshot".to_owned(),
        cleanup_queue_name: "mini-chat.attachment_cleanup".to_owned(),
        chat_cleanup_queue_name: "mini-chat.chat_cleanup".to_owned(),
        thread_summary_queue_name: "mini-chat.thread_summary".to_owned(),
        audit_queue_name: "mini-chat.audit".to_owned(),
        num_partitions: partitions,
    }
}

fn spec_of(specs: &[QueueSpec], kind: QueueKind) -> &QueueSpec {
    specs
        .iter()
        .find(|s| s.kind == kind)
        .unwrap_or_else(|| panic!("no spec for {kind:?}"))
}

fn usage_event(tenant: Uuid, dedupe_key: Option<String>) -> UsageEvent {
    UsageEvent {
        tenant_id: tenant,
        user_id: Some(Uuid::from_u128(2)),
        chat_id: Uuid::from_u128(3),
        turn_id: Some(Uuid::from_u128(4)),
        request_id: Uuid::from_u128(5),
        effective_model: "m".to_owned(),
        selected_model: "m".to_owned(),
        terminal_state: "completed".to_owned(),
        billing_outcome: "completed".to_owned(),
        usage: None,
        actual_credits_micro: 10,
        settlement_method: "actual".to_owned(),
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: OffsetDateTime::UNIX_EPOCH,
        requester_type: "user".to_owned(),
        dedupe_key,
        system_task_type: None,
    }
}

// ── queue registrations ──────────────────────────────────────────────────────

#[test]
fn five_queues_are_registered_in_the_documented_order_with_the_default_names() {
    let specs = queue_specs(&cfg(4), 300);
    let registered: Vec<(QueueKind, &str)> =
        specs.iter().map(|s| (s.kind, s.name.as_str())).collect();
    assert_eq!(
        registered,
        vec![
            (QueueKind::Usage, "mini-chat.usage_snapshot"),
            (QueueKind::AttachmentCleanup, "mini-chat.attachment_cleanup"),
            (QueueKind::ChatCleanup, "mini-chat.chat_cleanup"),
            (QueueKind::ThreadSummary, "mini-chat.thread_summary"),
            (QueueKind::Audit, "mini-chat.audit"),
        ]
    );
}

#[test]
fn queue_names_come_from_the_outbox_config() {
    let mut custom = cfg(4);
    custom.queue_name = "acme.usage".to_owned();
    custom.cleanup_queue_name = "acme.att".to_owned();
    custom.chat_cleanup_queue_name = "acme.chat".to_owned();
    custom.thread_summary_queue_name = "acme.summary".to_owned();
    custom.audit_queue_name = "acme.audit".to_owned();
    let specs = queue_specs(&custom, 300);
    assert_eq!(spec_of(&specs, QueueKind::Usage).name, "acme.usage");
    assert_eq!(
        spec_of(&specs, QueueKind::AttachmentCleanup).name,
        "acme.att"
    );
    assert_eq!(spec_of(&specs, QueueKind::ChatCleanup).name, "acme.chat");
    assert_eq!(
        spec_of(&specs, QueueKind::ThreadSummary).name,
        "acme.summary"
    );
    assert_eq!(spec_of(&specs, QueueKind::Audit).name, "acme.audit");
}

#[test]
fn every_queue_gets_num_partitions_partitions() {
    for partitions in [1, 2, 4, 8, 64] {
        let specs = queue_specs(&cfg(partitions), 300);
        assert_eq!(specs.len(), 5);
        assert!(
            specs.iter().all(|s| s.partitions == partitions),
            "{partitions}"
        );
    }
}

#[test]
fn usage_and_audit_partition_by_tenant_and_chat_queues_by_chat() {
    let specs = queue_specs(&cfg(4), 300);
    assert_eq!(
        spec_of(&specs, QueueKind::Usage).partition_by,
        PartitionBy::Tenant
    );
    assert_eq!(
        spec_of(&specs, QueueKind::Audit).partition_by,
        PartitionBy::Tenant
    );
    assert_eq!(
        spec_of(&specs, QueueKind::ChatCleanup).partition_by,
        PartitionBy::Chat
    );
    assert_eq!(
        spec_of(&specs, QueueKind::ThreadSummary).partition_by,
        PartitionBy::Chat
    );
}

#[test]
fn leases_are_configured_for_the_thread_summary_and_audit_queues_only() {
    let specs = queue_specs(&cfg(4), 300);
    assert_eq!(
        spec_of(&specs, QueueKind::ThreadSummary).lease,
        Some(Duration::from_secs(300))
    );
    assert_eq!(
        spec_of(&specs, QueueKind::Audit).lease,
        Some(Duration::from_secs(60))
    );
    assert_eq!(AUDIT_LEASE, Duration::from_secs(60));
    for kind in [
        QueueKind::Usage,
        QueueKind::AttachmentCleanup,
        QueueKind::ChatCleanup,
    ] {
        assert_eq!(
            spec_of(&specs, kind).lease,
            None,
            "{kind:?} keeps the shared default lease"
        );
    }

    let tuned = queue_specs(&cfg(4), 120);
    assert_eq!(
        spec_of(&tuned, QueueKind::ThreadSummary).lease,
        Some(Duration::from_secs(120))
    );
    assert_eq!(
        spec_of(&tuned, QueueKind::Audit).lease,
        Some(Duration::from_secs(60))
    );
}

// ── partition routing ────────────────────────────────────────────────────────

#[test]
fn a_partition_is_always_below_num_partitions_and_stable_per_key() {
    for partitions in [1u32, 2, 4, 8, 16, 32, 64] {
        for i in 0..200u128 {
            let key = Uuid::from_u128(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let partition = partition_for(key, partitions);
            assert!(partition < partitions, "{partitions}: {partition}");
            assert_eq!(partition, partition_for(key, partitions));
        }
    }
    assert_eq!(partition_for(TENANT_A, 1), 0);
}

#[test]
fn keys_spread_over_all_partitions() {
    let hit: BTreeSet<u32> = (0..256u128)
        .map(|i| partition_for(Uuid::from_u128(i), 4))
        .collect();
    assert_eq!(hit, BTreeSet::from([0, 1, 2, 3]));
}

// ── dedupe keys ──────────────────────────────────────────────────────────────

#[test]
fn uuids_are_normalized_to_32_lowercase_hex_characters() {
    let id = Uuid::parse_str("F47AC10B-58CC-4372-A567-0E02B2C3D479").unwrap();
    assert_eq!(
        normalize_uuid_for_dedupe_key(id),
        "f47ac10b58cc4372a5670e02b2c3d479"
    );
}

#[test]
fn the_usage_dedupe_key_is_tenant_turn_request_in_simple_form() {
    let (tenant, turn, request) = (
        Uuid::from_u128(0xA1),
        Uuid::from_u128(0xB2),
        Uuid::from_u128(0xC3),
    );
    let key = usage_dedupe_key(tenant, turn, request);
    let expected = format!("{}/{}/{}", tenant.simple(), turn.simple(), request.simple());
    assert_eq!(key, expected);
    assert!(!key.contains('-'));
    assert_eq!(key.len(), 32 * 3 + 2);
}

#[test]
fn the_system_task_dedupe_key_is_tenant_task_type_request() {
    let (tenant, request) = (Uuid::from_u128(0xA1), Uuid::from_u128(0xC3));
    let key = system_task_dedupe_key(tenant, "thread_summary_update", request);
    assert_eq!(
        key,
        format!(
            "{}/thread_summary_update/{}",
            tenant.simple(),
            request.simple()
        )
    );
}

#[test]
fn well_formed_keys_pass_validation() {
    let tenant = TENANT_A;
    let usage = usage_dedupe_key(tenant, Uuid::from_u128(1), Uuid::from_u128(2));
    let system = system_task_dedupe_key(tenant, "thread_summary_update", Uuid::from_u128(2));
    assert_eq!(validate_dedupe_key(tenant, &usage), Ok(()));
    assert_eq!(validate_dedupe_key(tenant, &system), Ok(()));
}

#[test]
fn a_key_must_start_with_the_lowercase_hex_tenant_id_and_a_slash() {
    let tenant = TENANT_A;
    let good = usage_dedupe_key(tenant, Uuid::from_u128(1), Uuid::from_u128(2));
    let cases = vec![
        usage_dedupe_key(TENANT_B, Uuid::from_u128(1), Uuid::from_u128(2)),
        format!("{}/1/2", tenant.hyphenated()),
        format!("{}/1/2", tenant.simple().to_string().to_uppercase()),
        format!("{}-turn-request", tenant.simple()),
        tenant.simple().to_string(),
        format!("x{good}"),
        String::new(),
    ];
    for key in cases {
        assert_eq!(
            validate_dedupe_key(tenant, &key),
            Err(DedupeKeyError::BadTenantPrefix),
            "{key:?}"
        );
    }
}

#[test]
fn a_key_needs_components_after_the_tenant_prefix() {
    let key = format!("{}/", TENANT_A.simple());
    assert_eq!(
        validate_dedupe_key(TENANT_A, &key),
        Err(DedupeKeyError::MissingComponents)
    );
}

// ── pipeline on SQLite ───────────────────────────────────────────────────────

#[tokio::test]
async fn the_pipeline_starts_with_the_five_queues_and_their_partition_counts() {
    let test_db = sqlite_db(outbox_migrations()).await;
    let specs = queue_specs(&cfg(8), 300);
    let handle = start_pipeline(test_db.db.clone(), &specs)
        .await
        .expect("pipeline starts");
    let outbox = Arc::clone(handle.outbox());
    let conn = test_db.provider.conn().unwrap();

    for spec in &specs {
        for partition in [0, spec.partitions - 1] {
            let record = Record::to(&spec.name, partition)
                .payload(b"{}".to_vec(), "mini_chat.test")
                .build()
                .unwrap();
            outbox
                .enqueue(&conn, record)
                .await
                .unwrap_or_else(|e| panic!("{} partition {partition}: {e}", spec.name))
                .fire();
        }
        let record = Record::to(&spec.name, spec.partitions)
            .payload(b"{}".to_vec(), "mini_chat.test")
            .build()
            .unwrap();
        let err = outbox.enqueue(&conn, record).await.unwrap_err();
        assert!(
            matches!(err, OutboxError::PartitionOutOfRange { .. }),
            "{}: {err:?}",
            spec.name
        );
    }

    let record = Record::to("mini-chat.not_registered", 0)
        .payload(b"{}".to_vec(), "mini_chat.test")
        .build()
        .unwrap();
    let err = outbox.enqueue(&conn, record).await.unwrap_err();
    assert!(matches!(err, OutboxError::QueueNotRegistered(_)), "{err:?}");

    handle.stop().await;
}

#[tokio::test]
async fn the_outbox_tables_exist_after_the_platform_migrations() {
    let test_db = sqlite_db(outbox_migrations()).await;
    for table in [
        "toolkit_outbox_body",
        "toolkit_outbox_incoming",
        "toolkit_outbox_dead_letters",
    ] {
        let found = raw_i64(
            &test_db.raw,
            &format!(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '{table}'"
            ),
        )
        .await;
        assert_eq!(found, 1, "{table}");
    }
}

// ── transactional enqueue ────────────────────────────────────────────────────

async fn started(
    partitions: u32,
) -> (
    crate::test_support::TestDb,
    toolkit_db::outbox::OutboxHandle,
    Arc<OutboxEnqueuer>,
) {
    let test_db = sqlite_db(outbox_migrations()).await;
    let specs = queue_specs(&cfg(partitions), 300);
    let handle = start_pipeline(test_db.db.clone(), &specs)
        .await
        .expect("pipeline starts");
    let enqueuer = Arc::new(OutboxEnqueuer::new(specs));
    enqueuer.bind(handle.outbox());
    (test_db, handle, enqueuer)
}

async fn body_rows(test_db: &crate::test_support::TestDb) -> i64 {
    raw_i64(&test_db.raw, "SELECT COUNT(*) FROM toolkit_outbox_body").await
}

#[tokio::test]
async fn a_message_commits_with_the_caller_transaction() {
    let (test_db, handle, enqueuer) = started(4).await;
    let enq = Arc::clone(&enqueuer);
    let wake = test_db
        .provider
        .transaction(move |tx| {
            let enq = Arc::clone(&enq);
            Box::pin(async move {
                enq.enqueue_raw(
                    tx,
                    QueueKind::Usage,
                    TENANT_A,
                    b"{}".to_vec(),
                    "mini_chat.usage",
                    EnqueueSite::TurnRetry,
                )
                .await
            })
        })
        .await
        .expect("commit");
    drop(wake);
    assert_eq!(body_rows(&test_db).await, 1);
    handle.stop().await;
}

#[tokio::test]
async fn a_message_rolls_back_with_the_caller_transaction() {
    let (test_db, handle, enqueuer) = started(4).await;
    let enq = Arc::clone(&enqueuer);
    let outcome: Result<(), DomainError> = test_db
        .provider
        .transaction(move |tx| {
            let enq = Arc::clone(&enq);
            Box::pin(async move {
                let wake = enq
                    .enqueue_raw(
                        tx,
                        QueueKind::Audit,
                        TENANT_A,
                        b"{}".to_vec(),
                        "mini_chat.audit",
                        EnqueueSite::TurnDelete,
                    )
                    .await?;
                drop(wake);
                Err(DomainError::internal(
                    "domain write failed after the enqueue",
                ))
            })
        })
        .await;
    assert!(outcome.is_err());
    assert_eq!(
        body_rows(&test_db).await,
        0,
        "a rolled-back transaction leaves no outbox row"
    );
    handle.stop().await;
}

#[tokio::test]
async fn an_oversized_payload_is_reported_with_the_call_site() {
    let (test_db, handle, enqueuer) = started(4).await;
    let sites = [
        EnqueueSite::ChatDelete,
        EnqueueSite::AttachmentDelete,
        EnqueueSite::TurnRetry,
        EnqueueSite::TurnEdit,
        EnqueueSite::TurnDelete,
    ];
    for site in sites {
        let enq = Arc::clone(&enqueuer);
        let outcome = test_db
            .provider
            .transaction(move |tx| {
                let enq = Arc::clone(&enq);
                Box::pin(async move {
                    enq.enqueue_raw(
                        tx,
                        QueueKind::ChatCleanup,
                        Uuid::from_u128(9),
                        vec![b'x'; 70_000],
                        "mini_chat.cleanup",
                        site,
                    )
                    .await
                })
            })
            .await;
        match outcome {
            Err(DomainError::OutboxPayloadTooLarge { site: got }) => assert_eq!(got, site),
            other => panic!("expected OutboxPayloadTooLarge for {site:?}, got {other:?}"),
        }
    }
    assert_eq!(body_rows(&test_db).await, 0);
    handle.stop().await;
}

#[tokio::test]
async fn a_usage_event_with_a_foreign_dedupe_key_is_rejected_before_anything_is_written() {
    let (test_db, handle, enqueuer) = started(4).await;
    let foreign = usage_dedupe_key(TENANT_B, Uuid::from_u128(1), Uuid::from_u128(2));
    let event = usage_event(TENANT_A, Some(foreign));
    let enq = Arc::clone(&enqueuer);
    let outcome = test_db
        .provider
        .transaction(move |tx| {
            let enq = Arc::clone(&enq);
            Box::pin(async move { enq.enqueue_usage(tx, &event, EnqueueSite::TurnRetry).await })
        })
        .await;
    assert!(outcome.is_err());
    assert_eq!(body_rows(&test_db).await, 0);
    handle.stop().await;
}

#[tokio::test]
async fn a_usage_event_with_a_valid_dedupe_key_is_enqueued() {
    let (test_db, handle, enqueuer) = started(4).await;
    let key = usage_dedupe_key(TENANT_A, Uuid::from_u128(4), Uuid::from_u128(5));
    let event = usage_event(TENANT_A, Some(key));
    let enq = Arc::clone(&enqueuer);
    let wake = test_db
        .provider
        .transaction(move |tx| {
            let enq = Arc::clone(&enq);
            Box::pin(async move { enq.enqueue_usage(tx, &event, EnqueueSite::TurnRetry).await })
        })
        .await
        .expect("valid event is enqueued");
    drop(wake);
    assert_eq!(body_rows(&test_db).await, 1);
    handle.stop().await;
}

#[tokio::test]
async fn enqueueing_before_the_outbox_is_bound_is_an_internal_error() {
    let test_db = sqlite_db(outbox_migrations()).await;
    let enqueuer = Arc::new(OutboxEnqueuer::new(queue_specs(&cfg(4), 300)));
    let enq = Arc::clone(&enqueuer);
    let outcome = test_db
        .provider
        .transaction(move |tx| {
            let enq = Arc::clone(&enq);
            Box::pin(async move {
                enq.enqueue_raw(
                    tx,
                    QueueKind::Usage,
                    TENANT_A,
                    b"{}".to_vec(),
                    "mini_chat.usage",
                    EnqueueSite::TurnEdit,
                )
                .await
            })
        })
        .await;
    assert!(
        matches!(outcome, Err(DomainError::Internal { .. })),
        "{outcome:?}"
    );
}

// ── domain write and enqueue in ONE transaction ──────────────────────────────

/// The gear schema with the outbox pipeline bound to an enqueuer, plus one committed chat.
async fn started_with_chat() -> (
    crate::test_support::TestDb,
    toolkit_db::outbox::OutboxHandle,
    Arc<OutboxEnqueuer>,
    ChatRecord,
) {
    let test_db = sqlite_db(all_migrations()).await;
    let specs = queue_specs(&cfg(4), 300);
    let handle = start_pipeline(test_db.db.clone(), &specs)
        .await
        .expect("pipeline starts");
    let enqueuer = Arc::new(OutboxEnqueuer::new(specs));
    enqueuer.bind(handle.outbox());
    let chat = ChatRepo::create_chat(
        &test_db.provider.conn().unwrap(),
        &owner_scope(),
        &ctx(USER_A1, TENANT_A),
        NewChat {
            id: Uuid::new_v4(),
            model: "gpt-5.2".to_owned(),
            title: Some("chat".to_owned()),
        },
    )
    .await
    .expect("create chat");
    (test_db, handle, enqueuer, chat)
}

fn owner_scope() -> AccessScope {
    AccessScope::for_tenant(TENANT_A).ensure_owner(USER_A1)
}

fn message(content: &str) -> NewMessage {
    NewMessage {
        id: Uuid::new_v4(),
        request_id: None,
        role: "user".to_owned(),
        content: content.to_owned(),
    }
}

async fn live_chats(test_db: &crate::test_support::TestDb) -> i64 {
    raw_i64(
        &test_db.raw,
        "SELECT COUNT(*) FROM chats WHERE deleted_at IS NULL",
    )
    .await
}

async fn message_rows(test_db: &crate::test_support::TestDb) -> i64 {
    raw_i64(&test_db.raw, "SELECT COUNT(*) FROM messages").await
}

#[tokio::test]
async fn a_repository_write_and_an_enqueue_commit_in_one_transaction() {
    let (test_db, handle, enqueuer, chat) = started_with_chat().await;
    let (enq, scope, chat_id, record) =
        (Arc::clone(&enqueuer), owner_scope(), chat.id, chat.clone());
    let wake = test_db
        .provider
        .transaction(move |tx| {
            Box::pin(async move {
                ChatRepo::soft_delete_chat(tx, &scope, chat_id).await?;
                ChatRepo::add_message(tx, &record, message("last words")).await?;
                enq.enqueue_raw(
                    tx,
                    QueueKind::ChatCleanup,
                    chat_id,
                    b"{}".to_vec(),
                    "mini_chat.chat_cleanup",
                    EnqueueSite::ChatDelete,
                )
                .await
            })
        })
        .await
        .expect("the repository writes and the enqueue share one transaction");
    wake.fire();
    assert_eq!(live_chats(&test_db).await, 0, "the chat is soft-deleted");
    assert_eq!(message_rows(&test_db).await, 1);
    assert_eq!(body_rows(&test_db).await, 1);
    handle.stop().await;
}

#[tokio::test]
async fn a_failure_after_the_writes_rolls_back_the_domain_rows_and_the_outbox_row_together() {
    let (test_db, handle, enqueuer, chat) = started_with_chat().await;
    let (enq, scope, chat_id, record) =
        (Arc::clone(&enqueuer), owner_scope(), chat.id, chat.clone());
    let outcome: Result<(), DomainError> = test_db
        .provider
        .transaction(move |tx| {
            Box::pin(async move {
                ChatRepo::soft_delete_chat(tx, &scope, chat_id).await?;
                ChatRepo::add_message(tx, &record, message("never kept")).await?;
                let wake = enq
                    .enqueue_raw(
                        tx,
                        QueueKind::ChatCleanup,
                        chat_id,
                        b"{}".to_vec(),
                        "mini_chat.chat_cleanup",
                        EnqueueSite::ChatDelete,
                    )
                    .await?;
                drop(wake);
                Err(DomainError::internal("forced failure after the enqueue"))
            })
        })
        .await;
    assert!(outcome.is_err());
    assert_eq!(live_chats(&test_db).await, 1, "the soft-delete is undone");
    assert_eq!(message_rows(&test_db).await, 0, "the message is undone");
    assert_eq!(body_rows(&test_db).await, 0, "the outbox row is undone");
    handle.stop().await;
}

#[tokio::test]
async fn a_failing_enqueue_rolls_back_the_repository_write() {
    let (test_db, handle, enqueuer, chat) = started_with_chat().await;
    let (enq, scope, chat_id) = (Arc::clone(&enqueuer), owner_scope(), chat.id);
    let outcome = test_db
        .provider
        .transaction(move |tx| {
            Box::pin(async move {
                ChatRepo::soft_delete_chat(tx, &scope, chat_id).await?;
                enq.enqueue_raw(
                    tx,
                    QueueKind::ChatCleanup,
                    chat_id,
                    vec![b'x'; 70_000],
                    "mini_chat.chat_cleanup",
                    EnqueueSite::ChatDelete,
                )
                .await
            })
        })
        .await;
    assert!(matches!(
        outcome,
        Err(DomainError::OutboxPayloadTooLarge { .. })
    ));
    assert_eq!(live_chats(&test_db).await, 1);
    assert_eq!(body_rows(&test_db).await, 0);
    handle.stop().await;
}

// ── slice S2-deletion-cleanup-tools of FEATURE 0005: the cleanup handler slot ───────────────────
//
// Algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler` (the placeholder
// `UnwiredHandler` is replaced) and `-configuration` (`inst-cfg-workers`: `outbox.cleanup_queue_name`);
// definition of done `cpt-cf-mini-chat-dod-attachments-and-file-search-cleanup-handler`.

mod cleanup_handler_slot {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

    use super::*;
    use crate::infra::outbox::{QueueHandlers, start_pipeline_with_handlers};

    /// A handler that records the payload types of the messages it receives and acknowledges them.
    struct Recording {
        seen: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl LeasedMessageHandler for Recording {
        async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
            self.seen.lock().unwrap().push(msg.payload_type.clone());
            MessageResult::Ok
        }
    }

    async fn deliver_one(cleanup_queue_name: &str, handler: Option<Arc<Recording>>) -> Vec<String> {
        let test_db = sqlite_db(outbox_migrations()).await;
        let mut config = cfg(4);
        config.cleanup_queue_name = cleanup_queue_name.to_owned();
        let specs = queue_specs(&config, 300);
        let handlers = QueueHandlers {
            attachment_cleanup: handler
                .clone()
                .map(|handler| handler as Arc<dyn LeasedMessageHandler>),
            ..QueueHandlers::default()
        };
        let handle = start_pipeline_with_handlers(test_db.db.clone(), &specs, &handlers)
            .await
            .expect("pipeline starts");
        let outbox = Arc::clone(handle.outbox());
        let conn = test_db.provider.conn().unwrap();
        let record = Record::to(cleanup_queue_name, 0)
            .payload(b"{}".to_vec(), "mini_chat.attachment_cleanup")
            .build()
            .unwrap();
        outbox.enqueue(&conn, record).await.unwrap().fire();
        let mut seen = Vec::new();
        for _ in 0..100 {
            if let Some(handler) = &handler {
                seen = handler.seen.lock().unwrap().clone();
                if !seen.is_empty() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        handle.stop().await;
        seen
    }

    #[tokio::test]
    async fn the_attachment_cleanup_queue_is_served_by_the_wired_handler() {
        let handler = Arc::new(Recording {
            seen: Mutex::new(Vec::new()),
        });
        let seen = deliver_one("mini-chat.attachment_cleanup", Some(handler)).await;
        assert_eq!(seen, vec!["mini_chat.attachment_cleanup".to_owned()]);
    }

    #[tokio::test]
    async fn a_configured_cleanup_queue_name_is_served_by_the_wired_handler() {
        // inst-cfg-workers: `outbox.cleanup_queue_name`
        let handler = Arc::new(Recording {
            seen: Mutex::new(Vec::new()),
        });
        let seen = deliver_one("custom.attachment_cleanup", Some(handler)).await;
        assert_eq!(seen, vec!["mini_chat.attachment_cleanup".to_owned()]);
    }

    #[tokio::test]
    async fn the_handler_of_the_cleanup_queue_does_not_serve_the_other_queues() {
        let test_db = sqlite_db(outbox_migrations()).await;
        let specs = queue_specs(&cfg(4), 300);
        let handler = Arc::new(Recording {
            seen: Mutex::new(Vec::new()),
        });
        let handlers = QueueHandlers {
            attachment_cleanup: Some(handler.clone() as Arc<dyn LeasedMessageHandler>),
            ..QueueHandlers::default()
        };
        let handle = start_pipeline_with_handlers(test_db.db.clone(), &specs, &handlers)
            .await
            .expect("pipeline starts");
        let outbox = Arc::clone(handle.outbox());
        let conn = test_db.provider.conn().unwrap();
        for spec in specs
            .iter()
            .filter(|spec| spec.kind != QueueKind::AttachmentCleanup)
        {
            let record = Record::to(&spec.name, 0)
                .payload(b"{}".to_vec(), "mini_chat.other")
                .build()
                .unwrap();
            outbox.enqueue(&conn, record).await.unwrap().fire();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(handler.seen.lock().unwrap().is_empty());
        handle.stop().await;
    }
}
