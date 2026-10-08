//! Transport-agnostic models shared between the mini-chat gear and its
//! plugins: the policy snapshot (model catalog, kill switches), per-user
//! limits and the usage event published after every settled turn.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Rate-limit tier of a catalog model. Serialized lowercase; the capitalized
/// spelling (`Premium`, `Standard`) is accepted on input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    #[serde(alias = "Standard", alias = "STANDARD")]
    Standard,
    #[serde(alias = "Premium", alias = "PREMIUM")]
    Premium,
}

impl ModelTier {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Premium => "premium",
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
    /// Present on the catalog entry but not read by the gear (the floor comes
    /// from the gear configuration).
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

/// Optional provider request parameters of a catalog model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelFeatures {
    pub streaming: bool,
    pub structured_output: bool,
}

/// Built-in tool support flags. `mcp` is parsed and unused (ADR-0006).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelToolSupport {
    pub web_search: bool,
    pub file_search: bool,
    pub image_generation: bool,
    pub code_interpreter: bool,
    pub mcp: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelGeneralConfig {
    #[serde(rename = "type")]
    pub config_type: String,
    pub available_from: String,
    /// Per-model upload cap in MiB.
    pub max_file_size_mb: u32,
    pub api_params: ModelApiParams,
    pub features: ModelFeatures,
    pub tool_support: ModelToolSupport,
    pub supported_endpoints: ModelSupportedEndpoints,
}

impl Default for ModelGeneralConfig {
    fn default() -> Self {
        Self {
            config_type: String::new(),
            available_from: String::new(),
            max_file_size_mb: 25,
            api_params: ModelApiParams::default(),
            features: ModelFeatures::default(),
            tool_support: ModelToolSupport::default(),
            supported_endpoints: ModelSupportedEndpoints::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelPreference {
    pub is_default: bool,
    pub sort_order: i32,
}

/// Search context size hint for the `web_search` tool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchContextSize {
    #[default]
    Low,
    Medium,
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

fn default_max_tool_calls() -> u32 {
    2
}

/// One entry of the model catalog (`PolicySnapshot.model_catalog`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    pub id: String,
    #[serde(default)]
    pub provider_model_id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
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
    #[serde(default)]
    pub context_window: u32,
    #[serde(default)]
    pub max_output_tokens: u32,
    #[serde(default)]
    pub max_input_tokens: u32,
    #[serde(default)]
    pub input_tokens_credit_multiplier_micro: i64,
    #[serde(default)]
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
            .any(|c| c.eq_ignore_ascii_case("VISION_INPUT"))
    }

    #[must_use]
    pub fn is_default(&self) -> bool {
        self.preference.as_ref().is_some_and(|p| p.is_default)
    }
}

/// Global kill switches. Every field is required on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(default)]
    pub kill_switches: KillSwitches,
}

impl PolicySnapshot {
    #[must_use]
    pub fn find_model(&self, id: &str) -> Option<&ModelCatalogEntry> {
        self.model_catalog.iter().find(|m| m.id == id)
    }

    #[must_use]
    pub fn find_enabled_model(&self, id: &str) -> Option<&ModelCatalogEntry> {
        self.model_catalog.iter().find(|m| m.id == id && m.enabled)
    }

    /// Default model for new chats: the first enabled `is_default` entry,
    /// otherwise the first enabled entry.
    #[must_use]
    pub fn default_model(&self) -> Option<&ModelCatalogEntry> {
        self.model_catalog
            .iter()
            .find(|m| m.enabled && m.is_default())
            .or_else(|| self.model_catalog.iter().find(|m| m.enabled))
    }
}

/// Current policy version as reported by the plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyVersionInfo {
    pub policy_version: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
}

/// Credit limits of one tier (micro-credits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierLimits {
    pub limit_daily_credits_micro: i64,
    pub limit_monthly_credits_micro: i64,
}

/// Per-user allocation tied to a policy version. `standard` limits apply to
/// the `total` bucket (global ceiling), `premium` to `tier:premium`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    pub user_id: Uuid,
    pub policy_version: u64,
    pub standard: TierLimits,
    pub premium: TierLimits,
}

/// Token usage carried by a usage event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTokens {
    pub input_tokens: i64,
    pub output_tokens: i64,
    #[serde(default)]
    pub cache_read_input_tokens: i64,
    #[serde(default)]
    pub cache_write_input_tokens: i64,
    #[serde(default)]
    pub reasoning_tokens: i64,
}

/// Usage event serialized into the usage outbox queue and handed to the
/// policy plugin's `publish_usage` (DESIGN §5.6, Appendix A.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageEvent {
    pub tenant_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    pub chat_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<Uuid>,
    pub request_id: Uuid,
    pub effective_model: String,
    pub selected_model: String,
    pub terminal_state: String,
    pub billing_outcome: String,
    pub usage: Option<UsageTokens>,
    pub actual_credits_micro: i64,
    pub settlement_method: String,
    pub policy_version_applied: u64,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
    pub file_search_calls: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub requester_type: String,
    pub dedupe_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_task_type: Option<String>,
}

/// Result of `check_user_license` (unused by the gear, ADR-0008).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    pub active: bool,
}
