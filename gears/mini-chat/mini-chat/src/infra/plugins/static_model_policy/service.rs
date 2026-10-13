//! Client of the static model policy plugin.

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, PolicyPluginError, PolicySnapshot, PolicyVersionInfo,
    PublishError, UsageEvent, UserLimits,
};
use time::OffsetDateTime;
use uuid::Uuid;

use super::config::StaticModelPolicyConfig;

/// The only policy version served by the static plugin.
pub const STATIC_POLICY_VERSION: u64 = 1;

/// Model policy plugin serving a fixed snapshot (version 1).
pub struct StaticModelPolicyService {
    config: StaticModelPolicyConfig,
    generated_at: OffsetDateTime,
}

impl StaticModelPolicyService {
    /// Creates the service; the policy `generated_at` is the creation time.
    #[must_use]
    pub fn new(config: StaticModelPolicyConfig) -> Self {
        Self {
            config,
            generated_at: OffsetDateTime::now_utc(),
        }
    }

    /// Rejects every version but the only one the static plugin has: serving
    /// version 1 data under another version would break snapshot immutability.
    fn ensure_version(policy_version: u64) -> Result<(), PolicyPluginError> {
        if policy_version == STATIC_POLICY_VERSION {
            Ok(())
        } else {
            Err(PolicyPluginError::NotFound(format!(
                "policy version {policy_version} (the static plugin serves version {STATIC_POLICY_VERSION} only)"
            )))
        }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StaticModelPolicyService {
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, PolicyPluginError> {
        let _ = user_id;
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-serve-version
        Ok(PolicyVersionInfo {
            policy_version: STATIC_POLICY_VERSION,
            generated_at: self.generated_at,
        })
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-serve-version
    }

    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, PolicyPluginError> {
        let _ = user_id;
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-serve-snapshot
        Self::ensure_version(policy_version)?;
        Ok(PolicySnapshot {
            policy_version: STATIC_POLICY_VERSION,
            model_catalog: self.config.model_catalog.clone(),
            kill_switches: self.config.kill_switches,
        })
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-serve-snapshot
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, PolicyPluginError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-serve-limits
        Self::ensure_version(policy_version)?;
        Ok(UserLimits {
            user_id,
            policy_version: STATIC_POLICY_VERSION,
            standard: self.config.default_standard_limits,
            premium: self.config.default_premium_limits,
        })
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-serve-limits
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-serve-publish
        tracing::info!(
            target: "mini_chat::usage",
            tenant_id = %event.tenant_id,
            chat_id = %event.chat_id,
            request_id = %event.request_id,
            effective_model = %event.effective_model,
            terminal_state = ?event.terminal_state,
            billing_outcome = ?event.billing_outcome,
            actual_credits_micro = event.actual_credits_micro,
            policy_version_applied = event.policy_version_applied,
            "mini-chat usage event"
        );
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-serve-publish
    }

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-keep-license-default
    // `check_user_license` is deliberately not overridden: the trait default
    // reports `active = false` and the gear never calls it.
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-keep-license-default
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
