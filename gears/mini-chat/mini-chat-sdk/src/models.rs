//! Policy snapshot models: model catalog, kill switches and user limits.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Rate-limit tier of a catalog model. Accepts `premium` / `Premium` and
/// `standard` / `Standard` on input; serializes lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    #[serde(alias = "Premium", alias = "PREMIUM")]
    Premium,
    #[serde(alias = "Standard", alias = "STANDARD")]
    Standard,
}

impl ModelTier {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Premium => "premium",
            Self::Standard => "standard",
        }
    }
}

/// Search context size hint of the web search tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchContextSize {
    #[default]
    #[serde(alias = "Low")]
    Low,
    #[serde(alias = "Medium")]
    Medium,
    #[serde(alias = "High")]
    High,
}

impl WebSearchContextSize {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Per-model token estimation budgets (preflight reserve, admission control
/// and context assembly).
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
    /// Present for compatibility; the gear reads the floor from its own
    /// configuration.
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

/// Provider request parameters of a model; each is sent only when set.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    pub stop: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelFeatures {
    pub streaming: bool,
    pub structured_output: bool,
}

/// Built-in tools a model supports.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ModelToolSupport {
    pub web_search: bool,
    pub file_search: bool,
    pub image_generation: bool,
    pub code_interpreter: bool,
    /// Unused (MCP is not implemented).
    pub mcp: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct SupportedEndpoints {
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
#[serde(default)]
pub struct ModelGeneralConfig {
    #[serde(rename = "type")]
    pub config_type: String,
    pub available_from: String,
    pub max_file_size_mb: u32,
    pub api_params: ApiParams,
    pub features: ModelFeatures,
    pub tool_support: ModelToolSupport,
    pub supported_endpoints: SupportedEndpoints,
}

impl Default for ModelGeneralConfig {
    fn default() -> Self {
        Self {
            config_type: String::new(),
            available_from: String::new(),
            max_file_size_mb: 25,
            api_params: ApiParams::default(),
            features: ModelFeatures::default(),
            tool_support: ModelToolSupport::default(),
            supported_endpoints: SupportedEndpoints::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelPreference {
    pub is_default: bool,
    pub sort_order: i32,
}

/// One entry of the policy model catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    /// Stable internal model id (`model_id` in the API, `chats.model`).
    pub id: String,
    /// Model name sent to the provider.
    pub provider_model_id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: String,
    /// Key of the `providers.<id>` entry that serves the model.
    pub provider_id: String,
    #[serde(default)]
    pub provider_display_name: String,
    pub tier: ModelTier,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub thread_summary_prompt: String,
    #[serde(default)]
    pub multimodal_capabilities: Vec<String>,
    pub context_window: u32,
    pub max_output_tokens: u32,
    #[serde(default)]
    pub max_input_tokens: u32,
    pub input_tokens_credit_multiplier_micro: u64,
    pub output_tokens_credit_multiplier_micro: u64,
    #[serde(default)]
    pub multiplier_display: String,
    #[serde(default)]
    pub estimation_budgets: EstimationBudgets,
    #[serde(default = "default_max_num_results")]
    pub max_num_results: u32,
    #[serde(default)]
    pub web_search_context_size: WebSearchContextSize,
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    #[serde(default)]
    pub general_config: ModelGeneralConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preference: Option<ModelPreference>,
}

fn default_max_tool_calls() -> u32 {
    2
}

fn default_max_num_results() -> u32 {
    5
}

impl ModelCatalogEntry {
    /// Whether the model accepts image input (`VISION_INPUT`).
    #[must_use]
    pub fn supports_vision(&self) -> bool {
        self.multimodal_capabilities
            .iter()
            .any(|c| c == "VISION_INPUT")
    }

    #[must_use]
    pub fn is_default(&self) -> bool {
        self.preference.as_ref().is_some_and(|p| p.is_default)
    }
}

/// Global kill switches carried by the policy snapshot. Every field is
/// required on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct KillSwitches {
    pub disable_premium_tier: bool,
    pub force_standard_tier: bool,
    pub disable_web_search: bool,
    pub disable_file_search: bool,
    pub disable_images: bool,
    pub disable_code_interpreter: bool,
}

/// Immutable, versioned policy configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    pub policy_version: u64,
    pub model_catalog: Vec<ModelCatalogEntry>,
    pub kill_switches: KillSwitches,
}

impl PolicySnapshot {
    /// Catalog entry by id (enabled or not).
    #[must_use]
    pub fn find(&self, model_id: &str) -> Option<&ModelCatalogEntry> {
        self.model_catalog.iter().find(|m| m.id == model_id)
    }

    /// Enabled catalog entry by id.
    #[must_use]
    pub fn find_enabled(&self, model_id: &str) -> Option<&ModelCatalogEntry> {
        self.find(model_id).filter(|m| m.enabled)
    }

    /// Default model: the first enabled entry with `preference.is_default`,
    /// else the first enabled entry.
    #[must_use]
    pub fn default_model(&self) -> Option<&ModelCatalogEntry> {
        self.model_catalog
            .iter()
            .find(|m| m.enabled && m.is_default())
            .or_else(|| self.model_catalog.iter().find(|m| m.enabled))
    }
}

/// Current policy version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyVersionInfo {
    pub policy_version: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: time::OffsetDateTime,
}

/// Per-tier credit limits (micro-credits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TierLimits {
    pub limit_daily_credits_micro: i64,
    pub limit_monthly_credits_micro: i64,
}

