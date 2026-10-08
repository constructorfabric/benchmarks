use async_trait::async_trait;
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, ModelCatalogEntry, PolicyPluginError, PolicySnapshot,
    PolicyVersionInfo, PublishError, TierLimits, UsageEvent, UserLimits,
};
use serde::Deserialize;
use uuid::Uuid;

const POLICY_VERSION: u64 = 1;
const MAX_MULTIPLIER: u64 = 10_000_000_000;

/// Operator-facing kill switches; every field optional, unknown keys rejected.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[allow(clippy::struct_excessive_bools)]
pub struct KillSwitchesConfig {
    pub disable_premium_tier: bool,
    pub force_standard_tier: bool,
    pub disable_web_search: bool,
    pub disable_file_search: bool,
    pub disable_images: bool,
    pub disable_code_interpreter: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StaticModelPolicyConfig {
    pub vendor: String,
    pub priority: i16,
    pub model_catalog: Vec<ModelCatalogEntry>,
    pub kill_switches: KillSwitchesConfig,
    pub default_standard_limits: TierLimits,
    pub default_premium_limits: TierLimits,
}

impl Default for StaticModelPolicyConfig {
    fn default() -> Self {
        Self {
            vendor: "constructorfabric".to_owned(),
            priority: 100,
            model_catalog: Vec::new(),
            kill_switches: KillSwitchesConfig::default(),
            default_standard_limits: TierLimits {
                limit_daily_credits_micro: 100_000_000,
                limit_monthly_credits_micro: 1_000_000_000,
            },
            default_premium_limits: TierLimits {
                limit_daily_credits_micro: 50_000_000,
                limit_monthly_credits_micro: 500_000_000,
            },
        }
    }
}

/// In-process static policy.
pub struct StaticModelPolicyService {
    snapshot: PolicySnapshot,
    standard: TierLimits,
    premium: TierLimits,
    generated_at: time::OffsetDateTime,
}

impl StaticModelPolicyService {
    /// # Errors
    /// Returns a message when a catalog entry has an invalid multiplier or
    /// `bytes_per_token_conservative = 0`.
    pub fn from_config(cfg: &StaticModelPolicyConfig) -> Result<Self, String> {
        for m in &cfg.model_catalog {
            for (name, v) in [
                ("input_tokens_credit_multiplier_micro", m.input_tokens_credit_multiplier_micro),
                ("output_tokens_credit_multiplier_micro", m.output_tokens_credit_multiplier_micro),
            ] {
                if !(1..=MAX_MULTIPLIER).contains(&v) {
                    return Err(format!("model '{}': {name} must be in 1..={MAX_MULTIPLIER}", m.id));
                }
            }
            if m.estimation_budgets.bytes_per_token_conservative == 0 {
                return Err(format!("model '{}': bytes_per_token_conservative must be > 0", m.id));
            }
        }
        let k = &cfg.kill_switches;
        Ok(Self {
            snapshot: PolicySnapshot {
                policy_version: POLICY_VERSION,
                model_catalog: cfg.model_catalog.clone(),
                kill_switches: KillSwitches {
                    disable_premium_tier: k.disable_premium_tier,
                    force_standard_tier: k.force_standard_tier,
                    disable_web_search: k.disable_web_search,
                    disable_file_search: k.disable_file_search,
                    disable_images: k.disable_images,
                    disable_code_interpreter: k.disable_code_interpreter,
                },
            },
            standard: cfg.default_standard_limits,
            premium: cfg.default_premium_limits,
            generated_at: time::OffsetDateTime::now_utc(),
        })
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StaticModelPolicyService {
    async fn get_current_policy_version(&self, _user_id: Uuid) -> Result<PolicyVersionInfo, PolicyPluginError> {
        Ok(PolicyVersionInfo {
            policy_version: POLICY_VERSION,
            generated_at: self.generated_at,
        })
    }

    async fn get_policy_snapshot(&self, _user_id: Uuid, policy_version: u64) -> Result<PolicySnapshot, PolicyPluginError> {
        if policy_version != POLICY_VERSION {
            return Err(PolicyPluginError::VersionNotFound(policy_version));
        }
        Ok(self.snapshot.clone())
    }

    async fn get_user_limits(&self, user_id: Uuid, policy_version: u64) -> Result<UserLimits, PolicyPluginError> {
        Ok(UserLimits {
            user_id,
            policy_version,
            standard: self.standard,
            premium: self.premium,
        })
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        tracing::info!(
            tenant_id = %payload.tenant_id,
            request_id = %payload.request_id,
            billing_outcome = %payload.billing_outcome,
            settlement_method = %payload.settlement_method,
            actual_credits_micro = payload.actual_credits_micro,
            dedupe_key = %payload.dedupe_key,
            "mini-chat usage event"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_validation() {
        let cfg: StaticModelPolicyConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(cfg.default_standard_limits.limit_daily_credits_micro, 100_000_000);
        assert_eq!(cfg.default_premium_limits.limit_monthly_credits_micro, 500_000_000);
        assert!(serde_json::from_value::<StaticModelPolicyConfig>(serde_json::json!({"kill_switches": {"nope": true}})).is_err());
        let bad: StaticModelPolicyConfig = serde_json::from_value(serde_json::json!({"model_catalog": [{
            "id": "m", "provider_model_id": "m", "display_name": "m", "provider_id": "p", "tier": "standard",
            "context_window": 1, "max_output_tokens": 1, "input_tokens_credit_multiplier_micro": 0,
            "output_tokens_credit_multiplier_micro": 1
        }]}))
        .unwrap();
        assert!(StaticModelPolicyService::from_config(&bad).is_err());
    }
}
