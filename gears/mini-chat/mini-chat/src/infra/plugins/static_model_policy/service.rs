//! Static model policy service: serves policy version 1 built from configuration.

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError, PolicySnapshot,
    PolicyVersionInfo, PublishError, UsageEvent, UserLimits,
};
use time::OffsetDateTime;
use tracing::info;
use uuid::Uuid;

use super::config::StaticModelPolicyConfig;

/// The only policy version the static plugin serves.
const POLICY_VERSION: u64 = 1;

/// Static [`MiniChatModelPolicyPluginClientV1`]: same snapshot and limits for every user.
pub struct StaticModelPolicyService {
    snapshot: PolicySnapshot,
    limits: UserLimits,
    generated_at: OffsetDateTime,
}

impl StaticModelPolicyService {
    /// Validate `cfg` and build the service; `generated_at` is the construction time.
    ///
    /// # Errors
    ///
    /// Fails when a catalog entry violates the plugin-init bounds (see
    /// [`StaticModelPolicyConfig::validate`]).
    pub fn from_config(cfg: &StaticModelPolicyConfig) -> anyhow::Result<Self> {
        cfg.validate()?;
        Ok(Self {
            snapshot: PolicySnapshot {
                policy_version: POLICY_VERSION,
                model_catalog: cfg.model_catalog.clone(),
                kill_switches: cfg.kill_switches.into(),
            },
            limits: UserLimits {
                // Replaced per call.
                user_id: Uuid::nil(),
                policy_version: POLICY_VERSION,
                standard: cfg.default_standard_limits,
                premium: cfg.default_premium_limits,
            },
            generated_at: OffsetDateTime::now_utc(),
        })
    }

    fn check_version(policy_version: u64) -> Result<(), MiniChatModelPolicyPluginError> {
        if policy_version == POLICY_VERSION {
            Ok(())
        } else {
            Err(MiniChatModelPolicyPluginError::NotFound(format!(
                "policy version {policy_version}"
            )))
        }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StaticModelPolicyService {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError> {
        Ok(PolicyVersionInfo {
            policy_version: POLICY_VERSION,
            generated_at: self.generated_at,
        })
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        Self::check_version(policy_version)?;
        Ok(self.snapshot.clone())
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        Self::check_version(policy_version)?;
        Ok(UserLimits {
            user_id,
            ..self.limits
        })
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        info!(
            tenant_id = %payload.tenant_id,
            request_id = %payload.request_id,
            effective_model = %payload.effective_model,
            terminal_state = %payload.terminal_state,
            billing_outcome = %payload.billing_outcome,
            actual_credits_micro = payload.actual_credits_micro,
            policy_version_applied = payload.policy_version_applied,
            dedupe_key = %payload.dedupe_key,
            "usage event published"
        );
        Ok(())
    }
}