/// Per-user credit allocation for a policy version. `standard` limits the
/// `total` bucket (overall cap), `premium` the `tier:premium` bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    pub user_id: Uuid,
    pub policy_version: u64,
    pub standard: TierLimits,
    pub premium: TierLimits,
}

/// Result of a user license check.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    pub active: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_entry_defaults() {
        let e: ModelCatalogEntry = serde_json::from_value(serde_json::json!({
            "id": "m", "provider_model_id": "pm", "display_name": "M",
            "provider_id": "p", "provider_display_name": "P", "tier": "Premium",
            "context_window": 1000, "max_output_tokens": 100, "max_input_tokens": 0,
            "input_tokens_credit_multiplier_micro": 1, "output_tokens_credit_multiplier_micro": 1,
            "max_num_results": 5, "general_config": {}
        }))
        .unwrap();
        assert!(!e.enabled);
        assert!(e.multimodal_capabilities.is_empty());
        assert_eq!(e.estimation_budgets, EstimationBudgets::default());
        assert_eq!(e.estimation_budgets.bytes_per_token_conservative, 4);
        assert_eq!(e.web_search_context_size, WebSearchContextSize::Low);
        assert_eq!(e.max_tool_calls, 2);
        assert!(e.preference.is_none());
        assert_eq!(e.tier, ModelTier::Premium);
        assert_eq!(serde_json::to_value(e.tier).unwrap(), "premium");
    }

    #[test]
    fn kill_switches_require_every_field() {
        assert!(serde_json::from_str::<KillSwitches>(r#"{"disable_premium_tier":true}"#).is_err());
    }

    #[test]
    fn default_model_prefers_is_default() {
        let mk = |id: &str, enabled: bool, def: bool| {
            let mut v = serde_json::json!({
                "id": id, "provider_model_id": id, "display_name": id, "provider_id": "p",
                "tier": "standard", "context_window": 1, "max_output_tokens": 1,
                "input_tokens_credit_multiplier_micro": 1, "output_tokens_credit_multiplier_micro": 1,
                "enabled": enabled
            });
            if def {
                v["preference"] = serde_json::json!({"is_default": true, "sort_order": 0});
            }
            serde_json::from_value::<ModelCatalogEntry>(v).unwrap()
        };
        let snap = PolicySnapshot {
            policy_version: 1,
            model_catalog: vec![mk("a", false, true), mk("b", true, false), mk("c", true, true)],
            kill_switches: KillSwitches::default(),
        };
        assert_eq!(snap.default_model().unwrap().id, "c");
        let snap2 = PolicySnapshot { model_catalog: snap.model_catalog[..2].to_vec(), ..snap };
        assert_eq!(snap2.default_model().unwrap().id, "b");
    }
}
