//! Policy, catalog, limits, usage and audit models.
//!
//! Field names follow `gears/mini-chat/docs/DESIGN.md` §5.2.1, Appendix A.1/A.3
//! and §3.2 "System Task Attribution Rules". Nested catalog types ignore unknown
//! keys (a newer policy plugin may add fields); [`KillSwitches`] requires every
//! field so a renamed switch fails to deserialize instead of reading `false`.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Model catalog
// ---------------------------------------------------------------------------

/// Model tier. Accepts `premium`/`standard` and `Premium`/`Standard`;
/// serializes lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    #[serde(alias = "Premium")]
    Premium,
    #[serde(alias = "Standard")]
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

/// Per-model token estimation budgets (D§5.2.1). Missing fields take defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Provider request parameters of a catalog model. Every field is optional
/// (`stop` defaults to empty).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ApiParams {
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
    /// Extra provider body fields merged into the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// Feature flags of a catalog model (missing flags are `false`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelFeatures {
    pub streaming: bool,
    pub structured_output: bool,
}

/// Built-in tool support of a catalog model (missing flags are `false`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ModelToolSupport {
    pub web_search: bool,
    pub file_search: bool,
    pub image_generation: bool,
    pub code_interpreter: bool,
    /// Parsed and unused (MCP is not implemented, ADR-0006).
    pub mcp: bool,
}

/// Provider endpoints a catalog model supports (missing flags are `false`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct SupportedEndpoints {
    pub chat_completions: bool,
    pub responses: bool,
    pub embeddings: bool,
    pub image_generation: bool,
    pub audio_speech_generation: bool,
    pub audio_transcription: bool,
    pub audio_translation: bool,
}

/// General (provider-facing) configuration of a catalog model. The object
/// itself is required on a catalog entry; every field inside it defaults
/// when absent (strings empty, flags `false`, `api_params.stop` empty,
/// `max_file_size_mb` 0) so a catalog that omits one still loads.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelGeneralConfig {
    #[serde(rename = "type")]
    pub r#type: String,
    pub available_from: String,
    /// Per-model upload cap in MiB; `0` = no per-model cap (only the gear's
    /// `rag` limits apply).
    pub max_file_size_mb: u32,
    pub api_params: ApiParams,
    pub features: ModelFeatures,
    pub tool_support: ModelToolSupport,
    pub supported_endpoints: SupportedEndpoints,
}

/// UI preference of a catalog model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPreference {
    pub is_default: bool,
    pub sort_order: i32,
}

fn default_web_search_context_size() -> String {
    "low".to_owned()
}

const fn default_max_tool_calls() -> u32 {
    2
}

/// One model of the policy snapshot catalog (D§5.2.1, Appendix A.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    pub id: String,
    pub provider_model_id: String,
    pub display_name: String,
    pub provider_id: String,
    /// UI-only; never a routing key.
    pub provider_display_name: String,
    pub tier: ModelTier,
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub max_input_tokens: u32,
    /// Micro-credits per 1M input tokens.
    pub input_tokens_credit_multiplier_micro: u64,
    /// Micro-credits per 1M output tokens.
    pub output_tokens_credit_multiplier_micro: u64,
    /// `file_search` top-k.
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

// ---------------------------------------------------------------------------
// Policy snapshot and limits
// ---------------------------------------------------------------------------

/// Global kill switches carried by the policy snapshot. Every field is required.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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

/// Current policy version (`GetCurrentPolicyVersion`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyVersionInfo {
    pub policy_version: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
}

/// Per-tier credit limits in micro-credits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierLimits {
    pub limit_daily_credits_micro: i64,
    pub limit_monthly_credits_micro: i64,
}

/// Per-user credit allocation keyed by policy version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    pub user_id: Uuid,
    pub policy_version: u64,
    /// Limits of the `total` bucket.
    pub standard: TierLimits,
    /// Limits of the `tier:premium` bucket.
    pub premium: TierLimits,
}

/// Result of `check_user_license` (never called by the gear).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    pub active: bool,
}

// ---------------------------------------------------------------------------
// Usage events
// ---------------------------------------------------------------------------

