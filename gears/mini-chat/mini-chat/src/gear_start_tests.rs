//! Gear init and start/stop tests for slice S2: provider registry, gateways and metrics built
//! at init; S2S exchange, OAGW provisioning, the reconcile task, the leader elector, the
//! summary-model check, the workers spawn hook and the audit queue wiring at start; the
//! cancel-and-join order at stop. Stubbed platform clients; no network is used.
//!
//! Slice S2: algorithms `gear-init` (steps `inst-init-build-registry`,
//! `inst-init-create-gateways`, `inst-init-build-metrics-outbox`), `gear-start`, `gear-stop`,
//! `oagw-provisioning`, `oagw-reconcile`, `audit-delivery` (queue wiring); states
//! `gear-lifecycle` (S2 transitions) and `provider-provisioning`; definition of done
//! `provider-provisioning`, `audit`, `observability` (the instruments under the configured
//! `metrics.prefix`, for `cpt-cf-mini-chat-dod-message-streaming-observability`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::contracts::RunnableCapability;
use toolkit::gts::PluginV1;
use tracing_test::traced_test;
use types_registry_sdk::testing::make_test_instance;
use uuid::Uuid;

use super::{LifecycleState, MiniChatGear};
use crate::domain::error::EnqueueSite;
use crate::infra::audit_gateway::AUDIT_PAYLOAD_TYPE;
#[cfg(not(feature = "k8s"))]
use crate::infra::leader::LeaderRole;
use crate::infra::outbox::QueueKind;
use crate::infra::plugins::static_model_policy::config::StaticModelPolicyConfig;
use crate::infra::plugins::static_model_policy::service::StaticModelPolicyService;
use crate::infra::storage::migrations::all_migrations;
use crate::test_support::s2::{
    AuditScript, AuthNScript, Platform, RecordingOagw, S2S_SUBJECT, ScriptedAuditPlugin,
    ScriptedAuthN, UpstreamScript, all_audit_events, audit_instance, platform,
};
use crate::test_support::{TENANT_A, TestDb, entry_json, gear_ctx, minimal_gear_config, sqlite_db};

const GEAR_NAME: &str = "mini-chat";

struct Rig {
    gear: Arc<MiniChatGear>,
    platform: Platform,
    db: TestDb,
}

async fn rig_with(platform: Platform, config: Value) -> Rig {
    let db = sqlite_db(all_migrations()).await;
    let ctx = gear_ctx(
        GEAR_NAME,
        Some(config),
        Arc::clone(&platform.hub),
        Some(db.db.clone()),
    );
    let gear = Arc::new(MiniChatGear::default());
    gear.init(&ctx).await.expect("init succeeds");
    Rig { gear, platform, db }
}

async fn rig(config: Value) -> Rig {
    rig_with(
        platform(ScriptedAuthN::ok(), RecordingOagw::accepting()),
        config,
    )
    .await
}

/// The minimal configuration with the orphan watchdog disabled: the tests of the reconcile task
/// count the background tasks, and the watchdog (slice S2-provider-path of
/// `cpt-cf-mini-chat-feature-message-streaming`, spawned at start when enabled) is not what they
/// are about.
fn without_watchdog() -> Value {
    let mut config = minimal_gear_config();
    config["orphan_watchdog"] = json!({ "enabled": false });
    // the upload reaper (FEATURE 0005 slice S2) is a background task too: these tests count only
    // the reconcile task
    config["upload_reaper"] = json!({ "enabled": false });
    config
}

fn error_text(err: &anyhow::Error) -> String {
    format!("{err:#}").to_lowercase()
}

async fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    condition()
}

// ── init: provider registry, gateways, metrics ──────────────────────────────

#[tokio::test]
async fn init_builds_the_provider_registry_with_the_default_openai_entry() {
    let rig = rig(minimal_gear_config()).await;
    let registry = rig.gear.provider_registry().expect("built by init");
    let entries: Vec<_> = registry.entries().collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].base.provider_id, "openai");
    assert_eq!(entries[0].base.upstream_alias, "api.openai.com");
}

