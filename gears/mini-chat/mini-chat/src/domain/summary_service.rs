//! Thread-summary execution (DESIGN section 3.6 "Thread Summary Update",
//! "Execution stages and invariants", "Stable Range and Commit Invariant",
//! section 3.2 "System Task Attribution Rules", "Provider Request Metadata",
//! B.5.5 "Model-facing prompts").
//!
//! [`ThreadSummaryService::execute`] runs one delivery of a durable
//! thread-summary work item ([`ThreadSummaryTask`]) and returns its
//! [`SummaryOutcome`]; the queue handler maps the outcome and the delivery
//! number to the outbox result with [`disposition`]:
//!
//! 1. the summary model (`thread_summary_worker.summary_model_id`, empty =
//!    `gpt-4.1-mini`) is resolved in the current policy with the enabled
//!    filter: missing or disabled is [`SummaryOutcome::ModelUnavailable`]
//!    (`Reject`), a plugin failure is [`SummaryOutcome::Retry`];
//! 2. CAS pre-check: the stored frontier must equal the task's base
//!    frontier ([`SummaryOutcome::CasConflict`] otherwise; a base whose
//!    summary row is gone is [`SummaryOutcome::BaseMissing`]); both finish
//!    `Ok` without a provider call;
//! 3. the frozen range `(base, target]` (non-deleted, non-compressed,
//!    chronological) is loaded; system messages are not summarized;
//! 4. the prompt ([`build_user_prompt`], system prompt per
//!    [`select_system_prompt`]) is fitted to the summary model's input
//!    budget ([`input_budget`], oldest `ceil(n/5)` messages dropped per step,
//!    at least two kept) and sent with [`LlmProvider::complete`] under the
//!    system identity (`user` = chat tenant + [`DEFAULT_SUBJECT_ID`],
//!    `metadata.request_type = summary`, `feature = none`); a
//!    context-length-exceeded failure drops the oldest ~20% again, up to
//!    [`PTL_MAX_RETRIES`] times; any other failure is
//!    [`SummaryOutcome::ProviderError`] (`Retry`, previous summary kept);
//! 5. the answer is parsed ([`parse_summary`]); an empty result is
//!    [`SummaryOutcome::EmptySummary`] (`Retry`);
//! 6. the commit ([`ThreadSummaryStore::commit_summary`]) stores the summary
//!    with the CAS on the base, marks exactly the range compressed and
//!    enqueues the system usage event ([`system_usage_event`]); a deleted
//!    target or a lost CAS finishes `Ok` without changes.
//!
//! A `Retry` outcome on the `thread_summary_worker.max_attempts`-th delivery
//! becomes `Reject` (dead letter).

use std::sync::Arc;

use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, ModelCatalogEntry, UsageEvent, dedupe_key_for_system_task,
};
use time::OffsetDateTime;
use toolkit_security::AccessScope;
use toolkit_security::constants::DEFAULT_SUBJECT_ID;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::config::ThreadSummaryWorkerConfig;
use crate::domain::llm::{
    LlmFailure, LlmInputItem, LlmMetadata, LlmProvider, LlmProviderResolver, LlmRequest,
    LlmRequestType, LlmUsage, feature_label, user_field,
};
use crate::domain::ports::{
    ContextMessageRecord, MessageRole, ThreadSummaryCommit, ThreadSummaryCommitOutcome,
    ThreadSummaryStore, ThreadSummaryTask, storage_now,
};
use crate::domain::stream::finalize::usage_tokens;

// ── Prompt texts (B.5.5) ─────────────────────────────────────────────────

/// Opening of the summary request when the chat has no summary yet.
pub const SUMMARY_OPENING: &str = "Summarize the following conversation:";

/// Opening of the summary request when a summary exists (followed by the
/// `<existing_summary>` block).
pub const MERGE_OPENING: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";

/// Header of the new messages after the `<existing_summary>` block.
pub const NEW_MESSAGES_HEADER: &str = "New messages to incorporate:";

/// Analysis instruction closing the summary request.
pub const ANALYSIS_INSTRUCTION: &str =
    "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:
