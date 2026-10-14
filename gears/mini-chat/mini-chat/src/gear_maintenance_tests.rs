//! The gear wiring of background maintenance: `start` registers the thread-summary and the
//! chat-cleanup handlers in place of the placeholders (also under configured queue names and with
//! `thread_summary_worker.enabled` false, which gates only the trigger), the started gear deletes
//! the provider files and the vector store of a soft-deleted chat through the mock OAGW, the
//! summary handler acknowledges the work of a deleted chat and dead-letters a malformed message,
//! `cleanup_worker.max_attempts` is the store reject budget, no gear-local polling task is spawned
//! for either path, and `stop` stops the shared outbox pipeline so an interrupted delivery is
//! redelivered and re-runs idempotently. Stubbed platform clients and the provider mock; no network
//! is used.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-handler-wiring` (`inst-hw-build-summary`,
//! `inst-hw-build-cleanup`, `inst-hw-register`, `inst-hw-outcomes`, `inst-hw-attempts`,
//! `inst-hw-no-local-worker`, `inst-hw-enabled-flag`, `inst-hw-stop`, `inst-hw-return`),
//! `cpt-cf-mini-chat-algo-background-maintenance-configuration` (`inst-cf-summary`,
//! `inst-cf-summary-elsewhere`, `inst-cf-cleanup`, `inst-cf-outbox`, `inst-cf-return`); flow
//! `cpt-cf-mini-chat-flow-background-maintenance-chat-deletion-cleanup` (`inst-cd-deliver`,
//! `inst-cd-handle`); definitions of done
//! `cpt-cf-mini-chat-dod-background-maintenance-handler-wiring` and
//! `cpt-cf-mini-chat-dod-background-maintenance-observability-config`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(
    feature = "k8s",
    allow(unused_imports, dead_code, reason = "read by the non-k8s tests")
)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit::Gear;
use toolkit::contracts::RunnableCapability;
use uuid::Uuid;

use super::MiniChatGear;
use crate::domain::chats::{ChatCleanupMessage, ChatCleanupQueue};
use crate::domain::streaming::TurnOutbox;
use crate::domain::streaming::summary::{Frontier, ThreadSummaryWork};
use crate::infra::storage::migrations::all_migrations;
use crate::test_support::attachments::{RowSeed, seed_row, seed_vector_store, soft_delete_chat};
use crate::test_support::chats::{a1, at, attachment_rows, seed_chat};
use crate::test_support::provider::{MockProvider, Reply};
use crate::test_support::s2::{Platform, RecordingOagw, ScriptedAuthN, platform};
use crate::test_support::send::{MessageSeed, seed_msg, t};
use crate::test_support::{TENANT_A, TestDb, gear_ctx, minimal_gear_config, raw_i64, sqlite_db};

struct Rig {
    gear: Arc<MiniChatGear>,
    platform: Platform,
    db: TestDb,
}

async fn rig_with(config: Value) -> Rig {
    let platform = platform(ScriptedAuthN::ok(), RecordingOagw::accepting());
    let db = sqlite_db(all_migrations()).await;
    let ctx = gear_ctx(
        "mini-chat",
        Some(config),
        Arc::clone(&platform.hub),
        Some(db.db.clone()),
    );
    let gear = Arc::new(MiniChatGear::default());
    gear.init(&ctx).await.expect("init succeeds");
    Rig { gear, platform, db }
}

/// A configuration with the orphan watchdog and the upload reaper off, so no task is spawned.
fn quiet() -> Value {
    let mut config = minimal_gear_config();
    config["orphan_watchdog"] = json!({ "enabled": false });
    config["upload_reaper"] = json!({ "enabled": false });
    config
}

async fn wait_for<F, Fut>(mut done: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if done().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn enqueue_cleanup(rig: &Rig, chat_id: Uuid) {
    let enqueuer = Arc::clone(rig.gear.enqueuer().expect("built by init"));
    let message = ChatCleanupMessage {
        tenant_id: TENANT_A,
        chat_id,
        chat_deleted_at: OffsetDateTime::now_utc(),
    };
    let wake = rig
        .db
        .provider
        .transaction(move |tx| {
            let enqueuer = Arc::clone(&enqueuer);
            Box::pin(async move { enqueuer.enqueue_chat_cleanup(tx, &message).await })
        })
        .await
        .expect("enqueue");
    wake.fire();
}

async fn dead_letters(rig: &Rig) -> i64 {
    raw_i64(
        &rig.db.raw,
        "SELECT COUNT(*) FROM toolkit_outbox_dead_letters",
    )
    .await
}

/// A soft-deleted chat with one pending attachment row (backend `openai`, the default provider)
/// and a bound vector store.
async fn deleted_chat_with_store(
    rig: &Rig,
) -> (crate::infra::storage::repo_impl::ChatRecord, Uuid) {
    let chat = seed_chat(&rig.db, &a1(), Uuid::new_v4(), Some("c"), "m", at(0)).await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .cleanup("pending")
            .backend("openai")
            .file_id("file-gear-7")
            .created(at(1)),
    )
    .await;
    seed_vector_store(&rig.db, &chat, Some("vs_gear_7"), "openai", at(2)).await;
    soft_delete_chat(&rig.db, chat.id, at(50)).await;
    (chat, id)
}

