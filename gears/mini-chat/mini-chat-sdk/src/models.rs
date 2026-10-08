//! Transport-agnostic models of the mini-chat plugin contracts.
//!
//! The policy snapshot types mirror the CCM policy API (DESIGN Appendix A):
//! nested types ignore unknown keys; required fields have no default.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

// ════════════════════════════════════════════════════════════════════════════
// Model catalog
// ════════════════════════════════════════════════════════════════════════════

/// Rate-limit tier of a catalog model. Determines the downgrade cascade order
/// (`premium` → `standard`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelTier {
    #[serde(rename = "premium", alias = "Premium", alias = "PREMIUM")]
    Premium,
    #[serde(rename = "standard", alias = "Standard", alias = "STANDARD")]
    Standard,
}

impl ModelTier {
    /// Lowercase wire name (`premium` / `standard`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Premium => "premium",
            Self::Standard => "standard",
        }
    }
}

/// Search context size hint of the `web_search` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchContextSize {
    #[default]
    #[serde(alias = "Low", alias = "LOW")]
    Low,
    #[serde(alias = "Medium", alias = "MEDIUM")]
    Medium,
    #[serde(alias = "High", alias = "HIGH")]
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

/// Per-model token estimation budgets (DESIGN §5.2.1).
///
/// `minimal_generation_floor` is carried for compatibility but not read by the
/// gear; the floor comes from the gear configuration.
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

/// Provider request parameters of a model. Each sampling parameter is sent
/// only when set.
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
    /// Extra top-level request body keys (merged by the OpenAI-compatible
    /// adapters, except keys the request controls).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// Model feature flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelFeatures {
    pub streaming: bool,
    pub structured_output: bool,
}

/// Built-in tool support of a model. Gates the tool list, the tool guards, the
/// reserve surcharges and the daily tool quota checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)] // wire contract: one flag per tool / switch
pub struct ModelToolSupport {
    pub web_search: bool,
    pub file_search: bool,
    pub image_generation: bool,
    pub code_interpreter: bool,
    /// Unused (MCP is deferred, ADR-0006).
    pub mcp: bool,
}

/// Provider endpoints a model supports (informational).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)] // wire contract: one flag per endpoint
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
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelGeneralConfig {
    #[serde(rename = "type", default)]
    pub config_type: String,
    #[serde(default)]
    pub available_from: String,
    /// Per-model upload cap in MiB (applies to documents and images).
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

/// UI preference of a catalog model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelPreference {
    pub is_default: bool,
    pub sort_order: i32,
}

/// One model of the policy catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    /// Stable internal model id (`model_id` in the REST API, `chats.model`).
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
    /// Maximum input tokens per request; `0` means no separate limit.
    pub max_input_tokens: u32,
    /// Micro-credits per 1,000,000 input tokens.
    pub input_tokens_credit_multiplier_micro: u64,
    /// Micro-credits per 1,000,000 output tokens.
    pub output_tokens_credit_multiplier_micro: u64,
    #[serde(default)]
    pub multiplier_display: String,
    #[serde(default)]
    pub estimation_budgets: EstimationBudgets,
    /// Top-k chunks per `file_search` call.
    pub max_num_results: u32,
    #[serde(default)]
    pub web_search_context_size: WebSearchContextSize,
    /// Built-in tool calls the provider may make per request.
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    pub general_config: ModelGeneralConfig,
    #[serde(default)]
    pub preference: Option<ModelPreference>,
}

const fn default_max_tool_calls() -> u32 {
    2
}

/// Capability flag that enables image input.
pub const VISION_INPUT: &str = "VISION_INPUT";

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
}

/// Global kill switches of a policy snapshot. Every field is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // wire contract: one flag per kill switch
pub struct KillSwitches {
    pub disable_premium_tier: bool,
    pub force_standard_tier: bool,
    pub disable_web_search: bool,
    pub disable_file_search: bool,
    pub disable_images: bool,
    pub disable_code_interpreter: bool,
}

/// Immutable, versioned shared policy configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    pub policy_version: u64,
    pub model_catalog: Vec<ModelCatalogEntry>,
    pub kill_switches: KillSwitches,
}

impl PolicySnapshot {
    /// Look a model up by id regardless of its `enabled` flag.
    #[must_use]
    pub fn find_model(&self, model_id: &str) -> Option<&ModelCatalogEntry> {
        self.model_catalog.iter().find(|m| m.id == model_id)
    }

