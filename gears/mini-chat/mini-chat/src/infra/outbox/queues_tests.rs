#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of `cpt-cf-mini-chat-algo-gear-foundation-start-outbox`: the five
//! queues with names, partitions, leases and handlers, and the partition keys.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxError, OutboxMessage, Record};
use tracing::Level;
use uuid::Uuid;

use super::{
    AUDIT_QUEUE_LEASE, OutboxEnqueuer, OutboxHandlers, PartitionKey, PlaceholderHandler, QueueKind,
    partition_for, partition_key_of, queue_specs, start_outbox,
};
use crate::config::{OutboxConfig, ThreadSummaryWorkerConfig};
use crate::infra::db::test_support::TestDb;
use crate::test_doubles::{LogCapture, outbox_message};

fn specs() -> Vec<super::QueueSpec> {
    queue_specs(
        &OutboxConfig::default(),
        &ThreadSummaryWorkerConfig::default(),
    )
}

fn spec_of(kind: QueueKind, all: &[super::QueueSpec]) -> &super::QueueSpec {
    all.iter().find(|spec| spec.kind == kind).expect("spec")
}

// ---- inst-register-* ------------------------------------------------------------------------

#[test]
fn five_queues_are_registered_in_order() {
    let kinds: Vec<QueueKind> = specs().iter().map(|spec| spec.kind).collect();

    assert_eq!(
        kinds,
        vec![
            QueueKind::Usage,
            QueueKind::AttachmentCleanup,
            QueueKind::ChatCleanup,
            QueueKind::ThreadSummary,
            QueueKind::Audit,
        ]
    );
    assert_eq!(QueueKind::ALL.len(), 5);
}