async fn cleanup_state(rig: &Rig, chat_id: Uuid) -> Option<String> {
    attachment_rows(&rig.db, chat_id)
        .await
        .remove(0)
        .cleanup_status
}

async fn store_rows(rig: &Rig) -> i64 {
    raw_i64(&rig.db.raw, "SELECT COUNT(*) FROM chat_vector_stores").await
}

// ── the chat-cleanup handler ─────────────────────────────────────────────────

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn the_started_gear_deletes_the_provider_files_and_the_store_of_a_deleted_chat() {
    // inst-hw-register, inst-hw-build-cleanup, inst-cd-deliver, inst-cd-handle: the placeholder of
    // `mini-chat.chat_cleanup` is replaced
    let rig = rig_with(quiet()).await;
    let upstream = MockProvider::answering(Reply::http(200, &json!({ "deleted": true })));
    rig.platform.oagw.proxy_via(upstream.clone());
    let (chat, _id) = deleted_chat_with_store(&rig).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    enqueue_cleanup(&rig, chat.id).await;
    let done = wait_for(|| async {
        cleanup_state(&rig, chat.id).await.as_deref() == Some("done") && store_rows(&rig).await == 0
    })
    .await;
    assert!(done, "the chat-cleanup message was never handled");
    let calls = upstream.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0].method, "DELETE");
    assert_eq!(calls[0].uri, "/api.openai.com/v1/files/file-gear-7");
    assert_eq!(calls[1].uri, "/api.openai.com/v1/vector_stores/vs_gear_7");
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn a_configured_chat_cleanup_queue_name_is_served_by_the_handler() {
    // inst-cf-outbox: `outbox.chat_cleanup_queue_name`
    let mut config = quiet();
    config["outbox"] = json!({ "chat_cleanup_queue_name": "custom.chat_cleanup" });
    let rig = rig_with(config).await;
    rig.platform
        .oagw
        .proxy_via(MockProvider::answering(Reply::http(
            200,
            &json!({ "deleted": true }),
        )));
    let (chat, _id) = deleted_chat_with_store(&rig).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    enqueue_cleanup(&rig, chat.id).await;
    let done =
        wait_for(|| async { cleanup_state(&rig, chat.id).await.as_deref() == Some("done") }).await;
    assert!(done, "the configured queue has no handler");
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn the_store_reject_budget_is_cleanup_worker_max_attempts() {
    // inst-cf-cleanup, inst-hw-outcomes: with a budget of 1 the first failed store delete rejects
    // the message (dead letter) and keeps the row for a replay
    let mut config = quiet();
    config["cleanup_worker"] = json!({ "max_attempts": 1 });
    let rig = rig_with(config).await;
    let upstream = MockProvider::answering(Reply::http(500, &json!({ "error": "boom" })));
    rig.platform.oagw.proxy_via(upstream.clone());
    let chat = seed_chat(&rig.db, &a1(), Uuid::new_v4(), Some("c"), "m", at(0)).await;
    seed_vector_store(&rig.db, &chat, Some("vs_stuck"), "openai", at(2)).await;
    soft_delete_chat(&rig.db, chat.id, at(50)).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    enqueue_cleanup(&rig, chat.id).await;
    assert!(
        wait_for(|| async { dead_letters(&rig).await == 1 }).await,
        "no dead letter"
    );
    assert_eq!(store_rows(&rig).await, 1, "the row is kept for the replay");
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn a_malformed_chat_cleanup_message_is_dead_lettered() {
    // inst-hw-outcomes: `Reject` for a payload that can never succeed
    use crate::domain::error::EnqueueSite;
    use crate::infra::outbox::QueueKind;
    let rig = rig_with(quiet()).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    let enqueuer = Arc::clone(rig.gear.enqueuer().expect("built by init"));
    let wake = rig
        .db
        .provider
        .transaction(move |tx| {
            let enqueuer = Arc::clone(&enqueuer);
            Box::pin(async move {
                enqueuer
                    .enqueue_raw(
                        tx,
                        QueueKind::ChatCleanup,
                        TENANT_A,
                        b"not a cleanup payload".to_vec(),
                        "mini_chat.chat_cleanup",
                        EnqueueSite::ChatDelete,
                    )
                    .await
            })
        })
        .await
        .expect("enqueue");
    wake.fire();
    assert!(
        wait_for(|| async { dead_letters(&rig).await == 1 }).await,
        "not dead-lettered"
    );
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

// ── the thread-summary handler ───────────────────────────────────────────────

async fn enqueue_summary_for_deleted_chat(rig: &Rig) -> Uuid {
    let chat = seed_chat(&rig.db, &a1(), Uuid::new_v4(), Some("c"), "m", at(0)).await;
    let message = seed_msg(&rig.db, &chat, MessageSeed::new("user", "hello", t(10))).await;
    soft_delete_chat(&rig.db, chat.id, at(50)).await;
    let work = ThreadSummaryWork {
        tenant_id: chat.tenant_id,
        chat_id: chat.id,
        system_request_id: Uuid::new_v4(),
        base_frontier: None,
        frozen_target: Frontier {
            created_at: t(10),
            message_id: message,
        },
    };
    let enqueuer = Arc::clone(rig.gear.enqueuer().expect("built by init"));
    let wake = rig
        .db
        .provider
        .transaction(move |tx| {
            let enqueuer = Arc::clone(&enqueuer);
            Box::pin(async move { enqueuer.enqueue_thread_summary(tx, &work).await })
        })
        .await
        .expect("enqueue");
    wake.fire();
    chat.id
}

/// How many messages the processors of the shared outbox acknowledged (the sum of the processed
/// sequence numbers of the partitions); a placeholder handler never acknowledges.
async fn acknowledged(rig: &Rig) -> i64 {
    raw_i64(
        &rig.db.raw,
        "SELECT COALESCE(SUM(processed_seq), 0) FROM toolkit_outbox_processor",
    )
    .await
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn the_started_gear_acknowledges_the_summary_work_of_a_deleted_chat() {
    // inst-hw-register, inst-hw-build-summary: the placeholder would retry forever; the wired
    // handler acknowledges the work of a deleted chat without a provider call
    let rig = rig_with(quiet()).await;
    let upstream = MockProvider::answering(Reply::http(200, &json!({})));
    rig.platform.oagw.proxy_via(upstream.clone());
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    enqueue_summary_for_deleted_chat(&rig).await;
    assert!(
        wait_for(|| async { acknowledged(&rig).await >= 1 }).await,
        "never acknowledged"
    );
    assert_eq!(upstream.call_count(), 0);
    assert_eq!(dead_letters(&rig).await, 0);
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn the_summary_handler_stays_wired_when_the_worker_flag_is_off() {
    // inst-hw-enabled-flag, inst-cf-summary-elsewhere: `thread_summary_worker.enabled` gates only
    // the trigger, so already enqueued work still runs; the deprecated key changes nothing
    let mut config = quiet();
    config["thread_summary_worker"] = json!({ "enabled": false, "reconcile_interval_secs": 5 });
    let rig = rig_with(config).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    enqueue_summary_for_deleted_chat(&rig).await;
    assert!(
        wait_for(|| async { acknowledged(&rig).await >= 1 }).await,
        "never acknowledged"
    );
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn a_malformed_summary_message_is_dead_lettered_for_the_operator() {
    // inst-hw-outcomes: the dead-letter store of the shared outbox
    use crate::domain::error::EnqueueSite;
    use crate::infra::outbox::QueueKind;
    let rig = rig_with(quiet()).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    let enqueuer = Arc::clone(rig.gear.enqueuer().expect("built by init"));
    let wake = rig
        .db
        .provider
        .transaction(move |tx| {
            let enqueuer = Arc::clone(&enqueuer);
            Box::pin(async move {
                enqueuer
                    .enqueue_raw(
                        tx,
                        QueueKind::ThreadSummary,
                        TENANT_A,
                        b"{\"tenant_id\": \"nope\"}".to_vec(),
                        "mini_chat.thread_summary",
                        EnqueueSite::TurnFinalize,
                    )
                    .await
            })
        })
        .await
        .expect("enqueue");
    wake.fire();
    assert!(
        wait_for(|| async { dead_letters(&rig).await == 1 }).await,
        "not dead-lettered"
    );
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

// ── no local worker, stop ────────────────────────────────────────────────────

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn no_gear_local_task_is_spawned_for_either_path() {
    // inst-hw-no-local-worker: no polling loop, claim, leader-elected worker or task state machine
    let rig = rig_with(quiet()).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    assert_eq!(rig.gear.background_tasks().unwrap().live_count(), 0);
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn stop_stops_the_shared_pipeline_and_a_redelivery_reruns_idempotently() {
    // inst-hw-stop: an interrupted delivery is redelivered after its lease and re-runs
    // idempotently: a second message for the same chat finds nothing left to do
    let rig = rig_with(quiet()).await;
    let upstream = MockProvider::answering(Reply::http(200, &json!({ "deleted": true })));
    rig.platform.oagw.proxy_via(upstream.clone());
    let (chat, _id) = deleted_chat_with_store(&rig).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    enqueue_cleanup(&rig, chat.id).await;
    assert!(wait_for(|| async { store_rows(&rig).await == 0 }).await);
    let calls = upstream.call_count();
    enqueue_cleanup(&rig, chat.id).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(upstream.call_count(), calls, "the replay changes nothing");
    assert!(rig.gear.outbox_running().await);
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
    assert!(
        !rig.gear.outbox_running().await,
        "the outbox pipeline stopped with the gear"
    );
}
