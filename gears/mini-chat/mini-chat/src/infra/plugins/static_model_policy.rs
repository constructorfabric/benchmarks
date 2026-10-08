//! Bundled static model policy plugin (`static-mini-chat-model-policy-plugin`):
//! serves a fixed snapshot (version 1) from its configuration; usage
//! publication only logs.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, ModelCatalogEntry, PolicyError,
    PolicySnapshot, PolicyVersionInfo, PublishError, TierLimits, UsageEvent, UserLimits,
};
use serde::Deserialize;
use time::OffsetDateTime;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};
use uuid::Uuid;

pub const POLICY_VERSION: u64 = 1;
const MAX_MULT: i64 = 10_000_000_000;

/// Operator-facing kill switches (unknown keys are rejected).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct KillSwitchesConfig {
    pub disable_premium_tier: bool,
    pub force_standard_tier: bool,
    pub disable_web_search: bool,
    pub disable_file_search: bool,
    pub disable_images: bool,
    pub disable_code_interpreter: bool,
}

impl From<&KillSwitchesConfig> for KillSwitches {
    fn from(k: &KillSwitchesConfig) -> Self {
        Self {
            disable_premium_tier: k.disable_premium_tier,
            force_standard_tier: k.force_standard_tier,
            disable_web_search: k.disable_web_search,
            disable_file_search: k.disable_file_search,
            disable_images: k.disable_images,
            disable_code_interpreter: k.disable_code_interpreter,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
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

impl StaticModelPolicyConfig {
    /// Validate the catalog (multipliers in `1..=10^10`, non-zero
    /// `bytes_per_token_conservative`).
    ///
    /// # Errors
    /// Descriptive message for the first invalid entry.
    pub fn validate(&self) -> Result<(), String> {
        for m in &self.model_catalog {
            for (k, v) in [
                ("input_tokens_credit_multiplier_micro", m.input_tokens_credit_multiplier_micro),
                ("output_tokens_credit_multiplier_micro", m.output_tokens_credit_multiplier_micro),
            ] {
                if !(1..=MAX_MULT).contains(&v) {
                    return Err(format!("model '{}': {k} must be in 1..=10000000000", m.id));
                }
            }
            if m.estimation_budgets.bytes_per_token_conservative == 0 {
                return Err(format!("model '{}': estimation_budgets.bytes_per_token_conservative must be > 0", m.id));
            }
        }
        Ok(())
    }
}

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
                kill_switches: KillSwitches::from(&cfg.kill_switches),
            },
            standard: cfg.default_standard_limits,
            premium: cfg.default_premium_limits,
            generated_at: OffsetDateTime::now_utc(),
        }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StaticModelPolicyService {
    async fn get_current_policy_version(&self, _user_id: Uuid) -> Result<PolicyVersionInfo, PolicyError> {
        Ok(PolicyVersionInfo {
            policy_version: POLICY_VERSION,
            generated_at: self.generated_at,
        })
    }

    async fn get_policy_snapshot(&self, _user_id: Uuid, _policy_version: u64) -> Result<PolicySnapshot, PolicyError> {
        Ok(self.snapshot.clone())
    }

    async fn get_user_limits(&self, user_id: Uuid, _policy_version: u64) -> Result<UserLimits, PolicyError> {
        Ok(UserLimits {
            user_id,
            policy_version: POLICY_VERSION,
            standard: self.standard,
            premium: self.premium,
        })
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        tracing::info!(
            dedupe_key = %payload.dedupe_key,
            billing_outcome = %payload.billing_outcome,
            settlement_method = %payload.settlement_method,
            actual_credits_micro = payload.actual_credits_micro,
            "mini-chat usage event"
        );
        Ok(())
    }
}

#[toolkit::gear(name = "static-mini-chat-model-policy-plugin", deps = [types_registry])]
#[derive(Default)]
pub struct StaticModelPolicyPlugin {
    service: OnceLock<Arc<StaticModelPolicyService>>,
}

#[async_trait]
impl Gear for StaticModelPolicyPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticModelPolicyConfig = ctx.config_or_default()?;
        cfg.validate().map_err(|e| anyhow::anyhow!("static model policy plugin config invalid: {e}"))?;
        let service = Arc::new(StaticModelPolicyService::from_config(&cfg));
        let (instance_id, instance_json) = PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
            "cf.core._.static_mini_chat_model_policy.v1",
            cfg.vendor.clone(),
            cfg.priority,
        )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        self.service
            .set(service.clone())
            .map_err(|_| anyhow::anyhow!("{} already initialized", Self::MODULE_NAME))?;
        let api: Arc<dyn MiniChatModelPolicyPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(ClientScope::gts_id(&instance_id), api);
        tracing::info!(instance_id = %instance_id, models = cfg.model_catalog.len(), "static model policy plugin registered");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_multiplier() {
        let cfg: StaticModelPolicyConfig = serde_json::from_value(serde_json::json!({
            "model_catalog": [{"id": "m", "tier": "Standard", "input_tokens_credit_multiplier_micro": 0, "output_tokens_credit_multiplier_micro": 1}]
        }))
        .unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_unknown_kill_switch() {
        let r: Result<StaticModelPolicyConfig, _> =
            serde_json::from_value(serde_json::json!({"model_catalog": [], "kill_switches": {"disable_everything": true}}));
        assert!(r.is_err());
    }

    #[test]
    fn defaults() {
        let cfg = StaticModelPolicyConfig::default();
        assert_eq!(cfg.default_standard_limits.limit_daily_credits_micro, 100_000_000);
        assert_eq!(cfg.default_premium_limits.limit_monthly_credits_micro, 500_000_000);
    }
}
