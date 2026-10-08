//! Static model policy plugin configuration
//! (`gears.static-mini-chat-model-policy-plugin.config`, DESIGN Appendix B.1).

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, TierLimits};
use serde::Deserialize;

fn default_vendor() -> String {
    "constructorfabric".to_owned()
}

const fn default_priority() -> i16 {
    100
}

/// Default per-user limits of the `total` bucket.
#[must_use]
pub const fn default_standard_limits() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 100_000_000,
        limit_monthly_credits_micro: 1_000_000_000,
    }
}

/// Default per-user limits of the `tier:premium` bucket.
#[must_use]
pub const fn default_premium_limits() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 50_000_000,
        limit_monthly_credits_micro: 500_000_000,
    }
}

/// Plugin configuration. `model_catalog` is required when the section is
/// present; an absent section uses [`Default`] (empty catalog).
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

/// Operator-facing mirror of [`KillSwitches`]: missing fields are `false`,
/// unknown keys are a config error.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
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