1. Chronologically review each exchange, identifying:
   - The user's requests and questions
   - Key decisions, answers, and information shared
   - Any follow-up actions or commitments
   - Specific names, dates, numbers, URLs, or references mentioned
2. Verify accuracy and completeness.

Your summary MUST include these sections:

1. Conversation Purpose: The user's primary goals and recurring themes
2. Key Information Exchanged: Important facts, decisions, recommendations, and answers
3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections
4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit
5. Current Topic: What was being discussed most recently, with enough detail to continue naturally

Respond with an <analysis> block followed by a <summary> block.";

/// Separator of the prompt parts and of the message entries.
const BLANK_LINE: &str = "\n\n";

/// Marker appended to message content cut to `message_content_limit`.
const CLIP_MARKER: &str = "...";

// ── System task attribution (section 3.2) ────────────────────────────────

/// `system_task_type` of the thread summary.
pub const SYSTEM_TASK_THREAD_SUMMARY: &str = "thread_summary_update";
/// `requester_type` of a system task.
pub const REQUESTER_SYSTEM: &str = "system";
/// `billing_outcome` of a system task.
pub const BILLING_OUTCOME_SYSTEM_TASK: &str = "system_task";
/// `settlement_method` of a system task (no quota settlement).
pub const SETTLEMENT_METHOD_NONE: &str = "none";
/// `terminal_state` of a committed system task.
pub const TERMINAL_COMPLETED: &str = "completed";

/// Context-length-exceeded retries of one delivery (dropping the oldest
/// ~20% of the messages each time).
pub const PTL_MAX_RETRIES: u32 = 2;

/// Bytes per token of the stored `token_estimate` fallback.
const TOKEN_ESTIMATE_BYTES_PER_TOKEN: u64 = 4;

// ── Settings ─────────────────────────────────────────────────────────────

/// Summary settings from `thread_summary_worker`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarySettings {
    /// Catalog id of the summary model (default applied).
    pub model_id: String,
    /// `summary_system_prompt` (fallback of the catalog prompt).
    pub system_prompt: String,
    /// Max characters per message in the prompt; 0 = no truncation.
    pub message_content_limit: usize,
    /// Deliveries after which a retryable outcome is rejected (>= 1).
    pub max_attempts: u32,
}

impl SummarySettings {
    /// Settings of `config`.
    #[must_use]
    pub fn from_config(config: &ThreadSummaryWorkerConfig) -> Self {
        Self {
            model_id: config.effective_summary_model_id().to_owned(),
            system_prompt: config.summary_system_prompt.clone(),
            message_content_limit: config.message_content_limit,
            max_attempts: config.max_attempts.max(1),
        }
    }
}

// ── Prompt ───────────────────────────────────────────────────────────────

/// System prompt of the summary request: the summary model's catalog
/// `thread_summary_prompt` when not empty, else the configured prompt, else
/// the built-in default.
#[must_use]
pub fn select_system_prompt<'a>(catalog: &'a str, configured: &'a str) -> &'a str {
    if !catalog.trim().is_empty() {
        catalog
    } else if !configured.trim().is_empty() {
        configured
    } else {
        crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT
    }
}

/// `content` cut to `limit` characters followed by `...` when longer; `limit
/// = 0` keeps it whole.
#[must_use]
pub fn clip_content(content: &str, limit: usize) -> String {
    if limit == 0 {
        return content.to_owned();
    }
    match content.char_indices().nth(limit) {
        Some((end, _)) => format!("{}{CLIP_MARKER}", &content[..end]),
        None => content.to_owned(),
    }
}

/// The `User: …` / `Assistant: …` entries of `messages` (system messages
/// skipped), separated by a blank line.
#[must_use]
pub fn prompt_entries(messages: &[ContextMessageRecord], limit: usize) -> String {
    messages
        .iter()
        .filter_map(|message| {
            let prefix = match message.role {
                MessageRole::User => "User",
                MessageRole::Assistant => "Assistant",
                MessageRole::System => return None,
            };
            Some(format!(
                "{prefix}: {}",
                clip_content(&message.content, limit)
            ))
        })
        .collect::<Vec<_>>()
        .join(BLANK_LINE)
}

