//! Model policy gateway: policy snapshots, user limits and usage publication.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, PolicyPluginError,
    PolicySnapshot, PublishError, UsageEvent, UserLimits,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{GtsPluginSelector, choose_plugin_instance};
use toolkit::telemetry::ThrottledLog;
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Outcome of a failed usage publication, as the outbox handler needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishOutcome {
    /// Transient: redeliver later.
    Retry,
    /// Permanent: dead-letter with this reason.
    Reject(String),
}

/// Access to the model policy plugin.
///
/// Plugin failures (including an unknown policy version) are `DomainError::Internal`.
#[async_trait]
pub trait PolicyGateway: Send + Sync {
    /// Snapshot of the user's current policy version.
    async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError>;
    /// Snapshot of a given policy version (used to settle with the version a turn started with).
    async fn snapshot(&self, user_id: Uuid, version: i64) -> Result<PolicySnapshot, DomainError>;
    /// Credit limits of the user for a policy version.
    async fn user_limits(&self, user_id: Uuid, version: i64) -> Result<UserLimits, DomainError>;
    /// Delivers one usage event: `Retry` on a transient plugin error or when the plugin cannot be
    /// resolved, `Reject` on a permanent plugin error.
    async fn publish_usage(&self, ev: UsageEvent) -> Result<(), PublishOutcome>;
}

/// Error text when no policy plugin can be resolved.
const PLUGIN_UNAVAILABLE: &str = "model policy plugin unavailable";

/// Throttle interval of the "plugin unavailable" warning.
const UNAVAILABLE_LOG_THROTTLE: Duration = Duration::from_secs(10);

#[allow(clippy::needless_pass_by_value)] // used as `map_err(map_plugin_err)`
fn map_plugin_err(err: PolicyPluginError) -> DomainError {
    DomainError::Internal(format!("model policy plugin: {err}"))
}

fn map_publish_err(err: PublishError) -> PublishOutcome {
    match err {
        PublishError::Transient(msg) => {
            tracing::warn!(error = %msg, "usage publication failed transiently");
            PublishOutcome::Retry
        }
        PublishError::Permanent(msg) => PublishOutcome::Reject(msg),
    }
}

async fn current_snapshot_of(
    plugin: &dyn MiniChatModelPolicyPluginClientV1,
    user_id: Uuid,
) -> Result<PolicySnapshot, DomainError> {
    let version = plugin
        .get_current_policy_version(user_id)
        .await
        .map_err(map_plugin_err)?;
    plugin
        .get_policy_snapshot(user_id, version.policy_version)
        .await
        .map_err(map_plugin_err)
}

/// [`PolicyGateway`] over an in-process plugin client (tests).
pub struct DirectPolicyGateway(pub Arc<dyn MiniChatModelPolicyPluginClientV1>);

#[async_trait]
impl PolicyGateway for DirectPolicyGateway {
    async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        current_snapshot_of(self.0.as_ref(), user_id).await
    }

    async fn snapshot(&self, user_id: Uuid, version: i64) -> Result<PolicySnapshot, DomainError> {
        self.0
            .get_policy_snapshot(user_id, version)
            .await
            .map_err(map_plugin_err)
    }

    async fn user_limits(&self, user_id: Uuid, version: i64) -> Result<UserLimits, DomainError> {
        self.0
            .get_user_limits(user_id, version)
            .await
            .map_err(map_plugin_err)
    }

    async fn publish_usage(&self, ev: UsageEvent) -> Result<(), PublishOutcome> {
        self.0.publish_usage(ev).await.map_err(map_publish_err)
    }
}

/// [`PolicyGateway`] resolving the `MiniChatModelPolicyPluginClientV1` instance of `vendor`
/// lazily (on first use, never in `init`) through types-registry and the `ClientHub`.
///
/// A found instance id is cached; when its scoped client is missing from the hub the cache is
/// reset. Every resolution failure is `DomainError::Internal("model policy plugin unavailable")`
/// (logged, throttled) and `PublishOutcome::Retry` for `publish_usage`.
pub struct PluginPolicyGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
    unavailable_log: ThrottledLog,
}

