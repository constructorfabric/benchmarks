use async_trait::async_trait;
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    PolicySnapshot, PolicyVersionInfo, PublishError, UsageEvent, UserLimits,
};
use time::OffsetDateTime;
use uuid::Uuid;

use super::config::StaticModelPolicyConfig;

/// Fixed policy version served by the static plugin.
pub const STATIC_POLICY_VERSION: u64 = 1;

/// In-process implementation of the model-policy plugin contract.
pub struct StaticModelPolicyService {
    snapshot: PolicySnapshot,
    cfg: StaticModelPolicyConfig,
    generated_at: OffsetDateTime,
}

impl StaticModelPolicyService {
    #[must_use]
    pub fn new(cfg: StaticModelPolicyConfig) -> Self {
        let snapshot = PolicySnapshot {
            policy_version: STATIC_POLICY_VERSION,
            model_catalog: cfg.model_catalog.clone(),
            kill_switches: KillSwitches::from(cfg.kill_switches),
        };
        Self {
            snapshot,
            cfg,
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
            policy_version: STATIC_POLICY_VERSION,
            generated_at: self.generated_at,
        })
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        if policy_version != STATIC_POLICY_VERSION {
            return Err(MiniChatModelPolicyPluginError::NotFound(format!(
                "policy version {policy_version}"
            )));
        }
        Ok(self.snapshot.clone())
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        Ok(UserLimits {
            user_id,
            policy_version,
            standard: self.cfg.default_standard_limits,
            premium: self.cfg.default_premium_limits,
        })
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        let json = serde_json::to_string(&payload).unwrap_or_default();
        tracing::info!(
            tenant_id = %payload.tenant_id,
            chat_id = %payload.chat_id,
            request_id = %payload.request_id,
            billing_outcome = %payload.billing_outcome,
            settlement_method = %payload.settlement_method,
            actual_credits_micro = payload.actual_credits_micro,
            dedupe_key = %payload.dedupe_key,
            event = %json,
            "mini-chat usage event"
        );
        Ok(())
    }
}
