//! Transport-agnostic models shared by the mini-chat gear and its plugins.
//!
//! - **Policy snapshot** (DESIGN section 5.2.1, "Model Catalog Configuration"):
//!   [`PolicySnapshot`], [`ModelCatalogEntry`] and its nested configuration,
//!   [`KillSwitches`].
//! - **Per-user allocation**: [`UserLimits`], [`TierLimits`].
//! - **Usage publication** (DESIGN sections 5.6-5.7, "System Task Attribution
//!   Rules"): [`UsageEvent`], [`UsageTokens`] and the outbox dedupe key helpers
//!   [`dedupe_key_for_turn`] / [`dedupe_key_for_system_task`].
//! - **Audit** (DESIGN section 3.9 "Audit Events for Turn Mutations",
//!   ADR-0009): [`AuditEvent`], [`TurnAuditEvent`], [`TurnMutationAuditEvent`].
//! - **License check** (DESIGN section 5.2.2): [`UserLicenseStatus`].

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use time::OffsetDateTime;
use uuid::Uuid;

// ── Model catalog ────────────────────────────────────────────────────────

/// Rate-limit tier of a catalog model; also the downgrade cascade order
/// (`premium` → `standard`).
///
/// Deserializes from `Premium` / `premium` / `Standard` / `standard` (the
/// capitalized form is the one used in `config/mini-chat.yaml`) and serializes
/// as lowercase `premium` / `standard`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    /// Premium tier: stricter limits, quota bucket `tier:premium`.
    #[serde(alias = "Premium")]
    Premium,
    /// Standard tier: its limits are the overall cap (quota bucket `total`).
    #[serde(alias = "Standard")]
    Standard,
}

impl ModelTier {
    /// Lowercase wire name of the tier (`premium` / `standard`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Premium => "premium",
            Self::Standard => "standard",
        }
    }
}

/// Per-model token estimation budgets (DESIGN section 5.2.1 "Estimation
/// Budgets Source", sections 5.5.4-5.5.6).
///
/// Every field has a default, so a partial object fills the missing fields
/// from [`EstimationBudgets::default`]; unknown keys are ignored.
/// `minimal_generation_floor` is carried but not read by the gear (the floor
/// comes from the gear configuration).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EstimationBudgets {
    /// Conservative bytes-per-token ratio for text estimation (default 4).
    pub bytes_per_token_conservative: u32,
    /// Constant protocol/framing overhead in tokens (default 100).
    pub fixed_overhead_tokens: u32,
    /// Percentage safety margin applied to the text estimate (default 10).
    pub safety_margin_pct: u32,
    /// Tokens reserved per image input (default 1000).
    pub image_token_budget: u32,
    /// Surcharge when the model gets `file_search` (default 500).
    pub tool_surcharge_tokens: u32,
    /// Surcharge when web search is enabled and supported (default 500).
    pub web_search_surcharge_tokens: u32,
    /// Surcharge when the model gets `code_interpreter` (default 1000).
    pub code_interpreter_surcharge_tokens: u32,
    /// Minimal generation floor (default 50); not read from the catalog.
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

/// Built-in tool support flags of a catalog model (`general_config.tool_support`).
///
/// All flags default to `false`; unknown keys are ignored. The `mcp` flag is
/// unused (ADR-0006).
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors the catalog `tool_support` object, one independent flag per tool"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelToolSupport {
    /// Provider web search tool is allowed.
    pub web_search: bool,
    /// Provider `file_search` tool is allowed.
    pub file_search: bool,
    /// Image generation is allowed.
    pub image_generation: bool,
    /// Provider code interpreter tool is allowed.
    pub code_interpreter: bool,
    /// MCP tools are allowed (unused).
    pub mcp: bool,
}