#[tokio::test]
async fn init_builds_the_registry_from_the_configured_providers_and_overrides() {
    let mut config = minimal_gear_config();
    config["providers"] = json!({
        "main": {
            "kind": "openai_chat_completions", "host": "llm.example.com", "storage_kind": "openai",
            "tenant_overrides": { (TENANT_A.to_string()): { "host": "tenant-a.example.com" } }
        }
    });
    let rig = rig(config).await;
    let registry = rig.gear.provider_registry().expect("built by init");
    assert!(
        registry.entry("openai").is_none(),
        "the default entry only fills an absent section"
    );
    assert_eq!(
        registry.resolve("main", TENANT_A).unwrap().upstream_alias,
        "tenant-a.example.com"
    );
    assert_eq!(
        registry
            .resolve("main", Uuid::from_u128(9))
            .unwrap()
            .upstream_alias,
        "llm.example.com"
    );
}

#[tokio::test]
async fn init_creates_the_audit_gateway_without_resolving_any_plugin() {
    let rig = rig(minimal_gear_config()).await;
    assert!(rig.gear.audit_gateway().is_some());
    assert_eq!(
        rig.platform.registry.list_call_count(),
        0,
        "plugins are resolved lazily, not at init"
    );
}

#[tokio::test]
async fn init_registers_the_instruments_under_the_default_and_a_custom_prefix() {
    let rig_default = rig(minimal_gear_config()).await;
    let names = rig_default
        .gear
        .metrics()
        .expect("built by init")
        .instrument_names();
    assert!(!names.is_empty());
    assert!(
        names.iter().all(|n| n.starts_with("mini_chat_")),
        "{names:?}"
    );
    assert!(names.contains(&"mini_chat_audit_emit".to_owned()));

    let mut config = minimal_gear_config();
    config["metrics"] = json!({ "prefix": "acme_chat" });
    let rig_custom = rig(config).await;
    let names = rig_custom.gear.metrics().unwrap().instrument_names();
    assert!(
        names.iter().all(|n| n.starts_with("acme_chat_")),
        "{names:?}"
    );
    assert!(names.contains(&"acme_chat_audit_emit".to_owned()));
}

#[tokio::test]
async fn a_failed_init_builds_none_of_the_s2_parts() {
    let platform = platform(ScriptedAuthN::ok(), RecordingOagw::accepting());
    let db = sqlite_db(all_migrations()).await;
    let mut config = minimal_gear_config();
    config["bogus_key"] = json!(1);
    let ctx = gear_ctx(
        GEAR_NAME,
        Some(config),
        Arc::clone(&platform.hub),
        Some(db.db.clone()),
    );
    let gear = MiniChatGear::default();
    assert!(gear.init(&ctx).await.is_err());
    assert!(gear.provider_registry().is_none());
    assert!(gear.metrics().is_none());
    assert!(gear.audit_gateway().is_none());
}

// ── start: S2S exchange and provisioning ─────────────────────────────────────