    /// Look an enabled model up by id.
    #[must_use]
    pub fn find_enabled_model(&self, model_id: &str) -> Option<&ModelCatalogEntry> {
        self.model_catalog
            .iter()
            .find(|m| m.id == model_id && m.enabled)
    }

    /// Default model for new chats: the first enabled entry with
    /// `preference.is_default`, otherwise the first enabled entry.
    #[must_use]
    pub fn default_model(&self) -> Option<&ModelCatalogEntry> {
        self.model_catalog
            .iter()
            .find(|m| m.enabled && m.is_default())
            .or_else(|| self.model_catalog.iter().find(|m| m.enabled))
    }
}

/// Current policy version of a user.
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

/// Per-user credit allocation bound to a policy version.
///
/// `standard` limits apply to the `total` bucket (overall cap); `premium`
/// limits apply to the `tier:premium` bucket (premium sub-cap).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    pub user_id: Uuid,
    pub policy_version: u64,
    pub standard: TierLimits,
    pub premium: TierLimits,
}

/// License status of a user (the gear does not call it in P1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    pub active: bool,
}

// ════════════════════════════════════════════════════════════════════════════
// Usage events
// ════════════════════════════════════════════════════════════════════════════

/// Provider-reported token usage (cache and reasoning counts are subsets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_field_names)] // wire contract field names
pub struct UsageTokens {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
}

/// Usage settlement event published through the usage outbox queue and
/// handed to the model policy plugin's `publish_usage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageEvent {
    pub tenant_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<Uuid>,
    pub request_id: Uuid,
    pub effective_model: String,
    pub selected_model: String,
    /// `completed` | `failed` | `cancelled`.
    pub terminal_state: String,
    /// `completed` | `failed` | `aborted` | `system_task`.
    pub billing_outcome: String,
    /// Provider-reported usage; `null` when the provider reported none.
    pub usage: Option<UsageTokens>,
    /// Committed credits; authoritative billing amount.
    pub actual_credits_micro: i64,
    /// `actual` | `estimated` | `released` | `none`.
    pub settlement_method: String,
    pub policy_version_applied: u64,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
    pub file_search_calls: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    /// `user` | `system`.
    pub requester_type: String,
    pub dedupe_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_task_type: Option<String>,
}

// ════════════════════════════════════════════════════════════════════════════
// Audit events
// ════════════════════════════════════════════════════════════════════════════

/// Latency metrics of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditLatency {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
}

/// Tool-call counts of a turn (no code interpreter count, DESIGN §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolCalls {
    pub web_search_calls: u64,
    pub file_search_calls: u64,
}

/// Quota decision recorded on a turn audit event.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct QuotaPolicyDecision {
    /// `allow` | `downgrade` | `unknown` (orphan watchdog).
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
}

/// Policy decisions of a turn.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PolicyDecisions {
    pub quota: QuotaPolicyDecision,
    /// Not populated in P1 (ADR-0009).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
}

/// Structured audit event of a finalized turn (`turn_completed` /
/// `turn_failed`). Prompt, response, attachments, license and quota scope are
/// empty in P1 (ADR-0009).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    pub event_type: String,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub tenant_id: Uuid,
    pub requester_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub terminal_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageTokens>,
    pub latency: AuditLatency,
    pub tool_calls: ToolCalls,
    pub policy_decisions: PolicyDecisions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    #[serde(default)]
    pub attachments: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// Audit event of a turn mutation (`turn_retry`, `turn_edit`, `turn_delete`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMutationAuditEvent {
    pub event_type: String,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub tenant_id: Uuid,
    pub actor_user_id: Uuid,
    pub chat_id: Uuid,
    /// Retry/edit: `request_id` of the replaced turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_request_id: Option<Uuid>,
    /// Retry/edit: `request_id` of the new turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_request_id: Option<Uuid>,
    /// Delete: `request_id` of the deleted turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Uuid>,
}

/// Audit event delivered to the audit plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AuditEvent {
    Turn(Box<TurnAuditEvent>),
    Mutation(TurnMutationAuditEvent),
}

impl AuditEvent {
    /// Event type name (`turn_completed`, `turn_failed`, `turn_retry`, ...).
    #[must_use]
    pub fn event_type(&self) -> &str {
        match self {
            Self::Turn(e) => &e.event_type,
            Self::Mutation(e) => &e.event_type,
        }
    }
}
