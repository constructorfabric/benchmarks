//! Gear tests: init (config, clients, services), the lifecycle state, migrations, route
//! registration and the outbox part of start and stop.
//!
//! Slice S1: algorithms `gear-init`, `route-registration`, the outbox part of `gear-start`
//! and `gear-stop`, state `gear-lifecycle`, definition of done `lifecycle-routes`. `OAGW` provisioning, the
//! S2S exchange and the reconcile task belong to slice S2.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use authn_resolver_sdk::AuthNResolverClient;
use authz_resolver_sdk::AuthZResolverApi;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1};
use oagw_sdk::api::ServiceGatewayClientV1;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistryImpl;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::contracts::{DatabaseCapability, RestApiCapability};
use toolkit::gts::PluginV1;
use toolkit::{Gear, GearCtx};
use tower::ServiceExt;
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use uuid::Uuid;

use super::{LifecycleState, MiniChatGear};
use crate::domain::error::EnqueueSite;
use crate::infra::outbox::QueueKind;
use crate::infra::plugins::static_model_policy::config::StaticModelPolicyConfig;
use crate::infra::plugins::static_model_policy::service::StaticModelPolicyService;
use crate::infra::storage::migrations::all_migrations;
use crate::test_support::{
    Pdp, RecordingPdp, SUBJECT_USER_TYPE, StubAuthN, StubOagw, TENANT_A, TestDb, USER_A1, ctx_with,
    entry_json, gear_ctx, minimal_gear_config, raw_i64, sqlite_db,
};

const GEAR_NAME: &str = "mini-chat";

/// Which platform clients the test hub holds.
#[derive(Clone, Copy)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per platform client"
)]
struct Clients {
    authz: bool,
    authn: bool,
    oagw: bool,
}

const ALL_CLIENTS: Clients = Clients {
    authz: true,
    authn: true,
    oagw: true,
};

fn hub(clients: Clients, registry: MockTypesRegistryClient) -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::new());
    let registry: Arc<dyn TypesRegistryClient> = Arc::new(registry);
    hub.register::<dyn TypesRegistryClient>(registry);
    if clients.authz {
        let pdp: Arc<dyn AuthZResolverApi> = RecordingPdp::new(Pdp::NoConstraints);
        hub.register::<dyn AuthZResolverApi>(pdp);
    }
    if clients.authn {
        let authn: Arc<dyn AuthNResolverClient> = Arc::new(StubAuthN);
        hub.register::<dyn AuthNResolverClient>(authn);
    }
    if clients.oagw {
        let oagw: Arc<dyn ServiceGatewayClientV1> = Arc::new(StubOagw);
        hub.register::<dyn ServiceGatewayClientV1>(oagw);
    }
    hub
}

async fn database() -> TestDb {
    sqlite_db(all_migrations()).await
}

fn ctx_for(config: Option<Value>, hub: Arc<ClientHub>, db: &TestDb) -> GearCtx {
    gear_ctx(GEAR_NAME, config, hub, Some(db.db.clone()))
}

async fn initialized(config: Value) -> (MiniChatGear, GearCtx, TestDb) {
    let db = database().await;
    let ctx = ctx_for(
        Some(config),
        hub(ALL_CLIENTS, MockTypesRegistryClient::new()),
        &db,
    );
    let gear = MiniChatGear::default();
    gear.init(&ctx).await.expect("init succeeds");
    (gear, ctx, db)
}

fn error_text(err: &anyhow::Error) -> String {
    format!("{err:#}").to_lowercase()
}

// ── lifecycle state and init ─────────────────────────────────────────────────

#[test]
fn a_new_gear_is_in_the_created_state() {
    assert_eq!(
        MiniChatGear::default().lifecycle_state(),
        LifecycleState::Created
    );
    assert!(MiniChatGear::default().services().is_none());
}

