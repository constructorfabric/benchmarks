//! Bundled static model policy plugin (`static-mini-chat-model-policy-plugin`).
//!
//! Serves a fixed policy snapshot (version 1) from its own configuration and
//! logs usage events received via `publish_usage`.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::models::{KillSwitches, PolicySnapshot, TierLimits, UserLimits};
use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, ModelPolicyError,
    PublishError, UsageEvent,
};
use serde::Deserialize;
use toolkit::{Gear, GearCtx};
use toolkit::client_hub::ClientScope;
use toolkit::gts::PluginV1;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};

#[allow(clippy::struct_excessive_bools, reason = "the kill-switch config mirrors the SDK wire type")]
/// Operator-facing kill-switch mirror: every field optional (missing =
/// `false`), unknown keys rejected.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KillSwitchesConfig {
    /// Disable the premium tier.
    pub disable_premium_tier: bool,
    /// Force the standard tier.
    pub force_standard_tier: bool,
    /// Disable web search.
    pub disable_web_search: bool,
    /// Disable file search.
    pub disable_file_search: bool,
    /// Disable images.
    pub disable_images: bool,
    /// Disable code interpreter.
    pub disable_code_interpreter: bool,
}

impl From<KillSwitchesConfig> for KillSwitches {
    fn from(c: KillSwitchesConfig) -> Self {
        Self {
            disable_premium_tier: c.disable_premium_tier,
            force_standard_tier: c.force_standard_tier,
            disable_web_search: c.disable_web_search,
            disable_file_search: c.disable_file_search,
            disable_images: c.disable_images,
            disable_code_interpreter: c.disable_code_interpreter,
        }
    }
}

/// Static model policy plugin configuration
/// (`gears.static-mini-chat-model-policy-plugin.config`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticModelPolicyConfig {
    /// Vendor of the registered GTS instance; must match the gear's vendor.
    pub vendor: String,
    /// Instance priority (lower = higher priority).
    pub priority: i16,
    /// The model catalog (required when the section is present; may be
    /// empty).
    pub model_catalog: Option<Vec<mini_chat_sdk::ModelCatalogEntry>>,
    /// Static kill switches.
    pub kill_switches: KillSwitchesConfig,
    /// Per-user limits for the `total` bucket.
    pub default_standard_limits: TierLimits,
    /// Per-user limits for the `tier:premium` bucket.
    pub default_premium_limits: TierLimits,
}

impl Default for StaticModelPolicyConfig {
    fn default() -> Self {
        Self {
            vendor: "constructorfabric".to_owned(),
            priority: 100,
            model_catalog: None,
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
    /// Validate the catalog (multipliers in range, non-zero
    /// `bytes_per_token_conservative`).
    ///
    /// # Errors
    ///
    /// Returns a descriptive error when an entry is invalid.
    pub fn validate(&self) -> Result<(), String> {
        if self.vendor.is_empty() {
            return Err("vendor must be non-empty".to_owned());
        }
        if let Some(catalog) = &self.model_catalog {
            for entry in catalog {
                for (name, mult) in [
                    (
                        "input_tokens_credit_multiplier_micro",
                        entry.input_tokens_credit_multiplier_micro,
                    ),
                    (
                        "output_tokens_credit_multiplier_micro",
                        entry.output_tokens_credit_multiplier_micro,
                    ),
                ] {
                    if !(1..=10_000_000_000).contains(&mult) {
                        return Err(format!(
                            "model_catalog entry '{}': {name} must be in 1..=10_000_000_000",
                            entry.id
                        ));
                    }
                }
                if entry.estimation_budgets.bytes_per_token_conservative == 0 {
                    return Err(format!(
                        "model_catalog entry '{}': estimation_budgets.bytes_per_token_conservative must be > 0",
                        entry.id
                    ));
                }
            }
        }
        Ok(())
    }
}

/// The static policy service.
pub struct StaticPolicyService {
    snapshot: PolicySnapshot,
    standard: TierLimits,
    premium: TierLimits,
}

impl StaticPolicyService {
    /// Build from config.
    #[must_use]
    pub fn from_config(cfg: &StaticModelPolicyConfig) -> Self {
        Self {
            snapshot: PolicySnapshot {
                policy_version: 1,
                model_catalog: cfg.model_catalog.clone().unwrap_or_default(),
                kill_switches: cfg.kill_switches.into(),
            },
            standard: cfg.default_standard_limits,
            premium: cfg.default_premium_limits,
        }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StaticPolicyService {
    async fn get_current_policy_version(&self, _user_id: uuid::Uuid) -> Result<i64, ModelPolicyError> {
        Ok(1)
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: uuid::Uuid,
        policy_version: i64,
    ) -> Result<PolicySnapshot, ModelPolicyError> {
        if policy_version != 1 {
            return Err(ModelPolicyError::NotFound(format!(
                "unknown policy version {policy_version}"
            )));
        }
        Ok(self.snapshot.clone())
    }

    async fn get_user_limits(
        &self,
        user_id: uuid::Uuid,
        policy_version: i64,
    ) -> Result<UserLimits, ModelPolicyError> {
        if policy_version != 1 {
            return Err(ModelPolicyError::NotFound(format!(
                "unknown policy version {policy_version}"
            )));
        }
        Ok(UserLimits {
            user_id,
            policy_version,
            standard: self.standard,
            premium: self.premium,
        })
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), PublishError> {
        tracing::info!(
            tenant_id = %event.tenant_id,
            chat_id = %event.chat_id,
            request_id = %event.request_id,
            billing_outcome = %event.billing_outcome,
            settlement_method = %event.settlement_method,
            actual_credits_micro = event.actual_credits_micro,
            "mini-chat usage event"
        );
        Ok(())
    }
}

/// The static model policy plugin gear.
#[toolkit::gear(name = "static-mini-chat-model-policy-plugin", deps = [types_registry])]
pub struct StaticModelPolicyPlugin {
    service: OnceLock<Arc<StaticPolicyService>>,
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

        let (instance_id, instance_json) =
            PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
                "cf.builtin.static_mini_chat_model_policy.plugin.v1",
                cfg.vendor.clone(),
                cfg.priority,
            )?;

        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        let service = Arc::new(StaticPolicyService::from_config(&cfg));
        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("static model policy plugin already initialized"))?;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
                ClientScope::gts_id(instance_id.as_ref()),
                service,
            );
        Ok(())
    }
}
