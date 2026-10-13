#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of the gear flows `cpt-cf-mini-chat-flow-gear-foundation-gear-init`,
//! `-gear-start`, `-gear-stop`, the state machine
//! `cpt-cf-mini-chat-state-gear-foundation-gear-lifecycle` and the REST
//! registration, over fake PDP, OAGW, authn, plugins and a SQLite outbox.

use std::sync::Arc;
use std::time::Duration;

use authn_resolver_sdk::AuthNResolverClient;
use authz_resolver_sdk::AuthZResolverApi;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use mini_chat_sdk::{MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1};
use oagw_sdk::ServiceGatewayClientV1;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use toolkit::client_hub::ClientHub;
use toolkit::context::GearCtx;
use toolkit::contracts::RunnableCapability;
use toolkit::{Gear, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_db::outbox::Record;
use tower::ServiceExt;
use tracing::Level;
use uuid::Uuid;

use super::{GearLifecycleState, MiniChatGear};
use crate::infra::db::test_support::TestDb;
use crate::infra::llm::oagw_provisioning::ProvisioningTimeouts;
use crate::infra::outbox::queues::QueueKind;
use crate::infra::test_support::{FakeTypesRegistry, FixedConfig};
use crate::test_doubles::{
    APIKEY_PLUGIN, AuthnReply, EmitBehavior, FakeAuditPlugin, FakeAuthn, FakeOagw, FakePdp,
    FakePolicyPlugin, Fault, LogCapture, PdpReply, PluginWorld, USER_TENANT, audit_event,
    audit_payload, entry, entry_json, usage_event, usage_payload, user_ctx,
};

const CLIENT_SECRET: &str = "S3CRET-SENTINEL";
const AUTH_SENTINEL: &str = "cred://AUTH-CONFIG-SENTINEL";

fn fast() -> ProvisioningTimeouts {
    ProvisioningTimeouts {
        s2s_exchange: Duration::from_millis(150),
        oagw_call: Duration::from_millis(150),
    }
}

fn gear() -> MiniChatGear {
    MiniChatGear::default().with_provisioning_timeouts(fast())
}

fn provider(host: &str) -> Value {
    json!({
        "kind": "openai_responses",
        "host": host,
        "storage_kind": "openai",
        "auth_plugin_type": APIKEY_PLUGIN,
        "auth_config": {
            "header": "Authorization",
            "prefix": "Bearer ",
            "secret_ref": AUTH_SENTINEL
        }
    })
}

/// A valid configuration section; `extra` keys are merged at the top level.
fn config_with(extra: &Value) -> Value {
    let mut config = json!({
        "client_credentials": { "client_id": "mini-chat-client", "client_secret": CLIENT_SECRET },
        "providers": { "primary": provider("api.openai.com") },
    });
    for (key, value) in extra.as_object().unwrap() {
        config[key] = value.clone();
    }
    config
}

fn config() -> Value {
    config_with(&json!({}))
}

struct Env {
    hub: Arc<ClientHub>,
    registry: Arc<FakeTypesRegistry>,
    oagw: Arc<FakeOagw>,
    authn: Arc<FakeAuthn>,
    pdp: Arc<FakePdp>,
    policy: Arc<FakePolicyPlugin>,
    audit: Arc<FakeAuditPlugin>,
    db: TestDb,
}

impl Env {
    async fn new() -> Self {
        Self::with(None, AuthnReply::Ok).await
    }

    /// `missing` names the client (`authz_resolver`, `oagw`, `authn_resolver`) left out of the hub.
    async fn with(missing: Option<&str>, authn_reply: AuthnReply) -> Self {
        let world = PluginWorld::new();
        let policy = FakePolicyPlugin::with_catalog(vec![
            entry(entry_json("gpt-4.1-mini", true)),
            entry(entry_json("model-a", true)),
            entry(entry_json("model-off", false)),
        ]);
        let audit = FakeAuditPlugin::new(EmitBehavior::Ok);
        world.register_policy(Arc::clone(&policy) as Arc<dyn MiniChatModelPolicyPluginClientV1>);
        world.register_audit(Arc::clone(&audit) as Arc<dyn MiniChatAuditPluginClientV1>);
        let oagw = FakeOagw::new();
        let authn = FakeAuthn::new(authn_reply);
        let pdp = FakePdp::new(PdpReply::Allow);
        if missing != Some("authz_resolver") {
            world
                .hub
                .register::<dyn AuthZResolverApi>(Arc::clone(&pdp) as Arc<dyn AuthZResolverApi>);
        }
        if missing != Some("oagw") {
            world.hub.register::<dyn ServiceGatewayClientV1>(
                Arc::clone(&oagw) as Arc<dyn ServiceGatewayClientV1>
            );
        }
        if missing != Some("authn_resolver") {
            world.hub.register::<dyn AuthNResolverClient>(
                Arc::clone(&authn) as Arc<dyn AuthNResolverClient>
            );
        }
        Self {
            hub: world.hub,
            registry: world.registry,
            oagw,
            authn,
            pdp,
            policy,
            audit,
            db: TestDb::migrated().await,
        }
    }

    fn ctx(&self, section: Value) -> GearCtx {
        GearCtx::new(
            "mini-chat",
            Uuid::nil(),
            Arc::new(FixedConfig::with_section("mini-chat", section)),
            Arc::clone(&self.hub),
            CancellationToken::new(),
        )
        .with_db(DBProvider::new(self.db.db.clone()))
    }

    async fn init(&self, gear: &MiniChatGear, section: Value) -> anyhow::Result<()> {
        gear.init(&self.ctx(section)).await
    }

    /// Initializes the gear and returns the context it was initialized with.
    async fn init_ctx(&self, gear: &MiniChatGear, section: Value) -> GearCtx {
        let ctx = self.ctx(section);
        gear.init(&ctx).await.expect("init");
        ctx
    }
}

async fn wait_until(what: &str, condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// The background tasks of the gear without the leader-only upload reaper
/// (`cpt-cf-mini-chat-feature-attachments`) and orphan watchdog
/// (`cpt-cf-mini-chat-feature-background-workers`), which run for the whole
/// life of a started gear and have their own tests (`gear_attachments_tests`,
/// `gear_workers_tests`).
fn workers(gear: &MiniChatGear) -> Vec<&'static str> {
    gear.running_background_tasks()
        .into_iter()
        .filter(|name| !matches!(*name, "upload-reaper" | "orphan-watchdog"))
        .collect()
}

fn error_text(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

// ---- identity and state ------------------------------------------------------------------

#[test]
fn the_gear_is_named_mini_chat() {
    assert_eq!(MiniChatGear::MODULE_NAME, "mini-chat");
}

#[test]
fn a_new_gear_is_created() {
    assert_eq!(
        MiniChatGear::default().lifecycle_state(),
        GearLifecycleState::Created
    );
}

// ---- gear-init -------------------------------------------------------------------------------

#[tokio::test]
async fn a_valid_configuration_initializes_the_gear() {
    let env = Env::new().await;
    let gear = gear();

    env.init(&gear, config()).await.expect("init");

    assert_eq!(gear.lifecycle_state(), GearLifecycleState::Initialized);
    assert!(
        gear.outbox_enqueuer().is_some(),
        "the enqueuer exists after init"
    );
}

#[tokio::test]
async fn init_exposes_the_services_state_with_the_provider_resolver() {
    let env = Env::new().await;
    let gear = gear();
    assert!(gear.services().is_none(), "no services before init");

    env.init(&gear, config()).await.expect("init");

    let services = gear.services().expect("services after init");
    let resolution = services
        .providers
        .resolve("primary", Uuid::from_u128(7))
        .expect("the configured provider resolves");
    assert_eq!(resolution.alias, "api.openai.com");
    assert_eq!(services.config.url_prefix, "/mini-chat");
}

#[tokio::test]
async fn init_calls_no_external_service_and_resolves_no_plugin() {
    let env = Env::new().await;
    env.registry.fail_list("registry down");
    let gear = gear();

    env.init(&gear, config())
        .await
        .expect("plugins are resolved lazily");

    assert_eq!(gear.lifecycle_state(), GearLifecycleState::Initialized);
    assert!(env.oagw.calls().is_empty());
    assert_eq!(env.authn.exchange_count(), 0);
    assert!(env.pdp.requests().is_empty());
    assert!(env.policy.snapshot_requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_unknown_key_fails_init_naming_it() {
    let env = Env::new().await;
    let gear = gear();

    let error = env
        .init(&gear, config_with(&json!({ "bogus_key": 1 })))
        .await
        .expect_err("unknown key");

    assert!(error_text(&error).contains("bogus_key"), "{error:#}");
    assert_eq!(gear.lifecycle_state(), GearLifecycleState::InitFailed);
}

#[tokio::test]
async fn the_removed_mcp_key_fails_init_naming_it() {
    let env = Env::new().await;
    let gear = gear();

    let error = env
        .init(&gear, config_with(&json!({ "mcp": { "servers": [] } })))
        .await
        .expect_err("mcp is removed");

    assert!(error_text(&error).contains("mcp"), "{error:#}");
    assert_eq!(gear.lifecycle_state(), GearLifecycleState::InitFailed);
}

#[tokio::test]
async fn an_out_of_range_value_fails_init_naming_the_key() {
    let env = Env::new().await;
    let gear = gear();

    let error = env
        .init(
            &gear,
            config_with(&json!({ "streaming": { "sse_ping_interval_seconds": 1 } })),
        )
        .await
        .expect_err("out of range");

    assert!(
        error_text(&error).contains("streaming.sse_ping_interval_seconds"),
        "{error:#}"
    );
}

#[tokio::test]
async fn missing_client_credentials_fail_init_naming_the_key() {
    let env = Env::new().await;
    let gear = gear();
    let mut section = config();
    section
        .as_object_mut()
        .unwrap()
        .remove("client_credentials");

    let error = env
        .init(&gear, section)
        .await
        .expect_err("credentials are required");

    assert!(
        error_text(&error).contains("client_credentials"),
        "{error:#}"
    );
}

#[tokio::test]
async fn targets_sharing_an_alias_with_different_settings_fail_init_naming_alias_and_targets() {
    let env = Env::new().await;
    let gear = gear();
    let mut other = provider("api.openai.com");
    other["auth_config"]["secret_ref"] = json!("cred://OTHER-SENTINEL");
    let section = config_with(&json!({
        "providers": { "first": provider("api.openai.com"), "second": other }
    }));

    let error = env.init(&gear, section).await.expect_err("alias conflict");

    let text = error_text(&error);
    assert!(text.contains("api.openai.com"), "{text}");
    assert!(text.contains("providers.first"), "{text}");
    assert!(text.contains("providers.second"), "{text}");
    assert!(
        !text.contains("AUTH-CONFIG-SENTINEL") && !text.contains("OTHER-SENTINEL"),
        "{text}"
    );
    assert_eq!(gear.lifecycle_state(), GearLifecycleState::InitFailed);
}

#[tokio::test]
async fn a_missing_required_client_fails_init_naming_it() {
    for name in ["authz_resolver", "oagw", "authn_resolver"] {
        let env = Env::with(Some(name), AuthnReply::Ok).await;
        let gear = gear();

        let error = env.init(&gear, config()).await.expect_err("client missing");

        assert!(error_text(&error).contains(name), "{name}: {error:#}");
        assert_eq!(
            gear.lifecycle_state(),
            GearLifecycleState::InitFailed,
            "{name}"
        );
    }
}

#[tokio::test]
async fn an_init_failure_message_carries_no_secret() {
    let env = Env::new().await;
    let logs = LogCapture::start();
    let gear = gear();

    let error = env
        .init(&gear, config_with(&json!({ "bogus_key": 1 })))
        .await
        .expect_err("invalid");

    for text in [error_text(&error), logs.all_text()] {
        assert!(!text.contains(CLIENT_SECRET), "{text}");
        assert!(!text.contains("AUTH-CONFIG-SENTINEL"), "{text}");
    }
}

// ---- REST registration (inst-register-routes) ----------------------------------------------------

async fn get(router: &Router, uri: &str) -> (StatusCode, Value) {
    let mut request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    request.extensions_mut().insert(user_ctx());
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn the_models_routes_are_served_under_the_default_url_prefix() {
    let env = Env::new().await;
    let gear = gear();
    let ctx = env.init_ctx(&gear, config()).await;
    let openapi = toolkit::api::OpenApiRegistryImpl::new();

    let router = gear
        .register_rest(&ctx, Router::new(), &openapi)
        .expect("routes");

    let (status, body) = get(&router, "/mini-chat/v1/models").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["model_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["gpt-4.1-mini", "model-a"]);
    let (status, body) = get(&router, "/mini-chat/v1/models/model-a").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = get(&router, "/mini-chat/v1/models/model-off").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body["context"]["resource_type"],
        "gts.cf.core.mini_chat.model.v1~"
    );
}

#[tokio::test]
async fn the_routes_follow_the_configured_url_prefix_and_declare_the_license_feature() {
    let env = Env::new().await;
    let gear = gear();
    let ctx = env
        .init_ctx(&gear, config_with(&json!({ "url_prefix": "/chat-api" })))
        .await;
    let openapi = toolkit::api::OpenApiRegistryImpl::new();

    let router = gear
        .register_rest(&ctx, Router::new(), &openapi)
        .expect("routes");

    let (status, _) = get(&router, "/chat-api/v1/models").await;
    assert_eq!(status, StatusCode::OK);
    for key in ["GET:/chat-api/v1/models", "GET:/chat-api/v1/models/{id}"] {
        let spec = openapi.operation_specs.get(key).expect(key).value().clone();
        assert!(spec.authenticated, "{key}");
        assert_eq!(
            spec.license_requirement.unwrap().license_names,
            vec!["gts.cf.core.lic.feat.v1~cf.core.global.base.v1".to_owned()],
            "{key}"
        );
    }
}

#[tokio::test]
async fn the_pep_of_the_gear_uses_the_authz_resolver_client() {
    let env = Env::new().await;
    let gear = gear();
    let ctx = env.init_ctx(&gear, config()).await;
    let router = gear
        .register_rest(
            &ctx,
            Router::new(),
            &toolkit::api::OpenApiRegistryImpl::new(),
        )
        .expect("routes");

    get(&router, "/mini-chat/v1/models").await;

    let requests = env.pdp.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].action.name, "list");
    assert_eq!(
        requests[0].resource.resource_type,
        "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~"
    );
}

#[tokio::test]
async fn registering_routes_before_init_is_an_error_not_a_panic() {
    let env = Env::new().await;
    let gear = gear();
    let ctx = env.ctx(config());

    let result = gear.register_rest(
        &ctx,
        Router::new(),
        &toolkit::api::OpenApiRegistryImpl::new(),
    );

    assert!(result.is_err());
}

// ---- gear-start ---------------------------------------------------------------------------------------

#[tokio::test]
async fn start_provisions_oagw_and_runs_the_five_queue_pipeline() {
    let env = Env::new().await;
    let gear = gear();
    env.init(&gear, config()).await.expect("init");

    gear.start(CancellationToken::new()).await.expect("start");

    assert_eq!(gear.lifecycle_state(), GearLifecycleState::Started);
    assert_eq!(
        env.authn.exchanges.lock().unwrap().clone(),
        vec![("mini-chat-client".to_owned(), CLIENT_SECRET.to_owned())]
    );
    assert_eq!(env.oagw.upstream_creates().len(), 1);
    assert_eq!(
        env.oagw.route_creates().len(),
        6,
        "chat route and five rag routes"
    );
    assert_eq!(
        env.oagw.call_subjects().len(),
        1,
        "every call uses the S2S context"
    );
    assert_eq!(
        env.db
            .query_i64("SELECT COUNT(DISTINCT queue) FROM mini_chat__outbox_partitions")
            .await,
        5
    );
    assert_eq!(
        env.db
            .query_i64("SELECT COUNT(*) FROM mini_chat__outbox_partitions")
            .await,
        20,
        "five queues with the default four partitions"
    );
    assert!(workers(&gear).is_empty(), "nothing was deferred");
    gear.stop(CancellationToken::new()).await.expect("stop");
}

#[tokio::test]
async fn the_configured_queue_names_and_partitions_are_registered() {
    let env = Env::new().await;
    let gear = gear();
    env.init(
        &gear,
        config_with(&json!({ "outbox": {
            "queue_name": "x.usage", "cleanup_queue_name": "x.att", "chat_cleanup_queue_name": "x.chat",
            "thread_summary_queue_name": "x.sum", "audit_queue_name": "x.audit", "num_partitions": 2
        } })),
    )
    .await
    .expect("init");

    gear.start(CancellationToken::new()).await.expect("start");

    assert_eq!(
        env.db
            .query_i64("SELECT COUNT(*) FROM mini_chat__outbox_partitions WHERE queue LIKE 'x.%'")
            .await,
        10
    );
    gear.stop(CancellationToken::new()).await.expect("stop");
}

#[tokio::test]
async fn usage_and_audit_messages_reach_their_plugins_through_the_started_pipeline() {
    let env = Env::new().await;
    let gear = gear();
    env.init(&gear, config()).await.expect("init");
    gear.start(CancellationToken::new()).await.expect("start");
    let enqueuer = gear.outbox_enqueuer().expect("enqueuer");
    let outbox = enqueuer.outbox().expect("bound at start");
    let conn = env.db.db.conn().expect("conn");
    let chat = Uuid::from_u128(0x2222);

    for (kind, payload) in [
        (QueueKind::Usage, usage_payload()),
        (QueueKind::Audit, audit_payload()),
    ] {
        let partition = enqueuer.partition(kind, USER_TENANT, chat);
        let record = Record::to(enqueuer.queue_name(kind), partition)
            .payload(payload, "application/json")
            .build()
            .expect("record");
        outbox.enqueue(&conn, record).await.expect("enqueue").fire();
    }

    wait_until("the usage event", || {
        env.policy.published_events().len() == 1
    })
    .await;
    wait_until("the audit event", || env.audit.emitted_events().len() == 1).await;
    assert_eq!(env.policy.published_events(), vec![usage_event()]);
    assert_eq!(env.audit.emitted_events(), vec![audit_event()]);
    gear.stop(CancellationToken::new()).await.expect("stop");
}

#[tokio::test]
async fn an_unreadable_secret_defers_the_provider_without_failing_start() {
    let env = Env::new().await;
    env.oagw
        .fail_upstream("api.openai.com", Fault::SecretNotReadable);
    let gear = gear();
    env.init(&gear, config()).await.expect("init");

    gear.start(CancellationToken::new())
        .await
        .expect("a deferred provider does not fail start");

    assert_eq!(gear.lifecycle_state(), GearLifecycleState::Started);
    assert_eq!(workers(&gear), vec!["provider-reconcile"]);
    assert_eq!(
        env.db
            .query_i64("SELECT COUNT(*) FROM mini_chat__outbox_partitions")
            .await,
        20,
        "the pipeline runs although a provider is deferred"
    );
    gear.stop(CancellationToken::new()).await.expect("stop");
}

#[tokio::test]
async fn an_oagw_timeout_defers_the_provider_without_failing_start() {
    let env = Env::new().await;
    env.oagw.fail_upstream("api.openai.com", Fault::Hang);
    let gear = gear();
    env.init(&gear, config()).await.expect("init");

    gear.start(CancellationToken::new()).await.expect("start");

    assert_eq!(gear.lifecycle_state(), GearLifecycleState::Started);
    assert_eq!(workers(&gear), vec!["provider-reconcile"]);
    gear.stop(CancellationToken::new()).await.expect("stop");
}

#[tokio::test]
async fn the_reconcile_task_provisions_a_deferred_provider_and_then_ends() {
    let env = Env::new().await;
    env.oagw
        .fail_upstream("api.openai.com", Fault::SecretNotReadable);
    let gear = gear();
    env.init(&gear, config()).await.expect("init");
    gear.start(CancellationToken::new()).await.expect("start");
    assert_eq!(env.oagw.route_creates().len(), 0);

    env.oagw.clear_faults();

    wait_until("the reconcile task to provision the provider", || {
        env.oagw.stored_routes("api.openai.com").len() == 6
    })
    .await;
    wait_until("the reconcile task to end", || workers(&gear).is_empty()).await;
    gear.stop(CancellationToken::new()).await.expect("stop");
}

#[tokio::test]
async fn a_misconfigured_provider_fails_start_naming_it_and_starts_no_outbox() {
    let env = Env::new().await;
    env.oagw
        .fail_upstream("api.openai.com", Fault::InvalidArgument);
    let gear = gear();
    env.init(&gear, config()).await.expect("init");

    let error = gear
        .start(CancellationToken::new())
        .await
        .expect_err("misconfigured");

    let text = error_text(&error);
    assert!(text.contains("primary"), "{text}");
    assert!(
        !text.contains("AUTH-CONFIG-SENTINEL") && !text.contains(CLIENT_SECRET),
        "{text}"
    );
    assert_eq!(gear.lifecycle_state(), GearLifecycleState::StartFailed);
    assert_eq!(
        env.db
            .query_i64("SELECT COUNT(*) FROM mini_chat__outbox_partitions")
            .await,
        0,
        "the outbox is started after provisioning"
    );
}

#[tokio::test]
async fn a_failed_s2s_exchange_fails_start_naming_the_client_id() {
    let env = Env::with(None, AuthnReply::Fail("idp down".to_owned())).await;
    let gear = gear();
    env.init(&gear, config()).await.expect("init");

    let error = gear
        .start(CancellationToken::new())
        .await
        .expect_err("exchange fails");

    let text = error_text(&error);
    assert!(text.contains("client_id"), "{text}");
    assert!(text.contains("mini-chat-client"), "{text}");
    assert!(!text.contains(CLIENT_SECRET), "{text}");
    assert_eq!(gear.lifecycle_state(), GearLifecycleState::StartFailed);
    assert!(env.oagw.calls().is_empty());
}

#[tokio::test]
async fn a_timed_out_s2s_exchange_fails_start_naming_client_id_and_timeout() {
    let env = Env::with(None, AuthnReply::Hang).await;
    let gear = gear();
    env.init(&gear, config()).await.expect("init");

    let error = tokio::time::timeout(
        Duration::from_secs(10),
        gear.start(CancellationToken::new()),
    )
    .await
    .expect("the exchange is bounded")
    .expect_err("exchange times out");

    let text = error_text(&error);
    assert!(text.contains("client_id"), "{text}");
    assert!(text.contains("timeout"), "{text}");
    assert_eq!(gear.lifecycle_state(), GearLifecycleState::StartFailed);
}

#[tokio::test]
async fn starting_before_init_is_an_error_not_a_panic() {
    let gear = gear();

    let result = gear.start(CancellationToken::new()).await;

    assert!(result.is_err());
}

#[tokio::test]
async fn a_missing_summary_model_logs_an_error_and_start_continues() {
    let env = Env::new().await;
    *env.policy.catalog.lock().unwrap() = vec![entry(entry_json("model-a", true))];
    let gear = gear();
    env.init(&gear, config()).await.expect("init");
    let logs = LogCapture::start();

    gear.start(CancellationToken::new())
        .await
        .expect("start continues");

    assert_eq!(gear.lifecycle_state(), GearLifecycleState::Started);
    assert_eq!(
        logs.at_containing(Level::ERROR, "gpt-4.1-mini").len(),
        1,
        "{:?}",
        logs.records()
    );
    gear.stop(CancellationToken::new()).await.expect("stop");
}

#[tokio::test]
async fn a_disabled_configured_summary_model_logs_an_error_naming_it() {
    let env = Env::new().await;
    let gear = gear();
    env.init(
        &gear,
        config_with(&json!({ "thread_summary_worker": { "summary_model_id": "model-off" } })),
    )
    .await
    .expect("init");
    let logs = LogCapture::start();

    gear.start(CancellationToken::new())
        .await
        .expect("start continues");

    assert_eq!(logs.at_containing(Level::ERROR, "model-off").len(), 1);
    gear.stop(CancellationToken::new()).await.expect("stop");
}

#[tokio::test]
async fn an_enabled_summary_model_logs_no_error() {
    let env = Env::new().await;
    let gear = gear();
    env.init(&gear, config()).await.expect("init");
    let logs = LogCapture::start();

    gear.start(CancellationToken::new()).await.expect("start");

    assert_eq!(gear.lifecycle_state(), GearLifecycleState::Started);
    assert!(
        logs.at_containing(Level::ERROR, "gpt-4.1-mini").is_empty(),
        "{:?}",
        logs.records()
    );
    gear.stop(CancellationToken::new()).await.expect("stop");
}

#[tokio::test]
async fn an_unreadable_policy_snapshot_at_start_logs_an_error_and_start_continues() {
    let env = Env::new().await;
    env.registry.fail_list("registry down");
    let gear = gear();
    env.init(&gear, config()).await.expect("init");
    let logs = LogCapture::start();

    gear.start(CancellationToken::new())
        .await
        .expect("start continues");

    assert_eq!(gear.lifecycle_state(), GearLifecycleState::Started);
    assert_eq!(logs.at_containing(Level::ERROR, "gpt-4.1-mini").len(), 1);
    gear.stop(CancellationToken::new()).await.expect("stop");
}

#[tokio::test]
async fn start_logs_never_carry_a_secret() {
    let env = Env::new().await;
    env.oagw
        .fail_upstream("api.openai.com", Fault::SecretNotReadable);
    let logs = LogCapture::start();
    let gear = gear();
    env.init(&gear, config()).await.expect("init");

    gear.start(CancellationToken::new()).await.expect("start");
    assert_eq!(gear.lifecycle_state(), GearLifecycleState::Started);
    assert!(!env.oagw.upstream_creates().is_empty(), "provisioning ran");
    gear.stop(CancellationToken::new()).await.expect("stop");

    let text = logs.all_text();
    assert!(!text.contains(CLIENT_SECRET), "{text}");
    assert!(!text.contains("AUTH-CONFIG-SENTINEL"), "{text}");
}

// ---- gear-stop ------------------------------------------------------------------------------------------

#[tokio::test]
async fn stop_cancels_the_background_tasks_and_ends_the_lifecycle() {
    let env = Env::new().await;
    env.oagw
        .fail_upstream("api.openai.com", Fault::SecretNotReadable);
    let gear = gear();
    env.init(&gear, config()).await.expect("init");
    gear.start(CancellationToken::new()).await.expect("start");
    assert_eq!(workers(&gear), vec!["provider-reconcile"]);
    let attempts = env.oagw.upstream_creates().len();

    tokio::time::timeout(Duration::from_secs(5), gear.stop(CancellationToken::new()))
        .await
        .expect("stop is bounded")
        .expect("stop");

    assert_eq!(gear.lifecycle_state(), GearLifecycleState::Stopped);
    assert!(workers(&gear).is_empty());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        env.oagw.upstream_creates().len(),
        attempts,
        "no retry after stop"
    );
}

#[tokio::test]
async fn stop_gives_up_when_the_framework_deadline_has_already_fired() {
    let env = Env::new().await;
    let gear = gear();
    env.init(&gear, config()).await.expect("init");
    gear.start(CancellationToken::new()).await.expect("start");
    let deadline = CancellationToken::new();
    deadline.cancel();

    let result = tokio::time::timeout(Duration::from_secs(5), gear.stop(deadline))
        .await
        .expect("stop does not block past the deadline");

    result.expect("stop");
    assert_eq!(gear.lifecycle_state(), GearLifecycleState::Stopped);
}

#[tokio::test]
async fn stopping_a_gear_that_never_started_is_harmless() {
    let env = Env::new().await;
    let gear = gear();
    env.init(&gear, config()).await.expect("init");

    let result = gear.stop(CancellationToken::new()).await;

    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn the_outbox_stops_with_the_gear() {
    let env = Env::new().await;
    let gear = gear();
    env.init(&gear, config()).await.expect("init");
    gear.start(CancellationToken::new()).await.expect("start");
    let enqueuer = gear.outbox_enqueuer().expect("enqueuer");
    assert!(enqueuer.outbox().is_some());

    gear.stop(CancellationToken::new()).await.expect("stop");

    assert!(enqueuer.outbox().is_none(), "the pipeline was released");
}

// ---- state machine --------------------------------------------------------------------------------------

#[tokio::test]
async fn the_lifecycle_walks_created_initialized_started_stopped() {
    let env = Env::new().await;
    let gear = gear();
    assert_eq!(gear.lifecycle_state(), GearLifecycleState::Created);

    env.init(&gear, config()).await.expect("init");
    assert_eq!(gear.lifecycle_state(), GearLifecycleState::Initialized);

    gear.start(CancellationToken::new()).await.expect("start");
    assert_eq!(gear.lifecycle_state(), GearLifecycleState::Started);

    gear.stop(CancellationToken::new()).await.expect("stop");
    assert_eq!(gear.lifecycle_state(), GearLifecycleState::Stopped);
}
