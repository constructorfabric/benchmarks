//! `static-mini-chat-model-policy-plugin`: serves a fixed policy snapshot
//! (version 1) from its configuration (DESIGN §5.2.5, Appendix B.1).

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError, MiniChatModelPolicyPluginSpecV1,
    ModelCatalogEntry, PolicySnapshot, PublishError, TierLimits, UsageEvent, UserLimits,
};
use serde::Deserialize;
use toolkit::client_hub::ClientScope;
use toolkit::gts::PluginV1;
use toolkit::{Gear, GearCtx};
use types_registry_sdk::{RegisterResult, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::estimate::MAX_MULT;

/// Operator-facing mirror of [`KillSwitches`]: every field optional, unknown keys rejected.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
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
    /// Validates the catalog (multipliers in `1..=10^10`, non-zero bytes per token).
    ///
    /// # Errors
    /// Description of the first invalid entry.
    pub fn validate(&self) -> Result<(), String> {
        for m in &self.model_catalog {
            for (name, v) in [
                ("input_tokens_credit_multiplier_micro", m.input_tokens_credit_multiplier_micro),
                ("output_tokens_credit_multiplier_micro", m.output_tokens_credit_multiplier_micro),
            ] {
                if !(1..=MAX_MULT).contains(&v) {
                    return Err(format!("model '{}': {name} must be in 1..=10000000000", m.id));
                }
            }
            if m.estimation_budgets.bytes_per_token_conservative == 0 {
                return Err(format!("model '{}': bytes_per_token_conservative must be > 0", m.id));
            }
        }
        Ok(())
    }
}

/// Static policy service.
pub struct StaticModelPolicy {
    snapshot: PolicySnapshot,
    standard: TierLimits,
    premium: TierLimits,
}

impl StaticModelPolicy {
    #[must_use]
    pub fn new(cfg: &StaticModelPolicyConfig) -> Self {
        Self {
            snapshot: PolicySnapshot {
                policy_version: 1,
                model_catalog: cfg.model_catalog.clone(),
                kill_switches: cfg.kill_switches.into(),
            },
            standard: cfg.default_standard_limits,
            premium: cfg.default_premium_limits,
        }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StaticModelPolicy {
    async fn get_current_policy_version(&self, _user_id: Uuid) -> Result<u64, MiniChatModelPolicyPluginError> {
        Ok(self.snapshot.policy_version)
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        if policy_version == self.snapshot.policy_version {
            Ok(self.snapshot.clone())
        } else {
            Err(MiniChatModelPolicyPluginError::VersionNotFound(policy_version))
        }
    }

    async fn get_user_limits(&self, user_id: Uuid, policy_version: u64) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
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

#[toolkit::gear(name = "static-mini-chat-model-policy-plugin", deps = [types_registry])]
pub struct StaticMiniChatModelPolicyPlugin {
    service: OnceLock<Arc<StaticModelPolicy>>,
}

impl Default for StaticMiniChatModelPolicyPlugin {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for StaticMiniChatModelPolicyPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticModelPolicyConfig = ctx.config_or_default()?;
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("static-mini-chat-model-policy-plugin config invalid: {e}"))?;
        let (instance_id, payload) = PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
            "cf.builtin.static_mini_chat_model_policy.plugin.v1",
            cfg.vendor.clone(),
            cfg.priority,
        )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![payload]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        let service = Arc::new(StaticModelPolicy::new(&cfg));
        self.service
            .set(Arc::clone(&service))
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
    fn rejects_zero_multiplier_and_unknown_switch() {
        let cfg: StaticModelPolicyConfig = serde_json::from_value(serde_json::json!({
            "model_catalog": [{"id": "m", "tier": "standard", "input_tokens_credit_multiplier_micro": 0, "output_tokens_credit_multiplier_micro": 1}]
        }))
        .unwrap();
        assert!(cfg.validate().is_err());
        assert!(
            serde_json::from_value::<StaticModelPolicyConfig>(serde_json::json!({"kill_switches": {"disable_everything": true}}))
                .is_err()
        );
    }

    #[tokio::test]
    async fn serves_version_one_and_default_limits() {
        let p = StaticModelPolicy::new(&StaticModelPolicyConfig::default());
        assert_eq!(p.get_current_policy_version(Uuid::nil()).await.unwrap(), 1);
        assert!(p.get_policy_snapshot(Uuid::nil(), 2).await.is_err());
        let l = p.get_user_limits(Uuid::nil(), 1).await.unwrap();
        assert_eq!(l.standard.limit_daily_credits_micro, 100_000_000);
        assert_eq!(l.premium.limit_monthly_credits_micro, 500_000_000);
    }
}
