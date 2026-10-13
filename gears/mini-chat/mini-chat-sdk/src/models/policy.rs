//! Policy snapshot, model catalog and user-limit models (DESIGN Appendix A.1).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Model tier. Serialized lowercase; the capitalized form is accepted on input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    #[serde(alias = "Premium")]
    Premium,
    #[serde(alias = "Standard")]
    Standard,
}

/// Web search context size passed to the provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchContextSize {
    #[default]
    Low,
    Medium,
    High,
}

/// Tenant preference for a catalog entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPreference {
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub sort_order: i32,
}

/// Token estimation parameters (DESIGN section 5.2.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EstimationBudgets {
    pub bytes_per_token_conservative: u32,
    pub fixed_overhead_tokens: u32,
    pub safety_margin_pct: u32,
    pub image_token_budget: u32,
    pub tool_surcharge_tokens: u32,
    pub web_search_surcharge_tokens: u32,
    pub code_interpreter_surcharge_tokens: u32,
    pub minimal_generation_floor: u32,
}

impl Default for EstimationBudgets {
    fn default() -> Self {
        Self {
            bytes_per_token_conservative: 4,
            fixed_overhead_tokens: 100,
            safety_margin_pct: 10,
            image_token_budget: 1000,
            tool_surcharge_tokens: 500,
            web_search_surcharge_tokens: 500,
            code_interpreter_surcharge_tokens: 1000,
            minimal_generation_floor: 50,
        }
    }
}

/// Provider API parameters of a model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelApiParams {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub stop: Vec<String>,
    #[serde(default)]
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

/// Feature flags of a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFeatures {
    pub streaming: bool,
    pub structured_output: bool,
}

/// Tools a model supports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ModelToolSupport {
    pub web_search: bool,
    pub file_search: bool,
    pub image_generation: bool,
    pub code_interpreter: bool,
    pub mcp: bool,
}

/// Provider endpoints a model supports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ModelSupportedEndpoints {
    pub chat_completions: bool,
    pub responses: bool,
    pub embeddings: bool,
    pub image_generation: bool,
    pub audio_speech_generation: bool,
    pub audio_transcription: bool,
    pub audio_translation: bool,
}

/// General model configuration (`general_config`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelGeneralConfig {
    pub r#type: String,
    pub available_from: String,
    pub max_file_size_mb: u32,
    pub api_params: ModelApiParams,
    pub features: ModelFeatures,
    pub tool_support: ModelToolSupport,
    pub supported_endpoints: ModelSupportedEndpoints,
}

const fn default_max_tool_calls() -> u32 {
    2
}

/// One entry of the policy model catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    pub id: String,
    pub provider_model_id: String,
    pub display_name: String,
    pub provider_id: String,
    /// UI-only; never a routing key.
    pub provider_display_name: String,
    pub tier: ModelTier,
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub max_input_tokens: u32,
    pub input_tokens_credit_multiplier_micro: i64,
    pub output_tokens_credit_multiplier_micro: i64,
    pub max_num_results: u32,
    pub general_config: ModelGeneralConfig,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub multiplier_display: String,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub thread_summary_prompt: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub multimodal_capabilities: Vec<String>,
    #[serde(default)]
    pub estimation_budgets: EstimationBudgets,
    #[serde(default)]
    pub web_search_context_size: WebSearchContextSize,
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    #[serde(default)]
    pub preference: Option<ModelPreference>,
}

impl ModelCatalogEntry {
    #[must_use]
    pub fn is_premium(&self) -> bool {
        self.tier == ModelTier::Premium
    }

    #[must_use]
    pub fn supports_vision(&self) -> bool {
        self.multimodal_capabilities
            .iter()
            .any(|c| c == "VISION_INPUT")
    }

    #[must_use]
    pub fn tool_support(&self) -> &ModelToolSupport {
        &self.general_config.tool_support
    }
}

/// Kill switches of a policy snapshot. Every field is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct KillSwitches {
    pub disable_premium_tier: bool,
    pub force_standard_tier: bool,
    pub disable_web_search: bool,
    pub disable_file_search: bool,
    pub disable_images: bool,
    pub disable_code_interpreter: bool,
}

