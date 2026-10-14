//! The handler slots of the shared outbox for the two background queues: the wired thread-summary
//! and chat-cleanup handlers replace the placeholder `UnwiredHandler` (default and configured
//! queue names), a handler serves only its own queue, the partitioning by `chat_id`, the
//! lease of the summary queue (`thread_summary_worker.claim_timeout_secs`) and the shared default
//! lease of chat cleanup stay unchanged, and a rejected message moves to the shared dead-letter
//! store while a malformed message of either queue is never retried.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-handler-wiring` (`inst-hw-register`,
//! `inst-hw-outcomes`, `inst-hw-no-local-worker`, `inst-hw-return`) and
//! `cpt-cf-mini-chat-algo-background-maintenance-configuration` (`inst-cf-outbox`); flow
//! `cpt-cf-mini-chat-flow-background-maintenance-admin-observability` (`inst-adm-outbox-surface`);
//! definition of done `cpt-cf-mini-chat-dod-background-maintenance-handler-wiring`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use toolkit_db::outbox::{
    LeasedMessageHandler, MessageResult, OutboxMessage, Record, outbox_migrations,
};

use crate::config::OutboxConfig;
use crate::infra::outbox::{
    PartitionBy, QueueHandlers, QueueKind, queue_specs, start_pipeline_with_handlers,
};
use crate::test_support::{raw_i64, sqlite_db};

/// A handler that records the payload types it receives and answers with `answer`.
struct Recording {
    seen: Mutex<Vec<String>>,
    answer: Mutex<MessageResult>,
}

impl Recording {
    fn new(answer: MessageResult) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            answer: Mutex::new(answer),
        })
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl LeasedMessageHandler for Recording {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        self.seen.lock().unwrap().push(msg.payload_type.clone());
        match &*self.answer.lock().unwrap() {
            MessageResult::Ok => MessageResult::Ok,
            MessageResult::Retry => MessageResult::Retry,
            MessageResult::Reject(reason) => MessageResult::Reject(reason.clone()),
        }
    }
}

fn config(summary_queue: &str, cleanup_queue: &str) -> OutboxConfig {
    OutboxConfig {
        queue_name: "mini-chat.usage_snapshot".to_owned(),
        cleanup_queue_name: "mini-chat.attachment_cleanup".to_owned(),
        chat_cleanup_queue_name: cleanup_queue.to_owned(),
        thread_summary_queue_name: summary_queue.to_owned(),
        audit_queue_name: "mini-chat.audit".to_owned(),
        num_partitions: 4,
    }
}

