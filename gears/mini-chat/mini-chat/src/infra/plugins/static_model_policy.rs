//! Static model policy plugin (`static-mini-chat-model-policy-plugin`, DESIGN
//! Appendix B.1 "Bundled plugins", section 5.2.5): a gear implementing
//! `MiniChatModelPolicyPluginClientV1` from a configured model catalog, kill
//! switches and per-tier limits.
//!
//! The plugin serves one fixed policy version ([`STATIC_POLICY_VERSION`]):
//! the snapshot is built once from the configuration, the per-user limits are
//! the configured tier defaults for every user, and `publish_usage` only logs.
//!
//! At `init` the gear reads its own config section (an absent section uses
//! the defaults), validates it (an invalid catalog fails plugin init),
//! registers its `PluginV1<MiniChatModelPolicyPluginSpecV1>` instance in
//! types-registry and publishes the service as a scoped client in
//! `ClientHub` under that instance id.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    MiniChatModelPolicyPluginSpecV1, ModelCatalogEntry, PolicySnapshot, PublishError, TierLimits,
    UsageEvent, UserLimits,
};
use serde::Deserialize;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::gts::PluginV1;
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::quota::MAX_CREDIT_MULTIPLIER_MICRO;

/// The only policy version the static plugin serves.
pub const STATIC_POLICY_VERSION: u64 = 1;

/// GTS instance segment of the plugin's registered `PluginV1` instance.
pub const STATIC_MODEL_POLICY_INSTANCE_SEGMENT: &str =
    "cf.core.mini_chat.static_model_policy_plugin.v1";

const CONFIG_PREFIX: &str = "static-mini-chat-model-policy-plugin config";

fn default_vendor() -> String {
    "constructorfabric".to_owned()
}

const fn default_priority() -> i16 {
    100
}

const fn default_standard_limits() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 100_000_000,
        limit_monthly_credits_micro: 1_000_000_000,
    }
}

const fn default_premium_limits() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 50_000_000,
        limit_monthly_credits_micro: 500_000_000,
    }
}

/// Configuration of `static-mini-chat-model-policy-plugin` (DESIGN Appendix
/// B.1 "Bundled plugins").
///
/// Unknown top-level keys are rejected. `model_catalog` is required when the
/// section is present (an empty list is valid); an absent section uses
/// [`Default`], which equals the serde defaults with an empty catalog.
/// The nested SDK types (`ModelCatalogEntry`, `TierLimits`, ...) ignore
/// unknown keys.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticModelPolicyConfig {
    /// Vendor of the registered GTS instance; must match the gear's `vendor`
    /// to be selected.
    #[serde(default = "default_vendor")]
    pub vendor: String,
    /// Priority of the registered GTS instance (lower is higher priority).
    #[serde(default = "default_priority")]
    pub priority: i16,
    /// Model catalog of the served snapshot.
    pub model_catalog: Vec<ModelCatalogEntry>,
    /// Static kill switches of the served snapshot.
    #[serde(default)]
    pub kill_switches: StaticKillSwitches,
    /// Per-user limits of the `total` bucket, same for every user.
    #[serde(default = "default_standard_limits")]
    pub default_standard_limits: TierLimits,
    /// Per-user limits of the `tier:premium` bucket, same for every user.
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
    /// Validates the catalog: both credit multipliers of every entry must be
    /// in `1..=MAX_CREDIT_MULTIPLIER_MICRO` and
    /// `estimation_budgets.bytes_per_token_conservative` must not be zero.
    ///
    /// # Errors
    ///
    /// A message naming the first offending entry and key; the gear fails
    /// plugin init with it.
    pub fn validate(&self) -> Result<(), String> {
        for (index, entry) in self.model_catalog.iter().enumerate() {
            for (key, multiplier) in [
                (
                    "input_tokens_credit_multiplier_micro",
                    entry.input_tokens_credit_multiplier_micro,
                ),
                (
                    "output_tokens_credit_multiplier_micro",
                    entry.output_tokens_credit_multiplier_micro,
                ),
            ] {
                if !(1..=MAX_CREDIT_MULTIPLIER_MICRO).contains(&multiplier) {
                    return Err(format!(
                        "{CONFIG_PREFIX}: model_catalog[{index}] (id `{}`): {key} = {multiplier} \
                         must be in 1..={MAX_CREDIT_MULTIPLIER_MICRO}",
                        entry.id
                    ));
                }
            }
            if entry.estimation_budgets.bytes_per_token_conservative == 0 {
                return Err(format!(
                    "{CONFIG_PREFIX}: model_catalog[{index}] (id `{}`): \
                     estimation_budgets.bytes_per_token_conservative must not be 0",
                    entry.id
                ));
            }
        }
        Ok(())
    }
}

