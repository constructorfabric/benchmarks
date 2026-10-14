//! The gear wiring of the message-streaming slice S2-provider-path: the real provider task
//! replaces the S1 placeholder, runs under the S2S context bound at start, and the orphan
//! watchdog is spawned at start when enabled and stops with the gear. Stubbed platform clients and
//! the provider mock; no network is used.
//!
//! Slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithms
//! `provider-call`, `orphan-watchdog` and `configuration`; flow `send-message` (provider task
//! part); definitions of done `provider-adapters` and `orphan-watchdog`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use toolkit::Gear;
use toolkit::contracts::RunnableCapability;

use super::MiniChatGear;
use crate::domain::streaming::ProviderTaskPort;
use crate::infra::storage::migrations::all_migrations;
use crate::test_support::provider::{MockProvider, Reply, happy};
use crate::test_support::s2::{
    Platform, RecordingOagw, S2S_SUBJECT, ScriptedAuthN, platform, s2s_ctx,
};
use crate::test_support::streaming::PlanBuilder;
use crate::test_support::{TestDb, gear_ctx, minimal_gear_config, sqlite_db};

struct Rig {
    gear: Arc<MiniChatGear>,
    platform: Platform,
    #[cfg_attr(
        feature = "k8s",
        allow(dead_code, reason = "read by the non-k8s watchdog test")
    )]
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

fn provider_task(rig: &Rig) -> Arc<dyn ProviderTaskPort> {
    let services = rig.gear.services().expect("built by init");
    Arc::clone(
        &services
            .streaming
            .as_ref()
            .expect("the send service")
            .provider_task,
    )
}

fn config_with_watchdog(enabled: bool) -> Value {
    let mut config = minimal_gear_config();
    config["orphan_watchdog"] = json!({ "enabled": enabled });
    // the upload reaper (FEATURE 0005 slice S2) is a background task too: these tests count only
    // the watchdog
    config["upload_reaper"] = json!({ "enabled": false });
    config
}

#[tokio::test]
async fn before_start_the_provider_task_has_no_s2s_context_and_cannot_start() {
    let rig = rig_with(minimal_gear_config()).await;
    let upstream = MockProvider::new();
    rig.platform.oagw.proxy_via(upstream.clone());
    let started = provider_task(&rig)
        .start(PlanBuilder::new().build(), CancellationToken::new())
        .await;
    assert!(started.is_err(), "no S2S context before start");
    assert_eq!(upstream.call_count(), 0);
}

#[tokio::test]
async fn after_start_the_real_provider_task_calls_oagw_under_the_s2s_context() {
    let rig = rig_with(minimal_gear_config()).await;
    let upstream = MockProvider::answering(Reply::Sse(happy("hi", (1, 1))));
    rig.platform.oagw.proxy_via(upstream.clone());
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");

    let plan = PlanBuilder::new().build();
    let _events = provider_task(&rig)
        .start(plan, CancellationToken::new())
        .await
        .expect("the task starts");
    upstream.wait_for_calls(1).await;
    let call = upstream.only_call();
    assert_eq!(call.uri, "/azure.example.com/v1/responses");
    // the S2S context exchanged at start
    assert_eq!(call.subject, S2S_SUBJECT);
    assert_eq!(call.tenant, s2s_ctx().subject_tenant_id());
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

#[tokio::test]
async fn start_spawns_the_orphan_watchdog_only_when_it_is_enabled() {
    let enabled = rig_with(config_with_watchdog(true)).await;
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

    let disabled = rig_with(config_with_watchdog(false)).await;
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

#[tokio::test]
async fn stop_cancels_and_joins_the_watchdog() {
    let rig = rig_with(config_with_watchdog(true)).await;
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
async fn the_started_watchdog_finalizes_a_stale_turn_of_the_gear_database() {
    use std::time::Duration;

    use crate::test_support::chats::{a1, at, seed_chat};
    use crate::test_support::quota::{TurnSeed, seed_turn, turn_row};
    use crate::test_support::send::t;

    let mut config = minimal_gear_config();
    config["orphan_watchdog"] =
        json!({ "enabled": true, "timeout_secs": 90, "scan_interval_secs": 1 });
    let rig = rig_with(config).await;
    let chat = seed_chat(
        &rig.db,
        &a1(),
        uuid::Uuid::new_v4(),
        Some("stale"),
        "m",
        at(0),
    )
    .await;
    // started in 2023: long past the 90 s timeout of the real application clock
    let id = seed_turn(&rig.db, &TurnSeed::running(&chat, t(0))).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");

    let db = &rig.db;
    let mut state = String::new();
    for _ in 0..200 {
        state = turn_row(db, id).await.state;
        if state != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(state, "failed");
    assert_eq!(
        turn_row(db, id).await.error_code.as_deref(),
        Some("orphan_timeout")
    );
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}