/// The user prompt of the summary request (B.5.5): the opening (with the
/// `<existing_summary>` block and the new-messages header when a summary
/// exists), the entries, then the analysis instruction.
#[must_use]
pub fn build_user_prompt(
    existing_summary: Option<&str>,
    messages: &[ContextMessageRecord],
    limit: usize,
) -> String {
    let entries = prompt_entries(messages, limit);
    match existing_summary {
        None => [SUMMARY_OPENING, &entries, ANALYSIS_INSTRUCTION].join(BLANK_LINE),
        Some(summary) => {
            let block = format!("<existing_summary>\n{summary}\n</existing_summary>");
            [
                MERGE_OPENING,
                &block,
                NEW_MESSAGES_HEADER,
                &entries,
                ANALYSIS_INSTRUCTION,
            ]
            .join(BLANK_LINE)
        }
    }
}

/// Input budget of the summary model: `context_window - max_output_tokens`,
/// capped by `max_input_tokens` when > 0; `None` (no fitting) when the
/// catalog `context_window` is 0.
#[must_use]
pub fn input_budget(model: &ModelCatalogEntry) -> Option<u64> {
    if model.context_window == 0 {
        return None;
    }
    let window = u64::from(model.context_window).saturating_sub(u64::from(model.max_output_tokens));
    Some(if model.max_input_tokens > 0 {
        window.min(u64::from(model.max_input_tokens))
    } else {
        window
    })
}

/// Estimated tokens of the system and user prompts at `bytes_per_token`
/// bytes per token (rounded up).
#[must_use]
pub fn estimate_prompt_tokens(system: &str, user: &str, bytes_per_token: u32) -> u64 {
    let bytes = u64::try_from(system.len().saturating_add(user.len())).unwrap_or(u64::MAX);
    bytes.div_ceil(u64::from(bytes_per_token.max(1)))
}

/// Number of oldest messages to drop from `n` in one step: `ceil(n/5)`,
/// keeping at least two messages.
#[must_use]
pub const fn drop_step(n: usize) -> usize {
    let step = n.div_ceil(5);
    let max = n.saturating_sub(2);
    if step < max { step } else { max }
}

/// Index of the first message kept after fitting the prompt to `budget`.
#[must_use]
pub fn fit_start(
    system: &str,
    existing_summary: Option<&str>,
    messages: &[ContextMessageRecord],
    limit: usize,
    budget: Option<u64>,
    bytes_per_token: u32,
) -> usize {
    let Some(budget) = budget else {
        return 0;
    };
    let mut start = 0;
    loop {
        let prompt = build_user_prompt(existing_summary, &messages[start..], limit);
        if estimate_prompt_tokens(system, &prompt, bytes_per_token) <= budget {
            return start;
        }
        let step = drop_step(messages.len() - start);
        if step == 0 {
            return start;
        }
        start += step;
    }
}

