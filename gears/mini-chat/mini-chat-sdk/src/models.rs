//! Public models of the mini-chat gear: policy snapshot, usage and audit events.
//!
//! The serde attributes on these types are the wire contract (DESIGN Appendix A,
//! sections 5.6 and 3.9). The `decode_*` / `encode_*` functions are the entry
//! points used by the gear and by the policy and audit plugins.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Error returned when a payload cannot be decoded.
///
/// The message names the offending field (for example ``missing field `id` ``).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("decode error: {message}")]
pub struct DecodeError {
    /// Human readable description naming the field.
    pub message: String,
}

impl DecodeError {
    /// Error for an absent required field; the message names the field.
    fn missing_field(field: &str) -> Self {
        Self {
            message: format!("missing field `{field}`"),
        }
    }

    /// Error for a payload that cannot be parsed; keeps the serde message,
    /// which names the offending field.
    fn from_serde(error: &serde_json::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

/// Error returned when a value cannot be encoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("encode error: {message}")]
pub struct EncodeError {
    /// Human readable description.
    pub message: String,
}

impl EncodeError {
    fn from_serde(error: &serde_json::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Policy snapshot
// ---------------------------------------------------------------------------

/// Rate-limit tier of a model; also the downgrade cascade order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelTier {
    /// Premium tier (`premium`, also accepts `Premium`).
    #[serde(rename = "premium", alias = "Premium")]
    Premium,
    /// Standard tier (`standard`, also accepts `Standard`).
    #[serde(rename = "standard", alias = "Standard")]
    Standard,
}

/// Search context size hint for web search.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchContextSize {
    /// Small context (default).
    #[default]
    Low,
    /// Medium context.
    Medium,
    /// Large context.
    High,
}

/// UI preference of a catalog entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPreference {
    /// Default model for new chats when it is the first enabled one flagged.
    pub is_default: bool,
    /// Sort order in the Models API.
    pub sort_order: i32,
}

/// Per-model token estimation budgets (DESIGN section 5.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-fill-budget-fields
#[serde(default)]
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-fill-budget-fields
pub struct EstimationBudgets {
    /// Conservative bytes-per-token ratio.
    pub bytes_per_token_conservative: u32,
    /// Fixed overhead tokens added to every estimate.
    pub fixed_overhead_tokens: u32,
    /// Safety margin in percent.
    pub safety_margin_pct: u32,
    /// Token budget per image.
    pub image_token_budget: u32,
    /// Surcharge when any tool is included.
    pub tool_surcharge_tokens: u32,
    /// Surcharge when web search is included.
    pub web_search_surcharge_tokens: u32,
    /// Surcharge when the code interpreter is included.
    pub code_interpreter_surcharge_tokens: u32,
    /// Minimal generation floor (ignored per model, taken from gear config).
    pub minimal_generation_floor: u32,
}

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-default-budgets
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
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-default-budgets

/// Provider API parameters of a model. The four sampling parameters,
/// `extra_body` and `reasoning_effort` are optional; `stop` is required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiParams {
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Frequency penalty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    /// Presence penalty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// Stop sequences.
    pub stop: Vec<String>,
    /// Extra body keys merged into the provider request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Value>,
    /// Reasoning effort hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// Model feature flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ModelFeatures {
    /// Streaming supported.
    pub streaming: bool,
    /// Structured output supported.
    pub structured_output: bool,
}

/// Tool support flags of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ModelToolSupport {
    /// Web search.
    pub web_search: bool,
    /// File search.
    pub file_search: bool,
    /// Image generation.
    pub image_generation: bool,
    /// Code interpreter.
    pub code_interpreter: bool,
    /// MCP (parsed, unused).
    pub mcp: bool,
}

/// Endpoint support flags of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct SupportedEndpoints {
    /// Chat completions.
    pub chat_completions: bool,
    /// Responses API.
    pub responses: bool,
    /// Embeddings.
    pub embeddings: bool,
    /// Image generation.
    pub image_generation: bool,
    /// Audio speech generation.
    pub audio_speech_generation: bool,
    /// Audio transcription.
    pub audio_transcription: bool,
    /// Audio translation.
    pub audio_translation: bool,
}