/// Provider request parameters of a catalog model (`general_config.api_params`).
///
/// The sampling parameters are sent only when set; leave them unset for
/// reasoning models that reject them. Unknown keys are ignored.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelApiParams {
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling probability mass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Frequency penalty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    /// Presence penalty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// Stop sequences.
    pub stop: Vec<String>,
    /// Extra top-level request body keys merged by the `OpenAI` Responses, `OpenAI`
    /// Chat and vLLM adapters (request-controlled keys are ignored).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<Map<String, Value>>,
    /// Reasoning effort hint for reasoning models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// General configuration of a catalog model (`general_config`).
///
/// Every field has a default; unknown keys are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelGeneralConfig {
    /// Free-form model type label (wire key `type`).
    pub r#type: String,
    /// Availability start as provided by the catalog (RFC 3339 text).
    pub available_from: Option<String>,
    /// Per-model upload size cap in MiB (default 25).
    pub max_file_size_mb: u32,
    /// Provider request parameters.
    pub api_params: ModelApiParams,
    /// Opaque feature flags (e.g. `streaming`, `structured_output`).
    pub features: Value,
    /// Built-in tool support flags.
    pub tool_support: ModelToolSupport,
    /// Opaque supported-endpoint flags (e.g. `chat_completions`, `responses`).
    pub supported_endpoints: Value,
}

impl Default for ModelGeneralConfig {
    fn default() -> Self {
        Self {
            r#type: String::new(),
            available_from: None,
            max_file_size_mb: 25,
            api_params: ModelApiParams::default(),
            features: Value::Null,
            tool_support: ModelToolSupport::default(),
            supported_endpoints: Value::Null,
        }
    }
}

/// UI preference of a catalog model (`preference`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelPreference {
    /// Candidate default model for new chats.
    pub is_default: bool,
    /// UI sort order.
    pub sort_order: i32,
}

/// One entry of the policy snapshot model catalog (DESIGN "Model Catalog
/// Configuration").
///
/// Required keys: `id`, `provider_model_id`, `display_name`, `provider_id`,
/// `tier`, `context_window`, `max_output_tokens` and both credit multipliers.
/// Every other key has a default; unknown keys are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    /// Stable internal model id (REST `model_id`, stored in `chats.model`).
    pub id: String,
    /// Model name on the provider side (sent in LLM API requests).
    pub provider_model_id: String,
    /// User-facing model name.
    pub display_name: String,
    /// User-facing help text.
    #[serde(default)]
    pub description: Option<String>,
    /// Key of the `providers.<id>` entry that serves the model.
    pub provider_id: String,
    /// Provider name for UI; not read by the gear.
    #[serde(default)]
    pub provider_display_name: String,
    /// UI icon.
    #[serde(default)]
    pub icon: String,
    /// Rate-limit tier.
    pub tier: ModelTier,
    /// Whether the model is visible and selectable (default `false`).
    #[serde(default)]
    pub enabled: bool,
    /// System instructions sent on every request for this model.
    #[serde(default)]
    pub system_prompt: String,
    /// System prompt of the thread-summary call (empty = gear fallback).
    #[serde(default)]
    pub thread_summary_prompt: String,
    /// Capability flags, e.g. `VISION_INPUT`.
    #[serde(default)]
    pub multimodal_capabilities: Vec<String>,
    /// Maximum context tokens.
    pub context_window: u32,
    /// Model output cap in tokens.
    pub max_output_tokens: u32,
    /// Maximum input tokens per request (`0` = no separate limit).
    #[serde(default)]
    pub max_input_tokens: u32,
    /// Micro-credits per 1M input tokens (section 5.3).
    pub input_tokens_credit_multiplier_micro: i64,
    /// Micro-credits per 1M output tokens (section 5.3).
    pub output_tokens_credit_multiplier_micro: i64,
    /// Human-readable multiplier, e.g. `1x`.
    #[serde(default)]
    pub multiplier_display: String,
    /// Per-model token estimation budgets.
    #[serde(default)]
    pub estimation_budgets: EstimationBudgets,
    /// Top-k chunks per `file_search` call (default 5).
    #[serde(default = "default_max_num_results")]
    pub max_num_results: u32,
    /// Web search context size hint: `low` | `medium` | `high` (default `low`).
    #[serde(default = "default_web_search_context_size")]
    pub web_search_context_size: String,
    /// Maximum built-in tool calls per provider request (default 2).
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    /// General model configuration.
    #[serde(default)]
    pub general_config: ModelGeneralConfig,
    /// Optional UI preference.
    #[serde(default)]
    pub preference: Option<ModelPreference>,
}

