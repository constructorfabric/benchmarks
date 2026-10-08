//! Configuration of the static model policy plugin.

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, TierLimits};
use serde::Deserialize;

/// Bounds of a credit multiplier (micro-credits per 1M tokens).
pub const MULTIPLIER_MIN: u64 = 1;
pub const MULTIPLIER_MAX: u64 = 10_000_000_000;

pub const DEFAULT_STANDARD_LIMITS: TierLimits = TierLimits {
    limit_daily_credits_micro: 100_000_000,
    limit_monthly_credits_micro: 1_000_000_000,
};
pub const DEFAULT_PREMIUM_LIMITS: TierLimits = TierLimits {
    limit_daily_credits_micro: 50_000_000,
    limit_monthly_credits_micro: 500_000_000,
};

fn default_vendor() -> String {
    "constructorfabric".to_owned()
}

const fn default_priority() -> i16 {
    100
}

const fn default_standard_limits() -> TierLimits {
    DEFAULT_STANDARD_LIMITS
}

const fn default_premium_limits() -> TierLimits {
    DEFAULT_PREMIUM_LIMITS
}

/// Operator-facing kill switches: every field optional, unknown keys rejected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // one flag per kill switch
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
            default_standard_limits: DEFAULT_STANDARD_LIMITS,
            default_premium_limits: DEFAULT_PREMIUM_LIMITS,
        }
    }
}

impl StaticModelPolicyConfig {
    /// Validate credit multipliers and estimation budgets of every entry.
    ///
    /// # Errors
    /// A multiplier outside `1..=10^10` or a zero `bytes_per_token_conservative`.
    pub fn validate(&self) -> Result<(), String> {
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
                if !(MULTIPLIER_MIN..=MULTIPLIER_MAX).contains(&v) {
                    return Err(format!(
                        "model_catalog['{}'].{name} must be in {MULTIPLIER_MIN}..={MULTIPLIER_MAX}, got {v}",
                        m.id
                    ));
                }
            }
            if m.estimation_budgets.bytes_per_token_conservative == 0 {
                return Err(format!(
                    "model_catalog['{}'].estimation_budgets.bytes_per_token_conservative must be > 0",
                    m.id
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(input: u64) -> serde_json::Value {
        serde_json::json!({
            "id": "m1",
            "provider_model_id": "gpt-x",
            "display_name": "M1",
            "provider_id": "openai",
            "provider_display_name": "OpenAI",
            "tier": "standard",
            "enabled": true,
            "context_window": 128_000,
            "max_output_tokens": 4096,
            "max_input_tokens": 100_000,
            "input_tokens_credit_multiplier_micro": input,
            "output_tokens_credit_multiplier_micro": 2,
            "max_num_results": 5,
            "general_config": {}
        })
    }

    #[test]
    fn absent_section_defaults() {
        let c = StaticModelPolicyConfig::default();
        assert!(c.model_catalog.is_empty());
        assert_eq!(c.default_standard_limits, DEFAULT_STANDARD_LIMITS);
        assert_eq!(
            c.default_premium_limits.limit_daily_credits_micro,
            50_000_000
        );
        assert_eq!(c.priority, 100);
    }

    #[test]
    fn model_catalog_required_when_present() {
        let r: Result<StaticModelPolicyConfig, _> =
            serde_json::from_value(serde_json::json!({"vendor": "x"}));
        assert!(r.is_err());
        let ok: StaticModelPolicyConfig =
            serde_json::from_value(serde_json::json!({"model_catalog": []})).unwrap();
        assert!(ok.model_catalog.is_empty());
    }

    #[test]
    fn unknown_keys_rejected() {
        let r: Result<StaticModelPolicyConfig, _> =
            serde_json::from_value(serde_json::json!({"model_catalog": [], "bogus": 1}));
        assert!(r.is_err());
        let r: Result<StaticModelPolicyConfig, _> = serde_json::from_value(
            serde_json::json!({"model_catalog": [], "kill_switches": {"disable_websearch": true}}),
        );
        assert!(r.is_err());
        let ok: StaticModelPolicyConfig = serde_json::from_value(
            serde_json::json!({"model_catalog": [], "kill_switches": {"disable_images": true}}),
        )
        .unwrap();
        assert!(ok.kill_switches.disable_images && !ok.kill_switches.disable_web_search);
    }

    #[test]
    fn multiplier_bounds() {
        let c: StaticModelPolicyConfig =
            serde_json::from_value(serde_json::json!({"model_catalog": [entry(0)]})).unwrap();
        assert!(c.validate().is_err());
        let c: StaticModelPolicyConfig =
            serde_json::from_value(serde_json::json!({"model_catalog": [entry(10_000_000_001)]}))
                .unwrap();
        assert!(c.validate().is_err());
        let c: StaticModelPolicyConfig =
            serde_json::from_value(serde_json::json!({"model_catalog": [entry(10_000_000_000)]}))
                .unwrap();
        assert!(c.validate().is_ok());
    }
}