impl PluginPolicyGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
            unavailable_log: ThrottledLog::new(UNAVAILABLE_LOG_THROTTLE),
        }
    }

    fn unavailable(&self, reason: &str) -> DomainError {
        if self.unavailable_log.should_log() {
            tracing::warn!(vendor = %self.vendor, reason, "{PLUGIN_UNAVAILABLE}");
        }
        DomainError::Internal(PLUGIN_UNAVAILABLE.to_owned())
    }

    async fn resolve_instance(&self) -> Result<String, String> {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| format!("types-registry client: {e}"))?;
        let type_id = MiniChatModelPolicyPluginSpecV1::gts_type_id();
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| format!("list plugin instances: {e}"))?;
        choose_plugin_instance::<MiniChatModelPolicyPluginSpecV1>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )
        .map_err(|e| e.to_string())
    }

    async fn plugin(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        let instance_id = self
            .selector
            .get_or_init(|| self.resolve_instance())
            .await
            .map_err(|reason| self.unavailable(&reason))?;
        if let Some(client) = self
            .hub
            .try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&ClientScope::gts_id(
                &instance_id,
            ))
        {
            return Ok(client);
        }
        self.selector.reset().await;
        Err(self.unavailable(&format!("client of instance {instance_id} not registered")))
    }
}

