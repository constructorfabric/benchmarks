use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, TierLimits};
use serde::Deserialize;

/// Configuration of the static model policy plugin.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaticModelPolicyConfig {
    pub vendor: String,
    pub priority: i16,
    pub model_catalog: Vec<ModelCatalogEntry>,
    pub kill_switches: StaticKillSwitches,
    pub default_standard_limits: TierLimits,
    pub default_premium_limits: TierLimits,
}

impl Default for StaticModelPolicyConfig {
    fn default() -> Self {
        Self {
            vendor: "constructorfabric".to_owned(),
            priority: 100,
            model_catalog: Vec::new(),
            kill_switches: StaticKillSwitches::default(),
            default_standard_limits: TierLimits {
                limit_daily_credits_micro: 100_000_000,
                limit_monthly_credits_micro: 1_000_000_000,
            },
            default_premium_limits: TierLimits {
                limit_daily_credits_micro: 50_000_000,
                limit_monthly_credits_micro: 500_000_000,
            },
        }
    }
}

/// Operator-facing kill switches: every field optional, unknown keys rejected.
#[allow(clippy::struct_excessive_bools)] // config wire format: one flag per switch
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
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

impl StaticModelPolicyConfig {
    /// Validates the catalog: multipliers in `1..=10_000_000_000` and a positive
    /// `bytes_per_token_conservative` on every entry.
    ///
    /// # Errors
    /// A message naming the offending entry.
    pub fn validate(&self) -> Result<(), String> {
        const MAX_MULT: i64 = 10_000_000_000;
        for m in &self.model_catalog {
            for (name, v) in [
                (
                    "input_tokens_credit_multiplier_micro",
                    m.input_tokens_credit_multiplier_micro,
                ),
                (
                    "output_tokens_credit_multiplier_micro",
                    m.output_tokens_credit_multiplier_micro,
                ),
            ] {
                if !(1..=MAX_MULT).contains(&v) {
                    return Err(format!(
                        "model_catalog[{}].{name} must be in 1..=10000000000, got {v}",
                        m.id
                    ));
                }
            }
            if m.estimation_budgets.bytes_per_token_conservative == 0 {
                return Err(format!(
                    "model_catalog[{}].estimation_budgets.bytes_per_token_conservative must be > 0",
                    m.id
                ));
            }
        }
        Ok(())
    }
}
