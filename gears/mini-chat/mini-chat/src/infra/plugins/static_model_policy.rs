//! `static-mini-chat-model-policy-plugin`: serves a fixed policy snapshot
//! (version 1) from its own configuration; `publish_usage` only logs.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    MiniChatModelPolicyPluginSpecV1, ModelCatalogEntry, PolicySnapshot, PublishError, TierLimits,
    UsageEvent, UserLimits,
};
use serde::Deserialize;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use toolkit::Gear;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};
use uuid::Uuid;

/// Fixed policy version served by the static plugin.
pub const STATIC_POLICY_VERSION: u64 = 1;

const MAX_MULTIPLIER: i64 = 10_000_000_000;

/// Operator-facing mirror of the SDK `KillSwitches`: every field optional,
/// unknown keys rejected.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // mirrors the SDK `KillSwitches` flag set
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

fn default_vendor() -> String {
    "constructorfabric".to_owned()
}
fn default_priority() -> i16 {
    100
}
fn default_standard_limits() -> TierLimits {
    TierLimits { limit_daily_credits_micro: 100_000_000, limit_monthly_credits_micro: 1_000_000_000 }
}
fn default_premium_limits() -> TierLimits {
    TierLimits { limit_daily_credits_micro: 50_000_000, limit_monthly_credits_micro: 500_000_000 }
}

/// Plugin configuration. `model_catalog` is required when the section is present.
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
    /// # Errors
    /// Fails on a multiplier outside `1..=10_000_000_000` or a zero bytes-per-token budget.
    pub fn validate(&self) -> Result<(), String> {
        for m in &self.model_catalog {
            for (name, v) in [
                ("input_tokens_credit_multiplier_micro", m.input_tokens_credit_multiplier_micro),
                ("output_tokens_credit_multiplier_micro", m.output_tokens_credit_multiplier_micro),
            ] {
                if !(1..=MAX_MULTIPLIER).contains(&v) {
                    return Err(format!(
                        "model_catalog[{}].{name} must be in 1..=10000000000, got {v}",
                        m.id
                    ));
                }
            }
            if m.estimation_budgets.bytes_per_token_conservative == 0 {
                return Err(format!(
                    "model_catalog[{}].estimation_budgets.bytes_per_token_conservative must be > 0",
                    m.id
                ));
            }
        }
        Ok(())
    }
}

/// In-process implementation of the policy plugin client.
pub struct StaticModelPolicyService {
    snapshot: PolicySnapshot,
    standard: TierLimits,
    premium: TierLimits,
}

impl StaticModelPolicyService {
    #[must_use]
    pub fn from_config(cfg: &StaticModelPolicyConfig) -> Self {
        Self {
            snapshot: PolicySnapshot {
                policy_version: STATIC_POLICY_VERSION,
                model_catalog: cfg.model_catalog.clone(),
                kill_switches: cfg.kill_switches.clone().into(),
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
    ) -> Result<u64, MiniChatModelPolicyPluginError> {
        Ok(STATIC_POLICY_VERSION)
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        if policy_version != STATIC_POLICY_VERSION {
            return Err(MiniChatModelPolicyPluginError::VersionNotFound(policy_version));
        }
        Ok(self.snapshot.clone())
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        Ok(UserLimits { user_id, policy_version, standard: self.standard, premium: self.premium })
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        tracing::info!(
            target: "mini_chat",
            tenant_id = %payload.tenant_id,
            chat_id = %payload.chat_id,
            request_id = %payload.request_id,
            billing_outcome = %payload.billing_outcome,
            settlement_method = %payload.settlement_method,
            actual_credits_micro = payload.actual_credits_micro,
            dedupe_key = %payload.dedupe_key,
            "static model policy plugin: usage published"
        );
        Ok(())
    }
}

#[toolkit::gear(name = "static-mini-chat-model-policy-plugin", deps = [types_registry])]
pub struct StaticModelPolicyPlugin {
    service: OnceLock<Arc<StaticModelPolicyService>>,
}

impl Default for StaticModelPolicyPlugin {
    fn default() -> Self {
        Self { service: OnceLock::new() }
    }
}

#[async_trait]
impl Gear for StaticModelPolicyPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticModelPolicyConfig = ctx.config_or_default()?;
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("static-mini-chat-model-policy-plugin config invalid: {e}"))?;
        let service = Arc::new(StaticModelPolicyService::from_config(&cfg));
        let (instance_id, instance_json) =
            PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
                "cf.core._.static_mini_chat_model_policy.v1",
                cfg.vendor.clone(),
                cfg.priority,
            )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        self.service
            .set(service.clone())
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        let api: Arc<dyn MiniChatModelPolicyPluginClientV1> = service;
        ctx.client_hub().register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
            ClientScope::gts_id(&instance_id),
            api,
        );
        tracing::info!(instance_id = %instance_id, models = cfg.model_catalog.len(), "static model policy plugin registered");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_section_uses_defaults_but_present_section_requires_catalog() {
        let d = StaticModelPolicyConfig::default();
        assert!(d.model_catalog.is_empty());
        assert_eq!(d.default_premium_limits.limit_daily_credits_micro, 50_000_000);
        assert!(serde_json::from_value::<StaticModelPolicyConfig>(serde_json::json!({"vendor": "v"})).is_err());
        assert!(
            serde_json::from_value::<StaticModelPolicyConfig>(serde_json::json!({
                "model_catalog": [], "kill_switches": {"disable_images": true, "typo": true}
            }))
            .is_err()
        );
    }

    #[test]
    fn multiplier_bounds_validated() {
        let cfg: StaticModelPolicyConfig = serde_json::from_value(serde_json::json!({
            "model_catalog": [{"id": "m", "input_tokens_credit_multiplier_micro": 0,
                               "output_tokens_credit_multiplier_micro": 1}]
        }))
        .expect("parse");
        assert!(cfg.validate().is_err());
    }
}