#[test]
fn the_gear_is_registered_as_mini_chat() {
    assert_eq!(MiniChatGear::MODULE_NAME, GEAR_NAME);
}

#[tokio::test]
async fn a_valid_config_and_all_clients_initialize_the_gear() {
    let (gear, _ctx, _db) = initialized(minimal_gear_config()).await;
    assert_eq!(gear.lifecycle_state(), LifecycleState::Initialized);
    assert!(gear.services().is_some());
    assert!(gear.enqueuer().is_some());
}

#[tokio::test]
async fn an_invalid_config_fails_init_naming_the_key_and_marks_the_gear_failed() {
    let cases = vec![
        (json!({}), "client_credentials"),
        (
            json!({ "client_credentials": { "client_id": "c", "client_secret": "s" }, "bogus_key": 1 }),
            "bogus_key",
        ),
        (
            json!({ "client_credentials": { "client_id": "c", "client_secret": "s" }, "mcp": {} }),
            "mcp",
        ),
        (
            json!({ "client_credentials": { "client_id": "c", "client_secret": "s" }, "streaming": { "sse_channel_capacity": 5 } }),
            "sse_channel_capacity",
        ),
    ];
    for (config, needle) in cases {
        let db = database().await;
        let ctx = ctx_for(
            Some(config.clone()),
            hub(ALL_CLIENTS, MockTypesRegistryClient::new()),
            &db,
        );
        let gear = MiniChatGear::default();
        let err = gear.init(&ctx).await.expect_err("init must fail");
        assert!(error_text(&err).contains(needle), "{config}: {err:#}");
        assert_eq!(gear.lifecycle_state(), LifecycleState::Failed, "{config}");
        assert!(gear.services().is_none());
    }
}

#[tokio::test]
async fn the_config_section_is_required() {
    let db = database().await;
    let ctx = ctx_for(None, hub(ALL_CLIENTS, MockTypesRegistryClient::new()), &db);
    let gear = MiniChatGear::default();
    assert!(gear.init(&ctx).await.is_err());
    assert_eq!(gear.lifecycle_state(), LifecycleState::Failed);
}

#[tokio::test]
async fn a_missing_required_client_fails_init_naming_the_dependency() {
    let cases = vec![
        (
            Clients {
                authz: false,
                ..ALL_CLIENTS
            },
            "authz",
        ),
        (
            Clients {
                authn: false,
                ..ALL_CLIENTS
            },
            "authn",
        ),
        (
            Clients {
                oagw: false,
                ..ALL_CLIENTS
            },
            "oagw",
        ),
    ];
    for (clients, needle) in cases {
        let db = database().await;
        let ctx = ctx_for(
            Some(minimal_gear_config()),
            hub(clients, MockTypesRegistryClient::new()),
            &db,
        );
        let gear = MiniChatGear::default();
        let err = gear.init(&ctx).await.expect_err("init must fail");
        assert!(error_text(&err).contains(needle), "{needle}: {err:#}");
        assert_eq!(gear.lifecycle_state(), LifecycleState::Failed, "{needle}");
    }
}

#[tokio::test]
async fn init_expands_variables_only_where_allowed_and_fails_on_an_unset_one() {
    let config = json!({
        "client_credentials": { "client_id": "${MC_GEAR_CLIENT_ID}", "client_secret": "s" },
        "providers": { "main": { "kind": "openai_responses", "host": "${MC_GEAR_HOST}", "storage_kind": "openai" } }
    });
    temp_env::async_with_vars(
        [
            ("MC_GEAR_CLIENT_ID", Some("mini-chat")),
            ("MC_GEAR_HOST", Some("api.example.com")),
        ],
        async {
            let (gear, _ctx, _db) = initialized(config.clone()).await;
            assert_eq!(gear.lifecycle_state(), LifecycleState::Initialized);
        },
    )
    .await;

    temp_env::async_with_vars(
        [
            ("MC_GEAR_CLIENT_ID", Some("mini-chat")),
            ("MC_GEAR_HOST", None::<&str>),
        ],
        async {
            let db = database().await;
            let ctx = ctx_for(
                Some(config.clone()),
                hub(ALL_CLIENTS, MockTypesRegistryClient::new()),
                &db,
            );
            let gear = MiniChatGear::default();
            assert!(
                gear.init(&ctx).await.is_err(),
                "an unset variable without a default is a hard error"
            );
            assert_eq!(gear.lifecycle_state(), LifecycleState::Failed);
        },
    )
    .await;
}

