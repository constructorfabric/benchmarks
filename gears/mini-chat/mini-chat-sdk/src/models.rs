//! Transport-agnostic model types for the mini-chat gear.
//!
//! These types define the policy contract between the mini-chat gear and its
//! model-policy plugin (`mini-chat-model-policy-plugin`): the versioned
//! [`PolicySnapshot`] (model catalog + kill switches), the per-user
//! [`UserLimits`] allocation, and the [`UsageEvent`] settlement payload that
//! the gear publishes through the transactional outbox.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Model rate-limit tier. Determines the downgrade cascade order
/// (`Premium` -> `Standard`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelTier {
    /// Premium tier: stricter limits, higher cost.
    Premium,
    /// Standard tier: higher limits, lower cost.
    Standard,
}

impl ModelTier {
    /// The bucket suffix of this tier's sub-cap, if any.
    ///
    /// Only the premium tier has a dedicated bucket (`tier:premium`);
    /// standard-tier enforcement uses the global `total` bucket.
    #[must_use]
    pub fn bucket(self) -> Option<&'static str> {
        match self {
            Self::Premium => Some("tier:premium"),
            Self::Standard => None,
        }
    }
}

/// Search context size hint for the `web_search` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchContextSize {
    /// Small search context.
    #[default]
    Low,
    /// Medium search context.
    Medium,
    /// Large search context.
    High,
}

/// Per-model token estimation budgets (preflight reserve, admission control
/// and context assembly). All fields default per the gear specification.
///
/// `minimal_generation_floor` is carried for forward compatibility but is not
/// read by the gear: the applied floor comes from the gear configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EstimationBudgets {
    /// Conservative bytes-per-token ratio for text estimation.
    pub bytes_per_token_conservative: u32,
    /// Constant overhead for protocol/framing tokens.
    pub fixed_overhead_tokens: u32,
    /// Percentage safety margin applied to text estimation.
    pub safety_margin_pct: u32,
    /// Tokens per image for the vision surcharge.
    pub image_token_budget: u32,
    /// Fixed token overhead when the chat has at least one ready document
    /// and the model gets the `file_search` tool.
    pub tool_surcharge_tokens: u32,
    /// Fixed token overhead when the request enables web search and the
    /// model supports it.
    pub web_search_surcharge_tokens: u32,
    /// Fixed token overhead when the chat has at least one ready
    /// code-interpreter attachment and the model supports it.
    pub code_interpreter_surcharge_tokens: u32,
    /// Minimum output token charge for estimated settlement. Carried on the
    /// catalog entry but not read by the gear (gear config wins).
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

/// Optional provider API parameters of a model (`general_config.api_params`).
///
/// `temperature`, `top_p`, `frequency_penalty` and `presence_penalty` are sent
/// only when set (reasoning models reject them). `extra_body` keys are merged
/// into the top level of the provider request body by the `OpenAI`-family
/// adapters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiParams {
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Nucleus sampling parameter.
    pub top_p: Option<f64>,
    /// Frequency penalty.
    pub frequency_penalty: Option<f64>,
    /// Presence penalty.
    pub presence_penalty: Option<f64>,
    /// Stop sequences.
    pub stop: Vec<String>,
    /// Extra provider-specific request body keys merged at the top level.
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    /// Reasoning effort hint (provider-specific).
    pub reasoning_effort: Option<String>,
}

/// Model feature flags (`general_config.features`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFeatures {
    /// Streaming support.
    pub streaming: bool,
    /// Structured output support.
    pub structured_output: bool,
}

/// Tool support flags of a model (`general_config.tool_support`).
///
/// The `mcp` flag is parsed but unused (MCP support is deferred, ADR-0006).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // wire contract fixed by the design
pub struct ModelToolSupport {
    /// Web search tool support.
    pub web_search: bool,
    /// File search (vector store retrieval) support.
    pub file_search: bool,
    /// Image generation support.
    pub image_generation: bool,
    /// Code interpreter tool support.
    pub code_interpreter: bool,
    /// MCP tool support (unused; MCP is deferred).
    pub mcp: bool,
}

/// Provider endpoints a model supports (`general_config.supported_endpoints`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // wire contract fixed by the design
pub struct SupportedEndpoints {
    /// Chat Completions endpoint.
    pub chat_completions: bool,
    /// `Responses` endpoint.
    pub responses: bool,
    /// Embeddings endpoint.
    pub embeddings: bool,
    /// Image generation endpoint.
    pub image_generation: bool,
    /// Audio speech generation endpoint.
    pub audio_speech_generation: bool,
    /// Audio transcription endpoint.
    pub audio_transcription: bool,
    /// Audio translation endpoint.
    pub audio_translation: bool,
}

/// General model configuration (`general_config`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelGeneralConfig {
    /// General config type tag.
    #[serde(rename = "type")]
    pub config_type: String,
    /// Availability timestamp (informational).
    pub available_from: String,
    /// Per-model upload cap in MiB (documents and images).
    pub max_file_size_mb: u32,
    /// Optional provider API parameters.
    pub api_params: ApiParams,
    /// Feature flags.
    pub features: ModelFeatures,
    /// Tool support flags.
    pub tool_support: ModelToolSupport,
    /// Supported provider endpoints.
    pub supported_endpoints: SupportedEndpoints,
}