const fn default_max_num_results() -> u32 {
    5
}

fn default_web_search_context_size() -> String {
    "low".to_owned()
}

const fn default_max_tool_calls() -> u32 {
    2
}

/// Operational kill switches carried by the policy snapshot.
///
/// Every field is required when deserializing (a partial object is
/// rejected); [`Default`] is all `false`.
#[allow(
    clippy::struct_excessive_bools,
    reason = "one independent operational switch per field, as delivered by the policy plugin"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KillSwitches {
    /// Premium tier is unavailable.
    pub disable_premium_tier: bool,
    /// Every request is served on the standard tier.
    pub force_standard_tier: bool,
    /// Web search tool is disabled.
    pub disable_web_search: bool,
    /// `file_search` tool is disabled.
    pub disable_file_search: bool,
    /// Image inputs are disabled.
    pub disable_images: bool,
    /// Code interpreter tool is disabled.
    pub disable_code_interpreter: bool,
}

/// Immutable, versioned policy snapshot (DESIGN section 5.2.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    /// Monotonic policy version.
    pub policy_version: u64,
    /// Model catalog of this version.
    pub model_catalog: Vec<ModelCatalogEntry>,
    /// Kill switches of this version.
    pub kill_switches: KillSwitches,
}

/// Credit limits of one tier, in micro-credits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierLimits {
    /// Daily limit in micro-credits.
    pub limit_daily_credits_micro: i64,
    /// Monthly limit in micro-credits.
    pub limit_monthly_credits_micro: i64,
}

/// Per-user allocation tied to a policy version (DESIGN section 5.2.1).
///
/// `standard` limits map to quota bucket `total` (overall cap); `premium`
/// limits map to bucket `tier:premium` (premium-only sub-cap).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    /// User the limits apply to.
    pub user_id: Uuid,
    /// Policy version the limits are keyed by.
    pub policy_version: u64,
    /// Standard-tier (overall) limits.
    pub standard: TierLimits,
    /// Premium-tier limits.
    pub premium: TierLimits,
}

/// Result of [`check_user_license`](crate::MiniChatModelPolicyPluginClientV1::check_user_license).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    /// Whether the user holds an active mini-chat license.
    pub active: bool,
}

// ── Usage publication ────────────────────────────────────────────────────

/// Provider-reported token usage.
#[allow(
    clippy::struct_field_names,
    reason = "field names are the fixed usage wire keys shared with the outbox payload"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UsageTokens {
    /// Input tokens.
    pub input_tokens: i64,
    /// Output tokens.
    pub output_tokens: i64,
    /// Input tokens served from the provider cache.
    pub cache_read_input_tokens: i64,
    /// Input tokens written to the provider cache.
    pub cache_write_input_tokens: i64,
    /// Reasoning tokens.
    pub reasoning_tokens: i64,
}

