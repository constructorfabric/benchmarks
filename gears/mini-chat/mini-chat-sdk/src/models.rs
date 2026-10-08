//! Model catalog and policy snapshot types shared between the gear and its
//! model-policy plugins.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use time::OffsetDateTime;
use uuid::Uuid;

/// Model tier. Accepts `standard`/`premium` and the capitalized spellings used
/// by the YAML catalog (`Standard`/`Premium`); always serializes lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    #[serde(alias = "Standard")]
    Standard,
    #[serde(alias = "Premium")]
    Premium,
}

impl ModelTier {
    #[must_use]
    #[allow(clippy::trivially_copy_pass_by_ref)] // signature fixed by the SDK contract
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Premium => "premium",
        }
    }
}

/// Token-estimation budgets applied by preflight reserve computation.
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

/// Provider API parameters forwarded with each request.
/// Sampling parameters are sent only when set; `stop` is a required key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelApiParams {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub stop: Vec<String>,
    pub extra_body: Option<Map<String, Value>>,
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFeatures {
    pub streaming: bool,
    pub structured_output: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ModelToolSupport {
    pub web_search: bool,
    pub file_search: bool,
    pub image_generation: bool,
    pub code_interpreter: bool,
    pub mcp: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelGeneralConfig {
    #[serde(rename = "type")]
    pub config_type: String,
    pub available_from: String,
    pub max_file_size_mb: u32,
    pub api_params: ModelApiParams,
    pub features: ModelFeatures,
    pub tool_support: ModelToolSupport,
    pub supported_endpoints: ModelSupportedEndpoints,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPreference {
    pub is_default: bool,
    pub sort_order: i32,
}

fn default_web_search_context_size() -> String {
    "low".to_owned()
}

fn default_max_tool_calls() -> u32 {
    2
}

/// One entry of the model catalog carried by a [`PolicySnapshot`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    pub id: String,
    pub provider_model_id: String,
    pub display_name: String,
    pub provider_id: String,
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
    #[serde(default = "default_web_search_context_size")]
    pub web_search_context_size: String,
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    #[serde(default)]
    pub preference: Option<ModelPreference>,
}

impl ModelCatalogEntry {
    /// Whether the model accepts image input (`VISION_INPUT` capability).
    #[must_use]
    pub fn supports_vision(&self) -> bool {
        self.multimodal_capabilities
            .iter()
            .any(|c| c == "VISION_INPUT")
    }

    /// Whether the catalog marks this model as the default choice.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.preference.is_some_and(|p| p.is_default)
    }
}

/// Operator kill switches. Every field is required (no defaults) so a plugin
/// cannot silently omit one.
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

/// Immutable policy snapshot identified by `policy_version`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    pub policy_version: u64,
    pub model_catalog: Vec<ModelCatalogEntry>,
    pub kill_switches: KillSwitches,
}

impl PolicySnapshot {
    /// Finds a catalog entry by id (enabled or not).
    #[must_use]
    pub fn find(&self, id: &str) -> Option<&ModelCatalogEntry> {
        self.model_catalog.iter().find(|m| m.id == id)
    }

    /// Iterates enabled catalog entries in catalog order.
    pub fn enabled(&self) -> impl Iterator<Item = &ModelCatalogEntry> {
        self.model_catalog.iter().filter(|m| m.enabled)
    }

    /// First enabled entry flagged `is_default`, else the first enabled entry.
    #[must_use]
    pub fn default_model(&self) -> Option<&ModelCatalogEntry> {
        self.enabled()
            .find(|m| m.is_default())
            .or_else(|| self.enabled().next())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyVersionInfo {
    pub policy_version: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierLimits {
    pub limit_daily_credits_micro: i64,
    pub limit_monthly_credits_micro: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    pub user_id: Uuid,
    pub policy_version: u64,
    pub standard: TierLimits,
    pub premium: TierLimits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    pub active: bool,
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod models_tests;
