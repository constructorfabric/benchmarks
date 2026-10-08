//! Static model policy plugin implementation.

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, PolicyError, PolicySnapshot, PolicyVersionInfo,
    PublishError, UsageEvent, UserLimits,
};
use uuid::Uuid;

use super::config::StaticModelPolicyConfig;

/// Fixed policy version served by the static plugin.
pub const STATIC_POLICY_VERSION: u64 = 1;

/// In-process static policy source.
pub struct StaticModelPolicyService {
    snapshot: PolicySnapshot,
    cfg: StaticModelPolicyConfig,
}

impl StaticModelPolicyService {
    #[must_use]
    pub fn new(cfg: StaticModelPolicyConfig) -> Self {
        let snapshot = PolicySnapshot {
            policy_version: STATIC_POLICY_VERSION,
            model_catalog: cfg.model_catalog.clone(),
            kill_switches: cfg.kill_switches.into(),
        };
        Self { snapshot, cfg }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StaticModelPolicyService {
    async fn get_current_policy_version(&self, _user_id: Uuid) -> Result<PolicyVersionInfo, PolicyError> {
        Ok(PolicyVersionInfo { policy_version: STATIC_POLICY_VERSION })
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, PolicyError> {
        if policy_version != STATIC_POLICY_VERSION {
            return Err(PolicyError::NotFound(format!("policy version {policy_version}")));
        }
        Ok(self.snapshot.clone())
    }

    async fn get_user_limits(&self, user_id: Uuid, policy_version: u64) -> Result<UserLimits, PolicyError> {
        Ok(UserLimits {
            user_id,
            policy_version,
            standard: self.cfg.default_standard_limits,
            premium: self.cfg.default_premium_limits,
        })
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError> {
        tracing::info!(
            target: "mini_chat::usage",
            dedupe_key = %event.dedupe_key,
            billing_outcome = %event.billing_outcome,
            settlement_method = %event.settlement_method,
            actual_credits_micro = event.actual_credits_micro,
            "usage event published"
        );
        Ok(())
    }
}