/// General model configuration; all fields are required except the optional
/// ones of [`ApiParams`].
// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-decode-general-config
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelGeneralConfig {
    /// Schema type label (for example `model.general.v1`, may be empty).
    #[serde(rename = "type")]
    pub config_type: String,
    /// Availability start.
    #[serde(with = "time::serde::rfc3339")]
    pub available_from: OffsetDateTime,
    /// Per-model upload cap in MiB.
    pub max_file_size_mb: u32,
    /// Provider API parameters.
    pub api_params: ApiParams,
    /// Feature flags.
    pub features: ModelFeatures,
    /// Tool support flags.
    pub tool_support: ModelToolSupport,
    /// Endpoint support flags.
    pub supported_endpoints: SupportedEndpoints,
}
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-decode-general-config

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-apply-entry-defaults
fn default_max_tool_calls() -> u32 {
    2
}
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-apply-entry-defaults

/// One model catalog entry (DESIGN "Model Catalog Configuration").
///
/// Unknown keys are ignored.
// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-ignore-unknown-nested
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-ignore-unknown-nested
pub struct ModelCatalogEntry {
    /// Stable internal model identifier.
    pub id: String,
    /// Model name on the provider side.
    pub provider_model_id: String,
    /// UI display name.
    pub display_name: String,
    /// UI description.
    #[serde(default)]
    pub description: String,
    /// Key of the `providers.<id>` entry that serves the model.
    pub provider_id: String,
    /// Provider name for UI only.
    pub provider_display_name: String,
    /// UI icon.
    #[serde(default)]
    pub icon: String,
    /// Rate-limit tier.
    pub tier: ModelTier,
    /// Whether the model is visible and usable.
    #[serde(default)]
    pub enabled: bool,
    /// System prompt sent on every request.
    #[serde(default)]
    pub system_prompt: String,
    /// System prompt of the thread-summary call.
    #[serde(default)]
    pub thread_summary_prompt: String,
    /// Multimodal capabilities such as `VISION_INPUT`.
    #[serde(default)]
    pub multimodal_capabilities: Vec<String>,
    /// Maximum context tokens.
    pub context_window: u32,
    /// Model output cap.
    pub max_output_tokens: u32,
    /// Maximum input tokens (0 = no separate limit).
    pub max_input_tokens: u32,
    /// Input credit multiplier (micro-credits per 1M tokens).
    pub input_tokens_credit_multiplier_micro: u64,
    /// Output credit multiplier (micro-credits per 1M tokens).
    pub output_tokens_credit_multiplier_micro: u64,
    /// UI multiplier label.
    #[serde(default)]
    pub multiplier_display: String,
    /// Estimation budgets.
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-if-budgets-absent
    #[serde(default)]
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-if-budgets-absent
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-else-budgets-present
    pub estimation_budgets: EstimationBudgets,
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-else-budgets-present
    /// Top-k chunks per `file_search` call.
    pub max_num_results: u32,
    /// Web search context size.
    #[serde(default)]
    pub web_search_context_size: WebSearchContextSize,
    /// Built-in tool calls per provider request.
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    /// General configuration.
    pub general_config: ModelGeneralConfig,
    /// UI preference.
    #[serde(default)]
    pub preference: Option<ModelPreference>,
}

/// Emergency switches of a policy snapshot. All six fields are required: a
/// missing or renamed switch is a decode error, never read as `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct KillSwitches {
    /// Disable the premium tier.
    pub disable_premium_tier: bool,
    /// Force the standard tier.
    pub force_standard_tier: bool,
    /// Disable web search.
    pub disable_web_search: bool,
    /// Disable file search.
    pub disable_file_search: bool,
    /// Disable images.
    pub disable_images: bool,
    /// Disable the code interpreter.
    pub disable_code_interpreter: bool,
}

/// Immutable policy snapshot for a `(user_id, policy_version)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    /// Monotonic policy version.
    pub policy_version: u64,
    /// Model catalog.
    pub model_catalog: Vec<ModelCatalogEntry>,
    /// Kill switches.
    pub kill_switches: KillSwitches,
}

/// Current policy version of a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyVersionInfo {
    /// Monotonic policy version.
    pub policy_version: u64,
    /// Generation time.
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
}

/// Per-tier credit limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierLimits {
    /// Daily limit in micro-credits.
    pub limit_daily_credits_micro: i64,
    /// Monthly limit in micro-credits.
    pub limit_monthly_credits_micro: i64,
}

