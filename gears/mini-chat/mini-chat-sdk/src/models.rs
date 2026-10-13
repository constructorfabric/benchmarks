//! Policy, catalog and usage models shared between the gear and its plugins.
//!
//! The nested catalog types do not reject unknown keys: unknown keys inside a catalog entry
//! or a limits object are ignored. [`KillSwitches`] is the exception: every switch is
//! required, so a snapshot from a plugin with a missing or renamed switch fails to
//! deserialize instead of being read as `false`.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Model catalog
// ---------------------------------------------------------------------------

/// Rate-limit tier of a model. The downgrade cascade is `premium` then `standard`.
///
/// Accepts `"Premium"`, `"premium"`, `"Standard"` and `"standard"`; serializes in lower case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelTier {
    /// Premium-tier model: stricter limits.
    #[serde(rename = "premium", alias = "Premium")]
    Premium,
    /// Standard-tier model: separate, higher limits.
    #[serde(rename = "standard", alias = "Standard")]
    Standard,
}

impl ModelTier {
    /// The lower-case wire name of the tier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Premium => "premium",
            Self::Standard => "standard",
        }
    }
}

/// Search context size hint of the web search tool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebSearchContextSize {
    /// Small context (default).
    #[default]
    #[serde(rename = "low", alias = "Low")]
    Low,
    /// Medium context.
    #[serde(rename = "medium", alias = "Medium")]
    Medium,
    /// Large context.
    #[serde(rename = "high", alias = "High")]
    High,
}

/// Per-model token estimation budgets (preflight reserve, admission control, context assembly).
///
/// `minimal_generation_floor` is present for catalog compatibility but is not read: the floor
/// comes from the gear configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EstimationBudgets {
    /// Conservative bytes-per-token ratio for text estimation.
    pub bytes_per_token_conservative: u32,
    /// Constant overhead for protocol and framing tokens.
    pub fixed_overhead_tokens: u32,
    /// Percentage safety margin applied to the text estimate.
    pub safety_margin_pct: u32,
    /// Tokens budgeted per image.
    pub image_token_budget: u32,
    /// Fixed overhead when `file_search` is offered.
    pub tool_surcharge_tokens: u32,
    /// Fixed overhead when `web_search` is offered.
    pub web_search_surcharge_tokens: u32,
    /// Fixed overhead when `code_interpreter` is offered.
    pub code_interpreter_surcharge_tokens: u32,
    /// Present on the entry but not read (see the type docs).
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

/// Request parameters sent to the provider with every call of a model.
///
/// Every field is required except `extra_body` and `reasoning_effort`; the sampling parameters
/// are optional values (`null`/absent means "not sent").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelApiParams {
    /// Sampling temperature.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Nucleus sampling parameter.
    #[serde(default)]
    pub top_p: Option<f64>,
    /// Frequency penalty.
    #[serde(default)]
    pub frequency_penalty: Option<f64>,
    /// Presence penalty.
    #[serde(default)]
    pub presence_penalty: Option<f64>,
    /// Stop sequences.
    pub stop: Vec<String>,
    /// Keys merged into the top level of the provider request body.
    #[serde(default)]
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    /// Reasoning effort hint.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

/// Model feature flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFeatures {
    /// The model streams its output.
    pub streaming: bool,
    /// The model supports structured output.
    pub structured_output: bool,
}

/// Tools the model can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per catalog tool; the shape mirrors the policy snapshot wire format"
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
    /// MCP tools (parsed, unused).
    pub mcp: bool,
}

/// Provider endpoints the model can be called through.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per provider endpoint; the shape mirrors the policy snapshot wire format"
)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelGeneralConfig {
    /// Configuration type tag.
    #[serde(rename = "type")]
    pub config_type: String,
    /// When the model becomes available (RFC 3339 text).
    pub available_from: String,
    /// Per-model upload size cap in MiB.
    pub max_file_size_mb: u32,
    /// Provider request parameters.
    pub api_params: ModelApiParams,
    /// Feature flags.
    pub features: ModelFeatures,
    /// Tool support flags.
    pub tool_support: ModelToolSupport,
    /// Supported provider endpoints.
    pub supported_endpoints: ModelSupportedEndpoints,
}

