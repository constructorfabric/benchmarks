//! Policy snapshot, model catalog and per-user limit models.
//!
//! These types are served by the model-policy plugin
//! ([`crate::MiniChatModelPolicyPluginClientV1`]). They are deserialized from
//! plugin configuration (static plugin) or from a remote policy service, so
//! they carry `serde` derives. Unknown keys inside a catalog entry are ignored.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Capability flag that marks a model as accepting image input.
pub const VISION_INPUT: &str = "VISION_INPUT";

/// Rate-limit tier of a catalog model. Determines the downgrade cascade order
/// (`premium` → `standard`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    #[serde(alias = "Premium", alias = "PREMIUM")]
    Premium,
    #[serde(alias = "Standard", alias = "STANDARD")]
    Standard,
}

impl ModelTier {
    /// Stable lowercase name (`premium` / `standard`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Premium => "premium",
            Self::Standard => "standard",
        }
    }
}

/// Search context size hint for the provider `web_search` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Per-model token estimation budgets used for preflight reserve estimation,
/// the `INPUT_TOO_LONG` check and context assembly.
///
/// `minimal_generation_floor` is carried for compatibility but is not read:
/// the floor comes from the gear configuration.
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

/// Provider request parameters of a catalog model. Each sampling parameter is
/// sent only when it is set.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelApiParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(default)]
    pub stop: Vec<String>,
    /// Extra top-level request body keys merged into the provider request
    /// (`OpenAI` Responses, `OpenAI` Chat and `vLLM` adapters).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    /// Reasoning effort hint (`low` / `medium` / `high`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// Feature flags of a catalog model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelFeatures {
    pub streaming: bool,
    pub structured_output: bool,
}

/// Tool support flags of a catalog model. They gate the tools that are sent
/// to the provider, the tool guards, the reserve surcharges and the daily
/// tool quota checks. `mcp` is parsed and unused (MCP is not implemented).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ModelToolSupport {
    pub web_search: bool,
    pub file_search: bool,
    pub image_generation: bool,
    pub code_interpreter: bool,
    pub mcp: bool,
}

/// Provider endpoints a model supports (informational).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
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

/// General configuration of a catalog model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelGeneralConfig {
    #[serde(rename = "type", default)]
    pub config_type: String,
    #[serde(default)]
    pub available_from: Option<String>,
    /// Per-model upload size cap in MiB (applies to documents and images).
    #[serde(default)]
    pub max_file_size_mb: u32,
    #[serde(default)]
    pub api_params: ModelApiParams,
    #[serde(default)]
    pub features: ModelFeatures,
    #[serde(default)]
    pub tool_support: ModelToolSupport,
    #[serde(default)]
    pub supported_endpoints: ModelSupportedEndpoints,
}

impl Default for ModelGeneralConfig {
    fn default() -> Self {
        Self {
            config_type: String::new(),
            available_from: None,
            max_file_size_mb: 25,
            api_params: ModelApiParams::default(),
            features: ModelFeatures::default(),
            tool_support: ModelToolSupport::default(),
            supported_endpoints: ModelSupportedEndpoints::default(),
        }
    }
}

/// Catalog ordering / default-model preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelPreference {
    pub is_default: bool,
    pub sort_order: i32,
}

fn default_max_tool_calls() -> u32 {
    2
}

/// One model of the policy catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    /// Stable internal model identifier (`model_id` in the REST API, stored in
    /// `chats.model` / `messages.model`).
    pub id: String,
    /// Model name sent to the provider.
    pub provider_model_id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    /// Key of the `providers.<id>` entry that serves the model.
    pub provider_id: String,
    #[serde(default)]
    pub provider_display_name: String,
    #[serde(default)]
    pub icon: String,
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
    /// Maximum input tokens per request; `0` means no separate limit.
    #[serde(default)]
    pub max_input_tokens: u32,
    pub input_tokens_credit_multiplier_micro: i64,
    pub output_tokens_credit_multiplier_micro: i64,
    #[serde(default)]
    pub multiplier_display: String,
    #[serde(default)]
    pub estimation_budgets: EstimationBudgets,
    #[serde(default)]
    pub max_num_results: u32,
    #[serde(default)]
    pub web_search_context_size: WebSearchContextSize,
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    #[serde(default)]
    pub general_config: ModelGeneralConfig,
    #[serde(default)]
    pub preference: Option<ModelPreference>,
}

impl ModelCatalogEntry {
    /// `true` when the model accepts image input (`VISION_INPUT`).
    #[must_use]
    pub fn supports_vision(&self) -> bool {
        self.multimodal_capabilities
            .iter()
            .any(|c| c.eq_ignore_ascii_case(VISION_INPUT))
    }

    /// `true` when the entry is marked as the default model.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.preference.is_some_and(|p| p.is_default)
    }

    /// Tool support flags of the model.
    #[must_use]
    pub fn tool_support(&self) -> ModelToolSupport {
        self.general_config.tool_support
    }
}

/// Global kill switches / emergency flags. Every field is required on the
/// wire so a renamed switch fails deserialization instead of reading `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct KillSwitches {
    pub disable_premium_tier: bool,
    pub force_standard_tier: bool,
    pub disable_web_search: bool,
    pub disable_file_search: bool,
    pub disable_images: bool,
    pub disable_code_interpreter: bool,
}

/// Immutable, versioned policy snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    pub policy_version: u64,
    pub model_catalog: Vec<ModelCatalogEntry>,
    pub kill_switches: KillSwitches,
}

impl PolicySnapshot {
    /// Look up a catalog entry by id (enabled or not).
    #[must_use]
    pub fn find_model(&self, model_id: &str) -> Option<&ModelCatalogEntry> {
        self.model_catalog.iter().find(|m| m.id == model_id)
    }

    /// Look up an enabled catalog entry by id.
    #[must_use]
    pub fn find_enabled_model(&self, model_id: &str) -> Option<&ModelCatalogEntry> {
        self.model_catalog
            .iter()
            .find(|m| m.id == model_id && m.enabled)
    }

    /// Default model for new chats: the first enabled model marked
    /// `is_default`, otherwise the first enabled model. The tier is not
    /// considered.
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

/// Credit limits of one tier, in micro-credits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierLimits {
    pub limit_daily_credits_micro: i64,
    pub limit_monthly_credits_micro: i64,
}

/// Per-user credit allocation tied to a policy version. `standard` limits
/// apply to the `total` bucket (the overall cap); `premium` limits apply to
/// the `tier:premium` bucket (a premium-only sub-cap).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    pub user_id: Uuid,
    pub policy_version: u64,
    pub standard: TierLimits,
    pub premium: TierLimits,
}

/// License status returned by `check_user_license` (not called by the gear in
/// P1; the license gate is enforced on the routes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    pub active: bool,
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod models_tests;
