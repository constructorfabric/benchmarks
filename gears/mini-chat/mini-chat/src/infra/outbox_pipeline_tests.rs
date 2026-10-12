#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;
use uuid::Uuid;

use super::*;
use crate::domain::error::DomainError;
use crate::infra::db::repo::tests::TestStore;

const TENANT: Uuid = Uuid::from_u128(0x7e7a);

/// Acknowledges every message and records its payload.
#[derive(Default)]
struct Recording {
    payloads: Mutex<Vec<serde_json::Value>>,
}

#[async_trait]
impl LeasedMessageHandler for Recording {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload = serde_json::from_slice(&msg.payload).unwrap();
        self.payloads.lock().unwrap().push(payload);
        MessageResult::Ok
    }
}

/// Delegates to a [`DeferredHandler`] and counts the deliveries.
struct CountingDeferred {
    inner: DeferredHandler,
    calls: AtomicUsize,
}

#[async_trait]
impl LeasedMessageHandler for CountingDeferred {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        LeasedMessageHandler::handle(&self.inner, msg).await
    }
}

async fn enqueue(store: &TestStore, queue: MiniChatQueue, payload: serde_json::Value) {
    let enqueuer = Arc::clone(&store.enqueuer);
    toolkit_db::outbox::in_transaction(&store.db, move |tx| {
        Box::pin(async move {
            let wake = enqueuer
                .enqueue_in_tx(tx, queue, TENANT, &payload)
                .await
                .map_err(DomainError::from)?;
            Ok::<_, DomainError>(((), wake))
        })
    })
    .await
    .unwrap();
}

async fn wait_for(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    done()
}

#[test]
fn leases_follow_the_design() {
    let mut config = MiniChatConfig::default();
    config.thread_summary_worker.claim_timeout_secs = 45;
    let default = LeaseConfig::default();

    for queue in [
        MiniChatQueue::Usage,
        MiniChatQueue::AttachmentCleanup,
        MiniChatQueue::ChatCleanup,
    ] {
        let lease = queue_lease(queue, &config);
        assert_eq!(lease.duration, Duration::from_secs(30), "{queue:?}");
        assert_eq!(lease.headroom, default.headroom, "{queue:?}");
    }
    assert_eq!(
        queue_lease(MiniChatQueue::ThreadSummary, &config).duration,
        Duration::from_secs(45)
    );
    assert_eq!(
        queue_lease(MiniChatQueue::Audit, &config).duration,
        Duration::from_secs(60)
    );
}

#[tokio::test]
async fn deferred_handler_keeps_every_message() {
    let handler = DeferredHandler::new("mini-chat.audit");
    let msg = OutboxMessage {
        partition_id: 1,
        seq: 1,
        payload: b"{}".to_vec(),
        payload_type: "application/json".to_owned(),
        created_at: chrono::Utc::now(),
        attempts: 3,
    };

    let result = LeasedMessageHandler::handle(&handler, &msg).await;

    assert!(matches!(result, MessageResult::Retry), "{result:?}");
    assert_eq!(handler.queue(), "mini-chat.audit");
}

#[test]
fn handlers_default_to_deferred_and_are_replaced_per_queue() {
    let handlers = QueueHandlers::deferred(&OutboxQueues::default());
    let replacement: Arc<dyn LeasedHandler> = Arc::new(Recording::default());

    let handlers = handlers.with(MiniChatQueue::Audit, Arc::clone(&replacement));

    assert!(Arc::ptr_eq(
        &handlers.get(MiniChatQueue::Audit),
        &replacement
    ));
    for queue in MiniChatQueue::ALL {
        if queue != MiniChatQueue::Audit {
            assert!(
                !Arc::ptr_eq(&handlers.get(queue), &replacement),
                "{queue:?}"
            );
        }
    }
}

#[tokio::test]
async fn start_registers_every_queue_and_attaches_the_enqueuer() {
    let store = TestStore::without_outbox().await;
    let queues = store.enqueuer.queues().clone();
    let recording = Arc::new(Recording::default());
    let deferred = Arc::new(CountingDeferred {
        inner: DeferredHandler::new(queues.name(MiniChatQueue::Audit)),
        calls: AtomicUsize::new(0),
    });
    let handlers = QueueHandlers::deferred(&queues)
        .with(MiniChatQueue::ChatCleanup, Arc::clone(&recording) as _)
        .with(MiniChatQueue::Audit, Arc::clone(&deferred) as _);
    assert!(!store.enqueuer.is_attached());

    let pipeline = start(
        store.db.clone(),
        &MiniChatConfig::default(),
        &handlers,
        &store.enqueuer,
    )
    .await
    .unwrap();

    assert!(store.enqueuer.is_attached());
    for queue in MiniChatQueue::ALL {
        enqueue(&store, queue, json!({ "queue": queues.name(queue) })).await;
    }
    assert!(
        wait_for(|| !recording.payloads.lock().unwrap().is_empty()).await,
        "the chat-cleanup handler receives its message"
    );
    assert_eq!(
        *recording.payloads.lock().unwrap(),
        vec![json!({ "queue": "mini-chat.chat_cleanup" })]
    );
    assert!(
        wait_for(|| deferred.calls.load(Ordering::SeqCst) >= 2).await,
        "a deferred message is redelivered, not acknowledged"
    );

    pipeline.stop().await;
}
