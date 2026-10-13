//! Configuration of the static model policy plugin.

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, TierLimits};
use serde::Deserialize;

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

/// Operator-facing kill switches. Mirrors [`KillSwitches`], but every field may be omitted
/// (defaults to `false`) and unknown keys are rejected, so a misspelled switch fails init.
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
    fn from(s: StaticKillSwitches) -> Self {
        Self {
            disable_premium_tier: s.disable_premium_tier,
            force_standard_tier: s.force_standard_tier,
            disable_web_search: s.disable_web_search,
            disable_file_search: s.disable_file_search,
            disable_images: s.disable_images,
            disable_code_interpreter: s.disable_code_interpreter,
        }
    }
}

/// `gears.static-mini-chat-model-policy-plugin.config`.
///
/// `model_catalog` has no serde default: when the section is present the key is required. An
/// absent section is handled by `config_or_default` and yields an empty catalog.
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

#[cfg(test)]
mod tests {
    use mini_chat_sdk::TierLimits;
    use serde_json::json;

    use super::*;
    use crate::test_support::fixtures::catalog_entry_json;

    #[test]
    fn catalog_required_when_section_present() {
        // The section is present but `model_catalog` is missing.
        let err = serde_json::from_value::<StaticModelPolicyConfig>(json!({"vendor": "x"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("model_catalog"), "{err}");

        // An explicit empty list is valid.
        let cfg: StaticModelPolicyConfig =
            serde_json::from_value(json!({"model_catalog": []})).unwrap();
        assert!(cfg.model_catalog.is_empty());
        assert_eq!(cfg.vendor, "constructorfabric");
        assert_eq!(cfg.priority, 100);

        // An absent section goes through `config_or_default`: empty catalog and default limits.
        let default = StaticModelPolicyConfig::default();
        assert!(default.model_catalog.is_empty());
        assert_eq!(default.vendor, "constructorfabric");
        assert_eq!(default.priority, 100);
        assert_eq!(
            default.default_standard_limits,
            TierLimits {
                limit_daily_credits_micro: 100_000_000,
                limit_monthly_credits_micro: 1_000_000_000,
            }
        );
        assert_eq!(
            default.default_premium_limits,
            TierLimits {
                limit_daily_credits_micro: 50_000_000,
                limit_monthly_credits_micro: 500_000_000,
            }
        );
        assert_eq!(default.kill_switches, StaticKillSwitches::default());
    }

    #[test]
    fn kill_switches_default_false_and_reject_unknown_keys() {
        let cfg: StaticModelPolicyConfig = serde_json::from_value(json!({
            "model_catalog": [catalog_entry_json("m")],
            "kill_switches": {"disable_web_search": true},
        }))
        .unwrap();
        assert!(cfg.kill_switches.disable_web_search);
        assert!(!cfg.kill_switches.disable_premium_tier);
        assert_eq!(cfg.model_catalog.len(), 1);

        let err = serde_json::from_value::<StaticModelPolicyConfig>(json!({
            "model_catalog": [],
            "kill_switches": {"disable_web_serach": true},
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("disable_web_serach"), "{err}");
    }

    #[test]
    fn rejects_unknown_top_level_keys() {
        assert!(
            serde_json::from_value::<StaticModelPolicyConfig>(
                json!({"model_catalog": [], "bogus": 1})
            )
            .is_err()
        );
    }
}
