//! `static-mini-chat-model-policy-plugin`: fixed policy snapshot (version 1) from configuration.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, ModelCatalogEntry,
    PolicyPluginError, PolicySnapshot, PolicyVersionInfo, PublishError, TierLimits, UsageEvent, UserLimits,
};
use serde::Deserialize;
use toolkit::client_hub::ClientScope;
use toolkit::gts::PluginV1;
use toolkit::{Gear, GearCtx};
use types_registry_sdk::{RegisterResult, TypesRegistryClient};
use uuid::Uuid;

pub const STATIC_POLICY_VERSION: i64 = 1;
const MAX_MULT: i64 = 10_000_000_000;

/// Operator-facing mirror of `KillSwitches` (each field optional, unknown keys rejected).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KillSwitchesConfig {
    pub disable_premium_tier: bool,
    pub force_standard_tier: bool,
    pub disable_web_search: bool,
    pub disable_file_search: bool,
    pub disable_images: bool,
    pub disable_code_interpreter: bool,
}

impl From<KillSwitchesConfig> for KillSwitches {
    fn from(k: KillSwitchesConfig) -> Self {
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

fn default_standard_limits() -> TierLimits {
    TierLimits { limit_daily_credits_micro: 100_000_000, limit_monthly_credits_micro: 1_000_000_000 }
}

fn default_premium_limits() -> TierLimits {
    TierLimits { limit_daily_credits_micro: 50_000_000, limit_monthly_credits_micro: 500_000_000 }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticModelPolicyConfig {
    #[serde(default = "default_vendor")]
    pub vendor: String,
    #[serde(default = "default_priority")]
    pub priority: i16,
    pub model_catalog: Vec<ModelCatalogEntry>,
    #[serde(default)]
    pub kill_switches: KillSwitchesConfig,
    #[serde(default = "default_standard_limits")]
    pub default_standard_limits: TierLimits,
    #[serde(default = "default_premium_limits")]
    pub default_premium_limits: TierLimits,
}

fn default_vendor() -> String {
    "constructorfabric".to_owned()
}

const fn default_priority() -> i16 {
    100
}

impl Default for StaticModelPolicyConfig {
    fn default() -> Self {
        Self {
            vendor: default_vendor(),
            priority: default_priority(),
            model_catalog: Vec::new(),
            kill_switches: KillSwitchesConfig::default(),
            default_standard_limits: default_standard_limits(),
            default_premium_limits: default_premium_limits(),
        }
    }
}

impl StaticModelPolicyConfig {
    /// Validates multipliers and estimation budgets of every catalog entry.
    ///
    /// # Errors
    /// Returns a description of the first invalid entry.
    pub fn validate(&self) -> Result<(), String> {
        for m in &self.model_catalog {
            for (name, v) in [
                ("input_tokens_credit_multiplier_micro", m.input_tokens_credit_multiplier_micro),
                ("output_tokens_credit_multiplier_micro", m.output_tokens_credit_multiplier_micro),
            ] {
                if !(1..=MAX_MULT).contains(&v) {
                    return Err(format!("model '{}': {name} must be in 1..=10000000000, got {v}", m.id));
                }
            }
            if m.estimation_budgets.bytes_per_token_conservative == 0 {
                return Err(format!("model '{}': estimation_budgets.bytes_per_token_conservative must be > 0", m.id));
            }
        }
        Ok(())
    }
}

/// Plugin service.
pub struct StaticModelPolicyService {
    snapshot: PolicySnapshot,
    standard: TierLimits,
    premium: TierLimits,
}

impl StaticModelPolicyService {
    #[must_use]
    pub fn new(cfg: &StaticModelPolicyConfig) -> Self {
        Self {
            snapshot: PolicySnapshot {
                policy_version: STATIC_POLICY_VERSION,
                model_catalog: cfg.model_catalog.clone(),
                kill_switches: cfg.kill_switches.into(),
            },
            standard: cfg.default_standard_limits,
            premium: cfg.default_premium_limits,
        }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StaticModelPolicyService {
    async fn get_current_policy_version(&self, _user_id: Uuid) -> Result<PolicyVersionInfo, PolicyPluginError> {
        Ok(PolicyVersionInfo { policy_version: STATIC_POLICY_VERSION, generated_at: None })
    }

    async fn get_policy_snapshot(&self, _user_id: Uuid, _policy_version: i64) -> Result<PolicySnapshot, PolicyPluginError> {
        Ok(self.snapshot.clone())
    }

    async fn get_user_limits(&self, user_id: Uuid, policy_version: i64) -> Result<UserLimits, PolicyPluginError> {
        Ok(UserLimits { user_id, policy_version, standard: self.standard, premium: self.premium })
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        tracing::info!(
            target: "mini_chat_usage",
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
pub struct StaticMiniChatModelPolicyPlugin {
    service: OnceLock<Arc<StaticModelPolicyService>>,
}

impl Default for StaticMiniChatModelPolicyPlugin {
    fn default() -> Self {
        Self { service: OnceLock::new() }
    }
}

#[async_trait]
impl Gear for StaticMiniChatModelPolicyPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticModelPolicyConfig = ctx.config_or_default()?;
        cfg.validate().map_err(|e| anyhow::anyhow!("invalid static-mini-chat-model-policy-plugin config: {e}"))?;
        let (instance_id, instance_json) = PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
            "cf.core._.static_mini_chat_model_policy.v1",
            cfg.vendor.clone(),
            cfg.priority,
        )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        let service = Arc::new(StaticModelPolicyService::new(&cfg));
        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} already initialized", Self::MODULE_NAME))?;
        let api: Arc<dyn MiniChatModelPolicyPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(ClientScope::gts_id(&instance_id), api);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_multiplier() {
        let cfg: StaticModelPolicyConfig = serde_json::from_value(serde_json::json!({
            "model_catalog": [{
                "id": "m", "provider_model_id": "m", "display_name": "M", "provider_id": "p", "tier": "standard",
                "context_window": 10, "max_output_tokens": 5,
                "input_tokens_credit_multiplier_micro": 0, "output_tokens_credit_multiplier_micro": 1
            }]
        }))
        .unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_unknown_kill_switch() {
        let r: Result<StaticModelPolicyConfig, _> = serde_json::from_value(serde_json::json!({
            "model_catalog": [], "kill_switches": { "disable_everything": true }
        }));
        assert!(r.is_err());
    }
}
