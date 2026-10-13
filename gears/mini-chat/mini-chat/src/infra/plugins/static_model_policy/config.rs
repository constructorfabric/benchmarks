//! Configuration of the static model policy plugin.

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, TierLimits, decode_model_catalog_entry};
use serde::Deserialize;

/// Configuration error of the static model policy plugin.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StaticModelPolicyConfigError {
    /// The section could not be parsed (unknown key, missing `model_catalog`, wrong type).
    #[error("invalid static-mini-chat-model-policy-plugin configuration: {0}")]
    Parse(String),
    /// A catalog entry violates a validation rule.
    #[error("invalid catalog entry `{model_id}`: field `{field}` {rule}")]
    InvalidEntry {
        /// Model id of the entry.
        model_id: String,
        /// Offending field.
        field: String,
        /// Violated rule.
        rule: String,
    },
}

/// Parsed `static-mini-chat-model-policy-plugin` section.
#[derive(Debug, Clone, PartialEq)]
pub struct StaticModelPolicyConfig {
    /// Vendor of the registered GTS instance.
    pub vendor: String,
    /// Priority of the registered GTS instance; lower wins.
    pub priority: i16,
    /// Served model catalog.
    pub model_catalog: Vec<ModelCatalogEntry>,
    /// Served kill switches.
    pub kill_switches: KillSwitches,
    /// Limits of the `total` bucket for every user.
    pub default_standard_limits: TierLimits,
    /// Limits of the `tier:premium` bucket for every user.
    pub default_premium_limits: TierLimits,
}

/// Highest credit multiplier a catalog entry may declare.
const MAX_CREDIT_MULTIPLIER_MICRO: u64 = 10_000_000_000;

/// The section as written by the operator: strict on unknown keys, with the
/// catalog required.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default = "default_vendor")]
    vendor: String,
    #[serde(default = "default_priority")]
    priority: i16,
    model_catalog: Vec<serde_json::Value>,
    #[serde(default)]
    kill_switches: RawKillSwitches,
    #[serde(default = "default_standard_limits")]
    default_standard_limits: TierLimits,
    #[serde(default = "default_premium_limits")]
    default_premium_limits: TierLimits,
}

/// Kill switches as written by the operator: every switch is optional and
/// defaults to `false`, unknown keys are rejected (the SDK type requires all six
/// and ignores unknown keys).
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
struct RawKillSwitches {
    disable_premium_tier: bool,
    force_standard_tier: bool,
    disable_web_search: bool,
    disable_file_search: bool,
    disable_images: bool,
    disable_code_interpreter: bool,
}

impl From<RawKillSwitches> for KillSwitches {
    fn from(raw: RawKillSwitches) -> Self {
        Self {
            disable_premium_tier: raw.disable_premium_tier,
            force_standard_tier: raw.force_standard_tier,
            disable_web_search: raw.disable_web_search,
            disable_file_search: raw.disable_file_search,
            disable_images: raw.disable_images,
            disable_code_interpreter: raw.disable_code_interpreter,
        }
    }
}

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-plugin-defaults
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

impl Default for StaticModelPolicyConfig {
    /// The configuration of an absent section: all defaults, empty catalog.
    fn default() -> Self {
        Self {
            vendor: default_vendor(),
            priority: default_priority(),
            model_catalog: Vec::new(),
            kill_switches: RawKillSwitches::default().into(),
            default_standard_limits: default_standard_limits(),
            default_premium_limits: default_premium_limits(),
        }
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-plugin-defaults

impl StaticModelPolicyConfig {
    /// Parses and validates the raw section; `None` (absent section) yields the
    /// defaults with an empty catalog.
    ///
    /// # Errors
    ///
    /// Returns [`StaticModelPolicyConfigError`] for unknown keys (also inside
    /// `kill_switches`), a missing `model_catalog` in a present section, a
    /// credit multiplier outside `1..=10_000_000_000` or a zero
    /// `bytes_per_token_conservative`.
    pub fn load(raw: Option<&serde_json::Value>) -> Result<Self, StaticModelPolicyConfigError> {
        let Some(raw) = raw else {
            return Ok(Self::default());
        };

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-parse-plugin-config
        let section: RawConfig = serde_json::from_value(raw.clone())
            .map_err(|error| StaticModelPolicyConfigError::Parse(error.to_string()))?;
        let model_catalog = section
            .model_catalog
            .iter()
            .map(|entry| {
                decode_model_catalog_entry(entry)
                    .map_err(|error| StaticModelPolicyConfigError::Parse(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-parse-plugin-config

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-for-each-catalog-entry
        for entry in &model_catalog {
            validate_entry(entry)?;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-for-each-catalog-entry

        Ok(Self {
            vendor: section.vendor,
            priority: section.priority,
            model_catalog,
            kill_switches: section.kill_switches.into(),
            default_standard_limits: section.default_standard_limits,
            default_premium_limits: section.default_premium_limits,
        })
    }
}

/// Rejects an entry with a credit multiplier outside the allowed range or a
/// zero `bytes_per_token_conservative`.
fn validate_entry(entry: &ModelCatalogEntry) -> Result<(), StaticModelPolicyConfigError> {
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
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-if-bad-entry
    for (field, value) in multipliers {
        if !(1..=MAX_CREDIT_MULTIPLIER_MICRO).contains(&value) {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-return-plugin-error
            return Err(StaticModelPolicyConfigError::InvalidEntry {
                model_id: entry.id.clone(),
                field: field.to_owned(),
                rule: format!("must be within 1..={MAX_CREDIT_MULTIPLIER_MICRO}"),
            });
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-return-plugin-error
        }
    }
    if entry.estimation_budgets.bytes_per_token_conservative == 0 {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-return-plugin-error
        return Err(StaticModelPolicyConfigError::InvalidEntry {
            model_id: entry.id.clone(),
            field: "estimation_budgets.bytes_per_token_conservative".to_owned(),
            rule: "must be greater than 0".to_owned(),
        });
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-return-plugin-error
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin:p1:inst-if-bad-entry
    Ok(())
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