/// Usage outbox payload published through
/// [`publish_usage`](crate::MiniChatModelPolicyPluginClientV1::publish_usage)
/// (DESIGN sections 5.6-5.7, "System Task Attribution Rules").
///
/// Wire rules: there is no `event_type` field; `user_id`, `chat_id`, `turn_id`
/// and `system_task_type` are omitted (absent, not `null`) when `None`;
/// `usage` is serialized as `null` when `None`; `timestamp` is RFC 3339.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageEvent {
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// Requesting user; `None` for system tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    /// Chat the usage belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<Uuid>,
    /// Turn id; `None` for system tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<Uuid>,
    /// Turn `request_id`, or the stable `system_request_id` of a system task.
    pub request_id: Uuid,
    /// Model that served the request (after the downgrade cascade).
    pub effective_model: String,
    /// Model the user selected.
    pub selected_model: String,
    /// Terminal turn state (e.g. `completed`, `failed`).
    pub terminal_state: String,
    /// Billing outcome (e.g. `completed`, `failed`, `system_task`).
    pub billing_outcome: String,
    /// Provider-reported usage; `None` when the provider reported none.
    pub usage: Option<UsageTokens>,
    /// Debited micro-credits (authoritative amount for the consumer).
    pub actual_credits_micro: i64,
    /// Settlement method (e.g. `actual`, `estimated`, `none`).
    pub settlement_method: String,
    /// Policy version applied to the turn.
    pub policy_version_applied: u64,
    /// Web search calls made in the turn.
    pub web_search_calls: u32,
    /// Code interpreter calls made in the turn.
    pub code_interpreter_calls: u32,
    /// File search / knowledge search calls made in the turn.
    pub file_search_calls: u32,
    /// Event time (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    /// `user` or `system`.
    pub requester_type: String,
    /// Stable idempotency key, see [`dedupe_key_for_turn`] and
    /// [`dedupe_key_for_system_task`].
    pub dedupe_key: String,
    /// System task type (e.g. `thread_summary_update`); `None` for user turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_task_type: Option<String>,
}

/// Canonical usage dedupe key of a user turn:
/// `"{tenant_id}/{turn_id}/{request_id}"`, each UUID in the simple
/// (32 lowercase hex characters, no hyphens) form.
#[must_use]
pub fn dedupe_key_for_turn(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant_id.simple(),
        turn_id.simple(),
        request_id.simple()
    )
}

/// Canonical usage dedupe key of a system task:
/// `"{tenant_id}/{system_task_type}/{system_request_id}"`, both UUIDs in the
/// simple (32 lowercase hex characters, no hyphens) form.
#[must_use]
pub fn dedupe_key_for_system_task(
    tenant_id: Uuid,
    system_task_type: &str,
    system_request_id: Uuid,
) -> String {
    format!(
        "{}/{system_task_type}/{}",
        tenant_id.simple(),
        system_request_id.simple()
    )
}

// ── Audit ────────────────────────────────────────────────────────────────

/// `event_type` of a [`TurnAuditEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TurnAuditEventType {
    /// `turn_completed`: turn state `completed` (including a provider
    /// `incomplete` response).
    #[serde(rename = "turn_completed")]
    Completed,
    /// `turn_failed`: every other terminal state (failed, cancelled, orphan
    /// timeout).
    #[serde(rename = "turn_failed")]
    Failed,
}

impl TurnAuditEventType {
    /// Wire name (`turn_completed` / `turn_failed`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "turn_completed",
            Self::Failed => "turn_failed",
        }
    }
}

/// `event_type` of a [`TurnMutationAuditEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TurnMutationAuditEventType {
    /// `turn_retry`: retry of the latest turn.
    #[serde(rename = "turn_retry")]
    Retry,
    /// `turn_edit`: edit of the latest turn.
    #[serde(rename = "turn_edit")]
    Edit,
    /// `turn_delete`: delete of the latest turn.
    #[serde(rename = "turn_delete")]
    Delete,
}

impl TurnMutationAuditEventType {
    /// Wire name (`turn_retry` / `turn_edit` / `turn_delete`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Retry => "turn_retry",
            Self::Edit => "turn_edit",
            Self::Delete => "turn_delete",
        }
    }
}

/// Tool-call counts of an audited turn. There is no code interpreter count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolCalls {
    /// Web search calls.
    pub web_search_calls: u32,
    /// File search / knowledge search calls.
    pub file_search_calls: u32,
}

/// Quota decision of an audited turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaDecision {
    /// `allow`, `downgrade` or `unknown`.
    pub decision: String,
    /// Model the turn was downgraded from, when downgraded.
    #[serde(default)]
    pub downgrade_from: Option<String>,
    /// Downgrade reason, when downgraded.
    #[serde(default)]
    pub downgrade_reason: Option<String>,
}