/// Per-user credit limits keyed by policy version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLimits {
    /// User identity.
    pub user_id: Uuid,
    /// Policy version the limits belong to.
    pub policy_version: u64,
    /// Standard (`total` bucket) limits.
    pub standard: TierLimits,
    /// Premium (`tier:premium` bucket) limits.
    pub premium: TierLimits,
}

/// License status of a user as reported by the policy plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserLicenseStatus {
    /// Whether the user holds an active license.
    pub active: bool,
}

/// Fields a catalog entry must carry (DESIGN Appendix A).
const REQUIRED_ENTRY_FIELDS: [&str; 13] = [
    "id",
    "provider_model_id",
    "display_name",
    "provider_id",
    "provider_display_name",
    "tier",
    "context_window",
    "max_output_tokens",
    "max_input_tokens",
    "input_tokens_credit_multiplier_micro",
    "output_tokens_credit_multiplier_micro",
    "max_num_results",
    "general_config",
];

/// The six kill switches; every one is required.
const KILL_SWITCH_FIELDS: [&str; 6] = [
    "disable_premium_tier",
    "force_standard_tier",
    "disable_web_search",
    "disable_file_search",
    "disable_images",
    "disable_code_interpreter",
];

/// Snapshot with undecoded entries and switches, so that each entry and each
/// switch is checked and reported on its own.
#[derive(Deserialize)]
struct RawPolicySnapshot {
    policy_version: u64,
    model_catalog: Vec<serde_json::Value>,
    kill_switches: serde_json::Value,
}

/// Decodes a serialized [`PolicySnapshot`] (flat shape: `policy_version`,
/// `model_catalog`, `kill_switches`).
///
/// # Errors
///
/// Returns [`DecodeError`] naming the field when a required field is missing,
/// the tier is unknown or a kill switch is absent.
pub fn decode_policy_snapshot(payload: &[u8]) -> Result<PolicySnapshot, DecodeError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-decode-top
    let raw: RawPolicySnapshot =
        serde_json::from_slice(payload).map_err(|e| DecodeError::from_serde(&e))?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-decode-top

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-for-each-entry
    let model_catalog = raw
        .model_catalog
        .iter()
        .map(decode_model_catalog_entry)
        .collect::<Result<Vec<_>, _>>()?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-for-each-entry

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-if-switch-missing
    let switches = raw
        .kill_switches
        .as_object()
        .ok_or_else(|| DecodeError::missing_field("kill_switches"))?;
    for switch in KILL_SWITCH_FIELDS {
        if !switches.contains_key(switch) {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-return-switch-error
            return Err(DecodeError::missing_field(switch));
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-return-switch-error
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-if-switch-missing
    let kill_switches: KillSwitches =
        serde_json::from_value(raw.kill_switches).map_err(|e| DecodeError::from_serde(&e))?;

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-return-snapshot
    Ok(PolicySnapshot {
        policy_version: raw.policy_version,
        model_catalog,
        kill_switches,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-return-snapshot
}

/// Decodes one serialized [`ModelCatalogEntry`].
///
/// # Errors
///
/// Returns [`DecodeError`] naming the field when a required field is missing or
/// the tier is unknown.
pub fn decode_model_catalog_entry(
    value: &serde_json::Value,
) -> Result<ModelCatalogEntry, DecodeError> {
    let object = value.as_object().ok_or_else(|| DecodeError {
        message: "catalog entry must be an object".to_owned(),
    })?;

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-if-required-missing
    for field in REQUIRED_ENTRY_FIELDS {
        if !object.contains_key(field) {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-return-required-error
            return Err(DecodeError::missing_field(field));
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-return-required-error
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-if-required-missing

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-if-bad-tier
    if let Some(tier) = object.get("tier")
        && serde_json::from_value::<ModelTier>(tier.clone()).is_err()
    {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-return-tier-error
        return Err(DecodeError {
            message: "invalid field `tier`: expected `premium` or `standard`".to_owned(),
        });
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-return-tier-error
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-decode-policy-snapshot:p1:inst-if-bad-tier

    serde_json::from_value(value.clone()).map_err(|e| DecodeError::from_serde(&e))
}

// ---------------------------------------------------------------------------
// Usage event
// ---------------------------------------------------------------------------

/// Provider-reported token usage of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
// The `*_tokens` names are the wire field names of DESIGN section 5.6.
#[allow(clippy::struct_field_names)]
pub struct UsageTokens {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Input tokens served from the cache.
    pub cache_read_input_tokens: u64,
    /// Input tokens written to the cache.
    pub cache_write_input_tokens: u64,
    /// Reasoning tokens.
    pub reasoning_tokens: u64,
}

/// Terminal state of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    /// Completed.
    Completed,
    /// Failed.
    Failed,
    /// Cancelled.
    Cancelled,
}

/// Billing outcome of a turn or system task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingOutcome {
    /// Completed.
    Completed,
    /// Failed.
    Failed,
    /// Aborted.
    Aborted,
    /// System task.
    SystemTask,
}

/// How credits were settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementMethod {
    /// From provider-reported usage.
    Actual,
    /// From the estimate.
    Estimated,
    /// Reserve released.
    Released,
    /// No settlement.
    None,
}

/// Requester kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequesterType {
    /// A user turn.
    User,
    /// A system (background) task.
    System,
}

/// Kind of system task that publishes usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemTaskType {
    /// Thread summary update.
    ThreadSummaryUpdate,
}

impl SystemTaskType {
    /// Wire name of the task type (also used in the dedupe key).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ThreadSummaryUpdate => "thread_summary_update",
        }
    }
}

