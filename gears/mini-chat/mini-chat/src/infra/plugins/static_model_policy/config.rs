//! Configuration of the static model policy plugin.

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, TierLimits};
use serde::Deserialize;

/// Operator-facing mirror of [`KillSwitches`]: every field optional,
/// unknown keys rejected.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools, reason = "mirror of the SDK kill switches")]
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

const fn default_standard_limits() -> TierLimits {
    TierLimits { limit_daily_credits_micro: 100_000_000, limit_monthly_credits_micro: 1_000_000_000 }
}

const fn default_premium_limits() -> TierLimits {
    TierLimits { limit_daily_credits_micro: 50_000_000, limit_monthly_credits_micro: 500_000_000 }
}

/// Plugin configuration. `model_catalog` is required when the section is
/// present; an absent section uses the defaults (empty catalog).
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
    /// Validate the catalog (credit multipliers, estimation budgets).
    ///
    /// # Errors
    /// A descriptive message for the first invalid entry.
    pub fn validate(&self) -> Result<(), String> {
        const MAX_MULT: i64 = 10_000_000_000;
        for e in &self.model_catalog {
            for (name, m) in [
                ("input_tokens_credit_multiplier_micro", e.input_tokens_credit_multiplier_micro),
                ("output_tokens_credit_multiplier_micro", e.output_tokens_credit_multiplier_micro),
            ] {
                if !(1..=MAX_MULT).contains(&m) {
                    return Err(format!(
                        "model_catalog entry `{}`: {name} must be in 1..=10000000000",
                        e.id
                    ));
                }
            }
            if e.estimation_budgets.bytes_per_token_conservative == 0 {
                return Err(format!(
                    "model_catalog entry `{}`: estimation_budgets.bytes_per_token_conservative must be > 0",
                    e.id
                ));
            }
        }
        Ok(())
    }
}
