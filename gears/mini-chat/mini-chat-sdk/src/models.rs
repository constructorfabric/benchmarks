//! Shared data types of the mini-chat SDK.
//!
//! Field-level definitions are normative in the mini-chat DESIGN (section 5.2
//! and Appendix B.2). The nested types (`ModelCatalogEntry`,
//! `EstimationBudgets`, `ModelGeneralConfig`, `TierLimits`) ignore unknown keys;
//! [`KillSwitches`] requires every field.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

// ── Model catalog ────────────────────────────────────────────────────────────

/// Rate-limit tier of a catalog model. Serialized lowercase; the capitalised
/// spelling of the sample config (`Premium`, `Standard`) is accepted on input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    #[serde(alias = "Standard")]
    Standard,
    #[serde(alias = "Premium")]
    Premium,
}

/// Search context size hint of the `web_search` tool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchContextSize {
    #[default]
    Low,
    Medium,
    High,
}

/// Preference flags of a catalog entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPreference {
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub sort_order: i32,
}

/// Per-model token estimation budgets (preflight only).
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
    /// Present on the entry but not read: the floor comes from the gear configuration.
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

/// Optional provider API parameters; each is sent only when set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiParams {
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub frequency_penalty: Option<f64>,
    #[serde(default)]
    pub presence_penalty: Option<f64>,
    pub stop: Vec<String>,
    #[serde(default)]
    pub extra_body: Option<serde_json::Value>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

/// Model feature flags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFeatures {
    pub streaming: bool,
    pub structured_output: bool,
}

/// Built-in tool support of a model. `mcp` is parsed and unused (ADR-0006).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors the policy wire shape"
)]
pub struct ToolSupport {
    pub web_search: bool,
    pub file_search: bool,
    pub image_generation: bool,
    pub code_interpreter: bool,
    pub mcp: bool,
}

/// Provider endpoints a model supports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors the policy wire shape"
)]
pub struct SupportedEndpoints {
    pub chat_completions: bool,
    pub responses: bool,
    pub embeddings: bool,
    pub image_generation: bool,
    pub audio_speech_generation: bool,
    pub audio_transcription: bool,
    pub audio_translation: bool,
}

/// General model configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelGeneralConfig {
    #[serde(rename = "type")]
    pub config_type: String,
    pub available_from: String,
    pub max_file_size_mb: u32,
    pub api_params: ApiParams,
    pub features: ModelFeatures,
    pub tool_support: ToolSupport,
    pub supported_endpoints: SupportedEndpoints,
}

/// One entry of the model catalog of a [`PolicySnapshot`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    /// Stable internal model id (the API `model_id`).
    pub id: String,
    /// Model name on the provider side. Never exposed through the Models API.
    pub provider_model_id: String,
    pub display_name: String,
    /// Key of the `providers.<id>` entry that serves the model.
    pub provider_id: String,
    /// UI-only provider name; not read by the gear.
    pub provider_display_name: String,
    pub tier: ModelTier,
    /// Defaults to `false` when omitted; a disabled entry is excluded everywhere.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub multiplier_display: String,
    #[serde(default)]
    pub multimodal_capabilities: Vec<String>,
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub max_input_tokens: u32,
    pub input_tokens_credit_multiplier_micro: u64,
    pub output_tokens_credit_multiplier_micro: u64,
    #[serde(default)]
    pub estimation_budgets: EstimationBudgets,
    pub max_num_results: u32,
    #[serde(default)]
    pub web_search_context_size: WebSearchContextSize,
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    pub general_config: ModelGeneralConfig,
    #[serde(default)]
    pub preference: Option<ModelPreference>,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub thread_summary_prompt: String,
}

fn default_max_tool_calls() -> u32 {
    2
}

/// Global kill switches. Every field is required: a snapshot from another
/// plugin with a missing or renamed switch fails to deserialize instead of
/// reading as `false`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the six kill switches are the contract"
)]
pub struct KillSwitches {
    pub disable_premium_tier: bool,
    pub force_standard_tier: bool,
    pub disable_web_search: bool,
    pub disable_file_search: bool,
    pub disable_images: bool,
    pub disable_code_interpreter: bool,
}

/// The immutable, versioned, shared policy snapshot. It never carries user limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    pub policy_version: u64,
    pub model_catalog: Vec<ModelCatalogEntry>,
    pub kill_switches: KillSwitches,
}

// ── Limits ───────────────────────────────────────────────────────────────────

/// Per-tier credit limits in micro-credits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierLimits {
    pub limit_daily_credits_micro: u64,
    pub limit_monthly_credits_micro: u64,
}

/// Per-user allocation under one policy version. `standard` maps to the quota
/// bucket `total` and `premium` to the bucket `tier:premium`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    pub user_id: Uuid,
    pub policy_version: u64,
    pub standard: TierLimits,
    pub premium: TierLimits,
}

/// Result of `check_user_license` (never called by the gear).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    pub active: bool,
}

// ── Usage events ─────────────────────────────────────────────────────────────

/// Provider-reported token usage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_field_names,
    reason = "wire contract field names (DESIGN A.3)"
)]
pub struct UsageTokens {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub reasoning_tokens: u64,
}

/// Usage settlement event handed to `publish_usage` (the usage outbox payload).
/// `user_id` and `turn_id` are absent for system tasks.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_task_type: Option<String>,
}

// ── Audit events ─────────────────────────────────────────────────────────────

/// Quota decision of a turn. `Unknown` is reported only by the orphan watchdog path, which
/// has no preflight decision at hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuotaDecision {
    Allow,
    Downgrade,
    Unknown,
}

/// Turn finalization audit event. `prompt`, `response`, `attachments`,
/// `license` and `quota_scope` stay empty in P1 and no redaction runs (ADR-0009).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub requester_type: String,
    pub selected_model: String,
    pub effective_model: String,
    pub terminal_state: String,
    #[serde(default)]
    pub error_code: Option<String>,
    pub usage: Option<UsageTokens>,
    pub latency_ms: u64,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
    pub file_search_calls: u32,
    pub quota_decision: QuotaDecision,
    #[serde(default)]
    pub downgrade_from: Option<String>,
    #[serde(default)]
    pub downgrade_reason: Option<String>,
    /// OpenTelemetry trace id; absent when no span is active.
    #[serde(default)]
    pub trace_id: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub response: String,
    #[serde(default)]
    pub attachments: Vec<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub quota_scope: Option<String>,
}

/// Audit event of a turn retry or edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMutationAuditEvent {
    pub actor_user_id: Uuid,
    pub chat_id: Uuid,
    pub original_request_id: Uuid,
    pub new_request_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

/// Audit event of a turn delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnDeleteAuditEvent {
    pub actor_user_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

/// Event delivered to the audit plugin. There is no `turn_cancelled` value and
/// chat deletion is not audited.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
#[allow(
    clippy::enum_variant_names,
    reason = "wire contract event types (turn_completed, ...)"
)]
pub enum AuditEvent {
    TurnCompleted(TurnAuditEvent),
    TurnFailed(TurnAuditEvent),
    TurnRetry(TurnMutationAuditEvent),
    TurnEdit(TurnMutationAuditEvent),
    TurnDelete(TurnDeleteAuditEvent),
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod models_tests;