/// Usage event published through the usage outbox queue (DESIGN section 5.6 and
/// A.3). There is no `event_type` field.
///
/// For `requester_type = system`, `user_id` and `turn_id` are absent (not
/// `null`) and `system_task_type` is present; for a user turn it is the other
/// way round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageEvent {
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// User of the turn (absent for system tasks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    /// Chat.
    pub chat_id: Uuid,
    /// Turn (absent for system tasks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<Uuid>,
    /// Request.
    pub request_id: Uuid,
    /// Model actually used.
    pub effective_model: String,
    /// Model selected by the user.
    pub selected_model: String,
    /// Terminal state.
    pub terminal_state: TerminalState,
    /// Billing outcome.
    pub billing_outcome: BillingOutcome,
    /// Provider usage, `null` when the provider reported none.
    pub usage: Option<UsageTokens>,
    /// Settled credits in micro-credits.
    pub actual_credits_micro: i64,
    /// Settlement method.
    pub settlement_method: SettlementMethod,
    /// Policy version applied to the turn.
    pub policy_version_applied: u64,
    /// Web search calls.
    pub web_search_calls: u32,
    /// Code interpreter calls.
    pub code_interpreter_calls: u32,
    /// File search calls.
    pub file_search_calls: u32,
    /// Event time.
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    /// Requester kind.
    pub requester_type: RequesterType,
    /// Idempotency key.
    pub dedupe_key: String,
    /// System task kind (absent for user turns).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_task_type: Option<SystemTaskType>,
}

/// Builds the user-turn dedupe key `{tenant}/{turn}/{request}` (32-char
/// lowercase simple hex components).
#[must_use]
pub fn usage_dedupe_key(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant_id.simple(),
        turn_id.simple(),
        request_id.simple()
    )
}

/// Builds the system-task dedupe key
/// `{tenant}/{system_task_type}/{system_request}` (simple hex UUIDs).
#[must_use]
pub fn system_task_dedupe_key(
    tenant_id: Uuid,
    task_type: SystemTaskType,
    system_request_id: Uuid,
) -> String {
    format!(
        "{}/{}/{}",
        tenant_id.simple(),
        task_type.as_str(),
        system_request_id.simple()
    )
}

/// Fields of the DESIGN usage event that the gear does not implement; they are
/// never encoded.
const UNIMPLEMENTED_USAGE_FIELDS: [&str; 6] = [
    "event_type",
    "completion_signal",
    "reserved_credits_micro",
    "committed_credits_micro",
    "overshoot_capped",
    "error_code",
];

/// Violation of the `requester_type` field invariants of a [`UsageEvent`].
///
/// `requester_type = user` requires `user_id` and `turn_id` and forbids
/// `system_task_type`; `requester_type = system` requires `system_task_type`
/// and forbids `user_id` and `turn_id`.
fn requester_shape_violation(event: &UsageEvent) -> Option<String> {
    let requester = match event.requester_type {
        RequesterType::User => "user",
        RequesterType::System => "system",
    };
    let fields = [
        ("user_id", event.user_id.is_some()),
        ("turn_id", event.turn_id.is_some()),
        ("system_task_type", event.system_task_type.is_some()),
    ];
    for (field, present) in fields {
        let required = match event.requester_type {
            RequesterType::User => field != "system_task_type",
            RequesterType::System => field == "system_task_type",
        };
        if required && !present {
            return Some(format!(
                "missing field `{field}` for requester_type {requester}"
            ));
        }
        if !required && present {
            return Some(format!(
                "unexpected field `{field}` for requester_type {requester}"
            ));
        }
    }
    None
}

