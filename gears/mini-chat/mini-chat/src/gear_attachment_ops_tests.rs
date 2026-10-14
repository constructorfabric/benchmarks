//! The gear wiring of the attachments slice S2-deletion-cleanup-tools: `start` spawns the upload
//! reaper on the leader gate when `upload_reaper.enabled`, `stop` cancels and joins it, the
//! started reaper fails a stale row of the gear database, the attachment cleanup handler replaces
//! the placeholder of the `outbox.cleanup_queue_name` queue (also under a configured queue name)
//! and honors `cleanup_worker.max_attempts`, and `init` registers the attachment tool
//! contributions with the send service. Stubbed platform clients and the provider mock; no
//! network is used.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`:
//! algorithms `cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper` (`inst-rp-disabled`,
//! `inst-rp-loop`, `inst-rp-return`), `-cleanup-handler` (the replaced placeholder), `-configuration`
//! (`inst-cfg-workers`, `inst-cfg-knowledge`); definitions of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-upload-reaper` and
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-cleanup-handler`.
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
use crate::domain::attachments::AttachmentKind;
use crate::domain::attachments::store::{
    AttachmentCleanupMessage, AttachmentCleanupQueue, CleanupEvent,
};
use crate::infra::storage::migrations::all_migrations;
use crate::test_support::attachments::{RowSeed, seed_row};
use crate::test_support::chats::{a1, at, attachment_rows, seed_chat};
use crate::test_support::provider::{MockProvider, Reply};
use crate::test_support::s2::{Platform, RecordingOagw, ScriptedAuthN, platform};
use crate::test_support::{TENANT_A, TestDb, gear_ctx, minimal_gear_config, sqlite_db};

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

/// A configuration with the orphan watchdog off, so the reaper is the only worker.
fn reaper_only(reaper: Value) -> Value {
    let mut config = minimal_gear_config();
    config["orphan_watchdog"] = json!({ "enabled": false });
    config["upload_reaper"] = reaper;
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

// ── the reaper task ──────────────────────────────────────────────────────────

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn start_spawns_the_upload_reaper_only_when_it_is_enabled() {
    // inst-rp-disabled: the gear spawns no task for a disabled reaper
    let enabled = rig_with(reaper_only(json!({ "enabled": true }))).await;
    enabled
        .gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    assert_eq!(enabled.gear.background_tasks().unwrap().live_count(), 1);
    enabled
        .gear
        .stop(CancellationToken::new())
        .await
        .expect("stop");

    let disabled = rig_with(reaper_only(json!({ "enabled": false }))).await;
    disabled
        .gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    assert_eq!(disabled.gear.background_tasks().unwrap().live_count(), 0);
    disabled
        .gear
        .stop(CancellationToken::new())
        .await
        .expect("stop");
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn stop_cancels_and_joins_the_reaper() {
    // inst-rp-return: the loop returns when the gear stops
    let rig = rig_with(reaper_only(json!({ "enabled": true }))).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    let tasks = Arc::clone(rig.gear.background_tasks().unwrap());
    assert_eq!(tasks.live_count(), 1);
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
    assert_eq!(tasks.live_count(), 0);
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn the_started_reaper_fails_a_stale_row_of_the_gear_database() {
    // inst-rp-loop, inst-rp-cutoff: the real clock and the real timer, `scan_interval_secs` 1
    let rig = rig_with(reaper_only(json!({
        "enabled": true, "scan_interval_secs": 1, "stale_after_secs": 60
    })))
    .await;
    let chat = seed_chat(&rig.db, &a1(), Uuid::new_v4(), Some("stale"), "m", at(0)).await;
    // updated in 2023: long past the 60 s threshold of the real application clock
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .status("uploaded")
            .backend("openai")
            .updated(at(0)),
    )
    .await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    let db = &rig.db;
    let reaped = wait_for(|| async {
        attachment_rows(db, chat.id)
            .await
            .iter()
            .any(|row| row.id == id && row.status == "failed")
    })
    .await;
    assert!(reaped, "the stale row was never reaped");
    let row = attachment_rows(&rig.db, chat.id).await.remove(0);
    assert_eq!(row.error_code.as_deref(), Some("upload_abandoned"));
    assert_eq!(
        row.cleanup_status
            .as_deref()
            .map(|s| s == "pending" || s == "done"),
        Some(true)
    );
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

// ── the cleanup handler ──────────────────────────────────────────────────────

fn deleted_message(chat_id: Uuid, attachment_id: Uuid) -> AttachmentCleanupMessage {
    AttachmentCleanupMessage {
        event_type: CleanupEvent::AttachmentDeleted,
        tenant_id: TENANT_A,
        chat_id,
        attachment_id,
        provider_file_id: Some("file-gear-1".to_owned()),
        storage_backend: "openai".to_owned(),
        attachment_kind: AttachmentKind::Document,
        enqueued_at: OffsetDateTime::now_utc(),
        secondary_ref: None,
    }
}

/// Enqueue the message through the gear's enqueuer in its own transaction and fire the wake.
async fn enqueue(rig: &Rig, message: AttachmentCleanupMessage) {
    let enqueuer = Arc::clone(rig.gear.enqueuer().expect("built by init"));
    let wake = rig
        .db
        .provider
        .transaction(move |tx| {
            let enqueuer = Arc::clone(&enqueuer);
            Box::pin(async move { enqueuer.enqueue_attachment_cleanup(tx, &message).await })
        })
        .await
        .expect("enqueue");
    wake.fire();
}

async fn cleanup_state(rig: &Rig, chat_id: Uuid) -> (Option<String>, i32) {
    let row = attachment_rows(&rig.db, chat_id).await.remove(0);
    (row.cleanup_status, row.cleanup_attempts)
}

async fn seeded_deleted_row(rig: &Rig) -> (crate::infra::storage::repo_impl::ChatRecord, Uuid) {
    let chat = seed_chat(&rig.db, &a1(), Uuid::new_v4(), Some("c"), "m", at(0)).await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .deleted(at(5))
            .cleanup("pending")
            .backend("openai")
            .file_id("file-gear-1")
            .updated(OffsetDateTime::now_utc()),
    )
    .await;
    (chat, id)
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn the_started_gear_deletes_the_provider_file_of_a_cleanup_message() {
    // the placeholder `UnwiredHandler` of `mini-chat.attachment_cleanup` is replaced
    let mut config = minimal_gear_config();
    config["orphan_watchdog"] = json!({ "enabled": false });
    config["upload_reaper"] = json!({ "enabled": false });
    let rig = rig_with(config).await;
    let upstream = MockProvider::answering(Reply::http(200, &json!({ "deleted": true })));
    rig.platform.oagw.proxy_via(upstream.clone());
    let (chat, id) = seeded_deleted_row(&rig).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    enqueue(&rig, deleted_message(chat.id, id)).await;
    let done =
        wait_for(|| async { cleanup_state(&rig, chat.id).await.0.as_deref() == Some("done") })
            .await;
    assert!(
        done,
        "the cleanup message was never handled: {:?}",
        cleanup_state(&rig, chat.id).await
    );
    let call = upstream.only_call();
    assert_eq!(call.method, "DELETE");
    assert_eq!(call.uri, "/api.openai.com/v1/files/file-gear-1");
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn a_configured_cleanup_queue_name_is_served_by_the_handler() {
    // inst-cfg-workers: `outbox.cleanup_queue_name`
    let mut config = minimal_gear_config();
    config["orphan_watchdog"] = json!({ "enabled": false });
    config["upload_reaper"] = json!({ "enabled": false });
    config["outbox"] = json!({ "cleanup_queue_name": "custom.attachment_cleanup" });
    let rig = rig_with(config).await;
    let upstream = MockProvider::answering(Reply::http(200, &json!({ "deleted": true })));
    rig.platform.oagw.proxy_via(upstream.clone());
    let (chat, id) = seeded_deleted_row(&rig).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    enqueue(&rig, deleted_message(chat.id, id)).await;
    let done =
        wait_for(|| async { cleanup_state(&rig, chat.id).await.0.as_deref() == Some("done") })
            .await;
    assert!(done, "the configured queue has no handler");
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn the_retry_budget_is_cleanup_worker_max_attempts() {
    // inst-cfg-workers: with a budget of 1 the first failed delete fails the row and rejects
    let mut config = minimal_gear_config();
    config["orphan_watchdog"] = json!({ "enabled": false });
    config["upload_reaper"] = json!({ "enabled": false });
    config["cleanup_worker"] = json!({ "max_attempts": 1 });
    let rig = rig_with(config).await;
    let upstream = MockProvider::answering(Reply::http(500, &json!({ "error": "boom" })));
    rig.platform.oagw.proxy_via(upstream.clone());
    let (chat, id) = seeded_deleted_row(&rig).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    enqueue(&rig, deleted_message(chat.id, id)).await;
    let failed =
        wait_for(|| async { cleanup_state(&rig, chat.id).await.0.as_deref() == Some("failed") })
            .await;
    assert!(failed, "{:?}", cleanup_state(&rig, chat.id).await);
    assert_eq!(cleanup_state(&rig, chat.id).await.1, 1);
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn a_malformed_cleanup_message_is_dead_lettered_for_the_operator() {
    // inst-ch-malformed, inst-adm-inspect: a rejected message moves to the shared outbox
    // dead-letter store
    use crate::domain::error::EnqueueSite;
    use crate::infra::outbox::QueueKind;
    let mut config = minimal_gear_config();
    config["orphan_watchdog"] = json!({ "enabled": false });
    config["upload_reaper"] = json!({ "enabled": false });
    let rig = rig_with(config).await;
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
                        QueueKind::AttachmentCleanup,
                        TENANT_A,
                        b"not a cleanup payload".to_vec(),
                        "mini_chat.attachment_cleanup",
                        EnqueueSite::AttachmentDelete,
                    )
                    .await
            })
        })
        .await
        .expect("enqueue");
    wake.fire();
    let db = &rig.db;
    let dead = wait_for(|| async {
        crate::test_support::raw_i64(&db.raw, "SELECT COUNT(*) FROM toolkit_outbox_dead_letters")
            .await
            == 1
    })
    .await;
    assert!(
        dead,
        "the malformed message never reached the dead-letter store"
    );
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

// ── the tool contributions ───────────────────────────────────────────────────

#[tokio::test]
async fn init_registers_the_attachment_tool_contributions_with_the_send_service() {
    // inst-ask-contributions, inst-cfg-knowledge, inst-cfg-policy
    let rig = rig_with(minimal_gear_config()).await;
    let services = rig.gear.services().expect("built by init");
    let streaming = services.streaming.as_ref().expect("the send service");
    assert!(
        streaming.attachment_tools.is_some(),
        "file_search, code_interpreter, search_knowledge"
    );
}

#[tokio::test]
async fn init_accepts_an_enabled_knowledge_search_over_a_configured_provider() {
    // inst-cfg-knowledge: `enabled`, `vector_store_id`, `provider_id`, `max_calls_per_message`,
    // `top_k`, `max_chunk_chars`, `guard`
    let mut config = minimal_gear_config();
    config["providers"] = json!({
        "kb": { "kind": "openai_responses", "host": "kb.example.com",
                 "storage_kind": "azure", "api_version": "2025-03-01-preview" }
    });
    config["knowledge_search"] = json!({
        "enabled": true, "vector_store_id": "vs_kb", "provider_id": "kb",
        "max_calls_per_message": 2, "top_k": 4, "max_chunk_chars": 500, "guard": "KB GUARD"
    });
    let rig = rig_with(config).await;
    let services = rig.gear.services().expect("built by init");
    assert!(
        services
            .streaming
            .as_ref()
            .unwrap()
            .attachment_tools
            .is_some()
    );
}
