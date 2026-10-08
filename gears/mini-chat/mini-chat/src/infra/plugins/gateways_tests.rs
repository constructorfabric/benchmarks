#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditPluginError, KillSwitches, MiniChatAuditEvent, MiniChatAuditPluginClientV1,
    MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    MiniChatModelPolicyPluginSpecV1, PolicyDecisions, PolicySnapshot, PolicyVersionInfo,
    PublishError, QuotaPolicyDecision, TierLimits, ToolCalls, TurnAuditEvent, UsageEvent,
    UserLimits,
};
use time::OffsetDateTime;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::gts::PluginV1;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use types_registry_sdk::{GtsInstance, TypesRegistryClient};
use uuid::Uuid;

use super::audit_gateway::AuditGateway;
use super::policy_gateway::PolicyGateway;
use crate::domain::error::DomainError;
use crate::domain::ports::{AuditDelivery, AuditSink, PolicyProvider};

const VENDOR: &str = "constructorfabric";

fn policy_instance(segment: &str, vendor: &str, priority: i16) -> (String, GtsInstance) {
    let (id, json) =
        PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(segment, vendor, priority)
            .unwrap();
    let id = id.to_string();
    (id.clone(), make_test_instance(&id, json))
}

fn audit_instance(segment: &str, vendor: &str, priority: i16) -> (String, GtsInstance) {
    let (id, json) =
        PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(segment, vendor, priority)
            .unwrap();
    let id = id.to_string();
    (id.clone(), make_test_instance(&id, json))
}

fn set_registry(hub: &ClientHub, instances: Vec<GtsInstance>) {
    let registry: Arc<dyn TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new().with_instances(instances));
    hub.register::<dyn TypesRegistryClient>(registry);
}

// ── fakes ───────────────────────────────────────────────────────────────────

struct FakePolicy {
    version: u64,
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for FakePolicy {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError> {
        Ok(PolicyVersionInfo {
            policy_version: self.version,
            generated_at: OffsetDateTime::now_utc(),
        })
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
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
        let tl = TierLimits {
            limit_daily_credits_micro: 1,
            limit_monthly_credits_micro: 2,
        };
        Ok(UserLimits {
            user_id,
            policy_version,
            standard: tl,
            premium: tl,
        })
    }

    async fn publish_usage(&self, _payload: UsageEvent) -> Result<(), PublishError> {
        Ok(())
    }
}

struct FailingPolicy;

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for FailingPolicy {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError> {
        Err(MiniChatModelPolicyPluginError::Unavailable(
            "down".to_owned(),
        ))
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        Err(MiniChatModelPolicyPluginError::NotFound("v".to_owned()))
    }

    async fn get_user_limits(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        Err(MiniChatModelPolicyPluginError::Internal("x".to_owned()))
    }

    async fn publish_usage(&self, _payload: UsageEvent) -> Result<(), PublishError> {
        Err(PublishError::Permanent("nope".to_owned()))
    }
}

/// Snapshot lookups fail transiently (`Unavailable`); nothing else is called.
struct UnavailableSnapshotPolicy;

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for UnavailableSnapshotPolicy {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError> {
        unreachable!()
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        Err(MiniChatModelPolicyPluginError::Unavailable(
            "down".to_owned(),
        ))
    }

    async fn get_user_limits(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        unreachable!()
    }

    async fn publish_usage(&self, _payload: UsageEvent) -> Result<(), PublishError> {
        unreachable!()
    }
}

#[tokio::test]
async fn policy_gateway_transient_snapshot_error_is_internal() {
    let hub = Arc::new(ClientHub::new());
    let (id, inst) = policy_instance("cf.test._.p.v1", VENDOR, 100);
    set_registry(&hub, vec![inst]);
    hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
        ClientScope::gts_id(&id),
        Arc::new(UnavailableSnapshotPolicy),
    );
    let gw = PolicyGateway::new(hub, VENDOR.to_owned());
    assert!(matches!(
        gw.snapshot(Uuid::new_v4(), 1).await,
        Err(DomainError::Internal(_))
    ));
}

enum AuditBehavior {
    Ok,
    Fail(fn() -> AuditPluginError),
    Hang,
}

struct FakeAudit {
    behavior: AuditBehavior,
    calls: AtomicUsize,
}

