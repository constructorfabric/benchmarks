//! Static model policy: serves the configured catalog, kill switches and
//! limits at a fixed policy version, for every user.

use async_trait::async_trait;
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    ModelCatalogEntry, PolicySnapshot, PolicyVersionInfo, PublishError, TierLimits, UsageEvent,
    UserLimits,
};
use time::OffsetDateTime;
use uuid::Uuid;

use super::config::StaticModelPolicyPluginConfig;

/// The only policy version a static policy ever has.
const POLICY_VERSION: u64 = 1;

pub struct StaticModelPolicyService {
    model_catalog: Vec<ModelCatalogEntry>,
    kill_switches: KillSwitches,
    standard: TierLimits,
    premium: TierLimits,
}

impl StaticModelPolicyService {
    #[must_use]
    pub fn from_config(cfg: &StaticModelPolicyPluginConfig) -> Self {
        Self {
            model_catalog: cfg.model_catalog.clone(),
            kill_switches: cfg.kill_switches.into(),
            standard: cfg.default_standard_limits,
            premium: cfg.default_premium_limits,
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
            generated_at: OffsetDateTime::now_utc(),
        })
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        Ok(PolicySnapshot {
            policy_version: POLICY_VERSION,
            model_catalog: self.model_catalog.clone(),
            kill_switches: self.kill_switches,
        })
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        _policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        Ok(UserLimits {
            user_id,
            policy_version: POLICY_VERSION,
            standard: self.standard,
            premium: self.premium,
        })
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        tracing::info!(
            tenant_id = %payload.tenant_id,
            chat_id = %payload.chat_id,
            request_id = %payload.request_id,
            effective_model = %payload.effective_model,
            billing_outcome = %payload.billing_outcome,
            actual_credits_micro = payload.actual_credits_micro,
            policy_version_applied = payload.policy_version_applied,
            "static model policy: usage settled"
        );
        Ok(())
    }
}