/// Model preference metadata (`preference`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPreference {
    /// Whether this is the default model for new chats.
    pub is_default: bool,
    /// Catalog sort order (UI hint).
    pub sort_order: i32,
}

/// One entry of the policy model catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    /// Stable internal model identifier used in the REST API.
    pub id: String,
    /// Model name on the provider side (sent in LLM API requests).
    pub provider_model_id: String,
    /// User-facing name.
    pub display_name: String,
    /// Key of the `providers.<id>` entry that serves the model.
    pub provider_id: String,
    /// Provider name for UI (not returned by the Models API).
    pub provider_display_name: String,
    /// Rate-limit tier.
    pub tier: ModelTier,
    /// Context window in tokens.
    pub context_window: u32,
    /// Model output cap.
    pub max_output_tokens: u32,
    /// Maximum input tokens per request (`0` = no separate limit).
    pub max_input_tokens: u32,
    /// Micro-credits per 1M input tokens (> 0).
    pub input_tokens_credit_multiplier_micro: i64,
    /// Micro-credits per 1M output tokens (> 0).
    pub output_tokens_credit_multiplier_micro: i64,
    /// Top-k chunks per `file_search` call.
    pub max_num_results: u32,
    /// General model configuration.
    pub general_config: ModelGeneralConfig,
    /// User-facing help text.
    #[serde(default)]
    pub description: String,
    /// UI icon reference.
    #[serde(default)]
    pub icon: String,
    /// Human-readable credit multiplier (e.g. `"1x"`).
    #[serde(default)]
    pub multiplier_display: String,
    /// System prompt sent on every request for this model.
    #[serde(default)]
    pub system_prompt: String,
    /// System prompt of the thread-summary call for this model.
    #[serde(default)]
    pub thread_summary_prompt: String,
    /// Disabled models are invisible in the Models API and rejected by
    /// `POST /v1/chats`.
    #[serde(default)]
    pub enabled: bool,
    /// Capability flags, e.g. `VISION_INPUT`.
    #[serde(default)]
    pub multimodal_capabilities: Vec<String>,
    /// Per-model token estimation budgets.
    #[serde(default)]
    pub estimation_budgets: EstimationBudgets,
    /// Search context size hint for web search.
    #[serde(default)]
    pub web_search_context_size: WebSearchContextSize,
    /// Maximum built-in tool calls per provider request.
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    /// Preference metadata.
    #[serde(default)]
    pub preference: Option<ModelPreference>,
}

const fn default_max_tool_calls() -> u32 {
    2
}

impl ModelCatalogEntry {
    /// Whether the model accepts image input (`VISION_INPUT` capability).
    #[must_use]
    pub fn supports_vision(&self) -> bool {
        self.multimodal_capabilities
            .iter()
            .any(|c| c == "VISION_INPUT")
    }
}

/// Global emergency kill switches carried by the policy snapshot.
///
/// Every field is required on deserialization: a snapshot with a missing or
/// renamed switch fails to deserialize instead of reading it as `false`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // wire contract fixed by the design
pub struct KillSwitches {
    /// Premium-tier models must not be used.
    pub disable_premium_tier: bool,
    /// All requests must use the standard tier.
    pub force_standard_tier: bool,
    /// Web search is disabled.
    pub disable_web_search: bool,
    /// File search is disabled.
    pub disable_file_search: bool,
    /// Image uploads and image inputs are disabled.
    pub disable_images: bool,
    /// Code interpreter is disabled.
    pub disable_code_interpreter: bool,
}

/// An immutable, versioned policy snapshot: model catalog + kill switches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    /// Monotonic policy version.
    pub policy_version: i64,
    /// Model catalog.
    pub model_catalog: Vec<ModelCatalogEntry>,
    /// Global kill switches.
    pub kill_switches: KillSwitches,
}

impl PolicySnapshot {
    /// Look up a catalog entry by model id, regardless of `enabled`.
    #[must_use]
    pub fn model(&self, id: &str) -> Option<&ModelCatalogEntry> {
        self.model_catalog.iter().find(|m| m.id == id)
    }

    /// Look up an enabled catalog entry by model id.
    #[must_use]
    pub fn enabled_model(&self, id: &str) -> Option<&ModelCatalogEntry> {
        self.model_catalog.iter().find(|m| m.id == id && m.enabled)
    }
}

/// Per-tier credit limits in micro-credits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierLimits {
    /// Daily limit (UTC calendar day).
    pub limit_daily_credits_micro: i64,
    /// Monthly limit (UTC calendar month).
    pub limit_monthly_credits_micro: i64,
}

/// Per-user credit allocation keyed by policy version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    /// User the allocation applies to.
    pub user_id: Uuid,
    /// Policy version the limits were derived under.
    pub policy_version: i64,
    /// Limits of the `total` bucket (global ceiling across all tiers).
    pub standard: TierLimits,
    /// Limits of the `tier:premium` bucket (premium sub-cap).
    pub premium: TierLimits,
}