impl FakeAudit {
    fn new(behavior: AuditBehavior) -> Arc<Self> {
        Arc::new(Self {
            behavior,
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for FakeAudit {
    async fn emit(&self, _event: MiniChatAuditEvent) -> Result<(), AuditPluginError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.behavior {
            AuditBehavior::Ok => Ok(()),
            AuditBehavior::Fail(f) => Err(f()),
            AuditBehavior::Hang => {
                std::future::pending::<()>().await;
                Ok(())
            }
        }
    }
}

fn audit_event() -> MiniChatAuditEvent {
    MiniChatAuditEvent::Turn(TurnAuditEvent {
        event_type: "turn_completed".to_owned(),
        tenant_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        turn_id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
        requester_type: "user".to_owned(),
        actor_user_id: None,
        selected_model: "m".to_owned(),
        effective_model: "m".to_owned(),
        terminal_state: "completed".to_owned(),
        error_code: None,
        usage: None,
        latency_ms: None,
        tool_calls: ToolCalls::default(),
        policy_decisions: PolicyDecisions {
            quota: QuotaPolicyDecision {
                decision: "allow".to_owned(),
                downgrade_from: None,
                downgrade_reason: None,
            },
            license: None,
            quota_scope: None,
        },
        prompt: None,
        response: None,
        attachments: vec![],
        trace_id: None,
        timestamp: OffsetDateTime::now_utc(),
    })
}

fn register_audit_client(hub: &ClientHub, id: &str, client: Arc<FakeAudit>) {
    hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(id), client);
}

// ── policy gateway ──────────────────────────────────────────────────────────

#[tokio::test]
async fn policy_gateway_resolves_lowest_priority_vendor_match() {
    let hub = Arc::new(ClientHub::new());
    let (slow_id, slow) = policy_instance("cf.test._.slow.v1", VENDOR, 100);
    let (fast_id, fast) = policy_instance("cf.test._.fast.v1", VENDOR, 10);
    let (other_id, other) = policy_instance("cf.test._.other.v1", "someone-else", 1);
    set_registry(&hub, vec![slow, fast, other]);
    for (id, version) in [(slow_id, 100), (fast_id, 10), (other_id, 1)] {
        hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
            ClientScope::gts_id(&id),
            Arc::new(FakePolicy { version }),
        );
    }

    let gw = PolicyGateway::new(hub, VENDOR.to_owned());
    let user = Uuid::new_v4();
    let snap = gw.current(user).await.unwrap();
    assert_eq!(snap.policy_version, 10);
    let snap = gw.snapshot(user, 7).await.unwrap();
    assert_eq!(snap.policy_version, 7);
    let limits = gw.user_limits(user, 7).await.unwrap();
    assert_eq!((limits.user_id, limits.policy_version), (user, 7));
}

#[tokio::test]
async fn policy_gateway_without_plugin_is_internal_error() {
    let hub = Arc::new(ClientHub::new());
    set_registry(&hub, vec![]);
    let gw = PolicyGateway::new(hub, VENDOR.to_owned());
    let err = gw.current(Uuid::new_v4()).await.unwrap_err();
    assert!(matches!(err, DomainError::Internal(_)), "{err:?}");
}

#[tokio::test]
async fn policy_gateway_without_registry_is_internal_error() {
    let gw = PolicyGateway::new(Arc::new(ClientHub::new()), VENDOR.to_owned());
    let err = gw.current(Uuid::new_v4()).await.unwrap_err();
    assert!(matches!(err, DomainError::Internal(_)), "{err:?}");
}

#[tokio::test]
async fn policy_gateway_plugin_errors_are_internal() {
    let hub = Arc::new(ClientHub::new());
    let (id, inst) = policy_instance("cf.test._.p.v1", VENDOR, 100);
    set_registry(&hub, vec![inst]);
    hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
        ClientScope::gts_id(&id),
        Arc::new(FailingPolicy),
    );
    let gw = PolicyGateway::new(hub, VENDOR.to_owned());
    let user = Uuid::new_v4();
    assert!(matches!(
        gw.current(user).await,
        Err(DomainError::Internal(_))
    ));
    // A dropped policy version (NotFound) is distinguishable; other plugin
    // failures stay `Internal`.
    assert!(matches!(
        gw.snapshot(user, 1).await,
        Err(DomainError::PolicySnapshotGone(_))
    ));
    assert!(matches!(
        gw.user_limits(user, 1).await,
        Err(DomainError::Internal(_))
    ));
}

#[tokio::test]
async fn policy_gateway_publish_usage_passes_plugin_error_through() {
    let hub = Arc::new(ClientHub::new());
    let (id, inst) = policy_instance("cf.test._.p.v1", VENDOR, 100);
    set_registry(&hub, vec![inst]);
    hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
        ClientScope::gts_id(&id),
        Arc::new(FailingPolicy),
    );
    let gw = PolicyGateway::new(hub, VENDOR.to_owned());
    let err = gw.publish_usage(usage_event()).await.unwrap_err();
    assert!(matches!(err, PublishError::Permanent(_)), "{err:?}");
}

#[tokio::test]
async fn policy_gateway_publish_usage_without_plugin_is_transient() {
    let hub = Arc::new(ClientHub::new());
    set_registry(&hub, vec![]);
    let gw = PolicyGateway::new(hub, VENDOR.to_owned());
    let err = gw.publish_usage(usage_event()).await.unwrap_err();
    assert!(matches!(err, PublishError::Transient(_)), "{err:?}");
}

fn usage_event() -> UsageEvent {
    UsageEvent {
        tenant_id: Uuid::new_v4(),
        user_id: None,
        chat_id: Uuid::new_v4(),
        turn_id: None,
        request_id: Uuid::new_v4(),
        effective_model: "m".to_owned(),
        selected_model: "m".to_owned(),
        terminal_state: "completed".to_owned(),
        billing_outcome: "settled".to_owned(),
        usage: None,
        actual_credits_micro: 0,
        settlement_method: "actual".to_owned(),
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: OffsetDateTime::now_utc(),
        requester_type: "user".to_owned(),
        dedupe_key: "k".to_owned(),
        system_task_type: None,
    }
}

