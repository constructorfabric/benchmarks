//! Bundled static model policy plugin (`static-mini-chat-model-policy-plugin`).
//!
//! Serves a fixed policy snapshot (version 1) from its own configuration and logs
//! published usage events.

use std::sync::Arc;

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

/// Fixed policy version served by the static plugin.
pub const STATIC_POLICY_VERSION: u64 = 1;

/// Operator-facing kill switches (unknown keys rejected, missing ones `false`).
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "operator config section whose keys mirror the documented `KillSwitches` contract"
)]
pub struct StaticKillSwitches {
    /// See [`KillSwitches::disable_premium_tier`].
    pub disable_premium_tier: bool,
    /// See [`KillSwitches::force_standard_tier`].
    pub force_standard_tier: bool,
    /// See [`KillSwitches::disable_web_search`].
    pub disable_web_search: bool,
    /// See [`KillSwitches::disable_file_search`].
    pub disable_file_search: bool,
    /// See [`KillSwitches::disable_images`].
    pub disable_images: bool,
    /// See [`KillSwitches::disable_code_interpreter`].
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
    /// Vendor of the registered instance.
    #[serde(default = "default_vendor")]
    pub vendor: String,
    /// Priority (lower wins).
    #[serde(default = "default_priority")]
    pub priority: i16,
    /// Model catalog.
    pub model_catalog: Vec<ModelCatalogEntry>,
    /// Kill switches.
    #[serde(default)]
    pub kill_switches: StaticKillSwitches,
    /// `total` bucket limits.
    #[serde(default = "default_standard_limits")]
    pub default_standard_limits: TierLimits,
    /// `tier:premium` bucket limits.
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
    /// Validates multipliers and estimation budgets.
    ///
    /// # Errors
    /// Descriptive message for the first invalid entry.
    pub fn validate(&self) -> Result<(), String> {
        const MAX_MULT: u64 = 10_000_000_000;
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
                return Err(format!(
                    "model '{}': estimation_budgets.bytes_per_token_conservative must be > 0",
                    m.id
                ));
            }
        }
        Ok(())
    }
}

/// Plugin client.
pub struct StaticModelPolicyClient {
    snapshot: PolicySnapshot,
    standard: TierLimits,
    premium: TierLimits,
    generated_at: OffsetDateTime,
}

impl StaticModelPolicyClient {
    /// Builds the client from a validated config.
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
            generated_at: OffsetDateTime::now_utc(),
        }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StaticModelPolicyClient {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError> {
        Ok(PolicyVersionInfo { policy_version: STATIC_POLICY_VERSION, generated_at: self.generated_at })
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        if policy_version != STATIC_POLICY_VERSION {
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
        Ok(UserLimits { user_id, policy_version, standard: self.standard, premium: self.premium })
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        tracing::info!(
            target: "mini_chat::usage",
            tenant_id = %payload.tenant_id,
            chat_id = %payload.chat_id,
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

/// The bundled static model policy plugin gear.
#[toolkit::gear(name = "static-mini-chat-model-policy-plugin", deps = [types_registry])]
#[derive(Default)]
pub struct StaticModelPolicyPlugin;

#[async_trait]
impl Gear for StaticModelPolicyPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: StaticModelPolicyConfig = ctx.config_or_default()?;
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("static-mini-chat-model-policy-plugin config invalid: {e}"))?;
        let (instance_id, json) = PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
            "cf.builtin.static_mini_chat_model_policy.plugin.v1",
            cfg.vendor.clone(),
            cfg.priority,
        )?;
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        RegisterResult::ensure_all_ok(&registry.register(vec![json]).await?)?;
        let api: Arc<dyn MiniChatModelPolicyPluginClientV1> =
            Arc::new(StaticModelPolicyClient::new(&cfg));
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(ClientScope::gts_id(&instance_id), api);
        tracing::info!(instance_id = %instance_id, models = cfg.model_catalog.len(), "static model policy plugin registered");
        Ok(())
    }
}