/// Default-model preference of a catalog entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPreference {
    /// The entry is the default model for new chats.
    #[serde(default)]
    pub is_default: bool,
    /// Presentation order.
    #[serde(default)]
    pub sort_order: i32,
}

fn default_max_tool_calls() -> u32 {
    2
}

/// One entry of the model catalog.
///
/// Required: `id`, `provider_model_id`, `display_name`, `provider_id`, `provider_display_name`,
/// `tier`, `context_window`, `max_output_tokens`, `max_input_tokens`, both credit multipliers,
/// `max_num_results` and `general_config`. The other fields default when absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    /// Stable internal model identifier (`model_id` in the REST API, `chats.model`).
    pub id: String,
    /// Model name on the provider side; sent in provider requests.
    pub provider_model_id: String,
    /// User-facing name.
    pub display_name: String,
    /// User-facing help text.
    #[serde(default)]
    pub description: String,
    /// Key of the `providers.<id>` entry of the gear configuration that serves the model.
    pub provider_id: String,
    /// User-facing provider name (UI only, never a routing key).
    pub provider_display_name: String,
    /// Icon reference.
    #[serde(default)]
    pub icon: String,
    /// Rate-limit tier.
    pub tier: ModelTier,
    /// Disabled models are hidden from the Models API and rejected by chat creation.
    #[serde(default)]
    pub enabled: bool,
    /// System instructions sent with every request of this model.
    #[serde(default)]
    pub system_prompt: String,
    /// System prompt of the thread-summary call (read from the summary model's entry).
    #[serde(default)]
    pub thread_summary_prompt: String,
    /// Capability flags such as `VISION_INPUT` and `RAG`.
    #[serde(default)]
    pub multimodal_capabilities: Vec<String>,
    /// Maximum context window in tokens.
    pub context_window: u32,
    /// Model output cap in tokens.
    pub max_output_tokens: u32,
    /// Maximum input tokens per request; `0` means no separate limit.
    pub max_input_tokens: u32,
    /// Micro-credits per one million input tokens.
    pub input_tokens_credit_multiplier_micro: u64,
    /// Micro-credits per one million output tokens.
    pub output_tokens_credit_multiplier_micro: u64,
    /// Human-readable multiplier (for example `"2x"`).
    #[serde(default)]
    pub multiplier_display: String,
    /// Token estimation budgets.
    #[serde(default)]
    pub estimation_budgets: EstimationBudgets,
    /// Top-k chunks per `file_search` call.
    pub max_num_results: u32,
    /// Web search context size hint.
    #[serde(default)]
    pub web_search_context_size: WebSearchContextSize,
    /// Maximum built-in tool calls per provider request.
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    /// General configuration.
    pub general_config: ModelGeneralConfig,
    /// Default-model preference.
    #[serde(default)]
    pub preference: Option<ModelPreference>,
}

impl ModelCatalogEntry {
    /// True when the entry is flagged as the default model.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.preference
            .is_some_and(|preference| preference.is_default)
    }
}

// ---------------------------------------------------------------------------
// Policy snapshot, kill switches and limits
// ---------------------------------------------------------------------------

/// Global emergency flags. Every field is required.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per kill switch; the shape is the policy snapshot wire format"
)]
pub struct KillSwitches {
    /// Premium-tier models must not be used.
    pub disable_premium_tier: bool,
    /// Every request uses the standard tier.
    pub force_standard_tier: bool,
    /// Requests enabling web search are rejected.
    pub disable_web_search: bool,
    /// `file_search` tool calls are skipped.
    pub disable_file_search: bool,
    /// Image uploads and image inputs are rejected.
    pub disable_images: bool,
    /// The `code_interpreter` tool is omitted.
    pub disable_code_interpreter: bool,
}

/// An immutable, versioned policy: the model catalog and the kill switches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    /// Monotonic policy version.
    pub policy_version: i64,
    /// Model catalog.
    pub model_catalog: Vec<ModelCatalogEntry>,
    /// Global kill switches.
    pub kill_switches: KillSwitches,
}

/// The current policy version as reported by a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyVersionInfo {
    /// Monotonic policy version.
    pub policy_version: i64,
}