#[async_trait]
impl PolicyGateway for PluginPolicyGateway {
    async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        current_snapshot_of(self.plugin().await?.as_ref(), user_id).await
    }

    async fn snapshot(&self, user_id: Uuid, version: i64) -> Result<PolicySnapshot, DomainError> {
        self.plugin()
            .await?
            .get_policy_snapshot(user_id, version)
            .await
            .map_err(map_plugin_err)
    }

    async fn user_limits(&self, user_id: Uuid, version: i64) -> Result<UserLimits, DomainError> {
        self.plugin()
            .await?
            .get_user_limits(user_id, version)
            .await
            .map_err(map_plugin_err)
    }

    async fn publish_usage(&self, ev: UsageEvent) -> Result<(), PublishOutcome> {
        let plugin = self.plugin().await.map_err(|_| PublishOutcome::Retry)?;
        plugin.publish_usage(ev).await.map_err(map_publish_err)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mini_chat_sdk::{
        KillSwitches, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1,
        PublishError, TierLimits, UsageEvent,
    };
    use toolkit::client_hub::{ClientHub, ClientScope};
    use toolkit::gts::PluginV1;
    use types_registry_sdk::TypesRegistryClient;
    use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
    use uuid::Uuid;

    use super::*;
    use crate::test_support::catalog::{standard_model, test_catalog};
    use crate::test_support::plugins::{PolicyCall, RecordingPolicy};

    const VENDOR: &str = "constructorfabric";

    fn no_kill_switches() -> KillSwitches {
        KillSwitches {
            disable_premium_tier: false,
            force_standard_tier: false,
            disable_web_search: false,
            disable_file_search: false,
            disable_images: false,
            disable_code_interpreter: false,
        }
    }

    fn limits(daily: i64) -> TierLimits {
        TierLimits {
            limit_daily_credits_micro: daily,
            limit_monthly_credits_micro: daily * 10,
        }
    }

    fn policy() -> Arc<RecordingPolicy> {
        Arc::new(RecordingPolicy::new(
            test_catalog(),
            no_kill_switches(),
            limits(7),
            limits(3),
        ))
    }

    fn usage() -> UsageEvent {
        UsageEvent {
            tenant_id: Uuid::from_u128(1),
            user_id: Some(Uuid::from_u128(2)),
            chat_id: Uuid::from_u128(3),
            turn_id: Some(Uuid::from_u128(4)),
            request_id: Uuid::from_u128(5),
            effective_model: "gpt-standard".to_owned(),
            selected_model: "gpt-standard".to_owned(),
            terminal_state: "completed".to_owned(),
            billing_outcome: "completed".to_owned(),
            usage: None,
            actual_credits_micro: 1,
            settlement_method: "actual".to_owned(),
            policy_version_applied: 1,
            web_search_calls: 0,
            code_interpreter_calls: 0,
            file_search_calls: 0,
            timestamp: time::OffsetDateTime::UNIX_EPOCH,
            requester_type: "user".to_owned(),
            dedupe_key: "k".to_owned(),
            system_task_type: None,
        }
    }

    #[tokio::test]
    async fn direct_gateway_reads_current_version_and_maps_errors() {
        let plugin = policy();
        let gw = DirectPolicyGateway(plugin.clone());
        let user = Uuid::new_v4();

        let snapshot = gw.current_snapshot(user).await.expect("snapshot");
        assert_eq!(snapshot.policy_version, 1);
        assert_eq!(snapshot.model_catalog, test_catalog());
        assert_eq!(
            plugin.calls(),
            [
                PolicyCall::CurrentVersion { user_id: user },
                PolicyCall::Snapshot {
                    user_id: user,
                    version: 1
                },
            ]
        );
        let user_limits = gw.user_limits(user, 1).await.expect("limits");
        assert_eq!(
            (user_limits.standard, user_limits.premium),
            (limits(7), limits(3))
        );

        let err = gw.snapshot(user, 2).await.expect_err("unknown version");
        assert!(matches!(err, DomainError::Internal(_)), "{err:?}");
        let err = gw.user_limits(user, 2).await.expect_err("unknown version");
        assert!(matches!(err, DomainError::Internal(_)), "{err:?}");

        plugin.fail_next_publish(PublishError::Transient("busy".into()));
        plugin.fail_next_publish(PublishError::Permanent("invalid".into()));
        assert_eq!(gw.publish_usage(usage()).await, Err(PublishOutcome::Retry));
        assert_eq!(
            gw.publish_usage(usage()).await,
            Err(PublishOutcome::Reject("invalid".to_owned()))
        );
        assert_eq!(gw.publish_usage(usage()).await, Ok(()));
        assert_eq!(plugin.usage_events(), [usage(), usage(), usage()]);
    }

    #[tokio::test]
    async fn plugin_gateway_resolves_lazily_and_reports_missing_plugin() {
        let hub = Arc::new(ClientHub::new());
        let empty: Arc<dyn TypesRegistryClient> = Arc::new(MockTypesRegistryClient::new());
        hub.register::<dyn TypesRegistryClient>(empty);
        let gw = PluginPolicyGateway::new(Arc::clone(&hub), VENDOR.to_owned());
        let user = Uuid::new_v4();

        let err = gw.current_snapshot(user).await.expect_err("no plugin");
        assert!(
            matches!(&err, DomainError::Internal(m) if m == "model policy plugin unavailable"),
            "{err:?}"
        );
        assert_eq!(gw.publish_usage(usage()).await, Err(PublishOutcome::Retry));

        let (id, json) = PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
            "cf.core._.test_policy.v1",
            VENDOR,
            10,
        )
        .expect("registration");
        let id = id.to_string();
        let registry: Arc<dyn TypesRegistryClient> = Arc::new(
            MockTypesRegistryClient::new().with_instances([make_test_instance(&id, json)]),
        );
        hub.register::<dyn TypesRegistryClient>(registry);
        let err = gw
            .snapshot(user, 1)
            .await
            .expect_err("client not registered");
        assert!(
            matches!(&err, DomainError::Internal(m) if m == "model policy plugin unavailable"),
            "{err:?}"
        );

        let plugin = Arc::new(RecordingPolicy::new(
            vec![standard_model("only")],
            no_kill_switches(),
            limits(7),
            limits(3),
        ));
        let client: Arc<dyn MiniChatModelPolicyPluginClientV1> = plugin.clone();
        hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
            ClientScope::gts_id(&id),
            client,
        );
        let snapshot = gw.current_snapshot(user).await.expect("snapshot");
        assert_eq!(snapshot.model_catalog[0].id, "only");
        assert_eq!(gw.snapshot(user, 1).await.expect("v1").policy_version, 1);
        assert_eq!(
            gw.user_limits(user, 1).await.expect("limits").standard,
            limits(7)
        );
        assert_eq!(gw.publish_usage(usage()).await, Ok(()));
        assert_eq!(plugin.usage_events(), [usage()]);
    }
}
