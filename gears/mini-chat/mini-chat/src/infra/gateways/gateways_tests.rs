#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    KillSwitches, MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginError,
    MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    MiniChatModelPolicyPluginSpecV1, PolicySnapshot, PolicyVersionInfo, PublishError, TierLimits,
    TurnMutationAuditEvent, UsageEvent, UserLimits,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::gts::PluginV1;
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use uuid::Uuid;

use super::audit::{AuditEmitResult, AuditGateway, PluginAuditGateway};
use super::model_policy::{
    InProcessModelPolicyGateway, ModelPolicyGateway, PluginModelPolicyGateway, PublishOutcome,
};
use crate::domain::error::DomainError;

// ---- helpers -------------------------------------------------------------

fn policy_instance(
    segment: &str,
    vendor: &str,
    priority: i16,
) -> (String, types_registry_sdk::GtsInstance) {
    let (id, json) = PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
        segment, vendor, priority,
    )
    .unwrap();
    let id = id.to_string();
    let inst = make_test_instance(&id, json);
    (id, inst)
}

fn audit_instance(
    segment: &str,
    vendor: &str,
    priority: i16,
) -> (String, types_registry_sdk::GtsInstance) {
    let (id, json) =
        PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(segment, vendor, priority)
            .unwrap();
    let id = id.to_string();
    let inst = make_test_instance(&id, json);
    (id, inst)
}

fn hub_with(registry: &Arc<MockTypesRegistryClient>) -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::new());
    let dynamic: Arc<dyn TypesRegistryClient> = registry.clone();
    hub.register::<dyn TypesRegistryClient>(dynamic);
    hub
}

struct FakePolicy {
    version: u64,
    publish: Result<(), PublishError>,
    fail: Option<MiniChatModelPolicyPluginError>,
    published: AtomicUsize,
}

