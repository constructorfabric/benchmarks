//! The gear wiring of the attachments slice S1-upload-lifecycle: `init` builds the attachments
//! service next to the other services, the upload permits follow `rag.max_concurrent_uploads`,
//! and `stop` cancels the token the background indexing tasks watch. Stubbed platform clients; no
//! network is used.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`: algorithm
//! `cpt-cf-mini-chat-algo-attachments-and-file-search-configuration` (`inst-cfg-rag`) and the
//! background indexing stop on the gear shutdown.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use toolkit::Gear;
use toolkit::contracts::RunnableCapability;

use super::MiniChatGear;
use crate::infra::storage::migrations::all_migrations;
use crate::test_support::s2::{RecordingOagw, ScriptedAuthN, platform};
use crate::test_support::{gear_ctx, minimal_gear_config, sqlite_db};

async fn initialized(config: Value) -> Arc<MiniChatGear> {
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
    gear
}

#[tokio::test]
async fn init_builds_the_attachments_service_next_to_the_other_services() {
    let gear = initialized(minimal_gear_config()).await;
    let services = gear.services().expect("built by init");
    assert!(services.attachments.is_some(), "the upload and get flows");
    assert!(services.chats.is_some() && services.streaming.is_some());
}

#[tokio::test]
async fn init_accepts_the_bounds_of_the_upload_concurrency_limit() {
    for permits in [1, 256] {
        let mut config = minimal_gear_config();
        config["rag"] = json!({ "max_concurrent_uploads": permits });
        let gear = initialized(config).await;
        assert!(gear.services().unwrap().attachments.is_some(), "{permits}");
    }
}

#[tokio::test]
async fn stop_cancels_the_token_of_the_background_indexing_tasks() {
    // no `start` is needed: the token belongs to the services built by `init`
    let gear = initialized(minimal_gear_config()).await;
    let token = gear.uploads_shutdown().expect("built by init").clone();
    assert!(!token.is_cancelled(), "initialized gear");
    gear.stop(CancellationToken::new()).await.expect("stop");
    assert!(token.is_cancelled(), "the background tasks must stop");
}