/// Provider-reported token usage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_field_names,
    reason = "wire field names are fixed by the contract"
)]
pub struct UsageTokens {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
}

/// Terminal state of a turn (or system task).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    Completed,
    Failed,
    Cancelled,
}

/// Billing outcome of a usage event (D§5.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingOutcome {
    Completed,
    Failed,
    Aborted,
    SystemTask,
}

/// How the debited credits were settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementMethod {
    Actual,
    Estimated,
    Released,
    None,
}

/// Who requested the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequesterType {
    User,
    System,
}

/// Usage event handed to `publish_usage` (D§5.6, Appendix A.3).
///
/// `user_id`, `turn_id` and `system_task_type` are omitted when `None`;
/// `usage` serializes as `null` when `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    pub terminal_state: TerminalState,
    pub billing_outcome: BillingOutcome,
    pub usage: Option<UsageTokens>,
    pub actual_credits_micro: i64,
    pub settlement_method: SettlementMethod,
    pub policy_version_applied: u64,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
    pub file_search_calls: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub requester_type: RequesterType,
    pub dedupe_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_task_type: Option<String>,
}

// ---------------------------------------------------------------------------
// Audit events
// ---------------------------------------------------------------------------

/// `event_type` of a turn finalization audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnAuditEventType {
    TurnCompleted,
    TurnFailed,
}

/// `event_type` of a turn mutation audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[allow(
    clippy::enum_variant_names,
    reason = "wire names are fixed by the contract"
)]
#[serde(rename_all = "snake_case")]
pub enum TurnMutationAuditEventType {
    TurnRetry,
    TurnEdit,
    TurnDelete,
}

/// Turn latency figures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditLatency {
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
}

/// Tool-call counts (no code interpreter count, D§3.9 audit content).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditToolCalls {
    pub web_search_calls: u32,
    pub file_search_calls: u32,
}

/// Quota decision of a turn (`allow`, `downgrade`, `unknown`, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditQuotaDecision {
    pub decision: String,
    pub downgrade_from: Option<String>,
    pub downgrade_reason: Option<String>,
}

/// Policy decisions of a turn. `license` is always `null` in P1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditPolicyDecisions {
    pub quota: AuditQuotaDecision,
    pub license: Option<serde_json::Value>,
}

/// Turn finalization audit event. `prompt`, `response`, `attachments` and
/// `quota_scope` are empty in P1 (ADR-0009).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    pub event_type: TurnAuditEventType,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub terminal_state: TerminalState,
    pub error_code: Option<String>,
    pub usage: Option<UsageTokens>,
    pub latency_ms: AuditLatency,
    pub tool_calls: AuditToolCalls,
    pub policy_decisions: AuditPolicyDecisions,
    pub prompt: String,
    pub response: String,
    pub attachments: Vec<serde_json::Value>,
    pub quota_scope: Option<String>,
    pub trace_id: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

/// Turn mutation audit event (`turn_retry` / `turn_edit` carry
/// `original_request_id` + `new_request_id`; `turn_delete` carries `request_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMutationAuditEvent {
    pub event_type: TurnMutationAuditEventType,
    pub actor_user_id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_request_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_request_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

/// Audit event delivered to the audit plugin; its JSON is the audit outbox payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::large_enum_variant,
    reason = "plain wire type, moved once per event"
)]
#[serde(untagged)]
pub enum MiniChatAuditEvent {
    Turn(TurnAuditEvent),
    Mutation(TurnMutationAuditEvent),
}

impl MiniChatAuditEvent {
    /// The `event_type` wire string.
    #[must_use]
    pub const fn event_type(&self) -> &'static str {
        match self {
            Self::Turn(e) => match e.event_type {
                TurnAuditEventType::TurnCompleted => "turn_completed",
                TurnAuditEventType::TurnFailed => "turn_failed",
            },
            Self::Mutation(e) => match e.event_type {
                TurnMutationAuditEventType::TurnRetry => "turn_retry",
                TurnMutationAuditEventType::TurnEdit => "turn_edit",
                TurnMutationAuditEventType::TurnDelete => "turn_delete",
            },
        }
    }
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;
