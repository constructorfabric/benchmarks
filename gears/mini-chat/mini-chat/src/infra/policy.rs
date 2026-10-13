//! Model policy gateway: resolves the `mini-chat` model policy plugin via
//! types-registry (selected by vendor) and serves policy snapshots, user
//! limits and usage publication. Without a registered plugin the catalog is
//! empty and the gear still boots.

use std::sync::Arc;

use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1,
    PolicySnapshot, PublishError, TierLimits, UsageEvent, UserLimits,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Default `total` bucket limits used when no plugin is registered.
pub const DEFAULT_STANDARD_LIMITS: TierLimits = TierLimits {
    limit_daily_credits_micro: 100_000_000,
    limit_monthly_credits_micro: 1_000_000_000,
};
/// Default `tier:premium` bucket limits used when no plugin is registered.
pub const DEFAULT_PREMIUM_LIMITS: TierLimits = TierLimits {
    limit_daily_credits_micro: 50_000_000,
    limit_monthly_credits_micro: 500_000_000,
};

/// Outcome of a usage publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishOutcome {
    Ok,
    Retry(String),
    Reject(String),
}

enum Source {
    Hub {
        hub: Arc<ClientHub>,
        vendor: String,
        selector: GtsPluginSelector,
    },
    Direct(Arc<dyn MiniChatModelPolicyPluginClientV1>),
}

/// Model policy gateway.
pub struct PolicyGateway {
    source: Source,
}

#[derive(Debug)]
enum ResolveError {
    NotFound,
    Other(String),
}

impl PolicyGateway {
    /// Gateway resolving the plugin through types-registry and `ClientHub`.
    #[must_use]
    pub fn from_hub(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            source: Source::Hub {
                hub,
                vendor,
                selector: GtsPluginSelector::new(),
            },
        }
    }

    /// Gateway over a fixed plugin client (tests, embedding).
    #[must_use]
    pub fn direct(client: Arc<dyn MiniChatModelPolicyPluginClientV1>) -> Self {
        Self {
            source: Source::Direct(client),
        }
    }

    async fn resolve_instance(hub: &ClientHub, vendor: &str) -> Result<String, ResolveError> {
        let registry = hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| ResolveError::Other(e.to_string()))?;
        let type_id = MiniChatModelPolicyPluginSpecV1::gts_type_id().clone();
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| ResolveError::Other(e.to_string()))?;
        choose_plugin_instance::<MiniChatModelPolicyPluginSpecV1>(
            vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )
        .map_err(|e| match e {
            ChoosePluginError::PluginNotFound { .. } => ResolveError::NotFound,
            other @ ChoosePluginError::InvalidPluginInstance { .. } => ResolveError::Other(other.to_string()),
        })
    }

    /// Plugin client, `None` when no plugin is registered.
    async fn plugin(&self) -> Result<Option<Arc<dyn MiniChatModelPolicyPluginClientV1>>, DomainError> {
        match &self.source {
            Source::Direct(c) => Ok(Some(Arc::clone(c))),
            Source::Hub {
                hub,
                vendor,
                selector,
            } => {
                let id = selector
                    .get_or_init(|| Self::resolve_instance(hub, vendor))
                    .await;
                match id {
                    Ok(id) => {
                        let scope = ClientScope::gts_id(id.as_ref());
                        if let Some(c) = hub.try_get_scoped::<dyn MiniChatModelPolicyPluginClientV1>(&scope) {
                            Ok(Some(c))
                        } else {
                            selector.reset().await;
                            Err(DomainError::internal("model policy plugin client not registered"))
                        }
                    }
                    Err(ResolveError::NotFound) => Ok(None),
                    Err(ResolveError::Other(e)) => Err(DomainError::Internal(format!(
                        "model policy plugin resolution failed: {e}"
                    ))),
                }
            }
        }
    }

    fn empty_snapshot() -> PolicySnapshot {
        PolicySnapshot {
            policy_version: 0,
            model_catalog: Vec::new(),
            kill_switches: KillSwitches::default(),
        }
    }

    /// Current policy snapshot for a user.
    ///
    /// # Errors
    /// Plugin failure (500).
    pub async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        let Some(p) = self.plugin().await? else {
            return Ok(Self::empty_snapshot());
        };
        let v = p
            .get_current_policy_version(user_id)
            .await
            .map_err(|e| DomainError::Internal(format!("policy version: {e}")))?;
        p.get_policy_snapshot(user_id, v.policy_version)
            .await
            .map_err(|e| DomainError::Internal(format!("policy snapshot: {e}")))
    }

    /// Snapshot of a specific version (settlement).
    ///
    /// # Errors
    /// Plugin failure.
    pub async fn snapshot_version(&self, user_id: Uuid, version: u64) -> Result<PolicySnapshot, DomainError> {
        let Some(p) = self.plugin().await? else {
            return Ok(Self::empty_snapshot());
        };
        p.get_policy_snapshot(user_id, version)
            .await
            .map_err(|e| DomainError::Internal(format!("policy snapshot: {e}")))
    }

    /// Per-user limits for a policy version.
    ///
    /// # Errors
    /// Plugin failure.
    pub async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError> {
        let Some(p) = self.plugin().await? else {
            return Ok(UserLimits {
                user_id,
                policy_version: version,
                standard: DEFAULT_STANDARD_LIMITS,
                premium: DEFAULT_PREMIUM_LIMITS,
            });
        };
        p.get_user_limits(user_id, version)
            .await
            .map_err(|e| DomainError::Internal(format!("user limits: {e}")))
    }

    /// Publishes a usage event.
    pub async fn publish_usage(&self, ev: UsageEvent) -> PublishOutcome {
        match self.plugin().await {
            Ok(Some(p)) => match p.publish_usage(ev).await {
                Ok(()) => PublishOutcome::Ok,
                Err(PublishError::Transient(e)) => PublishOutcome::Retry(e),
                Err(PublishError::Permanent(e)) => PublishOutcome::Reject(e),
            },
            Ok(None) => PublishOutcome::Ok,
            Err(e) => PublishOutcome::Retry(e.to_string()),
        }
    }
}
