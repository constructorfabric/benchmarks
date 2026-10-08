//! Contract models: model catalog, policy snapshot, user limits, usage and
//! audit events.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Rate-limit tier of a model. Accepts `premium`/`Premium` and
/// `standard`/`Standard` on input; serialized lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    #[serde(alias = "Standard", alias = "STANDARD")]
    Standard,
    #[serde(alias = "Premium", alias = "PREMIUM")]
    Premium,
}

impl ModelTier {
    /// Lowercase wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Premium => "premium",
        }
    }
}

/// Search context size hint for the provider web search tool.
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
    /// Lowercase wire name.
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

/// Feature flags of a model.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelFeatures {
    pub streaming: bool,
    pub structured_output: bool,
}

/// Built-in tool support of a model.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ModelToolSupport {
    pub web_search: bool,
    pub file_search: bool,
    pub image_generation: bool,
    pub code_interpreter: bool,
    /// Unused (MCP is deferred, ADR-0006).
    pub mcp: bool,
}

/// Provider endpoints supported by a model.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
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

/// General configuration block of a catalog entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelGeneralConfig {
    #[serde(rename = "type")]
    pub config_type: String,
    pub available_from: String,
    pub max_file_size_mb: u32,
    pub api_params: ApiParams,
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
            api_params: ApiParams::default(),
            features: ModelFeatures::default(),
            tool_support: ModelToolSupport::default(),
            supported_endpoints: ModelSupportedEndpoints::default(),
        }
    }
}

/// Catalog ordering/default preference.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelPreference {
    pub is_default: bool,
    pub sort_order: i32,
}

fn default_max_tool_calls() -> u32 {
    2
}

/// One entry of the model catalog (DESIGN "Model Catalog Configuration").
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
    pub input_tokens_credit_multiplier_micro: i64,
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
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub thread_summary_prompt: String,
}

impl ModelCatalogEntry {
    /// Whether the model accepts image input (`VISION_INPUT`).
    #[must_use]
    pub fn supports_vision(&self) -> bool {
        self.multimodal_capabilities
            .iter()
            .any(|c| c.eq_ignore_ascii_case("VISION_INPUT"))
    }

    /// Whether the entry is marked as the catalog default.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.preference.as_ref().is_some_and(|p| p.is_default)
    }
}

/// Global kill switches carried by the policy snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
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

impl PolicySnapshot {
    /// Looks up a catalog entry by id (enabled or not).
    #[must_use]
    pub fn find(&self, model_id: &str) -> Option<&ModelCatalogEntry> {
        self.model_catalog.iter().find(|m| m.id == model_id)
    }
}

/// Current policy version information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyVersionInfo {
    pub policy_version: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
}

/// Credit limits of one tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierLimits {
    pub limit_daily_credits_micro: i64,
    pub limit_monthly_credits_micro: i64,
}

/// Per-user credit allocation for a policy version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    pub user_id: Uuid,
    pub policy_version: u64,
    /// Limits of the `total` bucket (overall cap).
    pub standard: TierLimits,
    /// Limits of the `tier:premium` bucket (premium sub-cap).
    pub premium: TierLimits,
}

/// Result of the (unused) user license check.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    pub active: bool,
}

/// Token usage carried by usage and audit events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)]
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

/// Usage event enqueued to the usage outbox queue and published through the
/// model policy plugin (DESIGN §5.6, Appendix A.3).
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

/// Token usage in a turn audit event.
pub type AuditUsage = UsageTokens;

/// Latency metrics of a turn audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditLatency {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
}

/// Tool-call counts of a turn audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditToolCalls {
    pub web_search_calls: u32,
    pub file_search_calls: u32,
}

/// Quota decision recorded in a turn audit event.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditQuotaDecision {
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
}

/// Policy decisions recorded in a turn audit event.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditPolicyDecisions {
    pub quota: AuditQuotaDecision,
    /// Not populated in P1 (ADR-0009).
    #[serde(default)]
    pub license: Option<String>,
}

/// Audit event of a finalized turn (`turn_completed` / `turn_failed`).
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
    pub usage: Option<AuditUsage>,
    pub latency: AuditLatency,
    pub tool_calls: AuditToolCalls,
    pub policy_decisions: AuditPolicyDecisions,
    /// Empty in P1 (ADR-0009).
    pub prompt: String,
    /// Empty in P1 (ADR-0009).
    pub response: String,
    /// Empty in P1 (ADR-0009).
    pub attachments: Vec<serde_json::Value>,
    /// Empty in P1 (ADR-0009).
    #[serde(default)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_request_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_request_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Uuid>,
}

/// Any audit event delivered to the audit plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum AuditEvent {
    Turn(TurnAuditEvent),
    TurnMutation(TurnMutationAuditEvent),
}

impl AuditEvent {
    /// The `event_type` of the wrapped event.
    #[must_use]
    pub fn event_type(&self) -> &str {
        match self {
            Self::Turn(e) => &e.event_type,
            Self::TurnMutation(e) => &e.event_type,
        }
    }
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod models_tests;