// ── audit gateway ───────────────────────────────────────────────────────────

#[tokio::test]
async fn audit_gateway_no_plugin_is_not_cached() {
    let hub = Arc::new(ClientHub::new());
    set_registry(&hub, vec![]);
    let gw = AuditGateway::new(Arc::clone(&hub), VENDOR.to_owned());

    assert_eq!(gw.deliver(audit_event()).await, AuditDelivery::NoPlugin);

    let (id, inst) = audit_instance("cf.test._.audit.v1", VENDOR, 100);
    set_registry(&hub, vec![inst]);
    let client = FakeAudit::new(AuditBehavior::Ok);
    register_audit_client(&hub, &id, Arc::clone(&client));

    assert_eq!(gw.deliver(audit_event()).await, AuditDelivery::Delivered);
    assert_eq!(client.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn audit_gateway_found_instance_is_cached() {
    let hub = Arc::new(ClientHub::new());
    let (id, inst) = audit_instance("cf.test._.audit.v1", VENDOR, 100);
    set_registry(&hub, vec![inst]);
    let client = FakeAudit::new(AuditBehavior::Ok);
    register_audit_client(&hub, &id, Arc::clone(&client));
    let gw = AuditGateway::new(Arc::clone(&hub), VENDOR.to_owned());

    assert_eq!(gw.deliver(audit_event()).await, AuditDelivery::Delivered);
    // The registry no longer lists the instance; the cached id keeps working.
    set_registry(&hub, vec![]);
    assert_eq!(gw.deliver(audit_event()).await, AuditDelivery::Delivered);
    assert_eq!(client.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn audit_gateway_missing_client_returns_retry() {
    let hub = Arc::new(ClientHub::new());
    let (id, inst) = audit_instance("cf.test._.audit.v1", VENDOR, 100);
    set_registry(&hub, vec![inst]);
    let gw = AuditGateway::new(Arc::clone(&hub), VENDOR.to_owned());

    assert!(matches!(
        gw.deliver(audit_event()).await,
        AuditDelivery::Retry(_)
    ));

    // The cached id was reset: the next delivery resolves again, and a client
    // that appeared in the meantime is used.
    let client = FakeAudit::new(AuditBehavior::Ok);
    register_audit_client(&hub, &id, Arc::clone(&client));
    assert_eq!(gw.deliver(audit_event()).await, AuditDelivery::Delivered);
}

#[tokio::test]
async fn audit_gateway_resolution_error_returns_retry() {
    // No TypesRegistryClient in the hub at all.
    let gw = AuditGateway::new(Arc::new(ClientHub::new()), VENDOR.to_owned());
    assert!(matches!(
        gw.deliver(audit_event()).await,
        AuditDelivery::Retry(_)
    ));
}

#[tokio::test]
async fn audit_gateway_permanent_error_rejects() {
    let hub = Arc::new(ClientHub::new());
    let (id, inst) = audit_instance("cf.test._.audit.v1", VENDOR, 100);
    set_registry(&hub, vec![inst]);
    register_audit_client(
        &hub,
        &id,
        FakeAudit::new(AuditBehavior::Fail(|| {
            AuditPluginError::Permanent("bad payload".to_owned())
        })),
    );
    let gw = AuditGateway::new(hub, VENDOR.to_owned());
    assert!(
        matches!(gw.deliver(audit_event()).await, AuditDelivery::Reject(m) if m.contains("bad payload"))
    );
}

#[tokio::test]
async fn audit_gateway_transient_and_timeout_errors_retry() {
    for make in [
        (|| AuditPluginError::Transient("later".to_owned())) as fn() -> AuditPluginError,
        || AuditPluginError::PluginTimeout,
    ] {
        let hub = Arc::new(ClientHub::new());
        let (id, inst) = audit_instance("cf.test._.audit.v1", VENDOR, 100);
        set_registry(&hub, vec![inst]);
        register_audit_client(&hub, &id, FakeAudit::new(AuditBehavior::Fail(make)));
        let gw = AuditGateway::new(hub, VENDOR.to_owned());
        assert!(matches!(
            gw.deliver(audit_event()).await,
            AuditDelivery::Retry(_)
        ));
    }
}

#[tokio::test(start_paused = true)]
async fn audit_gateway_plugin_call_times_out_after_30s_and_retries() {
    let hub = Arc::new(ClientHub::new());
    let (id, inst) = audit_instance("cf.test._.audit.v1", VENDOR, 100);
    set_registry(&hub, vec![inst]);
    register_audit_client(&hub, &id, FakeAudit::new(AuditBehavior::Hang));
    let gw = AuditGateway::new(hub, VENDOR.to_owned());

    let started = tokio::time::Instant::now();
    assert!(matches!(
        gw.deliver(audit_event()).await,
        AuditDelivery::Retry(_)
    ));
    assert_eq!(started.elapsed(), std::time::Duration::from_secs(30));
}
