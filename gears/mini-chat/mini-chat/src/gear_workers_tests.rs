#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// the started-gear tests need the no-op elector: a `k8s` build fails `start` without a pod identity
#![cfg_attr(feature = "k8s", allow(dead_code, unused_imports))]
//! The background workers as wired by the gear
//! (`cpt-cf-mini-chat-feature-background-workers`): the chat cleanup and
//! thread-summary handlers replacing the foundation placeholders on the started
//! outbox pipeline, the orphan watchdog and the elector on the gear's background
//! registry, no polling worker for the deprecated keys, and (`k8s` builds) the
//! start failure naming a missing pod identity variable.

use std::sync::Arc;
use std::time::Duration;

use authn_resolver_sdk::AuthNResolverClient;
use authz_resolver_sdk::AuthZResolverApi;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use mini_chat_sdk::{
    BillingOutcome, MiniChatAuditEvent, MiniChatAuditPluginClientV1,
    MiniChatModelPolicyPluginClientV1, TurnAuditEventType,
};
use oagw_sdk::ServiceGatewayClientV1;
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit::context::GearCtx;
use toolkit::contracts::RunnableCapability;
use toolkit::{Gear, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_db::outbox::Record;
use tower::ServiceExt;
use uuid::Uuid;

use super::MiniChatGear;
use crate::chat_test_env::{blob, chat_id, ts};
use crate::domain::service::quota::test_support::design_limits;
use crate::domain::service::stream::ports::MessageBoundary;
use crate::domain::service::workers::summary_trigger::SummaryTask;
use crate::infra::db::test_support::{
    Row, TestDb, attachment_row, chat_row, message_row, turn_row,
};
use crate::infra::llm::oagw_provisioning::ProvisioningTimeouts;
use crate::infra::outbox::thread_summary::encode_task;
use crate::infra::test_support::FixedConfig;
use crate::test_doubles::{
    APIKEY_PLUGIN, AuthnReply, EmitBehavior, FakeAuditPlugin, FakeAuthn, FakeOagw, FakePdp,
    FakePolicyPlugin, POLICY_VERSION, PdpReply, PluginWorld, ProxyReply, S2S_SUBJECT, USER,
    USER_TENANT, entry, entry_json, user_ctx,
};

struct World {
    oagw: Arc<FakeOagw>,
    db: TestDb,
    gear: MiniChatGear,
    router: Router,
    policy: Arc<FakePolicyPlugin>,
    audit: Arc<FakeAuditPlugin>,
}

fn config(extra: &Value) -> Value {
    let mut config = json!({
        "client_credentials": { "client_id": "mini-chat-client", "client_secret": "S3CRET" },
        "providers": { "primary": {
            "kind": "openai_responses",
            "host": "api.openai.com",
            "storage_kind": "openai",
            "auth_plugin_type": APIKEY_PLUGIN,
            "auth_config": { "header": "Authorization", "prefix": "Bearer ", "secret_ref": "cred://x" }
        } },
        "thread_summary_worker": { "summary_model_id": "model-a" },
    });
    for (key, value) in extra.as_object().unwrap() {
        config[key] = value.clone();
    }
    config
}

impl World {
    async fn new(extra: &Value) -> Self {
        let plugins = PluginWorld::new();
        let mut model = entry_json("model-a", true);
        model["provider_id"] = json!("primary");
        let policy = FakePolicyPlugin::with_catalog(vec![entry(model)]);
        policy.set_limits(Some(design_limits(POLICY_VERSION)));
        plugins.register_policy(Arc::clone(&policy) as Arc<dyn MiniChatModelPolicyPluginClientV1>);
        let audit = FakeAuditPlugin::new(EmitBehavior::Ok);
        plugins.register_audit(Arc::clone(&audit) as Arc<dyn MiniChatAuditPluginClientV1>);
        plugins.hub.register::<dyn AuthZResolverApi>(
            FakePdp::new(PdpReply::AllowScoped) as Arc<dyn AuthZResolverApi>
        );
        let oagw = FakeOagw::new();
        plugins.hub.register::<dyn ServiceGatewayClientV1>(
            Arc::clone(&oagw) as Arc<dyn ServiceGatewayClientV1>
        );
        plugins.hub.register::<dyn AuthNResolverClient>(
            FakeAuthn::new(AuthnReply::Ok) as Arc<dyn AuthNResolverClient>
        );
        let db = TestDb::migrated().await;
        let ctx = GearCtx::new(
            "mini-chat",
            Uuid::nil(),
            Arc::new(FixedConfig::with_section("mini-chat", config(extra))),
            Arc::clone(&plugins.hub),
            CancellationToken::new(),
        )
        .with_db(DBProvider::new(db.db.clone()));
        let gear = MiniChatGear::default().with_provisioning_timeouts(ProvisioningTimeouts {
            s2s_exchange: Duration::from_millis(150),
            oagw_call: Duration::from_millis(150),
        });
        gear.init(&ctx).await.expect("init");
        gear.start(CancellationToken::new()).await.expect("start");
        let router = gear
            .register_rest(
                &ctx,
                Router::new(),
                &toolkit::api::OpenApiRegistryImpl::new(),
            )
            .expect("routes");
        Self {
            oagw,
            db,
            gear,
            router,
            policy,
            audit,
        }
    }

    async fn delete_chat(&self, chat: Uuid) -> StatusCode {
        let mut request = Request::builder()
            .method("DELETE")
            .uri(format!("/mini-chat/v1/chats/{chat}"))
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(user_ctx());
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        to_bytes(response.into_body(), 1 << 20).await.unwrap();
        status
    }

    async fn chat(&self, n: u128) -> Uuid {
        let id = chat_id(n);
        self.db
            .insert(
                "chats",
                chat_row(id, USER_TENANT, USER).set("model", "model-a"),
            )
            .await
            .unwrap();
        id
    }

    async fn text(&self, sql: &str) -> String {
        self.db.query_string(sql).await
    }

    async fn stop(self) {
        self.gear
            .stop(CancellationToken::new())
            .await
            .expect("stop");
    }
}

async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

async fn wait_for<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(Duration::from_secs(20), async {
        while !check().await {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

// ================================================================================================
// the chat cleanup handler replaces the placeholder
// ================================================================================================

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn the_started_gear_releases_the_provider_resources_of_a_deleted_chat_with_the_s2s_context() {
    let world = World::new(&json!({})).await;
    let chat = world.chat(1).await;
    let attachment = Uuid::from_u128(0xF100_0001);
    world
        .db
        .insert(
            "attachments",
            attachment_row(attachment, USER_TENANT, chat)
                .set("uploaded_by_user_id", USER)
                .set("storage_backend", "primary")
                .set("provider_file_id", "file-prov1")
                .set("created_at", ts(0))
                .set("updated_at", ts(0)),
        )
        .await
        .unwrap();
    world
        .db
        .insert(
            "chat_vector_stores",
            Row::default()
                .set("id", Uuid::new_v4())
                .set("tenant_id", USER_TENANT)
                .set("chat_id", chat)
                .set("vector_store_id", "vs_1")
                .set("provider", "primary")
                .set("file_count", 0_i32)
                .set("created_at", ts(0)),
        )
        .await
        .unwrap();
    world
        .oagw
        .on_path("DELETE", "/api.openai.com/v1/files/*", |_| {
            ProxyReply::json(200, &json!({}))
        });
    world
        .oagw
        .on_path("DELETE", "/api.openai.com/v1/vector_stores/*", |_| {
            ProxyReply::json(200, &json!({}))
        });

    let status = world.delete_chat(chat).await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    wait_for("the chat is released", || async {
        world.db.count("chat_vector_stores").await == 0
            && world
                .text(&format!(
                    "SELECT cleanup_status FROM attachments WHERE id = {}",
                    blob(attachment)
                ))
                .await
                == "done"
    })
    .await;
    let calls = world.oagw.proxied();
    let deletes: Vec<_> = calls.iter().filter(|c| c.method == "DELETE").collect();
    assert_eq!(deletes.len(), 2, "one file and one store");
    assert!(deletes.iter().all(|c| c.subject == S2S_SUBJECT));
    assert!(
        deletes.last().unwrap().uri.contains("vector_stores"),
        "the store last"
    );
    world.stop().await;
}

// ================================================================================================
// the thread-summary handler replaces the placeholder
// ================================================================================================

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn the_started_gear_summarizes_and_commits_a_work_item_from_the_summary_queue() {
    let world = World::new(&json!({})).await;
    let chat = world.chat(1).await;
    let (user_message, answer) = (Uuid::from_u128(0xD1), Uuid::from_u128(0xD2));
    for (id, role, secs) in [(user_message, "user", 10), (answer, "assistant", 11)] {
        world
            .db
            .insert(
                "messages",
                message_row(id, USER_TENANT, chat)
                    .set("role", role)
                    .set("content", format!("{role} content"))
                    .set("request_id", Uuid::from_u128(0xE1))
                    .set("created_at", ts(secs)),
            )
            .await
            .unwrap();
    }
    world.oagw.push_proxy(ProxyReply::json(
        200,
        &json!({
            "output": [ { "type": "message", "content": [ { "type": "output_text", "text": "<summary>Gear summary.</summary>" } ] } ],
            "usage": { "input_tokens": 50, "output_tokens": 12 }
        }),
    ));
    let task = SummaryTask {
        tenant_id: USER_TENANT,
        chat_id: chat,
        system_request_id: Uuid::from_u128(0x5157),
        base: None,
        target: MessageBoundary {
            created_at: OffsetDateTime::from_unix_timestamp(crate::chat_test_env::BASE_UNIX + 11)
                .unwrap(),
            id: answer,
        },
    };

    let enqueuer = world.gear.outbox_enqueuer().expect("the enqueuer");
    let outbox = enqueuer.outbox().expect("the pipeline runs");
    let queue = enqueuer
        .queue_name(crate::infra::outbox::queues::QueueKind::ThreadSummary)
        .to_owned();
    let partition = enqueuer.partition(
        crate::infra::outbox::queues::QueueKind::ThreadSummary,
        USER_TENANT,
        chat,
    );
    let conn = world.db.db.conn().unwrap();
    let record = Record::to(&queue, partition)
        .payload(encode_task(&task).unwrap(), "application/json")
        .build()
        .unwrap();
    outbox.enqueue(&conn, record).await.unwrap().fire();

    wait_for("the summary is committed", || async {
        world.db.count("thread_summaries").await == 1
    })
    .await;
    assert_eq!(
        world
            .text("SELECT summary_text FROM thread_summaries")
            .await,
        "Gear summary."
    );
    wait_until("the system-task usage event reaches the plugin", || {
        world
            .policy
            .published_events()
            .iter()
            .any(|e| e.billing_outcome == BillingOutcome::SystemTask)
    })
    .await;
    let calls = world.oagw.proxied();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].subject, S2S_SUBJECT);
    world.stop().await;
}

// ================================================================================================
// the orphan watchdog on the background registry
// ================================================================================================

async fn seed_stale_turn(world: &World, chat: Uuid) -> Uuid {
    let turn = Uuid::from_u128(0xA001);
    world
        .db
        .insert(
            "chat_turns",
            turn_row(turn, USER_TENANT, chat, Uuid::from_u128(0xE9), "running")
                .set("requester_user_id", USER)
                .set("started_at", "2026-01-01T00:00:00Z")
                .set("last_progress_at", "2026-01-01T00:00:10Z")
                .set("reserve_tokens", 1500_i64)
                .set("max_output_tokens_applied", 500_i32)
                .set("reserved_credits_micro", 1_500_000_i64)
                .set("policy_version_applied", 7_i64)
                .set("effective_model", "model-a")
                .set("minimal_generation_floor_applied", 50_i32),
        )
        .await
        .unwrap();
    for (period, start, bucket) in [
        ("daily", "2026-01-01", "total"),
        ("monthly", "2026-01-01", "total"),
        ("daily", "2026-01-01", "tier:premium"),
        ("monthly", "2026-01-01", "tier:premium"),
    ] {
        world
            .db
            .insert(
                "quota_usage",
                Row::default()
                    .set("id", Uuid::new_v4())
                    .set("tenant_id", USER_TENANT)
                    .set("user_id", USER)
                    .set("period_type", period)
                    .set("period_start", start)
                    .set("bucket", bucket)
                    .set("spent_credits_micro", 0_i64)
                    .set("reserved_credits_micro", 1_500_000_i64)
                    .set("calls", 0_i32)
                    .set("input_tokens", 0_i64)
                    .set("output_tokens", 0_i64)
                    .set("web_search_calls", 0_i32)
                    .set("code_interpreter_calls", 0_i32)
                    .set("updated_at", "2026-01-01T00:00:00Z"),
            )
            .await
            .unwrap();
    }
    turn
}

fn fast_watchdog() -> Value {
    json!({ "orphan_watchdog": { "enabled": true, "timeout_secs": 90, "scan_interval_secs": 1 } })
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn the_started_gear_finalizes_a_stale_turn_and_delivers_its_usage_and_audit_events() {
    let world = World::new(&fast_watchdog()).await;
    let chat = world.chat(1).await;
    let turn = seed_stale_turn(&world, chat).await;

    wait_for("the watchdog finalizes the turn", || async {
        world
            .text(&format!(
                "SELECT state FROM chat_turns WHERE id = {}",
                blob(turn)
            ))
            .await
            == "failed"
    })
    .await;

    assert_eq!(
        world
            .text(&format!(
                "SELECT error_code FROM chat_turns WHERE id = {}",
                blob(turn)
            ))
            .await,
        "orphan_timeout"
    );
    wait_until("the usage event reaches the model-policy plugin", || {
        world.policy.published_events().len() == 1
    })
    .await;
    let usage = world.policy.published_events();
    assert_eq!(usage[0].billing_outcome, BillingOutcome::Aborted);
    assert_eq!(usage[0].turn_id, Some(turn));
    wait_until("the audit event reaches the audit plugin", || {
        world.audit.emitted_events().len() == 1
    })
    .await;
    match &world.audit.emitted_events()[0] {
        MiniChatAuditEvent::Turn(event) => {
            assert_eq!(event.event_type, TurnAuditEventType::TurnFailed);
            assert_eq!(event.selected_model, "model-a");
            assert_eq!(event.policy_decisions.quota.decision, "unknown");
        }
        other @ MiniChatAuditEvent::Mutation(_) => panic!("a turn audit event expected: {other:?}"),
    }
    world.stop().await;
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn the_watchdog_runs_on_the_registry_and_stops_with_the_gear() {
    let world = World::new(&fast_watchdog()).await;

    assert!(
        world
            .gear
            .running_background_tasks()
            .contains(&"orphan-watchdog")
    );
    assert!(
        world
            .gear
            .running_background_tasks()
            .contains(&"upload-reaper")
    );
    let gear = &world.gear;
    gear.stop(CancellationToken::new()).await.expect("stop");

    assert!(
        gear.running_background_tasks().is_empty(),
        "{:?}",
        gear.running_background_tasks()
    );
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn a_disabled_watchdog_is_not_spawned() {
    let world = World::new(&json!({ "orphan_watchdog": { "enabled": false } })).await;

    let running = world.gear.running_background_tasks();

    assert!(!running.contains(&"orphan-watchdog"), "{running:?}");
    assert!(running.contains(&"upload-reaper"));
    world.stop().await;
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn the_elector_is_prepared_only_when_a_leader_only_worker_is_enabled() {
    let both = World::new(&json!({})).await;
    assert!(both.gear.leader_elector().is_some());
    both.stop().await;

    let none = World::new(&json!({
        "orphan_watchdog": { "enabled": false },
        "upload_reaper": { "enabled": false }
    }))
    .await;
    assert!(none.gear.leader_elector().is_none());
    assert!(
        none.gear
            .running_background_tasks()
            .iter()
            .all(|task| !task.starts_with("lease-")),
        "no renew task without a leader-only worker"
    );
    none.stop().await;
}

#[cfg(not(feature = "k8s"))]
#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn without_the_k8s_feature_the_gear_uses_the_noop_elector() {
    let world = World::new(&json!({})).await;

    let elector = world.gear.leader_elector().expect("an elector");

    assert_eq!(elector.name(), "noop");
    world.stop().await;
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn the_deprecated_worker_keys_spawn_no_polling_worker() {
    let world = World::new(&json!({
        "cleanup_worker": {
            "enabled": true, "poll_interval_secs": 1, "reconcile_interval_secs": 1,
            "stale_in_progress_timeout_secs": 60, "batch_size": 1, "max_attempts": 5
        },
        "thread_summary_worker": { "summary_model_id": "model-a", "reconcile_interval_secs": 1 }
    }))
    .await;

    let running = world.gear.running_background_tasks();

    for task in &running {
        assert!(
            ["upload-reaper", "orphan-watchdog", "provider-reconcile"].contains(task),
            "no gear-local polling worker exists: {running:?}"
        );
    }
    world.stop().await;
}

// ================================================================================================
// a k8s build without the pod identity
// ================================================================================================

#[cfg(feature = "k8s")]
#[test]
fn a_k8s_build_without_pod_namespace_or_pod_name_fails_start_naming_the_variable() {
    for (unset, name) in [("POD_NAMESPACE", "POD_NAMESPACE"), ("POD_NAME", "POD_NAME")] {
        let keep = if unset == "POD_NAME" {
            "POD_NAMESPACE"
        } else {
            "POD_NAME"
        };
        temp_env::with_vars([(keep, Some("present")), (unset, None::<&str>)], || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let plugins = PluginWorld::new();
                let mut model = entry_json("model-a", true);
                model["provider_id"] = json!("primary");
                plugins.register_policy(FakePolicyPlugin::with_catalog(vec![entry(model)])
                    as Arc<dyn MiniChatModelPolicyPluginClientV1>);
                plugins
                    .register_audit(FakeAuditPlugin::new(EmitBehavior::Ok)
                        as Arc<dyn MiniChatAuditPluginClientV1>);
                plugins.hub.register::<dyn AuthZResolverApi>(
                    FakePdp::new(PdpReply::AllowScoped) as Arc<dyn AuthZResolverApi>
                );
                plugins.hub.register::<dyn ServiceGatewayClientV1>(
                    FakeOagw::new() as Arc<dyn ServiceGatewayClientV1>
                );
                plugins.hub.register::<dyn AuthNResolverClient>(
                    FakeAuthn::new(AuthnReply::Ok) as Arc<dyn AuthNResolverClient>
                );
                let db = TestDb::migrated().await;
                let ctx = GearCtx::new(
                    "mini-chat",
                    Uuid::nil(),
                    Arc::new(FixedConfig::with_section("mini-chat", config(&json!({})))),
                    Arc::clone(&plugins.hub),
                    CancellationToken::new(),
                )
                .with_db(DBProvider::new(db.db.clone()));
                let gear = MiniChatGear::default();
                gear.init(&ctx).await.expect("init");

                let error = gear.start(CancellationToken::new()).await.unwrap_err();

                assert!(format!("{error:#}").contains(name), "{error:#}");
                assert_eq!(
                    gear.lifecycle_state(),
                    super::GearLifecycleState::StartFailed
                );
            });
        });
    }
}