impl FakePolicy {
    fn new(version: u64) -> Arc<Self> {
        Arc::new(Self {
            version,
            publish: Ok(()),
            fail: None,
            published: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for FakePolicy {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError> {
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        Ok(PolicyVersionInfo {
            policy_version: self.version,
            generated_at: time::OffsetDateTime::now_utc(),
        })
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        Ok(PolicySnapshot {
            policy_version,
            model_catalog: vec![],
            kill_switches: KillSwitches {
                disable_premium_tier: false,
                force_standard_tier: false,
                disable_web_search: false,
                disable_file_search: false,
                disable_images: false,
                disable_code_interpreter: false,
            },
        })
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        let t = TierLimits {
            limit_daily_credits_micro: i64::try_from(self.version).unwrap(),
            limit_monthly_credits_micro: 0,
        };
        Ok(UserLimits {
            user_id,
            policy_version,
            standard: t,
            premium: t,
        })
    }

    async fn publish_usage(&self, _payload: UsageEvent) -> Result<(), PublishError> {
        self.published.fetch_add(1, Ordering::SeqCst);
        self.publish.clone()
    }
}

fn usage_event() -> UsageEvent {
    serde_json::from_value(serde_json::json!({
        "tenant_id": Uuid::nil(),
        "chat_id": Uuid::nil(),
        "request_id": Uuid::nil(),
        "effective_model": "m",
        "selected_model": "m",
        "terminal_state": "completed",
        "billing_outcome": "completed",
        "usage": null,
        "actual_credits_micro": 0,
        "settlement_method": "actual",
        "policy_version_applied": 1,
        "web_search_calls": 0,
        "code_interpreter_calls": 0,
        "file_search_calls": 0,
        "timestamp": "2026-10-04T00:00:00Z",
        "requester_type": "user",
        "dedupe_key": "k"
    }))
    .unwrap()
}

struct FakeAudit {
    result: Result<(), MiniChatAuditPluginError>,
    delay: Option<Duration>,
    calls: AtomicUsize,
}

impl FakeAudit {
    fn new(result: Result<(), MiniChatAuditPluginError>) -> Arc<Self> {
        Arc::new(Self {
            result,
            delay: None,
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for FakeAudit {
    async fn emit(&self, _event: MiniChatAuditEvent) -> Result<(), MiniChatAuditPluginError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(d) = self.delay {
            tokio::time::sleep(d).await;
        }
        self.result.clone()
    }
}

fn audit_event() -> MiniChatAuditEvent {
    MiniChatAuditEvent::Mutation(TurnMutationAuditEvent::Delete {
        tenant_id: Uuid::nil(),
        actor_user_id: Uuid::nil(),
        chat_id: Uuid::nil(),
        request_id: Uuid::nil(),
        timestamp: "2026-10-04T00:00:00Z".to_owned(),
    })
}

const VENDOR: &str = "acme";

// ---- model policy gateway ------------------------------------------------

#[tokio::test]
async fn policy_gateway_picks_lowest_priority_instance_of_vendor() {
    let (id_a50, inst_a50) = policy_instance("acme.test._.policy_a50.v1", VENDOR, 50);
    let (id_a20, inst_a20) = policy_instance("acme.test._.policy_a20.v1", VENDOR, 20);
    let (id_other, inst_other) = policy_instance("other.test._.policy_o5.v1", "other", 5);
    let registry = Arc::new(
        MockTypesRegistryClient::new().with_instances([inst_a50, inst_a20, inst_other]),
    );
    let hub = hub_with(&registry);
    hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
        ClientScope::gts_id(&id_a50),
        FakePolicy::new(50),
    );
    hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
        ClientScope::gts_id(&id_a20),
        FakePolicy::new(20),
    );
    hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
        ClientScope::gts_id(&id_other),
        FakePolicy::new(5),
    );

    let gw = PluginModelPolicyGateway::new(hub, VENDOR.to_owned());
    let user = Uuid::new_v4();

    // current_snapshot asks the plugin for the version and then the snapshot.
    let snap = gw.current_snapshot(user).await.unwrap();
    assert_eq!(snap.policy_version, 20);
    let snap = gw.snapshot(user, 7).await.unwrap();
    assert_eq!(snap.policy_version, 7);
    let limits = gw.user_limits(user, 7).await.unwrap();
    assert_eq!(limits.standard.limit_daily_credits_micro, 20);
    assert_eq!(limits.user_id, user);
    gw.publish_usage(usage_event()).await.unwrap();

    // The resolved instance id is cached.
    assert_eq!(registry.list_instance_calls(), 1);
}

#[tokio::test]
async fn policy_gateway_without_plugin_is_internal_and_publish_retries() {
    let registry = Arc::new(MockTypesRegistryClient::new());
    let hub = hub_with(&registry);
    let gw = PluginModelPolicyGateway::new(hub, VENDOR.to_owned());

    assert!(matches!(
        gw.current_snapshot(Uuid::nil()).await,
        Err(DomainError::Internal(_))
    ));
    assert!(matches!(
        gw.snapshot(Uuid::nil(), 1).await,
        Err(DomainError::Internal(_))
    ));
    assert!(matches!(
        gw.user_limits(Uuid::nil(), 1).await,
        Err(DomainError::Internal(_))
    ));
    assert!(matches!(
        gw.publish_usage(usage_event()).await,
        Err(PublishOutcome::Retry(_))
    ));
}

#[tokio::test]
async fn policy_gateway_without_registry_is_internal() {
    let hub = Arc::new(ClientHub::new());
    let gw = PluginModelPolicyGateway::new(hub, VENDOR.to_owned());
    assert!(matches!(
        gw.current_snapshot(Uuid::nil()).await,
        Err(DomainError::Internal(_))
    ));
}

#[tokio::test]
async fn policy_gateway_instance_without_client_is_retryable_and_reselected() {
    let (_id, inst) = policy_instance("acme.test._.policy.v1", VENDOR, 100);
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([inst]));
    let hub = hub_with(&registry);
    let gw = PluginModelPolicyGateway::new(hub, VENDOR.to_owned());

    assert!(matches!(
        gw.current_snapshot(Uuid::nil()).await,
        Err(DomainError::Internal(_))
    ));
    assert!(matches!(
        gw.publish_usage(usage_event()).await,
        Err(PublishOutcome::Retry(_))
    ));
    // Each failed attempt re-resolves the instance.
    assert_eq!(registry.list_instance_calls(), 2);
}

#[tokio::test]
async fn policy_gateway_maps_plugin_errors() {
    let (id, inst) = policy_instance("acme.test._.policy.v1", VENDOR, 100);
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([inst]));
    let hub = hub_with(&registry);
    let failing = Arc::new(FakePolicy {
        version: 1,
        publish: Err(PublishError::Permanent("bad".to_owned())),
        fail: Some(MiniChatModelPolicyPluginError::NotFound("nope".to_owned())),
        published: AtomicUsize::new(0),
    });
    hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(ClientScope::gts_id(&id), failing);
    let gw = PluginModelPolicyGateway::new(hub, VENDOR.to_owned());

    assert!(matches!(
        gw.current_snapshot(Uuid::nil()).await,
        Err(DomainError::Internal(_))
    ));
    assert!(matches!(
        gw.snapshot(Uuid::nil(), 1).await,
        Err(DomainError::Internal(_))
    ));
    assert!(matches!(
        gw.user_limits(Uuid::nil(), 1).await,
        Err(DomainError::Internal(_))
    ));
    assert_eq!(
        gw.publish_usage(usage_event()).await,
        Err(PublishOutcome::Reject("permanent publish error: bad".to_owned()))
    );
}

#[tokio::test]
async fn in_process_policy_gateway_delegates_and_maps_publish_errors() {
    let user = Uuid::new_v4();
    let ok = InProcessModelPolicyGateway::new(FakePolicy::new(3));
    assert_eq!(ok.current_snapshot(user).await.unwrap().policy_version, 3);
    assert_eq!(ok.snapshot(user, 9).await.unwrap().policy_version, 9);
    assert_eq!(ok.user_limits(user, 9).await.unwrap().policy_version, 9);
    ok.publish_usage(usage_event()).await.unwrap();

    let transient = InProcessModelPolicyGateway::new(Arc::new(FakePolicy {
        version: 1,
        publish: Err(PublishError::Transient("later".to_owned())),
        fail: None,
        published: AtomicUsize::new(0),
    }));
    assert!(matches!(
        transient.publish_usage(usage_event()).await,
        Err(PublishOutcome::Retry(_))
    ));
}

// ---- audit gateway -------------------------------------------------------

#[tokio::test]
async fn audit_gateway_drops_when_no_plugin_and_finds_late_registered_plugin() {
    let empty = Arc::new(MockTypesRegistryClient::new());
    let hub = hub_with(&empty);
    let gw = PluginAuditGateway::new(hub.clone(), VENDOR.to_owned());

    // No plugin: dropped, and the miss is not cached.
    assert_eq!(gw.emit(audit_event()).await, AuditEmitResult::Dropped);
    assert_eq!(gw.emit(audit_event()).await, AuditEmitResult::Dropped);
    assert_eq!(empty.list_instance_calls(), 2);

    // The plugin registers later.
    let (id, inst) = audit_instance("acme.test._.audit.v1", VENDOR, 100);
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([inst]));
    let dynamic: Arc<dyn TypesRegistryClient> = registry.clone();
    hub.register::<dyn TypesRegistryClient>(dynamic);
    let plugin = FakeAudit::new(Ok(()));
    hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(
        ClientScope::gts_id(&id),
        plugin.clone(),
    );

    assert_eq!(gw.emit(audit_event()).await, AuditEmitResult::Delivered);
    assert_eq!(gw.emit(audit_event()).await, AuditEmitResult::Delivered);
    assert_eq!(plugin.calls.load(Ordering::SeqCst), 2);
    // A found instance id is cached.
    assert_eq!(registry.list_instance_calls(), 1);
}

#[tokio::test]
async fn audit_gateway_ignores_plugins_of_other_vendors() {
    let (id, inst) = audit_instance("other.test._.audit.v1", "other", 1);
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([inst]));
    let hub = hub_with(&registry);
    hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(
        ClientScope::gts_id(&id),
        FakeAudit::new(Ok(())),
    );
    let gw = PluginAuditGateway::new(hub, VENDOR.to_owned());
    assert_eq!(gw.emit(audit_event()).await, AuditEmitResult::Dropped);
}