/// Credit limits of one tier, in micro-credits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierLimits {
    /// Daily limit.
    pub limit_daily_credits_micro: i64,
    /// Monthly limit.
    pub limit_monthly_credits_micro: i64,
}

/// Per-user credit allocation under one policy version.
///
/// `standard` limits are the overall cap (quota bucket `total`); `premium` limits are the
/// premium-only sub-cap (bucket `tier:premium`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    /// The user the limits belong to.
    pub user_id: Uuid,
    /// The policy version the limits were derived under.
    pub policy_version: i64,
    /// Standard-tier (overall) limits.
    pub standard: TierLimits,
    /// Premium-tier limits.
    pub premium: TierLimits,
}

/// Result of the user license check.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    /// Whether the user holds an active license.
    pub active: bool,
}

// ---------------------------------------------------------------------------
// Usage events
// ---------------------------------------------------------------------------

/// Terminal state of a turn as reported in usage events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageTerminalState {
    /// The turn completed.
    Completed,
    /// The turn failed.
    Failed,
    /// The turn was cancelled.
    Cancelled,
}

/// How the usage of a turn was billed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingOutcome {
    /// Billed on actual provider usage.
    Completed,
    /// The turn failed after the provider started.
    Failed,
    /// The stream ended without a terminal event.
    Aborted,
    /// Background task usage, not charged to a user.
    SystemTask,
}

/// How the committed credits were derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementMethod {
    /// From provider-reported usage.
    Actual,
    /// From the persisted estimate.
    Estimated,
    /// The reserve was released without a debit.
    Released,
    /// No settlement (system tasks).
    None,
}

/// Who requested the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequesterType {
    /// An end user.
    User,
    /// A background system task.
    System,
}

/// Provider-reported token usage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_field_names,
    reason = "the field names are the wire format of the usage payload"
)]
pub struct TokenUsage {
    /// Input tokens.
    pub input_tokens: i64,
    /// Output tokens.
    pub output_tokens: i64,
    /// Input tokens served from the provider cache (subset of `input_tokens`).
    #[serde(default)]
    pub cache_read_input_tokens: i64,
    /// Input tokens written to the provider cache (subset of `input_tokens`).
    #[serde(default)]
    pub cache_write_input_tokens: i64,
    /// Output tokens consumed by reasoning (subset of `output_tokens`).
    #[serde(default)]
    pub reasoning_tokens: i64,
}

/// Settlement event handed to `publish_usage`. At-least-once delivery: consumers deduplicate
/// by `dedupe_key`.
///
/// `user_id` and `turn_id` are absent (not `null`) for system tasks; `system_task_type` is
/// absent for user turns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageEvent {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Requesting user (absent for system tasks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    /// Chat the work belongs to.
    pub chat_id: Uuid,
    /// Turn identifier (absent for system tasks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<Uuid>,
    /// Request identifier (server-generated for system tasks).
    pub request_id: Uuid,
    /// Model actually used.
    pub effective_model: String,
    /// Model selected for the chat.
    pub selected_model: String,
    /// Terminal state of the turn.
    pub terminal_state: UsageTerminalState,
    /// Billing outcome.
    pub billing_outcome: BillingOutcome,
    /// Provider-reported usage; `null` when the provider reported none.
    pub usage: Option<TokenUsage>,
    /// Committed credits in micro-credits (authoritative for billing).
    pub actual_credits_micro: i64,
    /// Settlement method.
    pub settlement_method: SettlementMethod,
    /// Policy version applied to the turn.
    pub policy_version_applied: i64,
    /// Web search calls of the turn.
    pub web_search_calls: u32,
    /// Code interpreter calls of the turn.
    pub code_interpreter_calls: u32,
    /// File search calls of the turn.
    pub file_search_calls: u32,
    /// Event time.
    #[serde(with = "crate::time_serde")]
    pub timestamp: OffsetDateTime,
    /// Requester type.
    pub requester_type: RequesterType,
    /// Idempotency key: `{tenant}/{turn}/{request}` for turns and
    /// `{tenant}/{system_task_type}/{system_request}` for system tasks (simple UUID forms).
    pub dedupe_key: String,
    /// System task type (absent for user turns).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_task_type: Option<String>,
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;