#[test]
fn the_default_queue_names_are_used() {
    let specs = specs();

    let names: Vec<(QueueKind, &str)> = specs
        .iter()
        .map(|spec| (spec.kind, spec.name.as_str()))
        .collect();
    assert_eq!(
        names,
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
fn the_configured_queue_names_are_used() {
    let config = OutboxConfig {
        queue_name: "q.usage".to_owned(),
        cleanup_queue_name: "q.attachments".to_owned(),
        chat_cleanup_queue_name: "q.chats".to_owned(),
        thread_summary_queue_name: "q.summary".to_owned(),
        audit_queue_name: "q.audit".to_owned(),
        num_partitions: 4,
    };

    let specs = queue_specs(&config, &ThreadSummaryWorkerConfig::default());

    assert_eq!(spec_of(QueueKind::Usage, &specs).name, "q.usage");
    assert_eq!(
        spec_of(QueueKind::AttachmentCleanup, &specs).name,
        "q.attachments"
    );
    assert_eq!(spec_of(QueueKind::ChatCleanup, &specs).name, "q.chats");
    assert_eq!(spec_of(QueueKind::ThreadSummary, &specs).name, "q.summary");
    assert_eq!(spec_of(QueueKind::Audit, &specs).name, "q.audit");
}

#[test]
fn every_queue_uses_the_configured_number_of_partitions() {
    for partitions in [1_u32, 2, 8, 64] {
        let config = OutboxConfig {
            num_partitions: partitions,
            ..OutboxConfig::default()
        };

        let specs = queue_specs(&config, &ThreadSummaryWorkerConfig::default());

        assert_eq!(specs.len(), 5);
        for spec in &specs {
            assert_eq!(u32::from(spec.partitions), partitions, "{:?}", spec.kind);
        }
    }
}

#[test]
fn usage_and_cleanup_queues_use_the_default_thirty_second_lease() {
    let specs = specs();

    for kind in [
        QueueKind::Usage,
        QueueKind::AttachmentCleanup,
        QueueKind::ChatCleanup,
    ] {
        assert_eq!(
            spec_of(kind, &specs).lease.duration,
            Duration::from_secs(30),
            "{kind:?}"
        );
    }
}

#[test]
fn the_thread_summary_queue_lease_is_the_claim_timeout() {
    let default = specs();
    assert_eq!(
        spec_of(QueueKind::ThreadSummary, &default).lease.duration,
        Duration::from_secs(300)
    );

    let summary = ThreadSummaryWorkerConfig {
        claim_timeout_secs: 45,
        ..ThreadSummaryWorkerConfig::default()
    };
    let custom = queue_specs(&OutboxConfig::default(), &summary);
    assert_eq!(
        spec_of(QueueKind::ThreadSummary, &custom).lease.duration,
        Duration::from_secs(45)
    );
}

#[test]
fn the_audit_queue_lease_is_sixty_seconds() {
    assert_eq!(AUDIT_QUEUE_LEASE, Duration::from_secs(60));
    assert_eq!(
        spec_of(QueueKind::Audit, &specs()).lease.duration,
        Duration::from_secs(60)
    );
}

#[test]
fn every_lease_leaves_headroom_for_the_ack() {
    for spec in specs() {
        assert!(spec.lease.headroom < spec.lease.duration, "{:?}", spec.kind);
    }
}

#[test]
fn only_the_three_unowned_queues_carry_placeholder_handlers() {
    let specs = specs();

    for (kind, placeholder) in [
        (QueueKind::Usage, false),
        (QueueKind::Audit, false),
        (QueueKind::AttachmentCleanup, true),
        (QueueKind::ChatCleanup, true),
        (QueueKind::ThreadSummary, true),
    ] {
        assert_eq!(
            spec_of(kind, &specs).placeholder_handler,
            placeholder,
            "{kind:?}"
        );
    }
}

#[test]
fn queue_names_are_distinct() {
    let names: HashSet<String> = specs().into_iter().map(|spec| spec.name).collect();

    assert_eq!(names.len(), 5);
}

// ---- inst-placeholder-handlers ------------------------------------------------------------------

#[tokio::test]
async fn a_placeholder_handler_logs_a_warning_and_returns_retry() {
    let logs = LogCapture::start();
    let handler = PlaceholderHandler::new("mini-chat.chat_cleanup");

    let result = handler.handle(&outbox_message(b"{}".to_vec(), 0)).await;

    assert!(matches!(result, MessageResult::Retry), "{result:?}");
    let warnings = logs.at_containing(Level::WARN, "mini-chat.chat_cleanup");
    assert_eq!(warnings.len(), 1, "{:?}", logs.records());
}

#[tokio::test]
async fn a_placeholder_handler_retries_every_delivery() {
    let handler = PlaceholderHandler::new("mini-chat.thread_summary");

    for attempts in [0, 1, 50, 500] {
        let result = handler
            .handle(&outbox_message(b"x".to_vec(), attempts))
            .await;
        assert!(
            matches!(result, MessageResult::Retry),
            "attempt {attempts}: {result:?}"
        );
    }
}

// ---- inst-partitions ----------------------------------------------------------------------------

#[test]
fn usage_attachment_cleanup_and_audit_messages_partition_by_tenant() {
    assert_eq!(partition_key_of(QueueKind::Usage), PartitionKey::TenantId);
    assert_eq!(
        partition_key_of(QueueKind::AttachmentCleanup),
        PartitionKey::TenantId
    );
    assert_eq!(partition_key_of(QueueKind::Audit), PartitionKey::TenantId);
}

#[test]
fn chat_cleanup_and_thread_summary_messages_partition_by_chat() {
    assert_eq!(
        partition_key_of(QueueKind::ChatCleanup),
        PartitionKey::ChatId
    );
    assert_eq!(
        partition_key_of(QueueKind::ThreadSummary),
        PartitionKey::ChatId
    );
}

#[test]
fn a_partition_is_within_the_partition_count_and_stable() {
    for partitions in [1_u16, 2, 4, 8, 64] {
        for n in 0..200_u128 {
            let key = Uuid::from_u128(n.wrapping_mul(0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C835));
            let partition = partition_for(key, partitions);
            assert!(
                partition < u32::from(partitions),
                "{partition} of {partitions}"
            );
            assert_eq!(partition, partition_for(key, partitions), "stable");
        }
    }
}

#[test]
fn one_partition_always_yields_partition_zero() {
    for n in 0..50_u128 {
        assert_eq!(partition_for(Uuid::from_u128(n * 7919), 1), 0);
    }
}

#[test]
fn random_keys_spread_over_every_partition() {
    for partitions in [2_u16, 4, 8] {
        let used: HashSet<u32> = (0..512)
            .map(|_| partition_for(Uuid::new_v4(), partitions))
            .collect();

        assert_eq!(used.len(), usize::from(partitions), "{used:?}");
    }
}

#[test]
fn the_enqueuer_knows_the_configured_queue_names() {
    let config = OutboxConfig {
        queue_name: "q.usage".to_owned(),
        cleanup_queue_name: "q.attachments".to_owned(),
        chat_cleanup_queue_name: "q.chats".to_owned(),
        thread_summary_queue_name: "q.summary".to_owned(),
        audit_queue_name: "q.audit".to_owned(),
        num_partitions: 4,
    };

    let enqueuer = OutboxEnqueuer::new(&config);

    assert_eq!(enqueuer.queue_name(QueueKind::Usage), "q.usage");
    assert_eq!(
        enqueuer.queue_name(QueueKind::AttachmentCleanup),
        "q.attachments"
    );
    assert_eq!(enqueuer.queue_name(QueueKind::ChatCleanup), "q.chats");
    assert_eq!(enqueuer.queue_name(QueueKind::ThreadSummary), "q.summary");
    assert_eq!(enqueuer.queue_name(QueueKind::Audit), "q.audit");
}

#[test]
fn the_enqueuer_partitions_by_the_key_of_the_queue() {
    let config = OutboxConfig {
        num_partitions: 8,
        ..OutboxConfig::default()
    };
    let enqueuer = OutboxEnqueuer::new(&config);
    let mut by_tenant = HashSet::new();
    let mut by_chat = HashSet::new();

    for n in 1..100_u128 {
        let tenant = Uuid::from_u128(n * 0x1234_5678_9ABC_DEF1);
        let chat = Uuid::from_u128(n * 0x0FED_CBA9_8765_4321 + 17);
        let other_chat = Uuid::new_v4();
        let other_tenant = Uuid::new_v4();
        by_tenant.insert(enqueuer.partition(QueueKind::Usage, tenant, chat));
        by_chat.insert(enqueuer.partition(QueueKind::ChatCleanup, tenant, chat));

        for kind in [
            QueueKind::Usage,
            QueueKind::AttachmentCleanup,
            QueueKind::Audit,
        ] {
            assert_eq!(
                enqueuer.partition(kind, tenant, chat),
                partition_for(tenant, 8),
                "{kind:?} by tenant"
            );
            assert_eq!(
                enqueuer.partition(kind, tenant, chat),
                enqueuer.partition(kind, tenant, other_chat),
                "{kind:?} ignores the chat"
            );
        }
        for kind in [QueueKind::ChatCleanup, QueueKind::ThreadSummary] {
            assert_eq!(
                enqueuer.partition(kind, tenant, chat),
                partition_for(chat, 8),
                "{kind:?} by chat"
            );
            assert_eq!(
                enqueuer.partition(kind, tenant, chat),
                enqueuer.partition(kind, other_tenant, chat),
                "{kind:?} ignores the tenant"
            );
        }
    }
    assert!(
        by_tenant.len() > 1,
        "tenant-keyed partitions are spread: {by_tenant:?}"
    );
    assert!(
        by_chat.len() > 1,
        "chat-keyed partitions are spread: {by_chat:?}"
    );
}

// ---- inst-return-pipeline: the real pipeline on SQLite ------------------------------------------

#[derive(Default)]
struct CountingHandler {
    payloads: Mutex<Vec<Vec<u8>>>,
}

impl CountingHandler {
    fn count(&self) -> usize {
        self.payloads.lock().unwrap().len()
    }
}

#[async_trait]
impl LeasedMessageHandler for CountingHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        self.payloads.lock().unwrap().push(msg.payload.clone());
        MessageResult::Ok
    }
}