/// Whether `failure` reports a prompt too large for the model.
#[must_use]
pub fn is_context_length_exceeded(failure: &LlmFailure) -> bool {
    let code = failure.provider_code.as_deref().unwrap_or_default();
    if code == "context_length_exceeded" {
        return true;
    }
    let message = failure.message_sanitized.to_ascii_lowercase();
    [
        "context_length_exceeded",
        "maximum context length",
        "context length",
        "prompt is too long",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

// ── Response parsing ─────────────────────────────────────────────────────

/// `text` without every complete `open … close` block.
fn strip_blocks(text: &str, open: &str, close: &str) -> String {
    let mut rest = text;
    let mut out = String::with_capacity(text.len());
    while let Some(start) = rest.find(open) {
        let Some(len) = rest[start + open.len()..].find(close) else {
            break;
        };
        out.push_str(&rest[..start]);
        rest = &rest[start + open.len() + len + close.len()..];
    }
    out.push_str(rest);
    out
}

/// Content of the first complete `open … close` block.
fn block_content<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let len = text[start..].find(close)?;
    Some(&text[start..start + len])
}

/// `text` trimmed, with every run of blank lines collapsed into one.
#[must_use]
pub fn collapse_blank_lines(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    let mut previous_blank = false;
    for line in text.trim().lines() {
        let blank = line.trim().is_empty();
        if blank && previous_blank {
            continue;
        }
        lines.push(if blank { "" } else { line });
        previous_blank = blank;
    }
    lines.join("\n")
}

/// The stored summary of a provider answer: `<analysis>…</analysis>` removed,
/// the `<summary>…</summary>` content (else the remaining text, unless it
/// still contains `<analysis` or `<summary` markup: empty), runs of blank
/// lines collapsed.
#[must_use]
pub fn parse_summary(raw: &str) -> String {
    let remaining = strip_blocks(raw, "<analysis>", "</analysis>");
    let body = match block_content(&remaining, "<summary>", "</summary>") {
        Some(content) => content.to_owned(),
        None if remaining.contains("<analysis") || remaining.contains("<summary") => String::new(),
        None => remaining,
    };
    collapse_blank_lines(&body)
}

/// Stored `token_estimate`: `output_tokens - reasoning_tokens` when positive,
/// else `ceil(summary bytes / 4)`.
#[must_use]
pub fn token_estimate(usage: Option<&LlmUsage>, summary: &str) -> i32 {
    let reported = usage.map_or(0, |usage| {
        usage.output_tokens.saturating_sub(usage.reasoning_tokens)
    });
    let estimate = if reported > 0 {
        u64::try_from(reported).unwrap_or(u64::MAX)
    } else {
        u64::try_from(summary.len())
            .unwrap_or(u64::MAX)
            .div_ceil(TOKEN_ESTIMATE_BYTES_PER_TOKEN)
    };
    i32::try_from(estimate).unwrap_or(i32::MAX)
}

// ── Usage event ──────────────────────────────────────────────────────────

/// The system usage event of a committed summary (section 3.2 "System Task
/// Attribution Rules"): `requester_type = system`, `billing_outcome =
/// system_task`, `settlement_method = none`, zero credits, the task's stable
/// `system_request_id` as `request_id`, no `user_id` / `turn_id`, token
/// counts of the provider call.
#[must_use]
pub fn system_usage_event(
    task: &ThreadSummaryTask,
    model_id: &str,
    usage: Option<&LlmUsage>,
    at: OffsetDateTime,
) -> UsageEvent {
    UsageEvent {
        tenant_id: task.tenant_id,
        user_id: None,
        chat_id: Some(task.chat_id),
        turn_id: None,
        request_id: task.system_request_id,
        effective_model: model_id.to_owned(),
        selected_model: model_id.to_owned(),
        terminal_state: TERMINAL_COMPLETED.to_owned(),
        billing_outcome: BILLING_OUTCOME_SYSTEM_TASK.to_owned(),
        usage: usage.map(usage_tokens),
        actual_credits_micro: 0,
        settlement_method: SETTLEMENT_METHOD_NONE.to_owned(),
        policy_version_applied: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: at,
        requester_type: REQUESTER_SYSTEM.to_owned(),
        dedupe_key: dedupe_key_for_system_task(
            task.tenant_id,
            SYSTEM_TASK_THREAD_SUMMARY,
            task.system_request_id,
        ),
        system_task_type: Some(SYSTEM_TASK_THREAD_SUMMARY.to_owned()),
    }
}

// ── Outcomes ─────────────────────────────────────────────────────────────

/// Result of one delivery of a thread-summary work item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryOutcome {
    /// Summary committed (frontier advanced, range compressed).
    Committed { token_estimate: i32 },
    /// The stored frontier differs from the base (pre-check or commit).
    CasConflict,
    /// The summary the task was based on no longer exists.
    BaseMissing,
    /// The target frontier message was deleted before the commit.
    FrontierDeleted,
    /// The range holds no message to summarize.
    NothingToSummarize,
    /// The summary model is missing from the catalog or disabled.
    ModelUnavailable(String),
    /// The summary LLM call failed; the previous summary is kept.
    ProviderError(String),
    /// The parsed summary is empty.
    EmptySummary,
    /// Any other transient failure (plugin, provider resolution, database).
    Retry(String),
}

