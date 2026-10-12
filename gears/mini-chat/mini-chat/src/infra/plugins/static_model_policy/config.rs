//! Configuration of the static model-policy plugin (DESIGN B.1).

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, TierLimits};
use serde::Deserialize;
use std::collections::HashSet;

/// Vendor of the registered GTS instance when none is configured.
pub const DEFAULT_VENDOR: &str = "constructorfabric";

/// Allowed range of both credit multipliers of a catalog entry.
const MULTIPLIER_RANGE: std::ops::RangeInclusive<u64> = 1..=10_000_000_000;

/// Operator-facing mirror of the SDK [`KillSwitches`]: every switch may be omitted (`false`) but an
/// unknown key is a configuration error, so a misspelled switch fails plugin init.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per kill switch; mirrors the SDK type"
)]
pub struct KillSwitchesConfig {
    /// Premium-tier models must not be used.
    pub disable_premium_tier: bool,
    /// Every request uses the standard tier.
    pub force_standard_tier: bool,
    /// Requests enabling web search are rejected.
    pub disable_web_search: bool,
    /// `file_search` tool calls are skipped.
    pub disable_file_search: bool,
    /// Image uploads and inputs are rejected.
    pub disable_images: bool,
    /// The `code_interpreter` tool is omitted.
    pub disable_code_interpreter: bool,
}

impl From<KillSwitchesConfig> for KillSwitches {
    fn from(cfg: KillSwitchesConfig) -> Self {
        Self {
            disable_premium_tier: cfg.disable_premium_tier,
            force_standard_tier: cfg.force_standard_tier,
            disable_web_search: cfg.disable_web_search,
            disable_file_search: cfg.disable_file_search,
            disable_images: cfg.disable_images,
            disable_code_interpreter: cfg.disable_code_interpreter,
        }
    }
}

fn default_vendor() -> String {
    DEFAULT_VENDOR.to_owned()
}

fn default_priority() -> i16 {
    100
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

/// Plugin configuration: `gears.static-mini-chat-model-policy-plugin.config`.
///
/// Unknown top-level keys are rejected. `model_catalog` is required whenever the section is
/// present (an empty list is valid); an absent section uses [`Default`], which has an empty
/// catalog. The nested SDK types ignore unknown keys.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticModelPolicyConfig {
    /// Vendor of the registered GTS instance; must match the gear's `vendor` to be selected.
    #[serde(default = "default_vendor")]
    pub vendor: String,
    /// Lower is higher priority.
    #[serde(default = "default_priority")]
    pub priority: i16,
    /// The model catalog.
    pub model_catalog: Vec<ModelCatalogEntry>,
    /// Static kill switches.
    #[serde(default)]
    pub kill_switches: KillSwitchesConfig,
    /// Per-user limits of the `total` bucket, the same for every user.
    #[serde(default = "default_standard_limits")]
    pub default_standard_limits: TierLimits,
    /// Per-user limits of the `tier:premium` bucket, the same for every user.
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
    /// Validates the catalog: unique non-empty ids, credit multipliers in `1..=10_000_000_000` and
    /// a positive `bytes_per_token_conservative` for every entry.
    ///
    /// # Errors
    /// A description of the first violated rule.
    pub fn validate(&self) -> Result<(), String> {
        if self.vendor.trim().is_empty() {
            return Err("vendor must be non-empty".to_owned());
        }
        let mut seen = HashSet::new();
        for entry in &self.model_catalog {
            if entry.id.trim().is_empty() {
                return Err("model_catalog: entry id must be non-empty".to_owned());
            }
            if !seen.insert(entry.id.as_str()) {
                return Err(format!("model_catalog: duplicate model id `{}`", entry.id));
            }
            validate_entry(entry)?;
        }
        Ok(())
    }
}

fn validate_entry(entry: &ModelCatalogEntry) -> Result<(), String> {
    let multipliers = [
        (
            "input_tokens_credit_multiplier_micro",
            entry.input_tokens_credit_multiplier_micro,
        ),
        (
            "output_tokens_credit_multiplier_micro",
            entry.output_tokens_credit_multiplier_micro,
        ),
    ];
    for (name, value) in multipliers {
        if !MULTIPLIER_RANGE.contains(&value) {
            return Err(format!(
                "model_catalog[{}].{name} must be in 1..=10_000_000_000, got {value}",
                entry.id
            ));
        }
    }
    if entry.estimation_budgets.bytes_per_token_conservative == 0 {
        return Err(format!(
            "model_catalog[{}].estimation_budgets.bytes_per_token_conservative must be > 0",
            entry.id
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