/// Forwards to a shared handler so the test can inspect it.
struct Shared(Arc<CountingHandler>);

#[async_trait]
impl LeasedMessageHandler for Shared {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        self.0.handle(msg).await
    }
}

async fn wait_for(handler: &CountingHandler, count: usize) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while handler.count() < count {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the message is delivered to its handler");
}

#[tokio::test]
async fn the_pipeline_runs_five_queues_and_routes_usage_and_audit_to_their_handlers() {
    let db = TestDb::migrated().await;
    let usage = Arc::new(CountingHandler::default());
    let audit = Arc::new(CountingHandler::default());
    let outbox_config = OutboxConfig {
        num_partitions: 2,
        ..OutboxConfig::default()
    };

    let handle = start_outbox(
        &db.db,
        &outbox_config,
        &ThreadSummaryWorkerConfig::default(),
        OutboxHandlers {
            usage: Arc::new(Shared(Arc::clone(&usage))),
            audit: Arc::new(Shared(Arc::clone(&audit))),
            attachment_cleanup: None,
            chat_cleanup: None,
            thread_summary: None,
        },
    )
    .await
    .expect("the pipeline starts");

    let conn = db.db.conn().expect("conn");
    let outbox = Arc::clone(handle.outbox());
    for kind in QueueKind::ALL {
        let name = OutboxEnqueuer::new(&outbox_config)
            .queue_name(kind)
            .to_owned();
        for partition in 0..2 {
            let record = Record::to(&name, partition)
                .payload(format!("{name}/{partition}").into_bytes(), "text/plain")
                .build()
                .expect("record");
            let wake = outbox
                .enqueue(&conn, record)
                .await
                .unwrap_or_else(|e| panic!("{name} partition {partition}: {e}"));
            wake.fire();
        }
    }
    let unknown = Record::to("mini-chat.unknown", 0)
        .payload(b"x".to_vec(), "text/plain")
        .build()
        .expect("record");
    assert!(matches!(
        outbox.enqueue(&conn, unknown).await,
        Err(OutboxError::QueueNotRegistered(_))
    ));
    let out_of_range = Record::to("mini-chat.audit", 2)
        .payload(b"x".to_vec(), "text/plain")
        .build()
        .expect("record");
    assert!(matches!(
        outbox.enqueue(&conn, out_of_range).await,
        Err(OutboxError::PartitionOutOfRange { .. })
    ));

    wait_for(&usage, 2).await;
    wait_for(&audit, 2).await;
    let mut usage_payloads: Vec<String> = usage
        .payloads
        .lock()
        .unwrap()
        .iter()
        .map(|p| String::from_utf8(p.clone()).unwrap())
        .collect();
    usage_payloads.sort();
    assert_eq!(
        usage_payloads,
        vec![
            "mini-chat.usage_snapshot/0".to_owned(),
            "mini-chat.usage_snapshot/1".to_owned()
        ]
    );
    handle.stop().await;
}

