//! Policy snapshot, model catalog and per-user limit types.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;
use uuid::Uuid;

/// Capability flag that enables image input on a model.
pub const VISION_INPUT: &str = "VISION_INPUT";

/// Rate-limit tier of a model. Deserializes case-insensitively (`premium`, `Premium`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, schemars::JsonSchema)]
pub enum ModelTier {
    /// Premium tier (stricter limits, `tier:premium` bucket).
    Premium,
    /// Standard tier (overall `total` bucket only).
    Standard,
}

impl ModelTier {
    /// Lower-case wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Premium => "premium",
            Self::Standard => "standard",
        }
    }
}

impl Serialize for ModelTier {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ModelTier {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        match raw.to_ascii_lowercase().as_str() {
            "premium" => Ok(Self::Premium),
            "standard" => Ok(Self::Standard),
            other => Err(serde::de::Error::custom(format!(
                "unknown model tier '{other}' (expected premium or standard)"
            ))),
        }
    }
}

/// Search context size hint of the `web_search` tool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, schemars::JsonSchema)]
pub enum WebSearchContextSize {
    /// `low` (default).
    #[default]
    Low,
    /// `medium`.
    Medium,
    /// `high`.
    High,
}

impl WebSearchContextSize {
    /// Wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl Serialize for WebSearchContextSize {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for WebSearchContextSize {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        match raw.to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            other => Err(serde::de::Error::custom(format!(
                "unknown web_search_context_size '{other}'"
            ))),
        }
    }
}

/// Per-model token estimation budgets (DESIGN §5.2.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct EstimationBudgets {
    /// Conservative bytes-per-token ratio for text estimation.
    pub bytes_per_token_conservative: u32,
    /// Constant protocol/framing overhead.
    pub fixed_overhead_tokens: u32,
    /// Safety margin applied to the text estimate, in percent.
    pub safety_margin_pct: u32,
    /// Tokens reserved per image.
    pub image_token_budget: u32,
    /// Surcharge when `file_search` is sent.
    pub tool_surcharge_tokens: u32,
    /// Surcharge when `web_search` is sent.
    pub web_search_surcharge_tokens: u32,
    /// Surcharge when `code_interpreter` is sent.
    pub code_interpreter_surcharge_tokens: u32,
    /// Present for compatibility; the gear reads the floor from its own configuration.
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

/// Optional provider request parameters of a model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ApiParams {
    /// Sampling temperature, sent only when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling, sent only when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Frequency penalty, sent only when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    /// Presence penalty, sent only when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// Stop sequences.
    pub stop: Vec<String>,
    /// Extra top-level request body keys (reserved keys are ignored).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    /// Reasoning effort hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// Model feature flags.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ModelFeatures {
    /// Streaming support.
    pub streaming: bool,
    /// Structured output support.
    pub structured_output: bool,
}

/// Built-in tool support of a model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools, reason = "wire format of the policy catalog (DESIGN B.2)")]
pub struct ModelToolSupport {
    /// `web_search` tool.
    pub web_search: bool,
    /// `file_search` tool.
    pub file_search: bool,
    /// Image generation (unused in P1).
    pub image_generation: bool,
    /// `code_interpreter` tool.
    pub code_interpreter: bool,
    /// MCP tools (unused, ADR-0006).
    pub mcp: bool,
}

/// Provider endpoints supported by a model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools, reason = "wire format of the policy catalog (DESIGN B.2)")]
pub struct ModelSupportedEndpoints {
    /// Chat Completions API.
    pub chat_completions: bool,
    /// Responses API.
    pub responses: bool,
    /// Embeddings API.
    pub embeddings: bool,
    /// Image generation API.
    pub image_generation: bool,
    /// Speech generation API.
    pub audio_speech_generation: bool,
    /// Transcription API.
    pub audio_transcription: bool,
    /// Translation API.
    pub audio_translation: bool,
}

/// General model configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ModelGeneralConfig {
    /// Configuration type tag.
    #[serde(rename = "type")]
    pub config_type: String,
    /// Availability start (informational).
    pub available_from: String,
    /// Per-model upload size cap in MiB.
    pub max_file_size_mb: u32,
    /// Request parameters.
    pub api_params: ApiParams,
    /// Feature flags.
    pub features: ModelFeatures,
    /// Built-in tool support.
    pub tool_support: ModelToolSupport,
    /// Supported endpoints.
    pub supported_endpoints: ModelSupportedEndpoints,
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
            supported_endpoints: ModelSupportedEndpoints::default(),
        }
    }
}

/// Catalog preference of a model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ModelPreference {
    /// Default model for new chats.
    pub is_default: bool,
    /// UI ordering.
    pub sort_order: i32,
}

fn default_max_tool_calls() -> u32 {
    2
}