#[tokio::test]
async fn start_exchanges_the_credentials_provisions_the_providers_and_starts_the_outbox() {
    let rig = rig(minimal_gear_config()).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");

    assert_eq!(rig.gear.lifecycle_state(), LifecycleState::Started);
    assert!(rig.gear.outbox_running().await);
    assert_eq!(
        rig.platform.authn.exchanges.lock().unwrap().clone(),
        vec![("mini-chat".to_owned(), "dev-secret".to_owned())]
    );
    assert_eq!(
        rig.platform.oagw.upstream_aliases(),
        vec!["api.openai.com".to_owned()]
    );
    assert_eq!(rig.platform.oagw.route_call_count(), 6);
    for route in rig.platform.oagw.routes() {
        assert_eq!(route.subject, S2S_SUBJECT);
    }
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

#[tokio::test]
async fn start_provisions_every_entry_and_override() {
    let mut config = minimal_gear_config();
    config["providers"] = json!({
        "main": {
            "kind": "openai_responses", "host": "api.example.com", "storage_kind": "openai",
            "tenant_overrides": { (TENANT_A.to_string()): {
                "host": "10.0.0.7", "upstream_alias": "tenant-a-alias"
            } }
        },
        "second": { "kind": "vllm_responses", "host": "llm.internal", "storage_kind": "openai" }
    });
    let rig = rig(config).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    let mut aliases = rig.platform.oagw.upstream_aliases();
    aliases.sort();
    assert_eq!(
        aliases,
        vec!["api.example.com", "llm.internal", "tenant-a-alias"]
    );
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

#[tokio::test]
async fn a_failed_s2s_exchange_fails_start_without_starting_the_outbox() {
    for script in [AuthNScript::TokenFailure, AuthNScript::Unavailable] {
        let oagw = RecordingOagw::accepting();
        let rig = rig_with(
            platform(ScriptedAuthN::new(script.clone()), Arc::clone(&oagw)),
            minimal_gear_config(),
        )
        .await;
        let error = rig
            .gear
            .start(CancellationToken::new())
            .await
            .expect_err("start must fail");
        assert!(!error_text(&error).contains("dev-secret"), "{error:#}");
        assert_eq!(
            rig.gear.lifecycle_state(),
            LifecycleState::Failed,
            "{script:?}"
        );
        assert_eq!(oagw.upstream_call_count(), 0);
        assert!(
            !rig.gear.outbox_running().await,
            "{script:?}: the outbox must not start"
        );
    }
}

#[tokio::test]
async fn a_misconfigured_provider_fails_start_naming_it_without_starting_the_outbox() {
    let mut config = minimal_gear_config();
    config["providers"] = json!({
        "broken-provider": { "kind": "openai_responses", "host": "broken.example.com", "storage_kind": "openai" }
    });
    let oagw = RecordingOagw::accepting();
    oagw.script("broken.example.com", UpstreamScript::Invalid);
    let rig = rig_with(platform(ScriptedAuthN::ok(), oagw), config).await;

    let error = rig
        .gear
        .start(CancellationToken::new())
        .await
        .expect_err("start must fail");

    assert!(error_text(&error).contains("broken-provider"), "{error:#}");
    assert_eq!(rig.gear.lifecycle_state(), LifecycleState::Failed);
    assert!(!rig.gear.outbox_running().await);
    // nothing can be enqueued: the enqueuer was never bound
    let enqueuer = Arc::clone(rig.gear.enqueuer().unwrap());
    let outcome = rig
        .db
        .provider
        .transaction(move |tx| {
            let enqueuer = Arc::clone(&enqueuer);
            Box::pin(async move {
                enqueuer
                    .enqueue_raw(
                        tx,
                        QueueKind::Audit,
                        Uuid::from_u128(1),
                        b"{}".to_vec(),
                        AUDIT_PAYLOAD_TYPE,
                        EnqueueSite::TurnRetry,
                    )
                    .await
                    .map(drop)
            })
        })
        .await;
    assert!(outcome.is_err());
}

// ── start: deferred entries and the reconcile task ───────────────────────────

fn deferring_platform() -> Platform {
    platform(
        ScriptedAuthN::ok(),
        RecordingOagw::new(UpstreamScript::SecretUnreadableForever),
    )
}

#[tokio::test]
async fn start_completes_with_deferred_entries_pending_and_spawns_the_reconcile_task() {
    let rig = rig_with(deferring_platform(), without_watchdog()).await;

    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start does not wait");

    assert_eq!(rig.gear.lifecycle_state(), LifecycleState::Started);
    assert!(
        rig.gear.outbox_running().await,
        "the outbox starts right away"
    );
    assert_eq!(
        rig.platform.oagw.upstream_call_count(),
        1,
        "the first pass tried once and did not retry"
    );
    assert_eq!(rig.gear.background_tasks().expect("tasks").live_count(), 1);

    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

#[tokio::test]
async fn stop_cancels_and_joins_the_reconcile_task_before_the_outbox_stops() {
    let rig = rig_with(deferring_platform(), without_watchdog()).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    let tasks = Arc::clone(rig.gear.background_tasks().unwrap());
    assert_eq!(tasks.live_count(), 1);

    rig.gear.stop(CancellationToken::new()).await.expect("stop");

    assert_eq!(
        tasks.live_count(),
        0,
        "the reconcile task was cancelled and joined"
    );
    assert!(!rig.gear.outbox_running().await);
    assert_eq!(rig.gear.lifecycle_state(), LifecycleState::Stopped);
    assert_eq!(
        rig.platform.oagw.upstream_call_count(),
        1,
        "no retry ran: the task was cancelled during its first wait"
    );
}

#[tokio::test]
async fn cancelling_the_start_token_ends_the_reconcile_task() {
    let rig = rig_with(deferring_platform(), without_watchdog()).await;
    let cancel = CancellationToken::new();
    rig.gear.start(cancel.clone()).await.expect("start");
    let tasks = Arc::clone(rig.gear.background_tasks().unwrap());
    assert_eq!(tasks.live_count(), 1);

    cancel.cancel();

    assert!(
        wait_for(|| tasks.live_count() == 0).await,
        "the task gets the gear cancellation token"
    );
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

#[tokio::test]
async fn without_deferred_entries_no_reconcile_task_is_spawned() {
    let rig = rig(without_watchdog()).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    assert_eq!(rig.gear.background_tasks().expect("tasks").live_count(), 0);
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

#[tokio::test]
async fn provisioning_does_not_alter_the_registry_built_at_init() {
    let oagw = RecordingOagw::accepting();
    oagw.rewrite_returned_alias("-normalised");
    let rig = rig_with(platform(ScriptedAuthN::ok(), oagw), minimal_gear_config()).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    assert_eq!(
        rig.gear
            .provider_registry()
            .unwrap()
            .entry("openai")
            .unwrap()
            .base
            .upstream_alias,
        "api.openai.com"
    );
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

// ── start: leader elector ────────────────────────────────────────────────────

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn start_prepares_the_leader_elector_only_when_a_leader_only_worker_is_enabled() {
    let first = rig(minimal_gear_config()).await;
    first
        .gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    let elector = first
        .gear
        .leader_elector()
        .expect("a worker is enabled by default");
    assert!(elector.is_leader(LeaderRole::OrphanWatchdog).await);
    assert!(elector.is_leader(LeaderRole::UploadReaper).await);
    first
        .gear
        .stop(CancellationToken::new())
        .await
        .expect("stop");

    let mut config = minimal_gear_config();
    config["orphan_watchdog"] = json!({ "enabled": false });
    config["upload_reaper"] = json!({ "enabled": false });
    let rig = rig(config).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    assert!(rig.gear.leader_elector().is_none());
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

// ── start: summary model check ───────────────────────────────────────────────

fn policy_rig_platform(catalog: &[Value]) -> Platform {
    let platform = platform(ScriptedAuthN::ok(), RecordingOagw::accepting());
    let (id, content) = PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
        "acme.core._.policy_a.v1",
        "constructorfabric",
        100,
    )
    .unwrap();
    let id: String = id.as_ref().to_owned();
    platform
        .registry
        .add_instance(make_test_instance(&id, content));
    let config = StaticModelPolicyConfig::parse(json!({ "model_catalog": catalog })).unwrap();
    let plugin: Arc<dyn MiniChatModelPolicyPluginClientV1> =
        Arc::new(StaticModelPolicyService::new(config).unwrap());
    platform
        .hub
        .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(ClientScope::gts_id(&id), plugin);
    platform
}

#[traced_test]
#[tokio::test]
async fn a_missing_or_disabled_summary_model_is_logged_as_an_error_and_start_continues() {
    for (summary_model, catalog) in [
        (
            "ghost-model",
            vec![entry_json("other-model", "standard", true)],
        ),
        (
            "retired-model",
            vec![entry_json("retired-model", "standard", false)],
        ),
    ] {
        let mut config = minimal_gear_config();
        config["thread_summary_worker"] = json!({ "summary_model_id": summary_model });
        let rig = rig_with(policy_rig_platform(&catalog), config).await;

        rig.gear
            .start(CancellationToken::new())
            .await
            .expect("startup continues");

        assert_eq!(
            rig.gear.lifecycle_state(),
            LifecycleState::Started,
            "{summary_model}"
        );
        logs_assert(|lines: &[&str]| {
            if lines
                .iter()
                .any(|line| line.contains("ERROR") && line.contains(summary_model))
            {
                Ok(())
            } else {
                Err(format!("no ERROR line names `{summary_model}`: {lines:#?}"))
            }
        });
        rig.gear.stop(CancellationToken::new()).await.expect("stop");
    }
}

#[traced_test]
#[tokio::test]
async fn an_enabled_summary_model_raises_no_error() {
    let mut config = minimal_gear_config();
    config["thread_summary_worker"] = json!({ "summary_model_id": "summarizer" });
    let rig = rig_with(
        policy_rig_platform(&[entry_json("summarizer", "standard", true)]),
        config,
    )
    .await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");
    logs_assert(|lines: &[&str]| {
        if lines.iter().any(|line| line.contains("ERROR")) {
            Err(format!("unexpected ERROR line: {lines:#?}"))
        } else {
            Ok(())
        }
    });
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}

// ── start/stop: workers hook and order ───────────────────────────────────────

#[tokio::test]
async fn stop_joins_the_workers_before_it_stops_the_outbox() {
    let rig = rig(minimal_gear_config()).await;
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");

    let finished = Arc::new(AtomicBool::new(false));
    let outbox_was_running = Arc::new(AtomicBool::new(false));
    let (gear, finished_flag, running_flag) = (
        Arc::clone(&rig.gear),
        Arc::clone(&finished),
        Arc::clone(&outbox_was_running),
    );
    rig.gear
        .background_tasks()
        .expect("tasks exist after start")
        .spawn("orphan-watchdog", move |cancel| async move {
            cancel.cancelled().await;
            // a worker may still enqueue while it winds down
            tokio::time::sleep(Duration::from_millis(100)).await;
            running_flag.store(gear.outbox_running().await, Ordering::SeqCst);
            finished_flag.store(true, Ordering::SeqCst);
        });

    rig.gear.stop(CancellationToken::new()).await.expect("stop");

    assert!(finished.load(Ordering::SeqCst), "the worker was joined");
    assert!(
        outbox_was_running.load(Ordering::SeqCst),
        "the outbox was still running while the worker finished"
    );
    assert!(!rig.gear.outbox_running().await);
}

#[tokio::test]
async fn stop_before_start_is_safe_and_reaches_the_stopped_state() {
    let rig = rig(minimal_gear_config()).await;
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
    assert_eq!(rig.gear.lifecycle_state(), LifecycleState::Stopped);
}

// ── start: audit queue wiring ────────────────────────────────────────────────

#[tokio::test]
async fn an_audit_event_enqueued_after_start_reaches_the_audit_plugin() {
    let rig = rig(minimal_gear_config()).await;
    let (id, instance) = audit_instance("acme.core._.audit_a.v1", "constructorfabric", 100);
    rig.platform.registry.add_instance(instance);
    let plugin = ScriptedAuditPlugin::new(AuditScript::Accept);
    let client: Arc<dyn mini_chat_sdk::MiniChatAuditPluginClientV1> = plugin.clone();
    rig.platform
        .hub
        .register_scoped::<dyn mini_chat_sdk::MiniChatAuditPluginClientV1>(
            ClientScope::gts_id(&id),
            client,
        );
    rig.gear
        .start(CancellationToken::new())
        .await
        .expect("start");

    let event = all_audit_events(Uuid::from_u128(0xC4A7)).remove(0);
    let payload = serde_json::to_vec(&event).unwrap();
    let enqueuer = Arc::clone(rig.gear.enqueuer().unwrap());
    let wake = rig
        .db
        .provider
        .transaction(move |tx| {
            let (enqueuer, payload) = (Arc::clone(&enqueuer), payload.clone());
            Box::pin(async move {
                enqueuer
                    .enqueue_raw(
                        tx,
                        QueueKind::Audit,
                        TENANT_A,
                        payload,
                        AUDIT_PAYLOAD_TYPE,
                        EnqueueSite::TurnRetry,
                    )
                    .await
            })
        })
        .await
        .expect("enqueue");
    wake.fire();

    assert!(
        wait_for(|| plugin.call_count() == 1).await,
        "the audit handler delivers the event to the plugin"
    );
    assert_eq!(plugin.received(), vec![event]);
    rig.gear.stop(CancellationToken::new()).await.expect("stop");
}
