//! Transport-agnostic models shared between the mini-chat gear and its plugins:
//! the policy snapshot (model catalog, kill switches), per-user limits and the
//! usage / audit event payloads.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use time::OffsetDateTime;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Model catalog
// ---------------------------------------------------------------------------

/// Rate-limit tier of a catalog model. Ordering of the downgrade cascade is
/// fixed: `premium` -> `standard`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelTier {
    Premium,
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

impl Serialize for ModelTier {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ModelTier {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        match raw.to_ascii_lowercase().as_str() {
            "premium" => Ok(Self::Premium),
            "standard" => Ok(Self::Standard),
            other => Err(serde::de::Error::custom(format!(
                "unknown model tier '{other}', expected 'premium' or 'standard'"
            ))),
        }
    }
}

/// Per-model token estimation budgets used for preflight reserve estimation
/// and context assembly. `minimal_generation_floor` is present for schema
/// compatibility but the gear reads it from its own configuration.
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

/// Provider API parameters of a catalog model. Each sampling parameter is
/// sent only when set.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelApiParams {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub stop: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelFeatures {
    pub streaming: bool,
    pub structured_output: bool,
}

/// Built-in tool support of a model. The `mcp` flag is parsed and unused.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "wire contract: one flag per tool"
)]
pub struct ModelToolSupport {
    pub web_search: bool,
    pub file_search: bool,
    pub image_generation: bool,
    pub code_interpreter: bool,
    pub mcp: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "wire contract: one flag per endpoint"
)]
pub struct ModelSupportedEndpoints {
    pub chat_completions: bool,
    pub responses: bool,
    pub embeddings: bool,
    pub image_generation: bool,
    pub audio_speech_generation: bool,
    pub audio_transcription: bool,
    pub audio_translation: bool,
}

fn default_max_file_size_mb() -> u32 {
    25
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelGeneralConfig {
    #[serde(rename = "type")]
    pub config_type: String,
    pub available_from: Option<String>,
    #[serde(default = "default_max_file_size_mb")]
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
            available_from: None,
            max_file_size_mb: default_max_file_size_mb(),
            api_params: ModelApiParams::default(),
            features: ModelFeatures::default(),
            tool_support: ModelToolSupport::default(),
            supported_endpoints: ModelSupportedEndpoints::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelPreference {
    pub is_default: bool,
    pub sort_order: i32,
}

fn default_web_search_context_size() -> String {
    "low".to_owned()
}

fn default_max_tool_calls() -> u32 {
    2
}

/// One entry of the model catalog delivered by the model policy plugin.
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
    /// Whether the model accepts image input (`VISION_INPUT`).
    #[must_use]
    pub fn supports_vision(&self) -> bool {
        self.multimodal_capabilities
            .iter()
            .any(|c| c.eq_ignore_ascii_case("VISION_INPUT"))
    }

    /// Whether the model is marked as the default model.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.preference.as_ref().is_some_and(|p| p.is_default)
    }

    #[must_use]
    pub fn tool_support(&self) -> &ModelToolSupport {
        &self.general_config.tool_support
    }
}

/// Global emergency flags carried by the policy snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "wire contract: one flag per kill switch"
)]
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
    /// Find a catalog entry by id regardless of its `enabled` flag.
    #[must_use]
    pub fn find(&self, model_id: &str) -> Option<&ModelCatalogEntry> {
        self.model_catalog.iter().find(|m| m.id == model_id)
    }

    /// Find an enabled catalog entry by id.
    #[must_use]
    pub fn find_enabled(&self, model_id: &str) -> Option<&ModelCatalogEntry> {
        self.find(model_id).filter(|m| m.enabled)
    }

    /// Iterate over enabled catalog entries in catalog order.
    pub fn enabled_models(&self) -> impl Iterator<Item = &ModelCatalogEntry> {
        self.model_catalog.iter().filter(|m| m.enabled)
    }

    /// Default model for new chats: first enabled `is_default` entry, else the
    /// first enabled entry. Tier is not considered.
    #[must_use]
    pub fn default_model(&self) -> Option<&ModelCatalogEntry> {
        self.enabled_models()
            .find(|m| m.is_default())
            .or_else(|| self.enabled_models().next())
    }
}

/// Current policy version as reported by the policy plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyVersionInfo {
    pub policy_version: u64,
}

/// Credit limits of one tier (micro-credits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierLimits {
    pub limit_daily_credits_micro: i64,
    pub limit_monthly_credits_micro: i64,
}

/// Per-user credit allocation. `standard` limits apply to the `total` bucket
/// (overall cap); `premium` limits apply to the `tier:premium` sub-cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    pub user_id: Uuid,
    pub policy_version: u64,
    pub standard: TierLimits,
    pub premium: TierLimits,
}

/// License status of a user (the gear does not call this in P1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    pub active: bool,
}

// ---------------------------------------------------------------------------
// Usage events
// ---------------------------------------------------------------------------

/// Provider-reported token usage (telemetry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[allow(clippy::struct_field_names, reason = "wire contract field names")]
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