/// One entry of the policy model catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelCatalogEntry {
    /// Stable model id used by the API (`model_id`, `chats.model`).
    pub id: String,
    /// Model name on the provider side.
    pub provider_model_id: String,
    /// User-facing name.
    pub display_name: String,
    /// User-facing help text.
    #[serde(default)]
    pub description: String,
    /// Key of the `providers.<id>` entry serving the model.
    pub provider_id: String,
    /// Provider display name (UI only).
    #[serde(default)]
    pub provider_display_name: String,
    /// Icon (UI only).
    #[serde(default)]
    pub icon: String,
    /// Rate-limit tier.
    pub tier: ModelTier,
    /// Visible and selectable when `true`.
    #[serde(default)]
    pub enabled: bool,
    /// System prompt sent on every request.
    #[serde(default)]
    pub system_prompt: String,
    /// System prompt of the thread-summary call (summary model entry).
    #[serde(default)]
    pub thread_summary_prompt: String,
    /// Capability flags, e.g. `VISION_INPUT`.
    #[serde(default)]
    pub multimodal_capabilities: Vec<String>,
    /// Context window in tokens.
    #[serde(default)]
    pub context_window: u32,
    /// Output cap in tokens.
    #[serde(default)]
    pub max_output_tokens: u32,
    /// Input cap in tokens (`0` = no separate limit).
    #[serde(default)]
    pub max_input_tokens: u32,
    /// Micro-credits per 1M input tokens.
    #[serde(default)]
    pub input_tokens_credit_multiplier_micro: u64,
    /// Micro-credits per 1M output tokens.
    #[serde(default)]
    pub output_tokens_credit_multiplier_micro: u64,
    /// Human-readable multiplier.
    #[serde(default)]
    pub multiplier_display: String,
    /// Estimation budgets.
    #[serde(default)]
    pub estimation_budgets: EstimationBudgets,
    /// Top-k results per `file_search` call.
    #[serde(default)]
    pub max_num_results: u32,
    /// Web search context size.
    #[serde(default)]
    pub web_search_context_size: WebSearchContextSize,
    /// Built-in tool calls per provider request.
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    /// General configuration.
    #[serde(default)]
    pub general_config: ModelGeneralConfig,
    /// Preference.
    #[serde(default)]
    pub preference: Option<ModelPreference>,
}

impl ModelCatalogEntry {
    /// `true` when the model accepts image input.
    #[must_use]
    pub fn supports_vision(&self) -> bool {
        self.multimodal_capabilities.iter().any(|c| c == VISION_INPUT)
    }

    /// `true` when the entry is marked as the default model.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.preference.as_ref().is_some_and(|p| p.is_default)
    }
}

/// Global kill switches of a policy snapshot (every field is required).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[allow(clippy::struct_excessive_bools, reason = "wire format of the policy catalog (DESIGN B.2)")]
pub struct KillSwitches {
    /// Premium models are not used.
    pub disable_premium_tier: bool,
    /// Every request uses the standard tier.
    pub force_standard_tier: bool,
    /// Requests with web search enabled are rejected.
    pub disable_web_search: bool,
    /// `file_search` is not sent.
    pub disable_file_search: bool,
    /// Image uploads and image inputs are rejected.
    pub disable_images: bool,
    /// Code interpreter is not used.
    pub disable_code_interpreter: bool,
}

/// Immutable policy snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PolicySnapshot {
    /// Monotonic version.
    pub policy_version: u64,
    /// Model catalog in catalog order.
    pub model_catalog: Vec<ModelCatalogEntry>,
    /// Kill switches.
    pub kill_switches: KillSwitches,
}

impl PolicySnapshot {
    /// Catalog entry by id (enabled or not).
    #[must_use]
    pub fn model(&self, id: &str) -> Option<&ModelCatalogEntry> {
        self.model_catalog.iter().find(|m| m.id == id)
    }

    /// Enabled catalog entry by id.
    #[must_use]
    pub fn enabled_model(&self, id: &str) -> Option<&ModelCatalogEntry> {
        self.model(id).filter(|m| m.enabled)
    }

    /// Default model: first enabled `is_default`, otherwise the first enabled entry.
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
    /// Version.
    pub policy_version: u64,
    /// Generation time.
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
}

/// Credit limits of one tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TierLimits {
    /// Daily limit in micro-credits.
    pub limit_daily_credits_micro: i64,
    /// Monthly limit in micro-credits.
    pub limit_monthly_credits_micro: i64,
}

/// Per-user credit allocation under a policy version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    /// User.
    pub user_id: Uuid,
    /// Policy version.
    pub policy_version: u64,
    /// `total` bucket limits (overall cap).
    pub standard: TierLimits,
    /// `tier:premium` bucket limits (premium sub-cap).
    pub premium: TierLimits,
}

/// License status of a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    /// `true` when the license is active.
    pub active: bool,
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod models_tests;
