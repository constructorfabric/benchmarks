//! Policy snapshot access: plugin resolution through types-registry, no cache, user limits
//! kept apart, `check_user_license` never called.
//!
//! Slice S1, algorithm `policy-snapshot-access`, definition of done `model-policy`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1,
    ModelPolicyPluginError, ModelTier, PolicySnapshot, PublishError, TierLimits, UsageEvent,
    UserLicenseStatus, UserLimits,
};
use serde_json::json;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::gts::PluginV1;
use types_registry_sdk::testing::{MockTypesRegistryClient, internal, make_test_instance};
use types_registry_sdk::{GtsInstance, TypesRegistryClient};
use uuid::Uuid;

use super::GtsModelPolicyGateway;
use crate::domain::error::DomainError;
use crate::domain::model_policy::ModelPolicyGateway;
use crate::test_support::{USER_A1, entry, snapshot};

/// A plugin that records how it is called.
struct StubPlugin {
    version: u64,
    snapshot: Option<PolicySnapshot>,
    fail_version: bool,
    version_calls: AtomicUsize,
    snapshot_calls: Mutex<Vec<(Uuid, u64)>>,
    limits_calls: Mutex<Vec<(Uuid, u64)>>,
    license_calls: AtomicUsize,
}

impl StubPlugin {
    fn serving(version: u64, model_id: &str) -> Arc<Self> {
        let mut snap = snapshot(vec![entry(model_id, ModelTier::Standard, true, &json!({}))]);
        snap.policy_version = version;
        Arc::new(Self {
            version,
            snapshot: Some(snap),
            fail_version: false,
            version_calls: AtomicUsize::new(0),
            snapshot_calls: Mutex::new(Vec::new()),
            limits_calls: Mutex::new(Vec::new()),
            license_calls: AtomicUsize::new(0),
        })
    }

    fn failing_snapshot(version: u64) -> Arc<Self> {
        Arc::new(Self {
            version,
            snapshot: None,
            fail_version: false,
            version_calls: AtomicUsize::new(0),
            snapshot_calls: Mutex::new(Vec::new()),
            limits_calls: Mutex::new(Vec::new()),
            license_calls: AtomicUsize::new(0),
        })
    }

