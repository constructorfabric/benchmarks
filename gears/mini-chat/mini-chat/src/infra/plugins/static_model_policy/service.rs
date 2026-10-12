//! The static model-policy plugin implementation.

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, ModelPolicyPluginError, PolicySnapshot, PolicyVersionInfo,
    PublishError, UsageEvent, UserLimits,
};
use tracing::info;
use uuid::Uuid;

use super::config::StaticModelPolicyConfig;

/// The only policy version the static plugin serves.
pub const STATIC_POLICY_VERSION: i64 = 1;

/// Serves the fixed policy built from the plugin configuration.
pub struct StaticModelPolicyService {
    snapshot: PolicySnapshot,
    config: StaticModelPolicyConfig,
}

impl StaticModelPolicyService {
    /// Builds the service from a validated configuration.
    #[must_use]
    pub fn from_config(config: StaticModelPolicyConfig) -> Self {
        let snapshot = PolicySnapshot {
            policy_version: STATIC_POLICY_VERSION,
            model_catalog: config.model_catalog.clone(),
            kill_switches: config.kill_switches.into(),
        };
        Self { snapshot, config }
    }

    fn check_version(policy_version: i64) -> Result<(), ModelPolicyPluginError> {
        if policy_version == STATIC_POLICY_VERSION {
            Ok(())
        } else {
            Err(ModelPolicyPluginError::PolicyVersionNotFound { policy_version })
        }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StaticModelPolicyService {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, ModelPolicyPluginError> {
        Ok(PolicyVersionInfo {
            policy_version: STATIC_POLICY_VERSION,
        })
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: i64,
    ) -> Result<PolicySnapshot, ModelPolicyPluginError> {
        Self::check_version(policy_version)?;
        Ok(self.snapshot.clone())
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: i64,
    ) -> Result<UserLimits, ModelPolicyPluginError> {
        Self::check_version(policy_version)?;
        Ok(UserLimits {
            user_id,
            policy_version: STATIC_POLICY_VERSION,
            standard: self.config.default_standard_limits,
            premium: self.config.default_premium_limits,
        })
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError> {
        info!(
            dedupe_key = %event.dedupe_key,
            tenant_id = %event.tenant_id,
            effective_model = %event.effective_model,
            actual_credits_micro = event.actual_credits_micro,
            policy_version_applied = event.policy_version_applied,
            "usage event published to the static model policy plugin"
        );
        Ok(())
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
