//! Configuration of the static model policy plugin
//! (`gears.static-mini-chat-model-policy-plugin.config`, DESIGN Appendix B.1 "Bundled plugins").

use anyhow::bail;
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, TierLimits};
use serde::Deserialize;

use crate::domain::credits::MAX_MULTIPLIER;

const DEFAULT_VENDOR: &str = "constructorfabric";
const DEFAULT_PRIORITY: i16 = 100;

fn default_vendor() -> String {
    DEFAULT_VENDOR.to_owned()
}

const fn default_priority() -> i16 {
    DEFAULT_PRIORITY
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

/// Operator-facing mirror of [`KillSwitches`]: every field is optional
/// (default `false`) and unknown keys are rejected, so a misspelled switch
/// fails plugin init instead of being ignored.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KillSwitchesConfig {
    pub disable_premium_tier: bool,
    pub force_standard_tier: bool,
    pub disable_web_search: bool,
    pub disable_file_search: bool,
    pub disable_images: bool,
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

/// Static model policy plugin configuration.
///
/// `model_catalog` is a required key whenever the section is present (an
/// empty list is valid); an absent section uses [`Default`].
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct StaticModelPolicyConfig {
    /// Vendor of the registered GTS instance (must match the gear's `vendor`).
    #[serde(default = "default_vendor")]
    pub vendor: String,
    /// Plugin priority (lower = higher priority).
    #[serde(default = "default_priority")]
    pub priority: i16,
    /// Model catalog served in the version-1 snapshot, order preserved.
    pub model_catalog: Vec<ModelCatalogEntry>,
    /// Static kill switches.
    #[serde(default)]
    pub kill_switches: KillSwitchesConfig,
    /// Per-user limits of the `total` bucket.
    #[serde(default = "default_standard_limits")]
    pub default_standard_limits: TierLimits,
    /// Per-user limits of the `tier:premium` bucket.
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
    /// Plugin-init validation (DESIGN §5.3 "Overflow Protection"): both credit
    /// multipliers of every entry in `1..=10_000_000_000` and
    /// `estimation_budgets.bytes_per_token_conservative > 0`.
    ///
    /// # Errors
    ///
    /// Returns an error naming the offending model and field.
    pub fn validate(&self) -> anyhow::Result<()> {
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
                if !(1..=MAX_MULTIPLIER).contains(&value) {
                    bail!(
                        "model `{}`: credit multiplier `{name}` must be in 1..={MAX_MULTIPLIER}, got {value}",
                        m.id
                    );
                }
            }
            if m.estimation_budgets.bytes_per_token_conservative == 0 {
                bail!(
                    "model `{}`: estimation_budgets.bytes_per_token_conservative must be greater than 0",
                    m.id
                );
            }
        }
        Ok(())
    }
}
