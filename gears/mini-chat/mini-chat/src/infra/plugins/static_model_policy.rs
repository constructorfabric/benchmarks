//! `static-mini-chat-model-policy-plugin`: fixed policy snapshot (version 1) from configuration.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    MiniChatModelPolicyPluginSpecV1, ModelCatalogEntry, PolicySnapshot, PolicyVersionInfo,
    PublishError, TierLimits, UsageEvent, UserLimits,
};
use serde::Deserialize;
use time::OffsetDateTime;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use toolkit::Gear;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};
use uuid::Uuid;

const MAX_MULTIPLIER: i64 = 10_000_000_000;

/// Operator-facing mirror of the SDK `KillSwitches` (every field optional, unknown keys rejected).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct StaticKillSwitches {
    pub disable_premium_tier: bool,
    pub force_standard_tier: bool,
    pub disable_web_search: bool,
    pub disable_file_search: bool,
    pub disable_images: bool,
    pub disable_code_interpreter: bool,
}

/// Plugin configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticModelPolicyConfig {
    pub vendor: String,
    pub priority: i16,
    /// Required when the plugin configuration section is present: a missing key
    /// deserializes to `None` (field-level default) and fails validation; the empty catalog
    /// of [`Default`] applies only when the whole section is absent.
    #[serde(default)]
    pub model_catalog: Option<Vec<ModelCatalogEntry>>,
    pub kill_switches: StaticKillSwitches,
    pub default_standard_limits: TierLimits,
    pub default_premium_limits: TierLimits,
}

impl Default for StaticModelPolicyConfig {
    fn default() -> Self {
        Self {
            vendor: "constructorfabric".to_owned(),
            priority: 100,
            model_catalog: Some(Vec::new()),
            kill_switches: StaticKillSwitches::default(),
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
    /// Validates the catalog (multipliers in `1..=10_000_000_000`, non-zero bytes per token).
    ///
    /// # Errors
    /// Description of the first invalid entry.
    pub fn validate(&self) -> Result<(), String> {
        let catalog = self
            .model_catalog
            .as_ref()
            .ok_or_else(|| "model_catalog is required".to_owned())?;
        for e in catalog {
            for (name, v) in [
                ("input_tokens_credit_multiplier_micro", e.input_tokens_credit_multiplier_micro),
                ("output_tokens_credit_multiplier_micro", e.output_tokens_credit_multiplier_micro),
            ] {
                if !(1..=MAX_MULTIPLIER).contains(&v) {
                    return Err(format!("model '{}': {name} must be in 1..=10000000000", e.id));
                }
            }
            if e.estimation_budgets.bytes_per_token_conservative == 0 {
                return Err(format!(
                    "model '{}': estimation_budgets.bytes_per_token_conservative must be > 0",
                    e.id
                ));
            }
        }
        Ok(())
    }
}

/// The static policy service.
pub struct StaticModelPolicy {
    snapshot: PolicySnapshot,
    generated_at: OffsetDateTime,
    standard: TierLimits,
    premium: TierLimits,
}

impl StaticModelPolicy {
    #[must_use]
    pub fn from_config(cfg: &StaticModelPolicyConfig) -> Self {
        let k = &cfg.kill_switches;
        Self {
            snapshot: PolicySnapshot {
                policy_version: 1,
                model_catalog: cfg.model_catalog.clone().unwrap_or_default(),
                kill_switches: KillSwitches {
                    disable_premium_tier: k.disable_premium_tier,
                    force_standard_tier: k.force_standard_tier,
                    disable_web_search: k.disable_web_search,
                    disable_file_search: k.disable_file_search,
                    disable_images: k.disable_images,
                    disable_code_interpreter: k.disable_code_interpreter,
                },
            },
            generated_at: OffsetDateTime::now_utc(),
            standard: cfg.default_standard_limits,
            premium: cfg.default_premium_limits,
        }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StaticModelPolicy {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError> {
        Ok(PolicyVersionInfo {
            policy_version: self.snapshot.policy_version,
            generated_at: self.generated_at,
        })
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        if policy_version != self.snapshot.policy_version {
            return Err(MiniChatModelPolicyPluginError::VersionNotFound(policy_version));
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
        tracing::info!(
            target: "mini_chat::usage",
            tenant_id = %payload.tenant_id,
            dedupe_key = %payload.dedupe_key,
            billing_outcome = %payload.billing_outcome,
            settlement_method = %payload.settlement_method,
            actual_credits_micro = payload.actual_credits_micro,
            "usage event published (static policy plugin)"
        );
        Ok(())
    }
}

/// Static model policy plugin gear.
#[toolkit::gear(name = "static-mini-chat-model-policy-plugin", deps = [types_registry])]
pub struct StaticModelPolicyPlugin {
    service: OnceLock<Arc<StaticModelPolicy>>,
}

impl Default for StaticModelPolicyPlugin {
    fn default() -> Self {
        Self {
            service: OnceLock::new(),
        }
    }
}

#[async_trait]
impl Gear for StaticModelPolicyPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticModelPolicyConfig = ctx.config_or_default()?;
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("static-mini-chat-model-policy-plugin config invalid: {e}"))?;
        let service = Arc::new(StaticModelPolicy::from_config(&cfg));
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
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;
        let api: Arc<dyn MiniChatModelPolicyPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(ClientScope::gts_id(&instance_id), api);
        tracing::info!(instance_id = %instance_id, models = cfg.model_catalog.map_or(0, |c| c.len()), "static model policy plugin registered");
        Ok(())
    }
}

#[cfg(test)]
#[path = "static_model_policy_tests.rs"]
mod static_model_policy_tests;
