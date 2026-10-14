//! The fixed-snapshot policy service behind the plugin client.

use async_trait::async_trait;
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, ModelPolicyPluginError, PolicySnapshot,
    PublishError, UsageEvent, UserLimits,
};
use uuid::Uuid;

use super::config::StaticModelPolicyConfig;
use crate::config::ConfigError;

/// The only policy version the static plugin serves.
const POLICY_VERSION: u64 = 1;

/// Serves `policy_version` 1 from the configured catalog and kill switches, and the same user
/// limits to every user (`default_standard_limits` for the `total` bucket,
/// `default_premium_limits` for `tier:premium`). `publish_usage` only logs.
pub struct StaticModelPolicyService {
    config: StaticModelPolicyConfig,
}

impl StaticModelPolicyService {
    /// Build the service; fails (plugin init) when a credit multiplier is out of range.
    ///
    /// # Errors
    ///
    /// A [`ConfigError`] from [`StaticModelPolicyConfig::validate`].
    pub fn new(config: StaticModelPolicyConfig) -> Result<Self, ConfigError> {
        config.validate()?;
        Ok(Self { config })
    }

    fn require_version(policy_version: u64) -> Result<(), ModelPolicyPluginError> {
        if policy_version == POLICY_VERSION {
            Ok(())
        } else {
            Err(ModelPolicyPluginError::PolicyVersionNotFound(
                policy_version,
            ))
        }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StaticModelPolicyService {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<u64, ModelPolicyPluginError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-serve
        Ok(POLICY_VERSION)
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-serve
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, ModelPolicyPluginError> {
        Self::require_version(policy_version)?;
        let kill_switches = &self.config.kill_switches;
        Ok(PolicySnapshot {
            policy_version: POLICY_VERSION,
            model_catalog: self.config.model_catalog.clone(),
            kill_switches: KillSwitches {
                disable_premium_tier: kill_switches.disable_premium_tier,
                force_standard_tier: kill_switches.force_standard_tier,
                disable_web_search: kill_switches.disable_web_search,
                disable_file_search: kill_switches.disable_file_search,
                disable_images: kill_switches.disable_images,
                disable_code_interpreter: kill_switches.disable_code_interpreter,
            },
        })
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, ModelPolicyPluginError> {
        Self::require_version(policy_version)?;
        // the same limits for every user
        Ok(UserLimits {
            user_id,
            policy_version: POLICY_VERSION,
            standard: self.config.default_standard_limits,
            premium: self.config.default_premium_limits,
        })
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-publish-usage
        tracing::info!(
            tenant_id = %event.tenant_id,
            chat_id = %event.chat_id,
            request_id = %event.request_id,
            effective_model = %event.effective_model,
            terminal_state = %event.terminal_state,
            billing_outcome = %event.billing_outcome,
            actual_credits_micro = event.actual_credits_micro,
            "static model policy plugin received a usage event"
        );
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-publish-usage
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod service_tests;