impl QuotaDecision {
    /// Decision value for a turn served at the requested tier.
    pub const ALLOW: &'static str = "allow";
    /// Decision value for a downgraded turn.
    pub const DOWNGRADE: &'static str = "downgrade";
    /// Decision value when the decision is not known (e.g. orphan finalization).
    pub const UNKNOWN: &'static str = "unknown";
}

/// Policy decisions of an audited turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecisions {
    /// Quota decision.
    pub quota: QuotaDecision,
}

/// Audit event emitted on turn finalization (DESIGN section 3.9, ADR-0009).
///
/// `prompt`, `response`, `attachments`, `license` and `quota_scope` are
/// always serialized but empty in P1 (empty string / empty array / `null`):
/// no content reaches the audit plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    /// `turn_completed` or `turn_failed`.
    pub event_type: TurnAuditEventType,
    /// Event time (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// Requester type (`user`).
    pub requester_type: String,
    /// Acting user.
    pub user_id: Uuid,
    /// Chat of the turn.
    pub chat_id: Uuid,
    /// Turn id.
    pub turn_id: Uuid,
    /// Turn `request_id`.
    pub request_id: Uuid,
    /// Model the user selected.
    pub selected_model: String,
    /// Model that served the turn.
    pub effective_model: String,
    /// Provider-reported usage, if any.
    pub usage: Option<UsageTokens>,
    /// Turn latency in milliseconds.
    pub latency_ms: u64,
    /// Tool-call counts.
    pub tool_calls: ToolCalls,
    /// Policy decisions.
    pub policy_decisions: PolicyDecisions,
    /// Terminal error code of a failed turn.
    #[serde(default)]
    pub error_code: Option<String>,
    /// Trace id of the request, when known.
    #[serde(default)]
    pub trace_id: Option<String>,
    /// Prompt content; empty in P1.
    #[serde(default)]
    pub prompt: String,
    /// Response content; empty in P1.
    #[serde(default)]
    pub response: String,
    /// Attachment metadata; empty in P1.
    #[serde(default)]
    pub attachments: Vec<Value>,
    /// License decision; `None` in P1.
    #[serde(default)]
    pub license: Option<Value>,
    /// Quota scope decision; `None` in P1.
    #[serde(default)]
    pub quota_scope: Option<Value>,
}

/// Audit event emitted on a turn mutation (retry, edit, delete).
///
/// Retry and edit carry `original_request_id` and `new_request_id`; delete
/// carries `request_id`. Absent ids are omitted from the payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMutationAuditEvent {
    /// `turn_retry`, `turn_edit` or `turn_delete`.
    pub event_type: TurnMutationAuditEventType,
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// Acting user.
    pub actor_user_id: Uuid,
    /// Chat of the mutated turn.
    pub chat_id: Uuid,
    /// `request_id` of the replaced turn (retry / edit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_request_id: Option<Uuid>,
    /// `request_id` of the replacement turn (retry / edit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_request_id: Option<Uuid>,
    /// `request_id` of the deleted turn (delete).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Uuid>,
    /// Event time (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

impl TurnMutationAuditEvent {
    /// Builds a `turn_retry` event.
    #[must_use]
    pub const fn retry(
        tenant_id: Uuid,
        actor_user_id: Uuid,
        chat_id: Uuid,
        original_request_id: Uuid,
        new_request_id: Uuid,
        timestamp: OffsetDateTime,
    ) -> Self {
        Self::replacement(
            TurnMutationAuditEventType::Retry,
            tenant_id,
            actor_user_id,
            chat_id,
            original_request_id,
            new_request_id,
            timestamp,
        )
    }