#[tokio::test]
async fn init_resolves_no_plugin_so_an_empty_registry_does_not_fail_it() {
    let (gear, _ctx, _db) = initialized(minimal_gear_config()).await;
    assert_eq!(gear.lifecycle_state(), LifecycleState::Initialized);
}

// ── migrations ───────────────────────────────────────────────────────────────

#[test]
fn the_gear_declares_its_migrations_including_the_outbox_ones() {
    let names: Vec<String> = MiniChatGear::default()
        .migrations()
        .iter()
        .map(|m| m.name().to_owned())
        .collect();
    assert!(
        names
            .iter()
            .any(|n| n == "m001_create_toolkit_outbox_schema"),
        "{names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("initial_schema")),
        "{names:?}"
    );
}

// ── REST registration ────────────────────────────────────────────────────────

#[tokio::test]
async fn registering_routes_before_init_is_an_error() {
    let db = database().await;
    let ctx = ctx_for(
        Some(minimal_gear_config()),
        hub(ALL_CLIENTS, MockTypesRegistryClient::new()),
        &db,
    );
    let gear = MiniChatGear::default();
    let outcome = gear.register_rest(&ctx, Router::new(), &OpenApiRegistryImpl::new());
    assert!(outcome.is_err());
}

fn plugin_registry_with_catalog() -> (
    MockTypesRegistryClient,
    String,
    Arc<dyn MiniChatModelPolicyPluginClientV1>,
) {
    let (id, content) = PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
        "acme.core._.policy_a.v1",
        "constructorfabric",
        100,
    )
    .unwrap();
    let id: String = id.as_ref().to_owned();
    let instance = make_test_instance(&id, content);
    let cfg = StaticModelPolicyConfig::parse(json!({
        "model_catalog": [entry_json("gpt-on", "premium", true), entry_json("gpt-off", "standard", false)]
    }))
    .unwrap();
    let plugin: Arc<dyn MiniChatModelPolicyPluginClientV1> =
        Arc::new(StaticModelPolicyService::new(cfg).unwrap());
    (
        MockTypesRegistryClient::new().with_instances([instance]),
        id,
        plugin,
    )
}

async fn get(router: &Router, uri: &str) -> (u16, Value) {
    let mut request = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(ctx_with(USER_A1, TENANT_A, Some(SUBJECT_USER_TYPE), &["*"]));
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn the_registered_routes_serve_the_models_api_through_the_resolved_plugin() {
    let (registry, plugin_id, plugin) = plugin_registry_with_catalog();
    let hub = hub(ALL_CLIENTS, registry);
    hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
        ClientScope::gts_id(&plugin_id),
        plugin,
    );
    let db = database().await;
    let ctx = ctx_for(Some(minimal_gear_config()), hub, &db);
    let gear = MiniChatGear::default();
    gear.init(&ctx).await.expect("init");

    let router = gear
        .register_rest(&ctx, Router::new(), &OpenApiRegistryImpl::new())
        .expect("routes register after init");

    let (status, body) = get(&router, "/mini-chat/v1/models").await;
    assert_eq!(status, 200, "{body}");
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["model_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["gpt-on"]);

    assert_eq!(get(&router, "/mini-chat/v1/models/gpt-on").await.0, 200);
    let (status, body) = get(&router, "/mini-chat/v1/models/gpt-off").await;
    assert_eq!(status, 404);
    assert_eq!(
        body["context"]["resource_type"],
        "gts.cf.core.mini_chat.model.v1~"
    );
}

#[tokio::test]
async fn routes_follow_the_configured_url_prefix() {
    let (registry, plugin_id, plugin) = plugin_registry_with_catalog();
    let hub = hub(ALL_CLIENTS, registry);
    hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
        ClientScope::gts_id(&plugin_id),
        plugin,
    );
    let db = database().await;
    let mut config = minimal_gear_config();
    config["url_prefix"] = json!("/chat-api");
    let ctx = ctx_for(Some(config), hub, &db);
    let gear = MiniChatGear::default();
    gear.init(&ctx).await.expect("init");
    let router = gear
        .register_rest(&ctx, Router::new(), &OpenApiRegistryImpl::new())
        .unwrap();
    assert_eq!(get(&router, "/chat-api/v1/models").await.0, 200);
    assert_eq!(get(&router, "/mini-chat/v1/models").await.0, 404);
}