/// Provider-reported token usage (source of truth for settlement).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)] // provider field names are the contract
pub struct UsageTokens {
    /// Input tokens.
    pub input_tokens: i64,
    /// Output tokens.
    pub output_tokens: i64,
    /// Input tokens served from the provider cache (subset of input).
    #[serde(default)]
    pub cache_read_input_tokens: i64,
    /// Input tokens written to the provider cache (subset of input).
    #[serde(default)]
    pub cache_write_input_tokens: i64,
    /// Output tokens consumed by reasoning (subset of output).
    #[serde(default)]
    pub reasoning_tokens: i64,
}

/// The settlement/usage event published through the outbox queue
/// (`mini-chat.usage_snapshot`) and handed to the model policy plugin's
/// `publish_usage`.
///
/// `user_id` and `turn_id` are omitted for system tasks; `system_task_type`
/// is omitted for user turns. `usage` is `null` when the provider reported
/// no usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageEvent {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Requesting user (absent for system tasks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    /// Chat the turn belongs to.
    pub chat_id: Uuid,
    /// Internal turn id (absent for system tasks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<Uuid>,
    /// Turn correlation key.
    pub request_id: Uuid,
    /// Model actually used for the turn.
    pub effective_model: String,
    /// Model selected at chat creation.
    pub selected_model: String,
    /// Internal terminal state (`completed` | `failed` | `cancelled`).
    pub terminal_state: String,
    /// Billing outcome (`completed` | `failed` | `aborted` | `system_task`).
    pub billing_outcome: String,
    /// Provider-reported usage; `null` when unknown.
    pub usage: Option<UsageTokens>,
    /// Authoritative billing debit in micro-credits.
    pub actual_credits_micro: i64,
    /// Settlement method (`actual` | `estimated` | `released` | `none`).
    pub settlement_method: String,
    /// Policy version the turn was settled under.
    pub policy_version_applied: i64,
    /// Completed web search calls in the turn.
    pub web_search_calls: u32,
    /// Completed code interpreter calls in the turn.
    pub code_interpreter_calls: u32,
    /// Completed file search calls in the turn (provider-native
    /// `file_search` plus `search_knowledge` retrievals).
    pub file_search_calls: u32,
    /// Event timestamp (RFC 3339).
    pub timestamp: String,
    /// `user` or `system`.
    pub requester_type: String,
    /// Stable idempotency key (`{tenant}/{turn_or_task}/{request}` as
    /// 32-char lowercase hex UUIDs).
    pub dedupe_key: String,
    /// System task type (`thread_summary_update`); omitted for user turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_task_type: Option<String>,
}

/// Tool-call counters carried by a turn audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolCalls {
    /// Completed web search calls.
    pub web_search_calls: u64,
    /// Completed file search calls (provider-native `file_search` plus
    /// `search_knowledge` retrievals).
    pub file_search_calls: u64,
}

/// The quota policy decision recorded in a turn audit event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaAuditDecision {
    /// `allow`, `downgrade` or `unknown` (orphan watchdog path).
    pub decision: String,
    /// Model downgraded from, when a downgrade occurred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    /// Why the downgrade occurred, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
}

/// Policy decisions recorded in a turn audit event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecisions {
    /// Quota decision.
    pub quota: QuotaAuditDecision,
    /// License gate outcome (empty in P1).
    pub license: String,
}

/// A structured audit event for a completed/failed turn or a turn mutation.
///
/// P1 content scope (ADR-0009): `prompt`, `response`, `attachments`,
/// `license` and `quota_scope` are empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    /// `turn_completed` | `turn_failed` | `turn_retry` | `turn_edit` |
    /// `turn_delete`.
    pub event_type: String,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Owning user (requester of the turn), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    /// Acting user for mutation events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_user_id: Option<Uuid>,
    /// Chat the turn belongs to.
    pub chat_id: Uuid,
    /// Internal turn id, when a turn exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<Uuid>,
    /// Turn correlation key.
    pub request_id: Uuid,
    /// Original request id for retry/edit events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_request_id: Option<Uuid>,
    /// New request id for retry/edit events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_request_id: Option<Uuid>,
    /// Model selected at chat creation.
    pub selected_model: String,
    /// Model actually used for the turn.
    pub effective_model: String,
    /// Provider-reported token usage, when known.
    pub usage: Option<UsageTokens>,
    /// Turn latency in milliseconds, when measured.
    pub latency_ms: Option<u64>,
    /// Tool-call counters.
    pub tool_calls: ToolCalls,
    /// Policy decisions.
    pub policy_decisions: PolicyDecisions,
    /// Terminal error code, when the turn failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Prompt content (empty in P1).
    pub prompt: String,
    /// Response content (empty in P1).
    pub response: String,
    /// Attachment metadata (empty in P1).
    pub attachments: Vec<serde_json::Value>,
    /// Quota scope (empty in P1).
    pub quota_scope: String,
    /// OpenTelemetry trace id of the finalizing request, when active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Event timestamp (RFC 3339).
    pub timestamp: String,
}