/// Encodes a [`UsageEvent`] into the outbox JSON payload.
///
/// # Errors
///
/// Returns [`EncodeError`] naming the field when the `requester_type`
/// invariants are violated (see [`UsageEvent`]) or serialization fails.
pub fn encode_usage_event(event: &UsageEvent) -> Result<Vec<u8>, EncodeError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-encode-usage-core
    let mut value = serde_json::to_value(event).map_err(|e| EncodeError::from_serde(&e))?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-encode-usage-core
    let object = value.as_object_mut().ok_or_else(|| EncodeError {
        message: "usage event must encode to an object".to_owned(),
    })?;

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-if-system-task
    if event.requester_type == RequesterType::System {
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-if-system-task
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-omit-user-turn
        // `user_id` / `turn_id` are rejected (not silently stripped) so they are
        // absent from the payload by construction.
        if let Some(message) = requester_shape_violation(event) {
            return Err(EncodeError { message });
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-omit-user-turn
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-else-user-turn
    } else {
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-else-user-turn
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-include-user-turn
        // `system_task_type` is rejected (not silently stripped) for a user turn.
        let violation = requester_shape_violation(event);
        if let Some(message) = violation {
            return Err(EncodeError { message });
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-include-user-turn
    }

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-if-no-usage
    if event.usage.is_none() {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-usage-null
        object.insert("usage".to_owned(), serde_json::Value::Null);
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-usage-null
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-if-no-usage

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-no-unimplemented-fields
    for field in UNIMPLEMENTED_USAGE_FIELDS {
        object.remove(field);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-no-unimplemented-fields

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-return-payload
    serde_json::to_vec(&value).map_err(|e| EncodeError::from_serde(&e))
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-return-payload
}

/// Decodes an outbox payload into a [`UsageEvent`] (exact inverse of
/// [`encode_usage_event`]).
///
/// # Errors
///
/// Returns [`DecodeError`] naming the field on a malformed payload: a missing
/// `usage` key (an explicit `null` is allowed) or a violation of the
/// `requester_type` invariants (see [`UsageEvent`]).
pub fn decode_usage_event(payload: &[u8]) -> Result<UsageEvent, DecodeError> {
    let value: serde_json::Value =
        serde_json::from_slice(payload).map_err(|e| DecodeError::from_serde(&e))?;
    if value
        .as_object()
        .is_some_and(|object| !object.contains_key("usage"))
    {
        return Err(DecodeError::missing_field("usage"));
    }
    let event: UsageEvent =
        serde_json::from_value(value).map_err(|e| DecodeError::from_serde(&e))?;
    if let Some(message) = requester_shape_violation(&event) {
        return Err(DecodeError { message });
    }
    Ok(event)
}

// ---------------------------------------------------------------------------
// Audit events
// ---------------------------------------------------------------------------

/// Event type of a turn finalization audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnAuditEventType {
    /// Turn state `completed` (including a provider `incomplete` response).
    TurnCompleted,
    /// Every other terminal state (also cancelled and orphan-watchdog turns).
    TurnFailed,
}

/// Token usage carried by a turn audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditUsage {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
}

/// Tool-call counts of a turn audit event (no code interpreter count).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditToolCalls {
    /// Web search calls.
    pub web_search: u32,
    /// File search calls.
    pub file_search: u32,
}

/// Quota decision of a turn audit event (`allow`, `downgrade`, `unknown`, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditQuotaDecision {
    /// Decision label.
    pub decision: String,
}

/// Policy decisions of a turn audit event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditPolicyDecisions {
    /// Quota decision.
    pub quota: AuditQuotaDecision,
}