    fn failing_version() -> Arc<Self> {
        Arc::new(Self {
            version: 0,
            snapshot: None,
            fail_version: true,
            version_calls: AtomicUsize::new(0),
            snapshot_calls: Mutex::new(Vec::new()),
            limits_calls: Mutex::new(Vec::new()),
            license_calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StubPlugin {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<u64, ModelPolicyPluginError> {
        self.version_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_version {
            return Err(ModelPolicyPluginError::Unavailable(
                "ccm timeout provider-secret-detail".to_owned(),
            ));
        }
        Ok(self.version)
    }

    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, ModelPolicyPluginError> {
        self.snapshot_calls
            .lock()
            .unwrap()
            .push((user_id, policy_version));
        self.snapshot
            .clone()
            .ok_or(ModelPolicyPluginError::PolicyVersionNotFound(
                policy_version,
            ))
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, ModelPolicyPluginError> {
        self.limits_calls
            .lock()
            .unwrap()
            .push((user_id, policy_version));
        let tier = TierLimits {
            limit_daily_credits_micro: 11,
            limit_monthly_credits_micro: 22,
        };
        Ok(UserLimits {
            user_id,
            policy_version,
            standard: tier,
            premium: tier,
        })
    }

    async fn publish_usage(&self, _event: UsageEvent) -> Result<(), PublishError> {
        Ok(())
    }

    async fn check_user_license(
        &self,
        _user_id: Uuid,
    ) -> Result<UserLicenseStatus, ModelPolicyPluginError> {
        self.license_calls.fetch_add(1, Ordering::SeqCst);
        Ok(UserLicenseStatus { active: true })
    }
}

/// A plugin instance registered for the model-policy spec.
fn model_policy_instance(segment: &str, vendor: &str, priority: i16) -> (String, GtsInstance) {
    let (id, content) =
        PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(segment, vendor, priority)
            .unwrap();
    let id: String = id.as_ref().to_owned();
    let instance = make_test_instance(&id, content);
    (id, instance)
}

fn hub_with_registry(registry: Arc<MockTypesRegistryClient>) -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::new());
    let dynamic: Arc<dyn TypesRegistryClient> = registry;
    hub.register::<dyn TypesRegistryClient>(dynamic);
    hub
}

fn register_plugin(hub: &ClientHub, id: &str, plugin: Arc<StubPlugin>) {
    let client: Arc<dyn MiniChatModelPolicyPluginClientV1> = plugin;
    hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(ClientScope::gts_id(id), client);
}

/// A gateway over one registered plugin; returns the plugin for call assertions.
fn single_plugin(
    version: u64,
) -> (
    GtsModelPolicyGateway,
    Arc<StubPlugin>,
    Arc<MockTypesRegistryClient>,
) {
    let (id, instance) = model_policy_instance("acme.core._.policy_a.v1", "constructorfabric", 100);
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    let hub = hub_with_registry(Arc::clone(&registry));
    let plugin = StubPlugin::serving(version, "model-a");
    register_plugin(&hub, &id, Arc::clone(&plugin));
    (
        GtsModelPolicyGateway::new(hub, "constructorfabric".to_owned()),
        plugin,
        registry,
    )
}

// ── resolution ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_current_snapshot_is_the_snapshot_of_the_current_version() {
    let (gateway, plugin, _) = single_plugin(7);
    let snap = gateway.current_snapshot(USER_A1).await.expect("snapshot");
    assert_eq!(snap.policy_version, 7);
    assert_eq!(snap.model_catalog[0].id, "model-a");
    assert_eq!(plugin.version_calls.load(Ordering::SeqCst), 1);
    assert_eq!(*plugin.snapshot_calls.lock().unwrap(), vec![(USER_A1, 7)]);
}

#[tokio::test]
async fn among_the_configured_vendor_the_lowest_priority_value_wins() {
    let (id_low, low) =
        model_policy_instance("acme.core._.policy_low.v1", "constructorfabric", 100);
    let (id_high, high) =
        model_policy_instance("acme.core._.policy_high.v1", "constructorfabric", 200);
    let (id_other, other) = model_policy_instance("acme.core._.policy_other.v1", "acme", 1);
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([high, other, low]));
    let hub = hub_with_registry(registry);
    register_plugin(&hub, &id_low, StubPlugin::serving(10, "from-low"));
    register_plugin(&hub, &id_high, StubPlugin::serving(20, "from-high"));
    register_plugin(&hub, &id_other, StubPlugin::serving(30, "from-other"));

    let default_vendor =
        GtsModelPolicyGateway::new(Arc::clone(&hub), "constructorfabric".to_owned());
    assert_eq!(
        default_vendor
            .current_snapshot(USER_A1)
            .await
            .unwrap()
            .model_catalog[0]
            .id,
        "from-low"
    );

    let other_vendor = GtsModelPolicyGateway::new(hub, "acme".to_owned());
    assert_eq!(
        other_vendor
            .current_snapshot(USER_A1)
            .await
            .unwrap()
            .model_catalog[0]
            .id,
        "from-other"
    );
}

#[tokio::test]
async fn a_vendor_without_a_registered_plugin_is_policy_unavailable() {
    let (_, instance) = model_policy_instance("acme.core._.policy_a.v1", "acme", 100);
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    let gateway =
        GtsModelPolicyGateway::new(hub_with_registry(registry), "constructorfabric".to_owned());
    let err = gateway.current_snapshot(USER_A1).await.unwrap_err();
    assert!(
        matches!(err, DomainError::PolicyUnavailable { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn an_empty_registry_is_policy_unavailable() {
    let registry = Arc::new(MockTypesRegistryClient::new());
    let gateway =
        GtsModelPolicyGateway::new(hub_with_registry(registry), "constructorfabric".to_owned());
    let err = gateway.current_snapshot(USER_A1).await.unwrap_err();
    assert!(
        matches!(err, DomainError::PolicyUnavailable { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_registry_failure_is_policy_unavailable() {
    let registry =
        Arc::new(MockTypesRegistryClient::new().with_list_error(internal("registry down")));
    let gateway =
        GtsModelPolicyGateway::new(hub_with_registry(registry), "constructorfabric".to_owned());
    let err = gateway.current_snapshot(USER_A1).await.unwrap_err();
    assert!(
        matches!(err, DomainError::PolicyUnavailable { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_missing_registry_client_is_policy_unavailable() {
    let gateway =
        GtsModelPolicyGateway::new(Arc::new(ClientHub::new()), "constructorfabric".to_owned());
    let err = gateway.current_snapshot(USER_A1).await.unwrap_err();
    assert!(
        matches!(err, DomainError::PolicyUnavailable { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_resolved_instance_without_a_hub_client_is_policy_unavailable() {
    let (_, instance) = model_policy_instance("acme.core._.policy_a.v1", "constructorfabric", 100);
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    let gateway =
        GtsModelPolicyGateway::new(hub_with_registry(registry), "constructorfabric".to_owned());
    let err = gateway.current_snapshot(USER_A1).await.unwrap_err();
    assert!(
        matches!(err, DomainError::PolicyUnavailable { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn instances_of_another_plugin_spec_are_not_selected() {
    let (audit_id, audit_content) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
        "acme.core._.audit_a.v1",
        "constructorfabric",
        1,
    )
    .unwrap();
    let audit_instance = make_test_instance(audit_id.as_ref(), audit_content);
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([audit_instance]));
    let gateway =
        GtsModelPolicyGateway::new(hub_with_registry(registry), "constructorfabric".to_owned());
    // the mock ignores the query pattern, so the gateway itself must refuse a foreign spec
    let err = gateway.current_snapshot(USER_A1).await.unwrap_err();
    assert!(
        matches!(err, DomainError::PolicyUnavailable { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn the_plugin_is_resolved_lazily_and_the_found_instance_is_reused() {
    let (gateway, _, registry) = single_plugin(1);
    assert_eq!(
        registry.list_instance_calls(),
        0,
        "nothing is resolved at construction"
    );
    gateway.current_snapshot(USER_A1).await.unwrap();
    gateway.current_snapshot(USER_A1).await.unwrap();
    assert_eq!(registry.list_instance_calls(), 1);
    let queries = registry.received_instance_queries();
    assert_eq!(queries.len(), 1);
}

// ── plugin failures ──────────────────────────────────────────────────────────

#[tokio::test]
async fn a_failing_version_call_is_policy_unavailable_without_leaking_the_cause_text() {
    let (id, instance) = model_policy_instance("acme.core._.policy_a.v1", "constructorfabric", 100);
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    let hub = hub_with_registry(registry);
    register_plugin(&hub, &id, StubPlugin::failing_version());
    let gateway = GtsModelPolicyGateway::new(hub, "constructorfabric".to_owned());
    let err = gateway.current_snapshot(USER_A1).await.unwrap_err();
    assert!(
        matches!(err, DomainError::PolicyUnavailable { .. }),
        "{err:?}"
    );
    assert!(!err.to_string().contains("provider-secret-detail"));
}

#[tokio::test]
async fn a_failing_snapshot_call_is_policy_unavailable() {
    let (id, instance) = model_policy_instance("acme.core._.policy_a.v1", "constructorfabric", 100);
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    let hub = hub_with_registry(registry);
    register_plugin(&hub, &id, StubPlugin::failing_snapshot(3));
    let gateway = GtsModelPolicyGateway::new(hub, "constructorfabric".to_owned());
    for result in [
        gateway.current_snapshot(USER_A1).await,
        gateway.snapshot_of_version(USER_A1, 3).await,
    ] {
        assert!(
            matches!(result, Err(DomainError::PolicyUnavailable { .. })),
            "{result:?}"
        );
    }
}

// ── no cache, limits apart, no license call ──────────────────────────────────

#[tokio::test]
async fn there_is_no_local_snapshot_cache() {
    let (gateway, plugin, _) = single_plugin(4);
    gateway.current_snapshot(USER_A1).await.unwrap();
    gateway.current_snapshot(USER_A1).await.unwrap();
    gateway.current_snapshot(USER_A1).await.unwrap();
    assert_eq!(plugin.version_calls.load(Ordering::SeqCst), 3);
    assert_eq!(plugin.snapshot_calls.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn settlement_asks_only_for_the_snapshot_of_the_turn_version() {
    let (gateway, plugin, _) = single_plugin(9);
    let snap = gateway.snapshot_of_version(USER_A1, 5).await.unwrap();
    assert!(snap.policy_version == 9 || snap.policy_version == 5);
    assert_eq!(
        plugin.version_calls.load(Ordering::SeqCst),
        0,
        "the current version is not asked"
    );
    assert_eq!(*plugin.snapshot_calls.lock().unwrap(), vec![(USER_A1, 5)]);
}

#[tokio::test]
async fn user_limits_are_a_separate_call_keyed_by_user_and_version() {
    let (gateway, plugin, _) = single_plugin(2);
    gateway.current_snapshot(USER_A1).await.unwrap();
    assert!(
        plugin.limits_calls.lock().unwrap().is_empty(),
        "the snapshot never pulls limits"
    );

    let limits = gateway.user_limits(USER_A1, 2).await.unwrap();
    assert_eq!(limits.user_id, USER_A1);
    assert_eq!(limits.policy_version, 2);
    assert_eq!(limits.standard.limit_daily_credits_micro, 11);
    assert_eq!(*plugin.limits_calls.lock().unwrap(), vec![(USER_A1, 2)]);
}

#[tokio::test]
async fn check_user_license_is_never_called() {
    let (gateway, plugin, _) = single_plugin(1);
    gateway.current_snapshot(USER_A1).await.unwrap();
    gateway.snapshot_of_version(USER_A1, 1).await.unwrap();
    gateway.user_limits(USER_A1, 1).await.unwrap();
    assert_eq!(plugin.license_calls.load(Ordering::SeqCst), 0);
}
