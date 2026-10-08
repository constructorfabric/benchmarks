//! Bundled static model policy plugin (`static-mini-chat-model-policy-plugin`):
//! serves a fixed policy snapshot (version 1) from its own configuration.

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
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};
use uuid::Uuid;

pub const POLICY_VERSION: u64 = 1;
const MAX_MULT: i64 = 10_000_000_000;

fn default_vendor() -> String {
    crate::config::DEFAULT_VENDOR.to_owned()
}

fn default_priority() -> i16 {
    100
}

fn default_standard_limits() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 100_000_000,
        limit_monthly_credits_micro: 1_000_000_000,
    }
}

fn default_premium_limits() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 50_000_000,
        limit_monthly_credits_micro: 500_000_000,
    }
}

/// Operator-facing mirror of [`KillSwitches`] that rejects unknown keys.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[allow(clippy::struct_excessive_bools, reason = "independent feature flags")]
pub struct StaticKillSwitches {
    pub disable_premium_tier: bool,
    pub force_standard_tier: bool,
    pub disable_web_search: bool,
    pub disable_file_search: bool,
    pub disable_images: bool,
    pub disable_code_interpreter: bool,
}

impl From<StaticKillSwitches> for KillSwitches {
    fn from(k: StaticKillSwitches) -> Self {
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
#[serde(deny_unknown_fields)]
pub struct StaticModelPolicyConfig {
    #[serde(default = "default_vendor")]
    pub vendor: String,
    #[serde(default = "default_priority")]
    pub priority: i16,
    pub model_catalog: Vec<ModelCatalogEntry>,
    #[serde(default)]
    pub kill_switches: StaticKillSwitches,
    #[serde(default = "default_standard_limits")]
    pub default_standard_limits: TierLimits,
    #[serde(default = "default_premium_limits")]
    pub default_premium_limits: TierLimits,
}

impl Default for StaticModelPolicyConfig {
    fn default() -> Self {
        Self {
            vendor: default_vendor(),
            priority: default_priority(),
            model_catalog: Vec::new(),
            kill_switches: StaticKillSwitches::default(),
            default_standard_limits: default_standard_limits(),
            default_premium_limits: default_premium_limits(),
        }
    }
}

impl StaticModelPolicyConfig {
    /// Validate the catalog (credit multipliers and estimation budgets).
    ///
    /// # Errors
    /// Returns the first invalid entry.
    pub fn validate(&self) -> Result<(), String> {
        for m in &self.model_catalog {
            for (name, v) in [
                (
                    "input_tokens_credit_multiplier_micro",
                    m.input_tokens_credit_multiplier_micro,
                ),
                (
                    "output_tokens_credit_multiplier_micro",
                    m.output_tokens_credit_multiplier_micro,
                ),
            ] {
                if !(1..=MAX_MULT).contains(&v) {
                    return Err(format!(
                        "model '{}': {name} must be in 1..=10000000000 (got {v})",
                        m.id
                    ));
                }
            }
            if m.estimation_budgets.bytes_per_token_conservative == 0 {
                return Err(format!(
                    "model '{}': estimation_budgets.bytes_per_token_conservative must be > 0",
                    m.id
                ));
            }
        }
        Ok(())
    }
}

/// Plugin implementation.
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
                policy_version: POLICY_VERSION,
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
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError> {
        Ok(PolicyVersionInfo {
            policy_version: POLICY_VERSION,
        })
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        if policy_version != POLICY_VERSION {
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
            standard: self.standard,
            premium: self.premium,
        })
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        info!(
            target: "mini_chat.static_model_policy",
            dedupe_key = %payload.dedupe_key,
            billing_outcome = %payload.billing_outcome,
            settlement_method = %payload.settlement_method,
            actual_credits_micro = payload.actual_credits_micro,
            "usage event published"
        );
        Ok(())
    }
}

/// Static model policy plugin gear.
#[toolkit::gear(name = "static-mini-chat-model-policy-plugin", deps = [types_registry])]
pub struct StaticModelPolicyPlugin;

impl Default for StaticModelPolicyPlugin {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Gear for StaticModelPolicyPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticModelPolicyConfig = ctx.config_or_default()?;
        cfg.validate().map_err(|e| {
            anyhow::anyhow!("static-mini-chat-model-policy-plugin config invalid: {e}")
        })?;
        let (instance_id, instance_json) =
            PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
                "cf.builtin.static_mini_chat_model_policy.plugin.v1",
                &cfg.vendor,
                cfg.priority,
            )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        let svc: Arc<dyn MiniChatModelPolicyPluginClientV1> =
            Arc::new(StaticModelPolicyService::new(&cfg));
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                svc,
            );
        info!(
            instance_id = %instance_id,
            models = cfg.model_catalog.len(),
            "static mini-chat model policy plugin registered"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_requires_catalog_and_rejects_unknown_keys() {
        assert!(serde_json::from_value::<StaticModelPolicyConfig>(serde_json::json!({})).is_err());
        assert!(
            serde_json::from_value::<StaticModelPolicyConfig>(
                serde_json::json!({"model_catalog": [], "bogus": 1})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<StaticModelPolicyConfig>(serde_json::json!({
                "model_catalog": [], "kill_switches": {"disable_web_serch": true}
            }))
            .is_err()
        );
        let cfg: StaticModelPolicyConfig =
            serde_json::from_value(serde_json::json!({"model_catalog": []})).unwrap();
        assert_eq!(
            cfg.default_standard_limits.limit_daily_credits_micro,
            100_000_000
        );
    }

    #[test]
    fn zero_multiplier_is_rejected() {
        let cfg: StaticModelPolicyConfig = serde_json::from_value(serde_json::json!({
            "model_catalog": [{"id": "m", "tier": "Standard", "input_tokens_credit_multiplier_micro": 0,
                "output_tokens_credit_multiplier_micro": 1}]
        }))
        .unwrap();
        assert!(cfg.validate().is_err());
    }
}