    /// Builds a `turn_edit` event.
    #[must_use]
    pub const fn edit(
        tenant_id: Uuid,
        actor_user_id: Uuid,
        chat_id: Uuid,
        original_request_id: Uuid,
        new_request_id: Uuid,
        timestamp: OffsetDateTime,
    ) -> Self {
        Self::replacement(
            TurnMutationAuditEventType::Edit,
            tenant_id,
            actor_user_id,
            chat_id,
            original_request_id,
            new_request_id,
            timestamp,
        )
    }

    /// Builds a `turn_delete` event.
    #[must_use]
    pub const fn delete(
        tenant_id: Uuid,
        actor_user_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
        timestamp: OffsetDateTime,
    ) -> Self {
        Self {
            event_type: TurnMutationAuditEventType::Delete,
            tenant_id,
            actor_user_id,
            chat_id,
            original_request_id: None,
            new_request_id: None,
            request_id: Some(request_id),
            timestamp,
        }
    }

    const fn replacement(
        event_type: TurnMutationAuditEventType,
        tenant_id: Uuid,
        actor_user_id: Uuid,
        chat_id: Uuid,
        original_request_id: Uuid,
        new_request_id: Uuid,
        timestamp: OffsetDateTime,
    ) -> Self {
        Self {
            event_type,
            tenant_id,
            actor_user_id,
            chat_id,
            original_request_id: Some(original_request_id),
            new_request_id: Some(new_request_id),
            request_id: None,
            timestamp,
        }
    }
}

/// Audit outbox payload delivered to
/// [`MiniChatAuditPluginClientV1::emit`](crate::MiniChatAuditPluginClientV1::emit).
///
/// Serialized flat (the inner event's fields, including `event_type`, at the
/// top level). Deserialization dispatches on `event_type`, so a payload with a
/// missing or unknown `event_type` is rejected with a precise error.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AuditEvent {
    /// Turn finalization event (boxed: it is much larger than a mutation event).
    Turn(Box<TurnAuditEvent>),
    /// Turn mutation event.
    Mutation(TurnMutationAuditEvent),
}

impl AuditEvent {
    const EVENT_TYPES: &'static [&'static str] = &[
        "turn_completed",
        "turn_failed",
        "turn_retry",
        "turn_edit",
        "turn_delete",
    ];

    /// Wire `event_type` of the inner event.
    #[must_use]
    pub const fn event_type(&self) -> &'static str {
        match self {
            Self::Turn(event) => event.event_type.as_str(),
            Self::Mutation(event) => event.event_type.as_str(),
        }
    }

    /// Tenant of the inner event (the audit outbox partition key).
    #[must_use]
    pub const fn tenant_id(&self) -> Uuid {
        match self {
            Self::Turn(event) => event.tenant_id,
            Self::Mutation(event) => event.tenant_id,
        }
    }
}

impl From<TurnAuditEvent> for AuditEvent {
    fn from(event: TurnAuditEvent) -> Self {
        Self::Turn(Box::new(event))
    }
}

impl From<TurnMutationAuditEvent> for AuditEvent {
    fn from(event: TurnMutationAuditEvent) -> Self {
        Self::Mutation(event)
    }
}