/// Audit event emitted when a turn is finalized.
///
/// `prompt`, `response`, `attachments`, `license` and `quota_scope` are always
/// empty (ADR-0009): no content reaches the audit plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    /// `turn_completed` or `turn_failed`.
    pub event_type: TurnAuditEventType,
    /// Tenant.
    pub tenant_id: Uuid,
    /// Requesting user.
    pub requester_user_id: Uuid,
    /// Chat.
    pub chat_id: Uuid,
    /// Turn.
    pub turn_id: Uuid,
    /// Request.
    pub request_id: Uuid,
    /// Model selected by the user.
    pub selected_model: String,
    /// Model actually used.
    pub effective_model: String,
    /// Token usage.
    pub usage: AuditUsage,
    /// Turn latency in milliseconds.
    pub latency_ms: u64,
    /// Tool-call counts.
    pub tool_calls: AuditToolCalls,
    /// Policy decisions.
    pub policy_decisions: AuditPolicyDecisions,
    /// Event time.
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    /// Always empty.
    pub prompt: String,
    /// Always empty.
    pub response: String,
    /// Always empty.
    pub attachments: Vec<serde_json::Value>,
    /// Always empty (`null`).
    #[serde(default)]
    pub license: Option<serde_json::Value>,
    /// Always empty (`null`).
    #[serde(default)]
    pub quota_scope: Option<serde_json::Value>,
}

/// Audit event emitted for a turn mutation (retry, edit, delete).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
// The variant names mirror the wire `event_type` values (`turn_retry`, ...).
#[allow(clippy::enum_variant_names)]
pub enum TurnMutationAuditEvent {
    /// Retry of the last turn.
    TurnRetry {
        /// Tenant.
        tenant_id: Uuid,
        /// Acting user.
        actor_user_id: Uuid,
        /// Chat.
        chat_id: Uuid,
        /// Replaced request.
        original_request_id: Uuid,
        /// Replacement request.
        new_request_id: Uuid,
        /// Event time.
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
    /// Edit of the last turn.
    TurnEdit {
        /// Tenant.
        tenant_id: Uuid,
        /// Acting user.
        actor_user_id: Uuid,
        /// Chat.
        chat_id: Uuid,
        /// Replaced request.
        original_request_id: Uuid,
        /// Replacement request.
        new_request_id: Uuid,
        /// Event time.
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
    /// Delete of the last turn.
    TurnDelete {
        /// Tenant.
        tenant_id: Uuid,
        /// Acting user.
        actor_user_id: Uuid,
        /// Chat.
        chat_id: Uuid,
        /// Deleted request.
        request_id: Uuid,
        /// Event time.
        #[serde(with = "time::serde::rfc3339")]
        timestamp: OffsetDateTime,
    },
}

/// Any audit event delivered to the audit plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
// The events are moved once into the outbox encoder; boxing would only add an
// allocation to the public constructor shape.
#[allow(clippy::large_enum_variant)]
pub enum MiniChatAuditEvent {
    /// Turn finalization event.
    Turn(TurnAuditEvent),
    /// Turn mutation event.
    Mutation(TurnMutationAuditEvent),
}

/// Encodes an audit event into the outbox JSON payload.
///
/// # Errors
///
/// Returns [`EncodeError`] when serialization fails.
pub fn encode_audit_event(event: &MiniChatAuditEvent) -> Result<Vec<u8>, EncodeError> {
    let encoded = match event {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-encode-turn-audit
        MiniChatAuditEvent::Turn(turn) => serde_json::to_vec(&TurnAuditEvent {
            // No content reaches the audit plugin (ADR-0009).
            prompt: String::new(),
            response: String::new(),
            attachments: Vec::new(),
            license: None,
            quota_scope: None,
            ..turn.clone()
        }),
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-encode-turn-audit
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-encode-mutation-audit
        MiniChatAuditEvent::Mutation(mutation) => serde_json::to_vec(mutation),
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-encode-events:p1:inst-encode-mutation-audit
    };
    encoded.map_err(|e| EncodeError::from_serde(&e))
}

/// Decodes an outbox payload into an audit event (exact inverse of
/// [`encode_audit_event`]).
///
/// # Errors
///
/// Returns [`DecodeError`] on a malformed payload.
pub fn decode_audit_event(payload: &[u8]) -> Result<MiniChatAuditEvent, DecodeError> {
    serde_json::from_slice(payload).map_err(|e| DecodeError::from_serde(&e))
}

#[cfg(test)]
#[path = "models_decode_tests.rs"]
mod decode_tests;

#[cfg(test)]
#[path = "models_events_tests.rs"]
mod events_tests;
