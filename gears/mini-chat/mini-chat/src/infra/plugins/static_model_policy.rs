//! Static model policy plugin: serves a fixed policy snapshot (version 1) from its configuration.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    MiniChatModelPolicyPluginSpecV1, ModelCatalogEntry, PolicySnapshot, PolicyVersionInfo,
    PublishError, TierLimits, UsageEvent, UserLimits,
};
use serde::Deserialize;
use time::OffsetDateTime;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::quota_math::MAX_MULT;

/// Fixed policy version served by the static plugin.
pub const STATIC_POLICY_VERSION: u64 = 1;
const INSTANCE_SEGMENT: &str = "cf.builtin.static_mini_chat_model_policy.plugin.v1";

/// Operator-facing mirror of the SDK kill switches (every field optional, unknown keys rejected).
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "operator config mirror of independent kill-switch flags"
)]
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

fn default_vendor() -> String {
    "constructorfabric".to_owned()
}
const fn default_priority() -> i16 {
    100
}
const fn default_standard() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 100_000_000,
        limit_monthly_credits_micro: 1_000_000_000,
    }
}
const fn default_premium() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 50_000_000,
        limit_monthly_credits_micro: 500_000_000,
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticModelPolicyConfig {
    #[serde(default = "default_vendor")]
    pub vendor: String,
    #[serde(default = "default_priority")]
    pub priority: i16,
    /// Required when the config section is present (an empty list is valid).
    pub model_catalog: Vec<ModelCatalogEntry>,
    #[serde(default)]
    pub kill_switches: StaticKillSwitches,
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
            model_catalog: Vec::new(),
            kill_switches: StaticKillSwitches::default(),
            default_standard_limits: default_standard(),
            default_premium_limits: default_premium(),
        }
    }
}

impl StaticModelPolicyConfig {
    /// Validates multipliers and estimation budgets of every catalog entry.
    ///
    /// # Errors
    /// Returns the first invalid entry.
    pub fn validate(&self) -> Result<(), String> {
        let max = u64::try_from(MAX_MULT).unwrap_or(u64::MAX);
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
                if v == 0 || v > max {
                    return Err(format!(
                        "model '{}': {name} must be in 1..=10000000000",
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

/// The plugin implementation (also used directly by tests).
#[derive(Debug, Clone)]
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
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError> {
        Ok(PolicyVersionInfo {
            policy_version: STATIC_POLICY_VERSION,
            generated_at: OffsetDateTime::UNIX_EPOCH,
        })
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        if policy_version != STATIC_POLICY_VERSION {
            return Err(MiniChatModelPolicyPluginError::NotFound(format!(
                "policy version {policy_version} not found"
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
        tracing::info!(
            target: "mini_chat.usage",
            tenant_id = %payload.tenant_id,
            request_id = %payload.request_id,
            billing_outcome = %payload.billing_outcome,
            settlement_method = %payload.settlement_method,
            actual_credits_micro = payload.actual_credits_micro,
            dedupe_key = %payload.dedupe_key,
            "static model policy plugin: usage event"
        );
        Ok(())
    }
}

/// `static-mini-chat-model-policy-plugin` gear.
#[toolkit::gear(name = "static-mini-chat-model-policy-plugin", deps = [types_registry])]
pub struct StaticModelPolicyPlugin {
    service: OnceLock<Arc<StaticModelPolicyService>>,
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
        cfg.validate().map_err(|e| {
            anyhow::anyhow!("static-mini-chat-model-policy-plugin config invalid: {e}")
        })?;
        let (instance_id, instance_json) =
            PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
                INSTANCE_SEGMENT,
                cfg.vendor.clone(),
                cfg.priority,
            )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;
        let svc = Arc::new(StaticModelPolicyService::new(&cfg));
        self.service
            .set(Arc::clone(&svc))
            .map_err(|_| anyhow::anyhow!("{} already initialized", Self::MODULE_NAME))?;
        let api: Arc<dyn MiniChatModelPolicyPluginClientV1> = svc;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                api,
            );
        tracing::info!(
            models = cfg.model_catalog.len(),
            "static mini-chat model policy plugin registered"
        );
        Ok(())
    }
}
