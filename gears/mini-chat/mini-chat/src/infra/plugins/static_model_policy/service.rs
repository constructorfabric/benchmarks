//! Static model-policy service: the same snapshot and limits for every user.

use async_trait::async_trait;
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, ModelPolicyPluginError, PolicySnapshot,
    PolicyVersionInfo, PublishError, TierLimits, UsageEvent, UserLimits,
};
use uuid::Uuid;

use super::config::StaticModelPolicyConfig;

/// The only policy version the static plugin serves.
const POLICY_VERSION: u64 = 1;

/// The `snake_case` wire name of a serde enum value.
fn wire_name<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Invalid static model-policy configuration.
#[derive(Debug, thiserror::Error)]
pub enum StaticModelPolicyError {
    /// A credit multiplier is outside `1..=10_000_000_000`.
    #[error("model `{model}`: {field} = {value} is outside 1..=10000000000")]
    MultiplierOutOfRange {
        /// Offending model id.
        model: String,
        /// Offending field.
        field: &'static str,
        /// Offending value.
        value: u64,
    },
    /// `estimation_budgets.bytes_per_token_conservative` is zero.
    #[error("model `{model}`: estimation_budgets.bytes_per_token_conservative must be > 0")]
    ZeroBytesPerToken {
        /// Offending model id.
        model: String,
    },
}

/// Serves the configured catalog as policy version `1`.
pub struct StaticModelPolicyService {
    snapshot: PolicySnapshot,
    standard: TierLimits,
    premium: TierLimits,
}

impl StaticModelPolicyService {
    /// Builds the service after validating the configuration.
    ///
    /// # Errors
    /// The first configuration error found by
    /// [`StaticModelPolicyConfig::validate`].
    pub fn from_config(cfg: &StaticModelPolicyConfig) -> Result<Self, StaticModelPolicyError> {
        cfg.validate()?;
        Ok(Self {
            snapshot: PolicySnapshot {
                policy_version: POLICY_VERSION,
                model_catalog: cfg.model_catalog.clone(),
                kill_switches: KillSwitches::from(&cfg.kill_switches),
            },
            standard: cfg.default_standard_limits.clone(),
            premium: cfg.default_premium_limits.clone(),
        })
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StaticModelPolicyService {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, ModelPolicyPluginError> {
        Ok(PolicyVersionInfo {
            policy_version: POLICY_VERSION,
            generated_at: None,
        })
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<PolicySnapshot, ModelPolicyPluginError> {
        Ok(self.snapshot.clone())
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, ModelPolicyPluginError> {
        Ok(UserLimits {
            user_id,
            policy_version,
            standard: self.standard.clone(),
            premium: self.premium.clone(),
        })
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        // `key=value` fields only: identities, models, outcome and counts, never
        // prompt or answer content. UUIDs are hyphenated, enums use their wire
        // name, `dedupe_key` is the 32-hex form of the payload.
        tracing::info!(
            tenant_id = %payload.tenant_id,
            user_id = %payload.user_id.unwrap_or_default(),
            chat_id = %payload.chat_id,
            turn_id = %payload.turn_id.unwrap_or_default(),
            request_id = %payload.request_id,
            dedupe_key = %payload.dedupe_key,
            effective_model = %payload.effective_model,
            selected_model = %payload.selected_model,
            terminal_state = wire_name(&payload.terminal_state),
            billing_outcome = wire_name(&payload.billing_outcome),
            settlement_method = wire_name(&payload.settlement_method),
            actual_credits_micro = payload.actual_credits_micro,
            policy_version_applied = payload.policy_version_applied,
            web_search_calls = payload.web_search_calls,
            code_interpreter_calls = payload.code_interpreter_calls,
            file_search_calls = payload.file_search_calls,
            "usage event received by the static model policy plugin"
        );
        Ok(())
    }
}