impl SummaryOutcome {
    /// `result` label of `{prefix}_thread_summary_execution`, `None` for the
    /// outcomes the counter does not record (CAS conflicts are counted by
    /// `{prefix}_thread_summary_cas_conflicts`).
    #[must_use]
    pub const fn metric_result(&self) -> Option<&'static str> {
        match self {
            Self::Committed { .. } => Some("success"),
            Self::ProviderError(_) => Some("provider_error"),
            Self::EmptySummary => Some("empty_summary"),
            Self::Retry(_) => Some("retry"),
            Self::ModelUnavailable(_) => Some("model_unavailable"),
            Self::FrontierDeleted => Some("frontier_deleted"),
            Self::BaseMissing => Some("base_missing"),
            Self::CasConflict | Self::NothingToSummarize => None,
        }
    }
}

/// What the queue handler returns for a delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// Acknowledge.
    Ok,
    /// Redeliver with the outbox backoff.
    Retry,
    /// Dead-letter with the reason.
    Reject(String),
}

/// Disposition of `outcome` on delivery `attempt` (1-based): a retryable
/// outcome on delivery `max_attempts` (or later) is rejected.
#[must_use]
pub fn disposition(outcome: &SummaryOutcome, attempt: u32, max_attempts: u32) -> Disposition {
    let reason = match outcome {
        SummaryOutcome::Committed { .. }
        | SummaryOutcome::CasConflict
        | SummaryOutcome::BaseMissing
        | SummaryOutcome::FrontierDeleted
        | SummaryOutcome::NothingToSummarize => return Disposition::Ok,
        SummaryOutcome::ModelUnavailable(reason) => {
            return Disposition::Reject(format!("summary model unavailable: {reason}"));
        }
        SummaryOutcome::ProviderError(reason) => format!("summary provider call failed: {reason}"),
        SummaryOutcome::EmptySummary => "summary provider returned an empty summary".to_owned(),
        SummaryOutcome::Retry(reason) => reason.clone(),
    };
    if attempt >= max_attempts.max(1) {
        Disposition::Reject(format!("{reason} (attempt {attempt} of {max_attempts})"))
    } else {
        Disposition::Retry
    }
}

// ── Model resolution ─────────────────────────────────────────────────────

/// The enabled catalog entry `model_id` of the current policy (read as the
/// platform system subject). `Err` carries the outcome of a failure.
async fn resolve_summary_model(
    policy: &dyn MiniChatModelPolicyPluginClientV1,
    model_id: &str,
) -> Result<ModelCatalogEntry, SummaryOutcome> {
    let version = policy
        .get_current_policy_version(DEFAULT_SUBJECT_ID)
        .await
        .map_err(|err| SummaryOutcome::Retry(format!("policy version read failed: {err}")))?;
    let snapshot = policy
        .get_policy_snapshot(DEFAULT_SUBJECT_ID, version)
        .await
        .map_err(|err| SummaryOutcome::Retry(format!("policy snapshot read failed: {err}")))?;
    snapshot
        .model_catalog
        .into_iter()
        .find(|entry| entry.enabled && entry.id == model_id)
        .ok_or_else(|| {
            SummaryOutcome::ModelUnavailable(format!(
                "model `{model_id}` is missing from the catalog or disabled"
            ))
        })
}

/// Result of the gear-start summary model check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryModelCheck {
    /// `thread_summary_worker.enabled = false`: not checked.
    NotEnabled,
    /// The model is an enabled catalog entry.
    Available,
    /// The model is missing or disabled (logged as an error).
    Unavailable,
    /// The catalog could not be read (logged as a warning).
    CatalogUnavailable(String),
}

/// Gear-start check of the summary model (DESIGN section 3.6): when
/// summaries are enabled, resolves the model with the enabled filter and
/// logs an error when it is missing or disabled. Startup continues in every
/// case: a dynamic policy plugin can add the model later.
pub async fn check_summary_model(
    policy: &dyn MiniChatModelPolicyPluginClientV1,
    config: &ThreadSummaryWorkerConfig,
) -> SummaryModelCheck {
    if !config.enabled {
        return SummaryModelCheck::NotEnabled;
    }
    let model_id = config.effective_summary_model_id();
    let check = match resolve_summary_model(policy, model_id).await {
        Ok(_) => SummaryModelCheck::Available,
        Err(SummaryOutcome::ModelUnavailable(_)) => SummaryModelCheck::Unavailable,
        Err(other) => SummaryModelCheck::CatalogUnavailable(format!("{other:?}")),
    };
    log_model_check(model_id, &check);
    check
}