#[tokio::test]
async fn audit_gateway_retries_when_client_missing() {
    let (id, inst) = audit_instance("acme.test._.audit.v1", VENDOR, 100);
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([inst]));
    let hub = hub_with(&registry);
    let gw = PluginAuditGateway::new(hub.clone(), VENDOR.to_owned());

    assert!(matches!(gw.emit(audit_event()).await, AuditEmitResult::Retry(_)));
    // The selector was reset: the next delivery resolves the instance again.
    assert!(matches!(gw.emit(audit_event()).await, AuditEmitResult::Retry(_)));
    assert_eq!(registry.list_instance_calls(), 2);

    // Once the client shows up the event is delivered.
    hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(
        ClientScope::gts_id(&id),
        FakeAudit::new(Ok(())),
    );
    assert_eq!(gw.emit(audit_event()).await, AuditEmitResult::Delivered);
}

#[tokio::test]
async fn audit_gateway_retries_when_registry_fails() {
    let hub = Arc::new(ClientHub::new());
    let gw = PluginAuditGateway::new(hub, VENDOR.to_owned());
    assert!(matches!(gw.emit(audit_event()).await, AuditEmitResult::Retry(_)));
}

#[tokio::test]
async fn audit_gateway_maps_plugin_errors() {
    let cases = [
        (
            MiniChatAuditPluginError::Transient("t".to_owned()),
            AuditEmitResult::Retry("transient audit error: t".to_owned()),
        ),
        (
            MiniChatAuditPluginError::Permanent("p".to_owned()),
            AuditEmitResult::Reject("permanent audit error: p".to_owned()),
        ),
        (
            MiniChatAuditPluginError::PluginTimeout,
            AuditEmitResult::Retry("audit plugin timeout".to_owned()),
        ),
    ];
    for (err, expected) in cases {
        let (id, inst) = audit_instance("acme.test._.audit.v1", VENDOR, 100);
        let registry = Arc::new(MockTypesRegistryClient::new().with_instances([inst]));
        let hub = hub_with(&registry);
        hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(
            ClientScope::gts_id(&id),
            FakeAudit::new(Err(err)),
        );
        let gw = PluginAuditGateway::new(hub, VENDOR.to_owned());
        assert_eq!(gw.emit(audit_event()).await, expected);
    }
}

#[tokio::test]
async fn audit_gateway_times_out_to_retry() {
    let (id, inst) = audit_instance("acme.test._.audit.v1", VENDOR, 100);
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([inst]));
    let hub = hub_with(&registry);
    let slow = Arc::new(FakeAudit {
        result: Ok(()),
        delay: Some(Duration::from_secs(5)),
        calls: AtomicUsize::new(0),
    });
    hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(&id), slow);
    let gw = PluginAuditGateway::with_timeout(hub, VENDOR.to_owned(), Duration::from_millis(50));
    assert!(matches!(gw.emit(audit_event()).await, AuditEmitResult::Retry(_)));
}