/// Usage settlement event published through the usage outbox queue.
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

// ---------------------------------------------------------------------------
// Audit events
// ---------------------------------------------------------------------------

/// Tool-call counters of a turn (no code interpreter count).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolCalls {
    pub web_search_calls: u32,
    pub file_search_calls: u32,
}

/// Quota decision recorded in the audit event.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct QuotaPolicyDecision {
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PolicyDecisions {
    pub quota: QuotaPolicyDecision,
    /// Not populated in P1.
    #[serde(default)]
    pub license: String,
}

/// Audit event emitted when a turn is finalized
/// (`turn_completed` / `turn_failed`).
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
    pub tool_calls: ToolCalls,
    pub policy_decisions: PolicyDecisions,
    /// Empty in P1 (no content reaches the audit plugin).
    #[serde(default)]
    pub prompt: String,
    /// Empty in P1.
    #[serde(default)]
    pub response: String,
    /// Empty in P1.
    #[serde(default)]
    pub attachments: Vec<serde_json::Value>,
    /// Empty in P1.
    #[serde(default)]
    pub quota_scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

/// Audit event emitted by turn mutations (`turn_retry`, `turn_edit`,
/// `turn_delete`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnMutationAuditEvent {
    pub event_type: String,
    pub tenant_id: Uuid,
    pub actor_user_id: Uuid,
    pub chat_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_request_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_request_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

/// Any audit event delivered to the audit plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(
    clippy::large_enum_variant,
    reason = "events are short-lived and serialized immediately"
)]
pub enum MiniChatAuditEvent {
    Turn(TurnAuditEvent),
    TurnMutation(TurnMutationAuditEvent),
}

impl MiniChatAuditEvent {
    #[must_use]
    pub fn event_type(&self) -> &str {
        match self {
            Self::Turn(e) => &e.event_type,
            Self::TurnMutation(e) => &e.event_type,
        }
    }
}

/// Free-form key/value labels (kept for forward compatibility).
pub type Labels = BTreeMap<String, String>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_deserializes_case_insensitively() {
        let t: ModelTier = serde_json::from_str("\"Premium\"").unwrap();
        assert_eq!(t, ModelTier::Premium);
        let t: ModelTier = serde_json::from_str("\"standard\"").unwrap();
        assert_eq!(t, ModelTier::Standard);
        assert_eq!(
            serde_json::to_string(&ModelTier::Premium).unwrap(),
            "\"premium\""
        );
        assert!(serde_json::from_str::<ModelTier>("\"gold\"").is_err());
    }

    #[test]
    fn catalog_entry_defaults() {
        let e: ModelCatalogEntry =
            serde_json::from_value(serde_json::json!({"id": "m", "tier": "standard"})).unwrap();
        assert!(!e.enabled);
        assert_eq!(e.max_tool_calls, 2);
        assert_eq!(e.web_search_context_size, "low");
        assert_eq!(e.estimation_budgets, EstimationBudgets::default());
    }

    #[test]
    fn default_model_prefers_is_default() {
        let mk = |id: &str, enabled: bool, is_default: bool| ModelCatalogEntry {
            enabled,
            preference: Some(ModelPreference {
                is_default,
                sort_order: 0,
            }),
            ..serde_json::from_value(serde_json::json!({"id": id, "tier": "standard"})).unwrap()
        };
        let snap = PolicySnapshot {
            policy_version: 1,
            model_catalog: vec![
                mk("a", true, false),
                mk("b", false, true),
                mk("c", true, true),
            ],
            kill_switches: KillSwitches::default(),
        };
        assert_eq!(snap.default_model().map(|m| m.id.as_str()), Some("c"));
        let snap2 = PolicySnapshot {
            model_catalog: vec![mk("a", false, true), mk("b", true, false)],
            ..snap
        };
        assert_eq!(snap2.default_model().map(|m| m.id.as_str()), Some("b"));
    }

    #[test]
    fn usage_event_omits_user_and_turn_for_system_tasks() {
        let ev = UsageEvent {
            tenant_id: Uuid::nil(),
            user_id: None,
            chat_id: Uuid::nil(),
            turn_id: None,
            request_id: Uuid::nil(),
            effective_model: "m".into(),
            selected_model: "m".into(),
            terminal_state: "completed".into(),
            billing_outcome: "system_task".into(),
            usage: None,
            actual_credits_micro: 0,
            settlement_method: "none".into(),
            policy_version_applied: 0,
            web_search_calls: 0,
            code_interpreter_calls: 0,
            file_search_calls: 0,
            timestamp: OffsetDateTime::UNIX_EPOCH,
            requester_type: "system".into(),
            dedupe_key: "k".into(),
            system_task_type: Some("thread_summary_update".into()),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert!(v.get("user_id").is_none());
        assert!(v.get("turn_id").is_none());
        assert!(v.get("usage").unwrap().is_null());
        assert_eq!(v["system_task_type"], "thread_summary_update");
    }
}
