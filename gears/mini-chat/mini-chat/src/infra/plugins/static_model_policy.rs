//! `static-mini-chat-model-policy-plugin`: serves a fixed policy snapshot
//! (version 1) from its configuration; `publish_usage` only logs.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    MiniChatModelPolicyPluginSpecV1, ModelCatalogEntry, PolicySnapshot, PolicyVersionInfo,
    PublishError, TierLimits, UsageEvent, UserLimits,
};
use serde::Deserialize;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::credits::MAX_MULTIPLIER;

/// The fixed policy version served by this plugin.
pub const POLICY_VERSION: u64 = 1;

fn default_vendor() -> String {
    "constructorfabric".to_owned()
}

fn default_priority() -> i16 {
    100
}

fn default_standard() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 100_000_000,
        limit_monthly_credits_micro: 1_000_000_000,
    }
}

fn default_premium() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 50_000_000,
        limit_monthly_credits_micro: 500_000_000,
    }
}

/// Operator-facing kill switches (unknown keys rejected; missing = false).
#[derive(Debug, Clone, Copy, Deserialize, Default)]
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

/// Plugin configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticModelPolicyConfig {
    #[serde(default = "default_vendor")]
    pub vendor: String,
    #[serde(default = "default_priority")]
    pub priority: i16,
    #[serde(default)]
    pub model_catalog: Option<Vec<ModelCatalogEntry>>,
    #[serde(default)]
    pub kill_switches: KillSwitchesConfig,
    #[serde(default = "default_standard")]
    pub default_standard_limits: TierLimits,
    #[serde(default = "default_premium")]
    pub default_premium_limits: TierLimits,
}

impl Default for StaticModelPolicyConfig {
    fn default() -> Self {
        Self {
            vendor: default_vendor(),
            priority: default_priority(),
            model_catalog: Some(Vec::new()),
            kill_switches: KillSwitchesConfig::default(),
            default_standard_limits: default_standard(),
            default_premium_limits: default_premium(),
        }
    }
}

impl StaticModelPolicyConfig {
    /// Parse the raw config section (`{}` when absent).
    ///
    /// # Errors
    /// Unknown keys, missing `model_catalog`, invalid multipliers.
    pub fn from_value(v: &serde_json::Value) -> anyhow::Result<Self> {
        let empty = v.as_object().is_none_or(serde_json::Map::is_empty);
        if empty {
            return Ok(Self::default());
        }
        let cfg: Self = serde_json::from_value(v.clone())?;
        let catalog = cfg
            .model_catalog
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("model_catalog is required"))?;
        for m in catalog {
            for (name, mult) in [
                ("input_tokens_credit_multiplier_micro", m.input_tokens_credit_multiplier_micro),
                ("output_tokens_credit_multiplier_micro", m.output_tokens_credit_multiplier_micro),
            ] {
                if !(1..=MAX_MULTIPLIER).contains(&mult) {
                    anyhow::bail!("model '{}': {name} must be in 1..=10000000000", m.id);
                }
            }
            if m.estimation_budgets.bytes_per_token_conservative == 0 {
                anyhow::bail!("model '{}': bytes_per_token_conservative must be > 0", m.id);
            }
        }
        Ok(cfg)
    }
}

/// Plugin client implementation.
pub struct StaticModelPolicy {
    snapshot: PolicySnapshot,
    standard: TierLimits,
    premium: TierLimits,
}

impl StaticModelPolicy {
    #[must_use]
    pub fn new(cfg: &StaticModelPolicyConfig) -> Self {
        let ks = cfg.kill_switches;
        Self {
            snapshot: PolicySnapshot {
                policy_version: POLICY_VERSION,
                model_catalog: cfg.model_catalog.clone().unwrap_or_default(),
                kill_switches: KillSwitches {
                    disable_premium_tier: ks.disable_premium_tier,
                    force_standard_tier: ks.force_standard_tier,
                    disable_web_search: ks.disable_web_search,
                    disable_file_search: ks.disable_file_search,
                    disable_images: ks.disable_images,
                    disable_code_interpreter: ks.disable_code_interpreter,
                },
            },
            standard: cfg.default_standard_limits,
            premium: cfg.default_premium_limits,
        }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StaticModelPolicy {
    async fn get_current_policy_version(&self, _user_id: Uuid) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError> {
        Ok(PolicyVersionInfo {
            policy_version: POLICY_VERSION,
            generated_at: None,
        })
    }

    async fn get_policy_snapshot(&self, _user_id: Uuid, policy_version: u64) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        if policy_version != POLICY_VERSION {
            return Err(MiniChatModelPolicyPluginError::PolicyVersionNotFound(policy_version));
        }
        Ok(self.snapshot.clone())
    }

    async fn get_user_limits(&self, user_id: Uuid, policy_version: u64) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        if policy_version != POLICY_VERSION {
            return Err(MiniChatModelPolicyPluginError::PolicyVersionNotFound(policy_version));
        }
        Ok(UserLimits {
            user_id,
            policy_version,
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

/// Static model policy plugin gear.
#[toolkit::gear(name = "static-mini-chat-model-policy-plugin", deps = [types_registry])]
#[derive(Default)]
pub struct StaticMiniChatModelPolicyPlugin;

#[async_trait]
impl Gear for StaticMiniChatModelPolicyPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg = StaticModelPolicyConfig::from_value(ctx.raw_config())?;
        let client: Arc<dyn MiniChatModelPolicyPluginClientV1> = Arc::new(StaticModelPolicy::new(&cfg));
        let (instance_id, instance_json) = PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
            "cf.core._.static_mini_chat_model_policy.v1",
            cfg.vendor.clone(),
            cfg.priority,
        )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(ClientScope::gts_id(&instance_id), client);
        tracing::info!(instance_id = %instance_id, models = cfg.model_catalog.as_ref().map_or(0, Vec::len), "static model policy plugin registered");
        Ok(())
    }
}

#[cfg(test)]
#[path = "static_model_policy_tests.rs"]
mod tests;
