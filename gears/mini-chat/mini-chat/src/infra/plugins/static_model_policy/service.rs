//! Static model policy plugin implementation.

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError, PolicySnapshot,
    PolicyVersionInfo, PublishError, TierLimits, UsageEvent, UserLimits,
};
use time::OffsetDateTime;
use tracing::info;
use uuid::Uuid;

use super::config::StaticModelPolicyConfig;

/// Fixed policy version of the static plugin.
pub const POLICY_VERSION: u64 = 1;

/// Serves a fixed snapshot and fixed limits.
pub struct StaticModelPolicyService {
    snapshot: PolicySnapshot,
    standard: TierLimits,
    premium: TierLimits,
    generated_at: OffsetDateTime,
}

impl StaticModelPolicyService {
    #[must_use]
    pub fn from_config(cfg: &StaticModelPolicyConfig) -> Self {
        Self {
            snapshot: PolicySnapshot {
                policy_version: POLICY_VERSION,
                model_catalog: cfg.model_catalog.clone(),
                kill_switches: cfg.kill_switches.into(),
            },
            standard: cfg.default_standard_limits,
            premium: cfg.default_premium_limits,
            generated_at: OffsetDateTime::now_utc(),
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
        if policy_version != POLICY_VERSION {
            return Err(MiniChatModelPolicyPluginError::PolicyVersionNotFound(
                policy_version,
            ));
        }
        Ok(self.snapshot.clone())
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        if policy_version != POLICY_VERSION {
            return Err(MiniChatModelPolicyPluginError::PolicyVersionNotFound(
                policy_version,
            ));
        }
        Ok(UserLimits {
            user_id,
            policy_version,
            standard: self.standard,
            premium: self.premium,
        })
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        info!(
            tenant_id = %payload.tenant_id,
            chat_id = ?payload.chat_id,
            request_id = %payload.request_id,
            requester_type = %payload.requester_type,
            billing_outcome = %payload.billing_outcome,
            settlement_method = %payload.settlement_method,
            actual_credits_micro = payload.actual_credits_micro,
            policy_version_applied = payload.policy_version_applied,
            dedupe_key = %payload.dedupe_key,
            "mini-chat usage event"
        );
        Ok(())
    }
}