impl<'de> Deserialize<'de> for AuditEvent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let is_turn = match value.get("event_type") {
            None => return Err(D::Error::missing_field("event_type")),
            Some(Value::String(event_type)) => match event_type.as_str() {
                "turn_completed" | "turn_failed" => true,
                "turn_retry" | "turn_edit" | "turn_delete" => false,
                other => return Err(D::Error::unknown_variant(other, Self::EVENT_TYPES)),
            },
            Some(_) => return Err(D::Error::custom("`event_type` must be a string")),
        };
        if is_turn {
            serde_json::from_value(value)
                .map(Self::Turn)
                .map_err(D::Error::custom)
        } else {
            serde_json::from_value(value)
                .map(Self::Mutation)
                .map_err(D::Error::custom)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn uuid(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn turn_event() -> TurnAuditEvent {
        TurnAuditEvent {
            event_type: TurnAuditEventType::Failed,
            timestamp: OffsetDateTime::UNIX_EPOCH,
            tenant_id: uuid(1),
            requester_type: "user".to_owned(),
            user_id: uuid(2),
            chat_id: uuid(3),
            turn_id: uuid(4),
            request_id: uuid(5),
            selected_model: "gpt-4.1".to_owned(),
            effective_model: "gpt-4.1-mini".to_owned(),
            usage: None,
            latency_ms: 1234,
            tool_calls: ToolCalls::default(),
            policy_decisions: PolicyDecisions {
                quota: QuotaDecision {
                    decision: QuotaDecision::DOWNGRADE.to_owned(),
                    downgrade_from: Some("gpt-4.1".to_owned()),
                    downgrade_reason: Some("premium_quota_exhausted".to_owned()),
                },
            },
            error_code: Some("orphan_timeout".to_owned()),
            trace_id: None,
            prompt: String::new(),
            response: String::new(),
            attachments: Vec::new(),
            license: None,
            quota_scope: None,
        }
    }

    #[test]
    fn turn_audit_event_round_trips_through_audit_event() {
        let event = AuditEvent::from(turn_event());

        let encoded = serde_json::to_value(&event).expect("serializes");
        let decoded: AuditEvent = serde_json::from_value(encoded.clone()).expect("deserializes");

        assert_eq!(decoded, event);
        assert_eq!(encoded["event_type"], json!("turn_failed"));
        assert_eq!(encoded["prompt"], json!(""));
        assert_eq!(encoded["response"], json!(""));
        assert_eq!(encoded["attachments"], json!([]));
        assert_eq!(encoded["license"], Value::Null);
        assert_eq!(encoded["quota_scope"], Value::Null);
        assert_eq!(encoded["usage"], Value::Null);
        assert_eq!(decoded.event_type(), "turn_failed");
        assert_eq!(decoded.tenant_id(), uuid(1));
    }

    #[test]
    fn mutation_audit_events_round_trip_and_omit_absent_ids() {
        let ts = OffsetDateTime::UNIX_EPOCH;
        let events = [
            TurnMutationAuditEvent::retry(uuid(1), uuid(2), uuid(3), uuid(4), uuid(5), ts),
            TurnMutationAuditEvent::edit(uuid(1), uuid(2), uuid(3), uuid(4), uuid(5), ts),
            TurnMutationAuditEvent::delete(uuid(1), uuid(2), uuid(3), uuid(6), ts),
        ];
        for mutation in events {
            let event = AuditEvent::from(mutation);
            let encoded = serde_json::to_value(&event).expect("serializes");
            let decoded: AuditEvent =
                serde_json::from_value(encoded.clone()).expect("deserializes");
            assert_eq!(decoded, event);
            assert_eq!(encoded["event_type"], json!(event.event_type()));
            let object = encoded.as_object().expect("object");
            if event.event_type() == "turn_delete" {
                assert!(object.contains_key("request_id"));
                assert!(!object.contains_key("original_request_id"));
            } else {
                assert!(object.contains_key("original_request_id"));
                assert!(object.contains_key("new_request_id"));
                assert!(!object.contains_key("request_id"));
            }
        }
    }

    #[test]
    fn audit_event_rejects_missing_or_unknown_event_type() {
        let mut encoded = serde_json::to_value(AuditEvent::from(turn_event())).expect("serializes");
        encoded["event_type"] = json!("turn_cancelled");
        assert!(serde_json::from_value::<AuditEvent>(encoded.clone()).is_err());

        encoded
            .as_object_mut()
            .expect("object")
            .remove("event_type");
        assert!(serde_json::from_value::<AuditEvent>(encoded).is_err());
        assert!(serde_json::from_value::<AuditEvent>(json!("not an object")).is_err());
    }

    #[test]
    fn model_tier_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(ModelTier::Premium).expect("serializes"),
            json!("premium")
        );
        assert_eq!(ModelTier::Standard.as_str(), "standard");
    }
}