#[tokio::test]
async fn an_unresolvable_plugin_surfaces_as_a_500_over_http_not_as_a_startup_failure() {
    let (gear, ctx, _db) = initialized(minimal_gear_config()).await;
    let router = gear
        .register_rest(&ctx, Router::new(), &OpenApiRegistryImpl::new())
        .unwrap();
    let (status, body) = get(&router, "/mini-chat/v1/models").await;
    assert_eq!(status, 500, "{body}");
}

// ── outbox part of start and stop ────────────────────────────────────────────

#[tokio::test]
async fn starting_the_outbox_before_init_is_an_error() {
    assert!(MiniChatGear::default().start_outbox().await.is_err());
}

#[tokio::test]
async fn start_registers_the_five_queues_from_the_config_and_binds_the_enqueuer() {
    let mut config = minimal_gear_config();
    config["outbox"] = json!({ "num_partitions": 2, "audit_queue_name": "acme.audit" });
    let (gear, _ctx, db) = initialized(config).await;
    gear.start_outbox().await.expect("outbox starts");

    let enqueuer = Arc::clone(gear.enqueuer().unwrap());
    for kind in [
        QueueKind::Usage,
        QueueKind::AttachmentCleanup,
        QueueKind::ChatCleanup,
        QueueKind::ThreadSummary,
        QueueKind::Audit,
    ] {
        let enq = Arc::clone(&enqueuer);
        let wake = db
            .provider
            .transaction(move |tx| {
                let enq = Arc::clone(&enq);
                Box::pin(async move {
                    enq.enqueue_raw(
                        tx,
                        kind,
                        Uuid::from_u128(77),
                        b"{}".to_vec(),
                        "mini_chat.test",
                        EnqueueSite::TurnRetry,
                    )
                    .await
                })
            })
            .await
            .unwrap_or_else(|e| panic!("{kind:?}: {e}"));
        drop(wake);
    }
    assert_eq!(
        raw_i64(&db.raw, "SELECT COUNT(*) FROM toolkit_outbox_body").await,
        5
    );
    gear.stop_outbox(&CancellationToken::new()).await;
}

#[tokio::test]
async fn stopping_the_outbox_is_idempotent_and_safe_before_start() {
    let (gear, _ctx, _db) = initialized(minimal_gear_config()).await;
    let token = CancellationToken::new();
    gear.stop_outbox(&token).await;
    gear.start_outbox().await.expect("outbox starts");
    gear.stop_outbox(&token).await;
    gear.stop_outbox(&token).await;
}

#[tokio::test]
async fn stopping_gives_up_when_the_framework_deadline_has_already_fired() {
    let (gear, _ctx, _db) = initialized(minimal_gear_config()).await;
    gear.start_outbox().await.expect("outbox starts");
    let fired = CancellationToken::new();
    fired.cancel();
    gear.stop_outbox(&fired).await;
}
