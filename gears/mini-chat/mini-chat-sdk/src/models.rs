//! Policy, catalog and limit types shared by the mini-chat gear and its
//! model-policy plugins (DESIGN §5.2, Appendix A.1).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Rate-limit tier of a catalog model. Accepts `premium` / `Premium`.
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

/// Search context size hint for the `web_search` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
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

/// Per-model token estimation budgets (DESIGN §5.2.1).
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
    /// Present on the catalog entry but not read (the gear config value applies).
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

/// Provider API parameters sent with each request (each only when set).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ModelApiParams {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ModelFeatures {
    pub streaming: bool,
    pub structured_output: bool,
}

/// Built-in tool support flags of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ModelToolSupport {
    pub web_search: bool,
    pub file_search: bool,
    pub image_generation: bool,
    pub code_interpreter: bool,
    /// Parsed and unused (MCP is deferred, ADR-0006).
    pub mcp: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
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

/// `general_config` of a catalog entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ModelPreference {
    pub is_default: bool,
    pub sort_order: i32,
}

/// Capability flag that enables image input.
pub const VISION_INPUT: &str = "VISION_INPUT";

fn default_max_tool_calls() -> u32 {
    2
}

/// One entry of the policy snapshot model catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    pub id: String,
    pub provider_model_id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
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
    #[must_use]
    pub fn supports_vision(&self) -> bool {
        self.multimodal_capabilities.iter().any(|c| c == VISION_INPUT)
    }

    #[must_use]
    pub fn is_default(&self) -> bool {
        self.preference.is_some_and(|p| p.is_default)
    }

    #[must_use]
    pub fn tool_support(&self) -> ModelToolSupport {
        self.general_config.tool_support
    }
}

/// Global kill switches. Every field is required (a renamed switch fails to
/// deserialize instead of silently reading `false`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
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
    #[must_use]
    pub fn find(&self, model_id: &str) -> Option<&ModelCatalogEntry> {
        self.model_catalog.iter().find(|m| m.id == model_id)
    }

    #[must_use]
    pub fn find_enabled(&self, model_id: &str) -> Option<&ModelCatalogEntry> {
        self.find(model_id).filter(|m| m.enabled)
    }

    /// Default model for new chats: first enabled `is_default` entry, else the
    /// first enabled entry (tier is not considered).
    #[must_use]
    pub fn default_model(&self) -> Option<&ModelCatalogEntry> {
        self.model_catalog
            .iter()
            .find(|m| m.enabled && m.is_default())
            .or_else(|| self.model_catalog.iter().find(|m| m.enabled))
    }
}

/// Current policy version answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyVersionInfo {
    pub policy_version: u64,
    #[serde(default)]
    pub generated_at: Option<String>,
}

/// Per-tier credit allocation in micro-credits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierLimits {
    pub limit_daily_credits_micro: i64,
    pub limit_monthly_credits_micro: i64,
}

/// Per-user limits: `standard` maps to bucket `total`, `premium` to `tier:premium`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    pub user_id: Uuid,
    pub policy_version: u64,
    pub standard: TierLimits,
    pub premium: TierLimits,
}

/// License status (the gear never calls the check, DESIGN §5.2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UserLicenseStatus {
    pub active: bool,
}
