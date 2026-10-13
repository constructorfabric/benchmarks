//! Config of `static-mini-chat-model-policy-plugin` (DESIGN Appendix B.1).

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, TierLimits};
use serde::Deserialize;

const DEFAULT_VENDOR: &str = "constructorfabric";
const DEFAULT_PRIORITY: i16 = 100;
const MULTIPLIER_RANGE: std::ops::RangeInclusive<i64> = 1..=10_000_000_000;

/// Operator-facing mirror of the SDK [`KillSwitches`]. Unlike the SDK type it
/// rejects unknown keys and defaults every switch to `false`, so a misspelled
/// switch fails plugin init instead of silently reading as `false`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
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

fn default_vendor() -> String {
    DEFAULT_VENDOR.to_owned()
}

fn default_priority() -> i16 {
    DEFAULT_PRIORITY
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

/// `gears.static-mini-chat-model-policy-plugin.config`.
///
/// `model_catalog` has no serde default: it is required when the section is
/// present. An absent section yields [`Default`] (empty catalog) through
/// `config_or_default`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticModelPolicyPluginConfig {
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

impl Default for StaticModelPolicyPluginConfig {
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

impl StaticModelPolicyPluginConfig {
    /// Both credit multipliers of every entry must be in `1..=10_000_000_000`
    /// and `bytes_per_token_conservative` must be positive.
    ///
    /// # Errors
    /// Describes the first offending entry.
    pub fn validate(&self) -> Result<(), String> {
        for m in &self.model_catalog {
            for (name, value) in [
                (
                    "input_tokens_credit_multiplier_micro",
                    m.input_tokens_credit_multiplier_micro,
                ),
                (
                    "output_tokens_credit_multiplier_micro",
                    m.output_tokens_credit_multiplier_micro,
                ),
            ] {
                if !MULTIPLIER_RANGE.contains(&value) {
                    return Err(format!(
                        "model '{}': {name} must be in 1..=10000000000, got {value}",
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