fn log_model_check(model_id: &str, check: &SummaryModelCheck) {
    match check {
        SummaryModelCheck::NotEnabled => {}
        SummaryModelCheck::Available => log_model_available(model_id),
        SummaryModelCheck::Unavailable => log_model_unavailable(model_id),
        SummaryModelCheck::CatalogUnavailable(reason) => log_catalog_unavailable(model_id, reason),
    }
}

fn log_model_available(model_id: &str) {
    info!(target: "mini_chat.thread_summary", model = %model_id, "thread summary model available");
}

fn log_model_unavailable(model_id: &str) {
    error!(
        target: "mini_chat.thread_summary",
        model = %model_id,
        "thread summary model is missing or disabled; summary tasks are rejected until it is enabled"
    );
}

fn log_catalog_unavailable(model_id: &str, reason: &str) {
    warn!(
        target: "mini_chat.thread_summary",
        model = %model_id,
        reason = %reason,
        "thread summary model could not be checked at start"
    );
}

// ── Service ──────────────────────────────────────────────────────────────

/// Executes thread-summary work items (module docs).
pub struct ThreadSummaryService {
    store: Arc<dyn ThreadSummaryStore>,
    policy: Arc<dyn MiniChatModelPolicyPluginClientV1>,
    resolver: Arc<dyn LlmProviderResolver>,
    llm: Arc<dyn LlmProvider>,
    settings: SummarySettings,
}

