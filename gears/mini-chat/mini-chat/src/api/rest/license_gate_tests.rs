//! License gate tests over the registered gear routes. The tenant license check itself is the
//! api-gateway `license_manager` middleware (403 `permission_denied` with
//! `LICENSE_FEATURE_REQUIRED` before the handler), which is not part of this crate; here the
//! gear side is pinned: every route declares exactly the base license feature, and the gear
//! performs no license check of its own.
//!
//! Slice S2: algorithm `license-gate`, definition of done `license-gate`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, ModelPolicyPluginError,
    PolicySnapshot, PublishError, UsageEvent, UserLicenseStatus, UserLimits,
};
use serde_json::{Value, json};
use toolkit::Gear;
use toolkit::api::operation_builder::OperationSpec;
use toolkit::api::{OpenApiRegistry, OpenApiRegistryImpl};
use toolkit::client_hub::ClientScope;
use toolkit::contracts::RestApiCapability;
use toolkit::gts::PluginV1;
use tower::ServiceExt;
use types_registry_sdk::testing::make_test_instance;
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::Schema;
use uuid::Uuid;

use crate::gear::MiniChatGear;
use crate::infra::plugins::static_model_policy::config::StaticModelPolicyConfig;
use crate::infra::plugins::static_model_policy::service::StaticModelPolicyService;
use crate::infra::storage::migrations::all_migrations;
use crate::test_support::s2::{RecordingOagw, ScriptedAuthN, platform};
use crate::test_support::{
    SUBJECT_USER_TYPE, TENANT_A, TENANT_B, USER_A1, ctx_with, entry_json, gear_ctx,
    minimal_gear_config, sqlite_db,
};

const BASE_LICENSE: &str = "gts.cf.core.lic.feat.v1~cf.core.global.base.v1";

/// An `OpenApiRegistry` that records every registered operation.
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

/// A policy plugin that serves a catalog and counts `check_user_license` calls.
struct CountingPolicy {
    inner: StaticModelPolicyService,
    license_checks: AtomicUsize,
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for CountingPolicy {
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<u64, ModelPolicyPluginError> {
        self.inner.get_current_policy_version(user_id).await
    }

    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, ModelPolicyPluginError> {
        self.inner
            .get_policy_snapshot(user_id, policy_version)
            .await
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, ModelPolicyPluginError> {
        self.inner.get_user_limits(user_id, policy_version).await
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError> {
        self.inner.publish_usage(event).await
    }

    async fn check_user_license(
        &self,
        _user_id: Uuid,
    ) -> Result<UserLicenseStatus, ModelPolicyPluginError> {
        self.license_checks.fetch_add(1, Ordering::SeqCst);
        Ok(UserLicenseStatus { active: false })
    }
}

struct Rig {
    router: Router,
    ops: Arc<RecordingOpenApi>,
    plugin: Arc<CountingPolicy>,
}

async fn rig() -> Rig {
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
    let config = StaticModelPolicyConfig::parse(json!({
        "model_catalog": [entry_json("gpt-on", "premium", true)]
    }))
    .unwrap();
    let plugin = Arc::new(CountingPolicy {
        inner: StaticModelPolicyService::new(config).unwrap(),
        license_checks: AtomicUsize::new(0),
    });
    let client: Arc<dyn MiniChatModelPolicyPluginClientV1> = plugin.clone();
    platform
        .hub
        .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(ClientScope::gts_id(&id), client);

    let db = sqlite_db(all_migrations()).await;
    let ctx = gear_ctx(
        "mini-chat",
        Some(minimal_gear_config()),
        Arc::clone(&platform.hub),
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
    Rig {
        router,
        ops,
        plugin,
    }
}

async fn get(router: &Router, uri: &str, tenant: Uuid) -> (u16, Value) {
    let mut request = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(ctx_with(USER_A1, tenant, Some(SUBJECT_USER_TYPE), &["*"]));
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
async fn every_registered_route_declares_exactly_the_base_license_feature() {
    let rig = rig().await;
    let ops = rig.ops.ops.lock().unwrap();
    assert!(!ops.is_empty(), "the gear registers routes");
    for op in ops.iter() {
        assert!(op.authenticated, "{}", op.path);
        let license = op
            .license_requirement
            .as_ref()
            .unwrap_or_else(|| panic!("{} declares no license feature", op.path));
        assert_eq!(
            license.license_names,
            vec![BASE_LICENSE.to_owned()],
            "{}",
            op.path
        );
    }
}

#[tokio::test]
async fn no_route_requires_the_ai_chat_feature_in_p1() {
    let rig = rig().await;
    for op in rig.ops.ops.lock().unwrap().iter() {
        let names = &op.license_requirement.as_ref().unwrap().license_names;
        assert!(
            names.iter().all(|name| !name.contains("ai_chat")),
            "{}",
            op.path
        );
    }
}

#[tokio::test]
async fn the_gear_never_calls_check_user_license_whatever_the_tenant() {
    let rig = rig().await;
    for tenant in [TENANT_A, TENANT_B] {
        assert_eq!(
            get(&rig.router, "/mini-chat/v1/models", tenant).await.0,
            200
        );
        assert_eq!(
            get(&rig.router, "/mini-chat/v1/models/gpt-on", tenant)
                .await
                .0,
            200
        );
        assert_eq!(
            get(&rig.router, "/mini-chat/v1/models/missing", tenant)
                .await
                .0,
            404
        );
    }
    assert_eq!(
        rig.plugin.license_checks.load(Ordering::SeqCst),
        0,
        "the license is checked by the api-gateway middleware, not by the gear"
    );
}
