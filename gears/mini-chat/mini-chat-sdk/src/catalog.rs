//! Model catalog contract (DESIGN 3.3 "Model Catalog Configuration", Appendix B.2.2).
//!
//! A [`ModelCatalogEntry`] is one model as served by the model-policy plugin.
//! Unknown keys are ignored on every type of this module so a plugin may
//! carry extra attributes without breaking the gear.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Model tier. Accepts the capitalised spelling of the operator YAML
/// (`Premium`) and the lowercase spelling of the policy API (`premium`);
/// always serialises lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    /// Premium tier (own sub-cap, first in the downgrade cascade).
    #[serde(alias = "Premium")]
    Premium,
    /// Standard tier (overall cap).
    #[serde(alias = "Standard")]
    Standard,
}

/// `web_search` context size sent to the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchContextSize {
    /// Low context (default).
    #[default]
    Low,
    /// Medium context.
    Medium,
    /// High context.
    High,
}

/// Per-model token estimation budgets used by the preflight reserve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EstimationBudgets {
    /// Conservative bytes-per-token ratio.
    pub bytes_per_token_conservative: u32,
    /// Fixed per-request overhead tokens.
    pub fixed_overhead_tokens: u32,
    /// Safety margin in percent.
    pub safety_margin_pct: u32,
    /// Token budget per image input.
    pub image_token_budget: u32,
    /// Surcharge when any tool is sent.
    pub tool_surcharge_tokens: u32,
    /// Surcharge for the `web_search` tool.
    pub web_search_surcharge_tokens: u32,
    /// Surcharge for the `code_interpreter` tool.
    pub code_interpreter_surcharge_tokens: u32,
    /// Lower bound of the generation budget.
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

/// Sampling and provider parameters of a model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelApiParams {
    /// Sampling temperature.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Nucleus sampling.
    #[serde(default)]
    pub top_p: Option<f64>,
    /// Frequency penalty.
    #[serde(default)]
    pub frequency_penalty: Option<f64>,
    /// Presence penalty.
    #[serde(default)]
    pub presence_penalty: Option<f64>,
    /// Stop sequences (the key is required, the list may be empty).
    pub stop: Vec<String>,
    /// Provider specific request body fragment.
    #[serde(default)]
    pub extra_body: Option<serde_json::Value>,
    /// Reasoning effort hint.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

/// Feature flags of a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFeatures {
    /// Supports streaming.
    pub streaming: bool,
    /// Supports structured output.
    pub structured_output: bool,
}

/// Tool support of a model (the gate for the tools sent to the provider).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors the catalog wire format"
)]
pub struct ModelToolSupport {
    /// `web_search` tool.
    pub web_search: bool,
    /// `file_search` tool.
    pub file_search: bool,
    /// Image generation tool.
    pub image_generation: bool,
    /// `code_interpreter` tool.
    pub code_interpreter: bool,
    /// MCP tools (parsed, unused in P1).
    pub mcp: bool,
}

/// Provider endpoints a model supports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors the catalog wire format"
)]
pub struct ModelSupportedEndpoints {
    /// Chat completions endpoint.
    pub chat_completions: bool,
    /// Responses endpoint.
    pub responses: bool,
    /// Embeddings endpoint.
    pub embeddings: bool,
    /// Image generation endpoint.
    pub image_generation: bool,
    /// Speech generation endpoint.
    pub audio_speech_generation: bool,
    /// Transcription endpoint.
    pub audio_transcription: bool,
    /// Translation endpoint.
    pub audio_translation: bool,
}

/// General model configuration (all keys are required).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelGeneralConfig {
    /// Model type label (may be empty).
    #[serde(rename = "type")]
    pub type_: String,
    /// Instant from which the model is available (RFC 3339).
    #[serde(with = "time::serde::rfc3339")]
    pub available_from: OffsetDateTime,
    /// Maximum upload size accepted for the model, in MB.
    pub max_file_size_mb: u32,
    /// Sampling and provider parameters.
    pub api_params: ModelApiParams,
    /// Feature flags.
    pub features: ModelFeatures,
    /// Tool support.
    pub tool_support: ModelToolSupport,
    /// Supported provider endpoints.
    pub supported_endpoints: ModelSupportedEndpoints,
}

/// Default-model preference of a catalog entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPreference {
    /// The entry is the default model for new chats.
    pub is_default: bool,
    /// Ordering hint (smaller first).
    pub sort_order: i32,
}

fn default_max_tool_calls() -> u32 {
    2
}

/// One model of the policy snapshot's catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    /// Public model identifier (`model_id` on the API).
    pub id: String,
    /// Model identifier on the provider side.
    pub provider_model_id: String,
    /// Display name.
    pub display_name: String,
    /// Description (empty when none).
    #[serde(default)]
    pub description: String,
    /// Key of the `providers.<id>` entry that serves the model.
    pub provider_id: String,
    /// Provider display name.
    pub provider_display_name: String,
    /// Icon reference.
    #[serde(default)]
    pub icon: String,
    /// Model tier.
    pub tier: ModelTier,
    /// Whether the model is selectable.
    #[serde(default)]
    pub enabled: bool,
    /// System prompt of the model.
    #[serde(default)]
    pub system_prompt: String,
    /// Thread-summary system prompt override.
    #[serde(default)]
    pub thread_summary_prompt: String,
    /// Multimodal capabilities (for example `VISION_INPUT`).
    #[serde(default)]
    pub multimodal_capabilities: Vec<String>,
    /// Context window in tokens.
    pub context_window: u32,
    /// Maximum output tokens.
    pub max_output_tokens: u32,
    /// Maximum input tokens (0 = no separate limit).
    pub max_input_tokens: u32,
    /// Credits (micro) per input token multiplier, `1..=10_000_000_000`.
    pub input_tokens_credit_multiplier_micro: u64,
    /// Credits (micro) per output token multiplier, `1..=10_000_000_000`.
    pub output_tokens_credit_multiplier_micro: u64,
    /// Human readable multiplier (for example `3x`).
    #[serde(default)]
    pub multiplier_display: String,
    /// Estimation budgets.
    #[serde(default)]
    pub estimation_budgets: EstimationBudgets,
    /// Top-k of `file_search`.
    pub max_num_results: u32,
    /// `web_search` context size.
    #[serde(default)]
    pub web_search_context_size: WebSearchContextSize,
    /// Built-in tool calls per provider request.
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    /// General configuration.
    pub general_config: ModelGeneralConfig,
    /// Default-model preference.
    #[serde(default)]
    pub preference: Option<ModelPreference>,
}