/// Immutable policy snapshot for a `(user, policy_version)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    pub policy_version: i64,
    pub model_catalog: Vec<ModelCatalogEntry>,
    pub kill_switches: KillSwitches,
}

/// Current policy version of a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyVersionInfo {
    pub policy_version: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
}

/// Credit limits of one tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierLimits {
    pub limit_daily_credits_micro: i64,
    pub limit_monthly_credits_micro: i64,
}

/// Per-user limits for a policy version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    pub user_id: Uuid,
    pub policy_version: i64,
    pub standard: TierLimits,
    pub premium: TierLimits,
}

/// License status of a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    pub active: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn minimal_entry() -> serde_json::Value {
        json!({
            "id": "gpt-4.1",
            "provider_model_id": "gpt-4.1",
            "display_name": "GPT-4.1",
            "provider_id": "azure_openai",
            "provider_display_name": "Azure OpenAI",
            "tier": "Premium",
            "enabled": true,
            "context_window": 1_047_576,
            "max_output_tokens": 32_768,
            "max_input_tokens": 1_047_576,
            "input_tokens_credit_multiplier_micro": 3_000_000,
            "output_tokens_credit_multiplier_micro": 15_000_000,
            "max_num_results": 5,
            "general_config": {
                "type": "",
                "available_from": "1970-01-01T00:00:00Z",
                "max_file_size_mb": 25,
                "api_params": {
                    "temperature": 0.7,
                    "top_p": 1.0,
                    "frequency_penalty": 0.0,
                    "presence_penalty": 0.0,
                    "stop": []
                },
                "features": { "streaming": true, "structured_output": true },
                "tool_support": {
                    "web_search": true,
                    "file_search": true,
                    "image_generation": false,
                    "code_interpreter": true,
                    "mcp": false
                },
                "supported_endpoints": {
                    "chat_completions": true,
                    "responses": true,
                    "embeddings": false,
                    "image_generation": false,
                    "audio_speech_generation": false,
                    "audio_transcription": false,
                    "audio_translation": false
                }
            }
        })
    }

    #[test]
    fn catalog_entry_defaults() {
        let entry: ModelCatalogEntry = serde_json::from_value(minimal_entry()).unwrap();
        assert!(entry.enabled);
        assert_eq!(entry.max_tool_calls, 2);
        assert_eq!(entry.web_search_context_size, WebSearchContextSize::Low);
        assert_eq!(entry.estimation_budgets, EstimationBudgets::default());
        assert_eq!(entry.estimation_budgets.bytes_per_token_conservative, 4);
        assert_eq!(entry.estimation_budgets.minimal_generation_floor, 50);
        assert!(entry.description.is_empty());
        assert!(entry.multimodal_capabilities.is_empty());
        assert!(entry.preference.is_none());
        assert_eq!(entry.tier, ModelTier::Premium);
        assert!(entry.is_premium());
        assert!(!entry.supports_vision());
        assert!(entry.tool_support().web_search);

        let back = serde_json::to_value(&entry).unwrap();
        assert_eq!(back["tier"], "premium");
        assert_eq!(back["web_search_context_size"], "low");

        // Absent `enabled` defaults to false; lowercase tier is accepted.
        let mut raw = minimal_entry();
        raw.as_object_mut().unwrap().remove("enabled");
        raw["tier"] = json!("standard");
        raw["multimodal_capabilities"] = json!(["VISION_INPUT"]);
        let entry: ModelCatalogEntry = serde_json::from_value(raw).unwrap();
        assert!(!entry.enabled);
        assert_eq!(entry.tier, ModelTier::Standard);
        assert!(entry.supports_vision());
    }

    #[test]
    fn kill_switches_require_all_fields() {
        let full = json!({
            "disable_premium_tier": false,
            "force_standard_tier": false,
            "disable_web_search": false,
            "disable_file_search": false,
            "disable_images": false,
            "disable_code_interpreter": false
        });
        assert!(serde_json::from_value::<KillSwitches>(full.clone()).is_ok());
        let mut missing = full;
        missing.as_object_mut().unwrap().remove("disable_images");
        assert!(serde_json::from_value::<KillSwitches>(missing).is_err());
    }
}
