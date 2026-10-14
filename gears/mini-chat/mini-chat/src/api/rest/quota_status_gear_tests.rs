//! Wiring test of the quota status endpoint through the real gear: `MiniChatGear` initialized
//! over `SQLite`, the bundled static model-policy plugin and a tenant-only PDP, then
//! `GET /mini-chat/v1/quota/status` through the registered router. The author stage wires the
//! quota service into the gear init so that the route is served.
//!
//! Slice S1-all: flow `cpt-cf-mini-chat-flow-quota-and-billing-quota-status`, definition of done
//! `cpt-cf-mini-chat-dod-quota-and-billing-status-api` (route registered under `url_prefix`
//! behind the license gate).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use authz_resolver_sdk::AuthZResolverApi;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1};
use serde_json::{Value, json};
use time::OffsetDateTime;
use toolkit::Gear;
use toolkit::api::operation_builder::OperationSpec;
use toolkit::api::{OpenApiRegistry, OpenApiRegistryImpl};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::contracts::RestApiCapability;
use toolkit::gts::PluginV1;
use tower::ServiceExt;
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::Schema;

use crate::gear::MiniChatGear;
use crate::infra::plugins::static_model_policy::config::StaticModelPolicyConfig;
use crate::infra::plugins::static_model_policy::service::StaticModelPolicyService;
use crate::infra::storage::migrations::all_migrations;
use crate::test_support::quota::{SeedRow, seed_rows};
use crate::test_support::s2::{RecordingOagw, ScriptedAuthN};
use crate::test_support::{
    Pdp, RecordingPdp, RecordingRegistry, SUBJECT_USER_TYPE, TENANT_A, USER_A1, ctx_with,
    entry_json, gear_ctx, minimal_gear_config, sqlite_db,
};

const BASE_LICENSE: &str = "gts.cf.core.lic.feat.v1~cf.core.global.base.v1";

struct RecordingOpenApi {
    inner: OpenApiRegistryImpl,
    ops: Mutex<Vec<OperationSpec>>,
}

impl OpenApiRegistry for RecordingOpenApi {
    fn register_operation(&self, spec: &OperationSpec) {
        self.ops.lock().unwrap().push(spec.clone());
        self.inner.register_operation(spec);
    }