/// Start the pipeline with the given handlers, enqueue one message per `(queue, payload_type)` and
/// wait until `done` holds (3 s at most).
async fn deliver(
    config: &OutboxConfig,
    handlers: &QueueHandlers,
    messages: &[(&str, &str, &[u8])],
    done: impl Fn() -> bool,
) -> crate::test_support::TestDb {
    let test_db = sqlite_db(outbox_migrations()).await;
    let specs = queue_specs(config, 300);
    let handle = start_pipeline_with_handlers(test_db.db.clone(), &specs, handlers)
        .await
        .expect("pipeline starts");
    let outbox = Arc::clone(handle.outbox());
    let conn = test_db.provider.conn().unwrap();
    for (queue, payload_type, payload) in messages {
        let record = Record::to(queue, 0)
            .payload(payload.to_vec(), payload_type)
            .build()
            .unwrap();
        outbox.enqueue(&conn, record).await.unwrap().fire();
    }
    for _ in 0..150 {
        if done() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.stop().await;
    test_db
}

#[tokio::test]
async fn the_thread_summary_queue_is_served_by_the_wired_handler() {
    // inst-hw-register: the handler slot of the queue
    let handler = Recording::new(MessageResult::Ok);
    let handlers = QueueHandlers {
        thread_summary: Some(handler.clone() as Arc<dyn LeasedMessageHandler>),
        ..QueueHandlers::default()
    };
    let probe = Arc::clone(&handler);
    deliver(
        &config("mini-chat.thread_summary", "mini-chat.chat_cleanup"),
        &handlers,
        &[(
            "mini-chat.thread_summary",
            "mini_chat.thread_summary",
            b"{}",
        )],
        || !probe.seen().is_empty(),
    )
    .await;
    assert_eq!(handler.seen(), vec!["mini_chat.thread_summary".to_owned()]);
}

#[tokio::test]
async fn the_chat_cleanup_queue_is_served_by_the_wired_handler() {
    // inst-hw-register
    let handler = Recording::new(MessageResult::Ok);
    let handlers = QueueHandlers {
        chat_cleanup: Some(handler.clone() as Arc<dyn LeasedMessageHandler>),
        ..QueueHandlers::default()
    };
    let probe = Arc::clone(&handler);
    deliver(
        &config("mini-chat.thread_summary", "mini-chat.chat_cleanup"),
        &handlers,
        &[("mini-chat.chat_cleanup", "mini_chat.chat_cleanup", b"{}")],
        || !probe.seen().is_empty(),
    )
    .await;
    assert_eq!(handler.seen(), vec!["mini_chat.chat_cleanup".to_owned()]);
}

#[tokio::test]
async fn configured_queue_names_are_served_by_the_wired_handlers() {
    // inst-cf-outbox: `outbox.thread_summary_queue_name` and `outbox.chat_cleanup_queue_name`
    let summary = Recording::new(MessageResult::Ok);
    let cleanup = Recording::new(MessageResult::Ok);
    let handlers = QueueHandlers {
        thread_summary: Some(summary.clone() as Arc<dyn LeasedMessageHandler>),
        chat_cleanup: Some(cleanup.clone() as Arc<dyn LeasedMessageHandler>),
        ..QueueHandlers::default()
    };
    let (a, b) = (Arc::clone(&summary), Arc::clone(&cleanup));
    deliver(
        &config("custom.summary", "custom.chat_cleanup"),
        &handlers,
        &[
            ("custom.summary", "mini_chat.thread_summary", b"{}"),
            ("custom.chat_cleanup", "mini_chat.chat_cleanup", b"{}"),
        ],
        || !a.seen().is_empty() && !b.seen().is_empty(),
    )
    .await;
    assert_eq!(summary.seen(), vec!["mini_chat.thread_summary".to_owned()]);
    assert_eq!(cleanup.seen(), vec!["mini_chat.chat_cleanup".to_owned()]);
}

#[tokio::test]
async fn a_handler_serves_only_its_own_queue() {
    // inst-hw-register: the other queues keep their own slots
    let summary = Recording::new(MessageResult::Ok);
    let cleanup = Recording::new(MessageResult::Ok);
    let handlers = QueueHandlers {
        thread_summary: Some(summary.clone() as Arc<dyn LeasedMessageHandler>),
        chat_cleanup: Some(cleanup.clone() as Arc<dyn LeasedMessageHandler>),
        ..QueueHandlers::default()
    };
    let probe = Arc::clone(&summary);
    deliver(
        &config("mini-chat.thread_summary", "mini-chat.chat_cleanup"),
        &handlers,
        &[
            (
                "mini-chat.thread_summary",
                "mini_chat.thread_summary",
                b"{}",
            ),
            ("mini-chat.usage_snapshot", "mini_chat.usage_event", b"{}"),
            (
                "mini-chat.attachment_cleanup",
                "mini_chat.attachment_cleanup",
                b"{}",
            ),
            ("mini-chat.audit", "mini_chat.audit", b"{}"),
        ],
        || !probe.seen().is_empty(),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(summary.seen(), vec!["mini_chat.thread_summary".to_owned()]);
    assert!(
        cleanup.seen().is_empty(),
        "the cleanup handler saw nothing of the other queues"
    );
}

#[tokio::test]
async fn a_rejected_message_moves_to_the_shared_dead_letter_store() {
    // inst-hw-outcomes, inst-adm-outbox-surface: `Reject(reason)` is dead-lettered by the shared
    // outbox; no gear-local store exists
    for (queue, payload_type, summary) in [
        ("mini-chat.thread_summary", "mini_chat.thread_summary", true),
        ("mini-chat.chat_cleanup", "mini_chat.chat_cleanup", false),
    ] {
        let handler = Recording::new(MessageResult::Reject("rejected by the test".to_owned()));
        let handlers = if summary {
            QueueHandlers {
                thread_summary: Some(handler.clone() as Arc<dyn LeasedMessageHandler>),
                ..QueueHandlers::default()
            }
        } else {
            QueueHandlers {
                chat_cleanup: Some(handler.clone() as Arc<dyn LeasedMessageHandler>),
                ..QueueHandlers::default()
            }
        };
        let probe = Arc::clone(&handler);
        let db = deliver(
            &config("mini-chat.thread_summary", "mini-chat.chat_cleanup"),
            &handlers,
            &[(queue, payload_type, b"{}")],
            || !probe.seen().is_empty(),
        )
        .await;
        let mut dead = 0;
        for _ in 0..100 {
            dead = raw_i64(&db.raw, "SELECT COUNT(*) FROM toolkit_outbox_dead_letters").await;
            if dead == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(dead, 1, "{queue}");
    }
}

#[tokio::test]
async fn a_retried_message_stays_queued_and_is_redelivered() {
    // inst-hw-outcomes: `Retry` leaves the message for the shared backoff
    let handler = Recording::new(MessageResult::Retry);
    let handlers = QueueHandlers {
        chat_cleanup: Some(handler.clone() as Arc<dyn LeasedMessageHandler>),
        ..QueueHandlers::default()
    };
    let probe = Arc::clone(&handler);
    let db = deliver(
        &config("mini-chat.thread_summary", "mini-chat.chat_cleanup"),
        &handlers,
        &[("mini-chat.chat_cleanup", "mini_chat.chat_cleanup", b"{}")],
        || probe.seen().len() >= 2,
    )
    .await;
    assert!(handler.seen().len() >= 2, "redelivered after its backoff");
    assert_eq!(
        raw_i64(&db.raw, "SELECT COUNT(*) FROM toolkit_outbox_dead_letters").await,
        0
    );
}

#[test]
fn the_partitioning_and_the_leases_of_both_queues_are_unchanged() {
    // inst-hw-register: both queues partition by `chat_id`; the summary queue's lease is
    // `thread_summary_worker.claim_timeout_secs`, chat cleanup keeps the shared default lease
    let specs = queue_specs(
        &config("mini-chat.thread_summary", "mini-chat.chat_cleanup"),
        120,
    );
    let summary = specs
        .iter()
        .find(|s| s.kind == QueueKind::ThreadSummary)
        .unwrap();
    let cleanup = specs
        .iter()
        .find(|s| s.kind == QueueKind::ChatCleanup)
        .unwrap();
    assert_eq!(summary.partition_by, PartitionBy::Chat);
    assert_eq!(cleanup.partition_by, PartitionBy::Chat);
    assert_eq!(summary.lease, Some(Duration::from_secs(120)));
    assert_eq!(cleanup.lease, None);
    assert!(specs.iter().all(|s| s.partitions == 4));
}

#[test]
fn no_gear_local_worker_state_is_part_of_the_handler_slots() {
    // inst-hw-no-local-worker, inst-hw-return: the handlers are plain slots next to the audit,
    // usage and attachment-cleanup ones
    let handlers = QueueHandlers::default();
    assert!(handlers.thread_summary.is_none() && handlers.chat_cleanup.is_none());
    assert!(
        handlers.audit.is_none()
            && handlers.usage.is_none()
            && handlers.attachment_cleanup.is_none()
    );
}
