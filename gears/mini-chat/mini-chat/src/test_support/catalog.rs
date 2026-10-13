//! Model-catalog fixtures shared by the API and domain tests.
//!
//! Every model is served by provider `openai` (the provider entry `TestApp` configures), has
//! `context_window 128000`, `max_output_tokens 4096`, `max_input_tokens 120000`, and uses the same
//! credit multiplier for input and output: `1_000_000` (standard) or `3_000_000` (premium).

use mini_chat_sdk::{ModelCatalogEntry, ModelPreference, ModelTier};

use super::fixtures::catalog_entry;

/// Vision capability flag (`ModelCatalogEntry::supports_vision`).
pub const VISION: &str = "VISION_INPUT";

const STANDARD_MULTIPLIER_MICRO: i64 = 1_000_000;
const PREMIUM_MULTIPLIER_MICRO: i64 = 3_000_000;

fn model(
    id: &str,
    tier: ModelTier,
    multiplier: i64,
    vision: bool,
    tools: bool,
) -> ModelCatalogEntry {
    let mut m = catalog_entry(id);
    m.display_name = format!("{id} display");
    m.tier = tier;
    m.enabled = true;
    m.input_tokens_credit_multiplier_micro = multiplier;
    m.output_tokens_credit_multiplier_micro = multiplier;
    m.multimodal_capabilities = if vision {
        vec![VISION.to_owned()]
    } else {
        Vec::new()
    };
    let t = &mut m.general_config.tool_support;
    t.web_search = tools;
    t.file_search = tools;
    t.code_interpreter = tools;
    m
}

/// Enabled premium model with vision and the web search, file search and code interpreter tools.
pub fn premium_model(id: &str) -> ModelCatalogEntry {
    model(id, ModelTier::Premium, PREMIUM_MULTIPLIER_MICRO, true, true)
}

/// Enabled standard model with vision and the web search, file search and code interpreter tools.
pub fn standard_model(id: &str) -> ModelCatalogEntry {
    model(
        id,
        ModelTier::Standard,
        STANDARD_MULTIPLIER_MICRO,
        true,
        true,
    )
}

/// Enabled standard model without vision and without any tool.
pub fn no_vision_model(id: &str) -> ModelCatalogEntry {
    model(
        id,
        ModelTier::Standard,
        STANDARD_MULTIPLIER_MICRO,
        false,
        false,
    )
}

/// [`standard_model`] served by provider `anthropic` (see `app::anthropic_config`).
pub fn anthropic_model(id: &str) -> ModelCatalogEntry {
    let mut m = standard_model(id);
    m.provider_id = "anthropic".to_owned();
    m
}

/// The default `TestApp` catalog, in this order:
/// 1. `gpt-premium`: [`premium_model`], `preference.is_default = true` (the default chat model);
/// 2. `gpt-standard`: [`standard_model`];
/// 3. `gpt-mini-novision`: [`no_vision_model`];
/// 4. `gpt-disabled`: [`premium_model`] with `enabled = false`.
pub fn test_catalog() -> Vec<ModelCatalogEntry> {
    let mut premium = premium_model("gpt-premium");
    premium.preference = Some(ModelPreference {
        is_default: true,
        sort_order: 0,
    });
    let mut disabled = premium_model("gpt-disabled");
    disabled.enabled = false;
    vec![
        premium,
        standard_model("gpt-standard"),
        no_vision_model("gpt-mini-novision"),
        disabled,
    ]
}

#[cfg(test)]
mod tests {
    use mini_chat_sdk::ModelTier;

    use super::*;

    #[test]
    fn test_catalog_shape() {
        let catalog = test_catalog();
        let ids: Vec<&str> = catalog.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "gpt-premium",
                "gpt-standard",
                "gpt-mini-novision",
                "gpt-disabled"
            ]
        );
        let enabled: Vec<bool> = catalog.iter().map(|m| m.enabled).collect();
        assert_eq!(enabled, [true, true, true, false]);
        let defaults: Vec<bool> = catalog
            .iter()
            .map(|m| m.preference.as_ref().is_some_and(|p| p.is_default))
            .collect();
        assert_eq!(defaults, [true, false, false, false]);
        let tiers: Vec<ModelTier> = catalog.iter().map(|m| m.tier).collect();
        assert_eq!(
            tiers,
            [
                ModelTier::Premium,
                ModelTier::Standard,
                ModelTier::Standard,
                ModelTier::Premium
            ]
        );
        for m in &catalog {
            assert_eq!(m.provider_id, "openai", "{}", m.id);
            assert_eq!(m.provider_model_id, m.id);
            assert_eq!(
                (m.context_window, m.max_output_tokens, m.max_input_tokens),
                (128_000, 4096, 120_000)
            );
        }
    }

    #[test]
    fn model_builders_set_tier_multipliers_vision_and_tools() {
        let p = premium_model("p");
        assert_eq!(p.tier, ModelTier::Premium);
        assert_eq!(
            (
                p.input_tokens_credit_multiplier_micro,
                p.output_tokens_credit_multiplier_micro
            ),
            (3_000_000, 3_000_000)
        );
        assert!(p.enabled && p.supports_vision());
        let t = p.tool_support();
        assert!(t.web_search && t.file_search && t.code_interpreter);
        assert!(p.preference.is_none());

        let s = standard_model("s");
        assert_eq!(s.tier, ModelTier::Standard);
        assert_eq!(
            (
                s.input_tokens_credit_multiplier_micro,
                s.output_tokens_credit_multiplier_micro
            ),
            (1_000_000, 1_000_000)
        );
        assert!(s.enabled && s.supports_vision());
        let t = s.tool_support();
        assert!(t.web_search && t.file_search && t.code_interpreter);

        let n = no_vision_model("n");
        assert_eq!(n.tier, ModelTier::Standard);
        assert!(n.enabled && !n.supports_vision());
        let t = n.tool_support();
        assert!(!t.web_search && !t.file_search && !t.code_interpreter);
    }
}
