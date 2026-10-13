//! Policy snapshot, model catalog, user limits and the usage/audit event payloads.
//!
//! The shapes follow `gears/mini-chat/docs/DESIGN.md` §5.2 and Appendix A.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Rate-limit tier of a catalog model. Accepts `premium` / `Premium` on input.
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
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Premium => "premium",
            Self::Standard => "standard",
        }
    }
}

/// Web search context size hint sent with the `web_search` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Per-model token estimation budgets (DESIGN §5.2.1). Missing fields take defaults.
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
    /// Present on the catalog entry but not read (the gear configuration value applies).
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

/// Provider request parameters of a model.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiParams {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelFeatures {
    pub streaming: bool,
    pub structured_output: bool,
}

/// Built-in tool support flags of a model (`mcp` is parsed and unused, ADR-0006).
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent capability flags of the catalog contract"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelToolSupport {
    pub web_search: bool,
    pub file_search: bool,
    pub image_generation: bool,
    pub code_interpreter: bool,
    pub mcp: bool,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "independent capability flags of the catalog contract"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SupportedEndpoints {
    pub chat_completions: bool,
    pub responses: bool,
    pub embeddings: bool,
    pub image_generation: bool,
    pub audio_speech_generation: bool,
    pub audio_transcription: bool,
    pub audio_translation: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelGeneralConfig {
    #[serde(rename = "type", default)]
    pub config_type: String,
    #[serde(default)]
    pub available_from: String,
    #[serde(default)]
    pub max_file_size_mb: u32,
    #[serde(default)]
    pub api_params: ApiParams,
    #[serde(default)]
    pub features: ModelFeatures,
    #[serde(default)]
    pub tool_support: ModelToolSupport,
    #[serde(default)]
    pub supported_endpoints: SupportedEndpoints,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelPreference {
    pub is_default: bool,
    pub sort_order: i32,
}

const fn default_max_tool_calls() -> u32 {
    2
}

/// One model of the policy catalog (DESIGN §4 "Model Catalog Configuration").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    pub id: String,
    pub provider_model_id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: String,
    pub provider_id: String,
    #[serde(default)]
    pub provider_display_name: String,
    pub tier: ModelTier,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub multimodal_capabilities: Vec<String>,
    pub context_window: u32,
    pub max_output_tokens: u32,
    #[serde(default)]
    pub max_input_tokens: u32,
    pub input_tokens_credit_multiplier_micro: u64,
    pub output_tokens_credit_multiplier_micro: u64,
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
    pub general_config: ModelGeneralConfig,
    #[serde(default)]
    pub preference: Option<ModelPreference>,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub thread_summary_prompt: String,
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
        self.preference.is_some_and(|p| p.is_default)
    }

    #[must_use]
    pub const fn tool_support(&self) -> ModelToolSupport {
        self.general_config.tool_support
    }
}

/// Emergency flags of the policy snapshot. Every field is required (DESIGN B.1).
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent kill switches of the policy contract"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KillSwitches {
    pub disable_premium_tier: bool,
    pub force_standard_tier: bool,
    pub disable_web_search: bool,
    pub disable_file_search: bool,
    pub disable_images: bool,
    pub disable_code_interpreter: bool,
}

/// Immutable, versioned, shared policy configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    pub policy_version: u64,
    pub model_catalog: Vec<ModelCatalogEntry>,
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

    /// Default model algorithm: first enabled `is_default`, else first enabled (tier not considered).
    #[must_use]
    pub fn default_model(&self) -> Option<&ModelCatalogEntry> {
        self.model_catalog
            .iter()
            .find(|m| m.enabled && m.is_default())
            .or_else(|| self.model_catalog.iter().find(|m| m.enabled))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyVersionInfo {
    pub policy_version: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
}

/// Per-tier credit allocation (micro-credits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierLimits {
    pub limit_daily_credits_micro: i64,
    pub limit_monthly_credits_micro: i64,
}

/// Per-user limits: `standard` maps to bucket `total`, `premium` to bucket `tier:premium`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    pub user_id: Uuid,
    pub policy_version: u64,
    pub standard: TierLimits,
    pub premium: TierLimits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    pub active: bool,
}

/// Provider-reported token usage.
#[allow(
    clippy::struct_field_names,
    reason = "field names mirror the provider usage contract"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageTokens {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
}

/// Usage settlement event published through the usage outbox (DESIGN §5.6, Appendix A.3).
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

/// Tool call counts carried by the turn audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolCalls {
    pub web_search_calls: u32,
    pub file_search_calls: u32,
}

/// Quota decision recorded in the audit event.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaDecisionAudit {
    pub decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyDecisions {
    pub quota: QuotaDecisionAudit,
    /// Not populated in P1 (ADR-0009).
    pub license: Option<String>,
}

/// Audit event emitted for a finalized turn (`turn_completed` / `turn_failed`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    pub event_type: String,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub terminal_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub usage: Option<UsageTokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    pub tool_calls: ToolCalls,
    pub policy_decisions: PolicyDecisions,
    /// Empty in P1 (ADR-0009).
    #[serde(default)]
    pub prompt: String,
    /// Empty in P1 (ADR-0009).
    #[serde(default)]
    pub response: String,
    /// Empty in P1 (ADR-0009).
    #[serde(default)]
    pub attachments: Vec<serde_json::Value>,
    /// Empty in P1 (ADR-0009).
    #[serde(default)]
    pub quota_scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

/// Audit event emitted for a turn mutation (`turn_retry`, `turn_edit`, `turn_delete`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMutationAuditEvent {
    pub event_type: String,
    pub tenant_id: Uuid,
    pub actor_user_id: Uuid,
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

/// Envelope stored on the audit outbox queue.
#[allow(
    clippy::large_enum_variant,
    reason = "wire envelope; events are short-lived"
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditEvent {
    Turn(TurnAuditEvent),
    Mutation(TurnMutationAuditEvent),
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;