#[tokio::test]
async fn the_chat_cleanup_and_thread_summary_queues_route_to_their_handlers_in_place_of_the_placeholders()
 {
    let db = TestDb::migrated().await;
    let chat_cleanup = Arc::new(CountingHandler::default());
    let thread_summary = Arc::new(CountingHandler::default());
    let outbox_config = OutboxConfig {
        num_partitions: 2,
        ..OutboxConfig::default()
    };
    let handle = start_outbox(
        &db.db,
        &outbox_config,
        &ThreadSummaryWorkerConfig::default(),
        OutboxHandlers {
            usage: Arc::new(CountingHandler::default()),
            audit: Arc::new(CountingHandler::default()),
            attachment_cleanup: None,
            chat_cleanup: Some(Arc::new(Shared(Arc::clone(&chat_cleanup)))),
            thread_summary: Some(Arc::new(Shared(Arc::clone(&thread_summary)))),
        },
    )
    .await
    .expect("the pipeline starts");
    let conn = db.db.conn().expect("conn");
    let outbox = Arc::clone(handle.outbox());
    for (queue, payload) in [
        ("mini-chat.chat_cleanup", "chat-cleanup-message"),
        ("mini-chat.thread_summary", "thread-summary-message"),
    ] {
        let record = Record::to(queue, 0)
            .payload(payload.as_bytes().to_vec(), "text/plain")
            .build()
            .expect("record");
        outbox.enqueue(&conn, record).await.expect("enqueue").fire();
    }

    wait_for(&chat_cleanup, 1).await;
    wait_for(&thread_summary, 1).await;

    assert_eq!(
        chat_cleanup.payloads.lock().unwrap().clone(),
        vec![b"chat-cleanup-message".to_vec()]
    );
    assert_eq!(
        thread_summary.payloads.lock().unwrap().clone(),
        vec![b"thread-summary-message".to_vec()]
    );
    handle.stop().await;
}

#[tokio::test]
async fn an_invalid_partition_count_fails_the_start_instead_of_panicking() {
    let db = TestDb::migrated().await;
    let outbox_config = OutboxConfig {
        num_partitions: 3,
        ..OutboxConfig::default()
    };

    let result = start_outbox(
        &db.db,
        &outbox_config,
        &ThreadSummaryWorkerConfig::default(),
        OutboxHandlers {
            usage: Arc::new(CountingHandler::default()),
            audit: Arc::new(CountingHandler::default()),
            attachment_cleanup: None,
            chat_cleanup: None,
            thread_summary: None,
        },
    )
    .await;

    assert!(result.is_err());
}
