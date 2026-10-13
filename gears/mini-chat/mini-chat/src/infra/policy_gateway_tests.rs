#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of the model-policy gateway over the real adapters.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, PolicyPluginError,
    PolicySnapshot, PolicyVersionInfo, PublishError, UsageEvent, UserLimits,
};
use serde_json::json;
use toolkit::client_hub::{ClientHub, ClientScope};
use types_registry_sdk::TypesRegistryClient;
use uuid::Uuid;

use super::{ModelPolicyGateway, POLICY_PLUGIN_CALL_TIMEOUT, PolicyGatewayError};
use crate::domain::error::DomainError;
use crate::infra::plugin_resolution::PluginResolutionError;
use crate::infra::plugins::static_model_policy::{
    StaticModelPolicyConfig, StaticModelPolicyService,
};
use crate::infra::test_support::FakeTypesRegistry;

const VENDOR: &str = "constructorfabric";

fn user() -> Uuid {
    Uuid::from_u128(0x55)
}

fn instance_id(segment: &str) -> String {
    format!(
        "{}{segment}",
        MiniChatModelPolicyPluginSpecV1::gts_type_id()
    )
}

/// A plugin whose calls never answer.
struct HangingPlugin;

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for HangingPlugin {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, PolicyPluginError> {
        std::future::pending().await
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<PolicySnapshot, PolicyPluginError> {
        std::future::pending().await
    }

    async fn get_user_limits(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<UserLimits, PolicyPluginError> {
        std::future::pending().await
    }

    async fn publish_usage(&self, _event: UsageEvent) -> Result<(), PublishError> {
        std::future::pending().await
    }
}

struct Fixture {
    registry: Arc<FakeTypesRegistry>,
    hub: Arc<ClientHub>,
}

impl Fixture {
    fn new() -> Self {
        let registry = FakeTypesRegistry::new();
        let hub = Arc::new(ClientHub::new());
        hub.register::<dyn TypesRegistryClient>(
            Arc::clone(&registry) as Arc<dyn TypesRegistryClient>
        );
        Self { registry, hub }
    }

    fn register(
        &self,
        segment: &str,
        vendor: &str,
        priority: i16,
        client: Arc<dyn MiniChatModelPolicyPluginClientV1>,
    ) {
        let id = instance_id(segment);
        self.registry.add_instance(
            &id,
            json!({ "id": id, "vendor": vendor, "priority": priority }),
        );
        self.hub
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
                ClientScope::gts_id(&id),
                client,
            );
    }

    fn gateway(&self) -> ModelPolicyGateway {
        ModelPolicyGateway::new(&self.hub, VENDOR)
    }
}

fn static_plugin() -> Arc<dyn MiniChatModelPolicyPluginClientV1> {
    Arc::new(StaticModelPolicyService::new(
        StaticModelPolicyConfig::default(),
    ))
}

#[test]
fn plugin_calls_are_bounded_by_ten_seconds() {
    assert_eq!(POLICY_PLUGIN_CALL_TIMEOUT, Duration::from_secs(10));
}

#[tokio::test]
async fn serves_the_policy_of_the_registered_plugin() {
    let fixture = Fixture::new();
    fixture.register("cf.core._.static.v1", VENDOR, 100, static_plugin());
    let gateway = fixture.gateway();

    let version = gateway
        .current_policy_version(user())
        .await
        .expect("version");
    let snapshot = gateway.policy_snapshot(user(), 1).await.expect("snapshot");
    let limits = gateway.user_limits(user(), 1).await.expect("limits");

    assert_eq!(version.policy_version, 1);
    assert_eq!(snapshot.policy_version, 1);
    assert_eq!(limits.user_id, user());
}

#[tokio::test]
async fn nothing_is_resolved_before_the_first_call() {
    let fixture = Fixture::new();
    let gateway = fixture.gateway();

    fixture.register("cf.core._.late.v1", VENDOR, 100, static_plugin());

    gateway
        .current_policy_version(user())
        .await
        .expect("a plugin registered after construction is found");
}

#[tokio::test]
async fn the_lowest_priority_plugin_of_the_vendor_wins() {
    let fixture = Fixture::new();
    fixture.register("cf.core._.hang.v1", VENDOR, 200, Arc::new(HangingPlugin));
    fixture.register("cf.core._.static.v1", VENDOR, 10, static_plugin());
    fixture.register("cf.core._.foreign.v1", "acme", 1, Arc::new(HangingPlugin));

    let version = fixture
        .gateway()
        .with_call_timeout(Duration::from_secs(2))
        .current_policy_version(user())
        .await
        .expect("priority 10 is the static plugin");

    assert_eq!(version.policy_version, 1);
}

#[tokio::test]
async fn no_plugin_of_the_vendor_is_not_registered() {
    let fixture = Fixture::new();
    fixture.register("cf.core._.foreign.v1", "acme", 1, static_plugin());

    let err = fixture
        .gateway()
        .current_policy_version(user())
        .await
        .expect_err("no plugin");

    assert_eq!(err, PolicyGatewayError::NotRegistered);
}

#[tokio::test]
async fn a_registry_failure_is_a_resolution_error() {
    let fixture = Fixture::new();
    fixture.registry.fail_list("registry down");

    let err = fixture
        .gateway()
        .policy_snapshot(user(), 1)
        .await
        .expect_err("registry failure");

    assert!(
        matches!(
            err,
            PolicyGatewayError::Resolution(PluginResolutionError::RegistryQuery(_))
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn a_registered_instance_without_a_client_is_a_resolution_error() {
    let fixture = Fixture::new();
    let id = instance_id("cf.core._.ghost.v1");
    fixture
        .registry
        .add_instance(&id, json!({ "id": id, "vendor": VENDOR, "priority": 1 }));

    let err = fixture
        .gateway()
        .user_limits(user(), 1)
        .await
        .expect_err("client missing");

    assert!(
        matches!(
            err,
            PolicyGatewayError::Resolution(PluginResolutionError::ClientMissing { .. })
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn a_plugin_error_is_passed_through() {
    let fixture = Fixture::new();
    fixture.register("cf.core._.static.v1", VENDOR, 100, static_plugin());

    let err = fixture
        .gateway()
        .policy_snapshot(user(), 7)
        .await
        .expect_err("unknown version");

    assert!(
        matches!(
            err,
            PolicyGatewayError::Plugin(PolicyPluginError::NotFound(_))
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn a_plugin_that_does_not_answer_in_time_is_a_timeout() {
    let fixture = Fixture::new();
    fixture.register("cf.core._.hang.v1", VENDOR, 100, Arc::new(HangingPlugin));
    let gateway = fixture
        .gateway()
        .with_call_timeout(Duration::from_millis(50));

    let err = tokio::time::timeout(Duration::from_secs(5), gateway.policy_snapshot(user(), 1))
        .await
        .expect("the gateway enforces its own bound")
        .expect_err("timeout");

    assert_eq!(err, PolicyGatewayError::Timeout);
}

#[test]
fn gateway_errors_become_a_policy_resolution_domain_error() {
    let err = PolicyGatewayError::Timeout.into_domain();

    assert!(
        matches!(err, DomainError::PolicyResolution(_)),
        "got {err:?}"
    );
}