    fn ensure_schema_raw(&self, name: &str, schemas: Vec<(String, RefOr<Schema>)>) -> String {
        self.inner.ensure_schema_raw(name, schemas)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct GearRig {
    router: Router,
    ops: Arc<RecordingOpenApi>,
    db: crate::test_support::TestDb,
}

async fn gear_rig() -> GearRig {
    let hub = Arc::new(ClientHub::new());
    let pdp: Arc<dyn AuthZResolverApi> = RecordingPdp::new(Pdp::TenantOnly);
    hub.register::<dyn AuthZResolverApi>(pdp);
    let authn: Arc<dyn authn_resolver_sdk::AuthNResolverClient> = ScriptedAuthN::ok();
    hub.register::<dyn authn_resolver_sdk::AuthNResolverClient>(authn);
    let oagw: Arc<dyn oagw_sdk::api::ServiceGatewayClientV1> = RecordingOagw::accepting();
    hub.register::<dyn oagw_sdk::api::ServiceGatewayClientV1>(oagw);

    let (id, content) = PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
        "acme.core._.policy_a.v1",
        "constructorfabric",
        100,
    )
    .unwrap();
    let id: String = id.as_ref().to_owned();
    let registry = RecordingRegistry::new(MockTypesRegistryClient::new());
    registry.add_instance(make_test_instance(&id, content));
    let dynamic_registry: Arc<dyn TypesRegistryClient> = registry;
    hub.register::<dyn TypesRegistryClient>(dynamic_registry);
    let config = StaticModelPolicyConfig::parse(json!({
        "model_catalog": [entry_json("gpt-on", "premium", true)]
    }))
    .unwrap();
    let plugin: Arc<dyn MiniChatModelPolicyPluginClientV1> =
        Arc::new(StaticModelPolicyService::new(config).unwrap());
    hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(ClientScope::gts_id(&id), plugin);

    let db = sqlite_db(all_migrations()).await;
    let ctx = gear_ctx(
        "mini-chat",
        Some(minimal_gear_config()),
        Arc::clone(&hub),
        Some(db.db.clone()),
    );
    let gear = MiniChatGear::default();
    gear.init(&ctx).await.expect("init");
    let ops = Arc::new(RecordingOpenApi {
        inner: OpenApiRegistryImpl::new(),
        ops: Mutex::new(Vec::new()),
    });
    let router = gear
        .register_rest(&ctx, Router::new(), ops.as_ref())
        .expect("routes register");
    GearRig { router, ops, db }
}

async fn get_status(router: &Router) -> (u16, Value) {
    let mut request = Request::builder()
        .method("GET")
        .uri("/mini-chat/v1/quota/status")
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(ctx_with(USER_A1, TENANT_A, Some(SUBJECT_USER_TYPE), &["*"]));
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn the_gear_serves_the_quota_status_with_the_limits_of_the_policy_plugin() {
    let rig = gear_rig().await;
    let (status, body) = get_status(&rig.router).await;
    assert_eq!(status, 200, "{body}");
    let find = |tier: &str, period: &str| -> Value {
        body["tiers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["tier"] == tier)
            .unwrap()["periods"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["period"] == period)
            .unwrap()
            .clone()
    };
    // the static plugin defaults: standard (total) 100M/1B, premium 50M/500M
    assert_eq!(find("total", "daily")["limit_credits_micro"], 100_000_000);
    assert_eq!(
        find("total", "monthly")["limit_credits_micro"],
        1_000_000_000
    );
    assert_eq!(find("premium", "daily")["limit_credits_micro"], 50_000_000);
    assert_eq!(
        find("premium", "monthly")["limit_credits_micro"],
        500_000_000
    );
    assert_eq!(find("total", "daily")["used_credits_micro"], 0);
    assert_eq!(body["warning_threshold_pct"], 80);
}

#[tokio::test]
async fn the_gear_reads_the_rows_of_the_current_utc_periods() {
    let rig = gear_rig().await;
    let today = OffsetDateTime::now_utc().date();
    let month = today.replace_day(1).unwrap();
    seed_rows(
        &rig.db,
        &[
            seed("daily", &today.to_string(), 25_000_000),
            seed("monthly", &month.to_string(), 40_000_000),
        ],
    )
    .await;
    let (status, body) = get_status(&rig.router).await;
    assert_eq!(status, 200, "{body}");
    let total = body["tiers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["tier"] == "total")
        .unwrap();
    assert_eq!(total["periods"][0]["used_credits_micro"], 25_000_000);
    assert_eq!(total["periods"][1]["used_credits_micro"], 40_000_000);
}

fn seed(period: &'static str, start: &str, spent: i64) -> SeedRow {
    crate::test_support::quota::row(period, start, "total").spent(spent)
}

#[tokio::test]
async fn the_status_route_is_registered_once_under_the_prefix_behind_the_base_license_gate() {
    let rig = gear_rig().await;
    let ops = rig.ops.ops.lock().unwrap();
    let status: Vec<_> = ops
        .iter()
        .filter(|op| op.method.as_str() == "GET" && op.path == "/mini-chat/v1/quota/status")
        .collect();
    assert_eq!(status.len(), 1);
    assert_eq!(
        status[0].operation_id.as_deref(),
        Some("mini_chat.get_quota_status")
    );
    assert!(status[0].authenticated);
    assert_eq!(
        status[0]
            .license_requirement
            .as_ref()
            .unwrap()
            .license_names,
        vec![BASE_LICENSE.to_owned()]
    );
}