impl std::fmt::Debug for ThreadSummaryService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadSummaryService")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl ThreadSummaryService {
    /// Service over the summary persistence, the model policy plugin (catalog
    /// of the summary model), the provider resolver and the LLM adapters.
    #[must_use]
    pub fn new(
        store: Arc<dyn ThreadSummaryStore>,
        policy: Arc<dyn MiniChatModelPolicyPluginClientV1>,
        resolver: Arc<dyn LlmProviderResolver>,
        llm: Arc<dyn LlmProvider>,
        settings: SummarySettings,
    ) -> Self {
        Self {
            store,
            policy,
            resolver,
            llm,
            settings,
        }
    }

    /// The settings.
    #[must_use]
    pub const fn settings(&self) -> &SummarySettings {
        &self.settings
    }

    /// Runs one delivery of `task` (module docs).
    pub async fn execute(&self, task: &ThreadSummaryTask) -> SummaryOutcome {
        let model = match resolve_summary_model(self.policy.as_ref(), &self.settings.model_id).await
        {
            Ok(model) => model,
            Err(outcome) => return outcome,
        };
        let scope = AccessScope::for_tenant(task.tenant_id);
        let existing = match self.precheck(&scope, task).await {
            Ok(existing) => existing,
            Err(outcome) => return outcome,
        };
        let messages: Vec<ContextMessageRecord> = match self
            .store
            .summary_range(&scope, task.chat_id, task.base_frontier, task.frozen_target)
            .await
        {
            Ok(range) => range
                .into_iter()
                .filter(|message| message.role != MessageRole::System)
                .collect(),
            Err(err) => return SummaryOutcome::Retry(format!("summary range read failed: {err}")),
        };
        if messages.is_empty() {
            return SummaryOutcome::NothingToSummarize;
        }
        let target = match self.resolver.resolve(&model.provider_id, task.tenant_id) {
            Ok(target) => target,
            Err(err) => return SummaryOutcome::Retry(format!("provider resolution failed: {err}")),
        };
        let system =
            select_system_prompt(&model.thread_summary_prompt, &self.settings.system_prompt);
        let limit = self.settings.message_content_limit;
        let mut start = fit_start(
            system,
            existing.as_deref(),
            &messages,
            limit,
            input_budget(&model),
            model.estimation_budgets.bytes_per_token_conservative,
        );
        let mut ptl_retries = 0;
        let completion = loop {
            let prompt = build_user_prompt(existing.as_deref(), &messages[start..], limit);
            let request = summary_request(&model, task, system, prompt);
            match self.llm.complete(&target, request).await {
                Ok(completion) => break completion,
                Err(failure) => {
                    let step = drop_step(messages.len() - start);
                    if is_context_length_exceeded(&failure)
                        && ptl_retries < PTL_MAX_RETRIES
                        && step > 0
                    {
                        ptl_retries += 1;
                        start += step;
                        warn!(
                            target: "mini_chat.thread_summary",
                            chat_id = %task.chat_id,
                            dropped = start,
                            "summary prompt too long; retrying without the oldest messages"
                        );
                        continue;
                    }
                    return SummaryOutcome::ProviderError(failure.to_string());
                }
            }
        };
        let summary = parse_summary(&completion.text);
        if summary.is_empty() {
            return SummaryOutcome::EmptySummary;
        }
        let estimate = token_estimate(completion.usage.as_ref(), &summary);
        let at = storage_now();
        let commit = ThreadSummaryCommit {
            tenant_id: task.tenant_id,
            chat_id: task.chat_id,
            summary_id: Uuid::new_v4(),
            base_frontier: task.base_frontier,
            target: task.frozen_target,
            summary_text: summary,
            token_estimate: estimate,
            usage_event: system_usage_event(task, &model.id, completion.usage.as_ref(), at),
            at,
        };
        match self.store.commit_summary(&scope, commit).await {
            Ok(ThreadSummaryCommitOutcome::Committed) => SummaryOutcome::Committed {
                token_estimate: estimate,
            },
            Ok(ThreadSummaryCommitOutcome::FrontierDeleted) => SummaryOutcome::FrontierDeleted,
            Ok(ThreadSummaryCommitOutcome::CasConflict) => SummaryOutcome::CasConflict,
            Ok(ThreadSummaryCommitOutcome::BaseMissing) => SummaryOutcome::BaseMissing,
            Err(err) => SummaryOutcome::Retry(format!("summary commit failed: {err}")),
        }
    }

    /// The CAS pre-check: the existing summary text when the stored frontier
    /// equals the task's base, else the finishing outcome.
    async fn precheck(
        &self,
        scope: &AccessScope,
        task: &ThreadSummaryTask,
    ) -> Result<Option<String>, SummaryOutcome> {
        let current = self
            .store
            .current_summary(scope, task.chat_id)
            .await
            .map_err(|err| SummaryOutcome::Retry(format!("summary read failed: {err}")))?;
        match (task.base_frontier, current) {
            (None, None) => Ok(None),
            (Some(base), Some(current)) if current.frontier == base => Ok(Some(current.text)),
            (Some(_), None) => Err(SummaryOutcome::BaseMissing),
            (None | Some(_), Some(_)) => {
                debug!(
                    target: "mini_chat.thread_summary",
                    chat_id = %task.chat_id,
                    "stored frontier differs from the task base; nothing to do"
                );
                Err(SummaryOutcome::CasConflict)
            }
        }
    }
}

/// The non-streaming summary request under the system identity.
fn summary_request(
    model: &ModelCatalogEntry,
    task: &ThreadSummaryTask,
    system: &str,
    prompt: String,
) -> LlmRequest {
    let tenant_id = task.tenant_id.to_string();
    let user_id = DEFAULT_SUBJECT_ID.to_string();
    LlmRequest {
        model: model.provider_model_id.clone(),
        instructions: system.to_owned(),
        input: vec![LlmInputItem::User {
            text: prompt,
            image_file_ids: Vec::new(),
        }],
        tools: Vec::new(),
        max_output_tokens: model.max_output_tokens,
        max_tool_calls: model.max_tool_calls,
        api_params: model.general_config.api_params.clone(),
        user: user_field(&tenant_id, &user_id),
        metadata: LlmMetadata {
            tenant_id,
            user_id,
            chat_id: task.chat_id.to_string(),
            request_type: LlmRequestType::Summary,
            feature: feature_label(&[]),
        },
        secondary_file_ids: std::collections::BTreeMap::new(),
    }
}

#[cfg(test)]
#[path = "summary_service_tests.rs"]
mod tests;