/// Operator-facing kill switches of the static plugin.
///
/// Mirrors the SDK [`KillSwitches`], but every field may be omitted (a
/// missing field is `false`) and unknown keys are rejected, so a misspelled
/// switch fails plugin init.
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors the SDK `KillSwitches`, one independent operational switch per field"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticKillSwitches {
    /// Premium tier is unavailable.
    pub disable_premium_tier: bool,
    /// Every request is served on the standard tier.
    pub force_standard_tier: bool,
    /// Web search tool is disabled.
    pub disable_web_search: bool,
    /// `file_search` tool is disabled.
    pub disable_file_search: bool,
    /// Image inputs are disabled.
    pub disable_images: bool,
    /// Code interpreter tool is disabled.
    pub disable_code_interpreter: bool,
}

impl From<StaticKillSwitches> for KillSwitches {
    fn from(switches: StaticKillSwitches) -> Self {
        Self {
            disable_premium_tier: switches.disable_premium_tier,
            force_standard_tier: switches.force_standard_tier,
            disable_web_search: switches.disable_web_search,
            disable_file_search: switches.disable_file_search,
            disable_images: switches.disable_images,
            disable_code_interpreter: switches.disable_code_interpreter,
        }
    }
}

/// Static `MiniChatModelPolicyPluginClientV1` implementation: one fixed
/// policy version built from the configuration.
#[derive(Debug, Clone)]
pub struct StaticModelPolicyService {
    snapshot: PolicySnapshot,
    standard_limits: TierLimits,
    premium_limits: TierLimits,
}

impl StaticModelPolicyService {
    /// Builds the service from a (validated) configuration.
    #[must_use]
    pub fn new(cfg: StaticModelPolicyConfig) -> Self {
        Self {
            snapshot: PolicySnapshot {
                policy_version: STATIC_POLICY_VERSION,
                model_catalog: cfg.model_catalog,
                kill_switches: cfg.kill_switches.into(),
            },
            standard_limits: cfg.default_standard_limits,
            premium_limits: cfg.default_premium_limits,
        }
    }

    const fn check_version(policy_version: u64) -> Result<(), MiniChatModelPolicyPluginError> {
        if policy_version == STATIC_POLICY_VERSION {
            Ok(())
        } else {
            Err(MiniChatModelPolicyPluginError::VersionNotFound { policy_version })
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
        Self::check_version(policy_version)?;
        Ok(self.snapshot.clone())
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        Self::check_version(policy_version)?;
        Ok(UserLimits {
            user_id,
            policy_version,
            standard: self.standard_limits,
            premium: self.premium_limits,
        })
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        info!(
            target: "mini_chat.static_model_policy",
            tenant_id = %payload.tenant_id,
            request_id = %payload.request_id,
            dedupe_key = %payload.dedupe_key,
            requester_type = %payload.requester_type,
            effective_model = %payload.effective_model,
            billing_outcome = %payload.billing_outcome,
            settlement_method = %payload.settlement_method,
            actual_credits_micro = payload.actual_credits_micro,
            policy_version_applied = payload.policy_version_applied,
            "usage event published"
        );
        Ok(())
    }
}

/// `static-mini-chat-model-policy-plugin` gear.
#[toolkit::gear(
    name = "static-mini-chat-model-policy-plugin",
    deps = [types_registry]
)]
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
        let cfg: StaticModelPolicyConfig = ctx
            .config_or_default()
            .map_err(|err| anyhow::anyhow!("{CONFIG_PREFIX} could not be loaded: {err}"))?;
        cfg.validate().map_err(anyhow::Error::msg)?;
        info!(
            vendor = %cfg.vendor,
            priority = cfg.priority,
            models = cfg.model_catalog.len(),
            "static model policy plugin configuration loaded"
        );

        let (instance_id, instance_json) =
            PluginV1::<MiniChatModelPolicyPluginSpecV1>::build_registration(
                STATIC_MODEL_POLICY_INSTANCE_SEGMENT,
                cfg.vendor.clone(),
                cfg.priority,
            )?;
        let service = Arc::new(StaticModelPolicyService::new(cfg));

        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        self.service
            .set(Arc::clone(&service))
            .map_err(|_| anyhow::anyhow!("{} gear already initialized", Self::MODULE_NAME))?;

        let api: Arc<dyn MiniChatModelPolicyPluginClientV1> = service;
        ctx.client_hub()
            .register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(
                ClientScope::gts_id(&instance_id),
                api,
            );

        info!(instance_id = %instance_id, "static model policy plugin registered");
        Ok(())
    }
}
