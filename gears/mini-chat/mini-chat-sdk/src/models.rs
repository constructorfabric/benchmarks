//! Shared models of the mini-chat policy contract (DESIGN §5.2, Appendix B.2).
//!
//! The nested catalog types do not reject unknown keys (Appendix B.1): unknown
//! keys inside a catalog entry or a limits object are ignored.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Rate-limit tier of a catalog model. Accepts `premium` / `Premium` spellings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// Present for compatibility; the gear reads the floor from its own config.
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

/// Provider API parameters sent with every request (each only when set).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelApiParams {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub stop: Vec<String>,
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelFeatures {
    pub streaming: bool,
    pub structured_output: bool,
}

/// Built-in tool support flags of a model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)] // flag set fixed by the plugin contract
pub struct ModelToolSupport {
    pub web_search: bool,
    pub file_search: bool,
    pub image_generation: bool,
    pub code_interpreter: bool,
    /// Unused (MCP is deferred, ADR-0006).
    pub mcp: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)] // flag set fixed by the plugin contract
pub struct ModelSupportedEndpoints {
    pub chat_completions: bool,
    pub responses: bool,
    pub embeddings: bool,
    pub image_generation: bool,
    pub audio_speech_generation: bool,
    pub audio_transcription: bool,
    pub audio_translation: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelGeneralConfig {
    #[serde(rename = "type")]
    pub config_type: String,
    pub available_from: Option<String>,
    /// Per-model upload cap in MiB; `None` or `0` means no per-model cap.
    pub max_file_size_mb: Option<u32>,
    pub api_params: ModelApiParams,
    pub features: ModelFeatures,
    pub tool_support: ModelToolSupport,
    pub supported_endpoints: ModelSupportedEndpoints,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelPreference {
    pub is_default: bool,
    pub sort_order: i32,
}

fn default_max_tool_calls() -> u32 {
    2
}
fn default_max_num_results() -> u32 {
    5
}
fn default_web_search_context_size() -> String {
    "medium".to_owned()
}
fn default_tier() -> ModelTier {
    ModelTier::Standard
}

/// One model of the policy catalog (DESIGN "Model Catalog Configuration").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    pub id: String,
    #[serde(default)]
    pub provider_model_id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub provider_id: String,
    #[serde(default)]
    pub provider_display_name: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default = "default_tier")]
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
    #[serde(default = "default_max_num_results")]
    pub max_num_results: u32,
    #[serde(default = "default_web_search_context_size")]
    pub web_search_context_size: String,
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
        self.multimodal_capabilities
            .iter()
            .any(|c| c.eq_ignore_ascii_case("VISION_INPUT"))
    }

    #[must_use]
    pub fn is_default(&self) -> bool {
        self.preference.as_ref().is_some_and(|p| p.is_default)
    }

    #[must_use]
    pub fn tool_support(&self) -> &ModelToolSupport {
        &self.general_config.tool_support
    }
}

/// Global kill switches. Every field is required on the SDK type so a renamed
/// switch fails to deserialize instead of reading as `false`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // flag set fixed by the plugin contract
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

    /// Default model: first enabled `is_default` entry, else first enabled entry.
    #[must_use]
    pub fn default_model(&self) -> Option<&ModelCatalogEntry> {
        self.model_catalog
            .iter()
            .find(|m| m.enabled && m.is_default())
            .or_else(|| self.model_catalog.iter().find(|m| m.enabled))
    }
}

/// Per-period credit limits of one tier (micro-credits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierLimits {
    pub limit_daily_credits_micro: i64,
    pub limit_monthly_credits_micro: i64,
}

/// Per-user allocation tied to a policy version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    pub user_id: Uuid,
    pub policy_version: u64,
    /// Limits of the `total` bucket (overall cap).
    pub standard: TierLimits,
    /// Limits of the `tier:premium` bucket (premium sub-cap).
    pub premium: TierLimits,
}

/// Provider-reported token usage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)] // provider wire names
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

/// Usage event published through the usage outbox queue (DESIGN §5.6–5.9).
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

/// License status returned by `check_user_license` (never called by the gear in P1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    pub active: bool,
}
