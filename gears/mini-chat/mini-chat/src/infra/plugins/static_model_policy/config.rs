//! Plugin configuration (DESIGN Appendix B.1, `static-mini-chat-model-policy-plugin`).
//!
//! SCAFFOLD: serde attributes, defaults and validation are implemented by the author stage.

use mini_chat_sdk::{ModelCatalogEntry, TierLimits};
use serde::Deserialize;

use crate::config::ConfigError;

/// Default vendor of the registered plugin instance.
const DEFAULT_VENDOR: &str = "constructorfabric";
/// Default priority of the registered plugin instance (lower is higher priority).
const DEFAULT_PRIORITY: i16 = 100;
/// Lowest accepted credit multiplier (micro-credits per token unit).
const MIN_MULTIPLIER: u64 = 1;
/// Highest accepted credit multiplier (micro-credits per token unit).
const MAX_MULTIPLIER: u64 = 10_000_000_000;

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

/// The operator-facing mirror of the SDK `KillSwitches`: every field may be omitted (read as
/// `false`) and an unknown key is a config error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the six kill switches are the contract"
)]
pub struct StaticKillSwitchesConfig {
    pub disable_premium_tier: bool,
    pub force_standard_tier: bool,
    pub disable_web_search: bool,
    pub disable_file_search: bool,
    pub disable_images: bool,
    pub disable_code_interpreter: bool,
}

/// The plugin config section. The nested SDK types (`ModelCatalogEntry`, `TierLimits`) ignore
/// unknown keys; the section itself and `kill_switches` reject them.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticModelPolicyConfig {
    #[serde(default = "default_vendor")]
    pub vendor: String,
    #[serde(default = "default_priority")]
    pub priority: i16,
    /// Required when the section is present; an empty list is valid.
    pub model_catalog: Vec<ModelCatalogEntry>,
    #[serde(default)]
    pub kill_switches: StaticKillSwitchesConfig,
    #[serde(default = "default_standard_limits")]
    pub default_standard_limits: TierLimits,
    #[serde(default = "default_premium_limits")]
    pub default_premium_limits: TierLimits,
}

impl Default for StaticModelPolicyConfig {
    /// The defaults of an absent section: vendor `constructorfabric`, priority `100`, empty
    /// catalog, all kill switches `false`, standard limits daily `100_000_000` and monthly
    /// `1_000_000_000`, premium limits daily `50_000_000` and monthly `500_000_000`.
    fn default() -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-defaults
        Self {
            vendor: default_vendor(),
            priority: default_priority(),
            model_catalog: Vec::new(),
            kill_switches: StaticKillSwitchesConfig::default(),
            default_standard_limits: default_standard_limits(),
            default_premium_limits: default_premium_limits(),
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-defaults
    }
}

impl StaticModelPolicyConfig {
    /// Parse a present config section. Rejects unknown top-level keys and unknown keys of
    /// `kill_switches`; `model_catalog` is a required key (an empty list is valid); ignores
    /// unknown keys inside the nested SDK types.
    ///
    /// # Errors
    ///
    /// A [`ConfigError`] naming the offending key.
    pub fn parse(raw: serde_json::Value) -> Result<Self, ConfigError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-load
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-kill-switches
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-nested-types
        serde_json::from_value(raw).map_err(|e| ConfigError::Parse(format!("{e}")))
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-nested-types
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-kill-switches
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-load
    }

    /// Validate that both credit multipliers of every catalog entry are within
    /// `1..=10_000_000_000`.
    ///
    /// # Errors
    ///
    /// A [`ConfigError`] naming the entry and the multiplier.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-multipliers
        for entry in &self.model_catalog {
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
            for (field, value) in multipliers {
                if !(MIN_MULTIPLIER..=MAX_MULTIPLIER).contains(&value) {
                    return Err(ConfigError::Invalid {
                        key: format!("model_catalog[{}].{field}", entry.id),
                        reason: format!(
                            "must be within {MIN_MULTIPLIER}..={MAX_MULTIPLIER}, got {value}"
                        ),
                    });
                }
            }
        }
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-static-policy-multipliers
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
