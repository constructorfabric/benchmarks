//! Configuration of the static model-policy plugin (DESIGN B.1 "Bundled
//! plugins"). Unknown top-level keys and unknown `kill_switches` keys are
//! errors; unknown keys inside catalog entries and limits are ignored (those
//! are SDK types).

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, TierLimits};
use serde::{Deserialize, Serialize};

use super::service::StaticModelPolicyError;

const DEFAULT_VENDOR: &str = "constructorfabric";
const DEFAULT_PRIORITY: i16 = 100;
const MAX_CREDIT_MULTIPLIER_MICRO: u64 = 10_000_000_000;

/// Operator-facing mirror of the SDK [`KillSwitches`]: every switch may be
/// omitted (defaults to `false`) and an unknown key is a config error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors the policy wire format"
)]
pub struct StaticKillSwitches {
    /// Premium tier is unavailable.
    pub disable_premium_tier: bool,
    /// Every turn runs on the standard tier.
    pub force_standard_tier: bool,
    /// `web_search` is never sent.
    pub disable_web_search: bool,
    /// `file_search` is never sent.
    pub disable_file_search: bool,
    /// Image inputs are rejected.
    pub disable_images: bool,
    /// `code_interpreter` is never sent.
    pub disable_code_interpreter: bool,
}

impl From<&StaticKillSwitches> for KillSwitches {
    fn from(value: &StaticKillSwitches) -> Self {
        Self {
            disable_premium_tier: value.disable_premium_tier,
            force_standard_tier: value.force_standard_tier,
            disable_web_search: value.disable_web_search,
            disable_file_search: value.disable_file_search,
            disable_images: value.disable_images,
            disable_code_interpreter: value.disable_code_interpreter,
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
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticModelPolicyConfig {
    /// Vendor of the registered GTS instance (must match the gear's `vendor`).
    #[serde(default = "default_vendor")]
    pub vendor: String,
    /// Plugin priority (lower wins).
    #[serde(default = "default_priority")]
    pub priority: i16,
    /// Model catalog (required key when the section is present; `[]` is valid).
    pub model_catalog: Vec<ModelCatalogEntry>,
    /// Static kill switches.
    #[serde(default)]
    pub kill_switches: StaticKillSwitches,
    /// Limits of the `total` bucket, same for every user.
    #[serde(default = "default_standard_limits")]
    pub default_standard_limits: TierLimits,
    /// Limits of the `tier:premium` bucket, same for every user.
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
    /// Both credit multipliers of every entry must be in `1..=10_000_000_000`
    /// and `estimation_budgets.bytes_per_token_conservative` must be non-zero.
    ///
    /// # Errors
    /// [`StaticModelPolicyError::MultiplierOutOfRange`] or
    /// [`StaticModelPolicyError::ZeroBytesPerToken`] for the first offender.
    pub fn validate(&self) -> Result<(), StaticModelPolicyError> {
        for entry in &self.model_catalog {
            for (field, value) in [
                (
                    "input_tokens_credit_multiplier_micro",
                    entry.input_tokens_credit_multiplier_micro,
                ),
                (
                    "output_tokens_credit_multiplier_micro",
                    entry.output_tokens_credit_multiplier_micro,
                ),
            ] {
                if !(1..=MAX_CREDIT_MULTIPLIER_MICRO).contains(&value) {
                    return Err(StaticModelPolicyError::MultiplierOutOfRange {
                        model: entry.id.clone(),
                        field,
                        value,
                    });
                }
            }
            if entry.estimation_budgets.bytes_per_token_conservative == 0 {
                return Err(StaticModelPolicyError::ZeroBytesPerToken {
                    model: entry.id.clone(),
                });
            }
        }
        Ok(())
    }
}
