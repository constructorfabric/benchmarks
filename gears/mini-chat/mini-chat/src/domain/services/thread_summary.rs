//! Thread summary (S§10.2, D§3.6 "Thread Summary Update", B.5.5): the
//! trigger evaluated in the finalization transaction of completed turns,
//! the outbox task that calls the summary model (non-streaming, system
//! identity) and commits the summary with a CAS on the frontier, and the
//! prompt / response helpers.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use mini_chat_sdk::{
    BillingOutcome, ModelCatalogEntry, PolicySnapshot, RequesterType, SettlementMethod,
    TerminalState, UsageEvent, UsageTokens,
};
use regex::Regex;
use time::OffsetDateTime;
use toolkit_db::secure::AccessScope;
use toolkit_db::{DBProvider, DbTx};
use toolkit_security::constants::DEFAULT_SUBJECT_ID;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::config::{DEFAULT_SUMMARY_SYSTEM_PROMPT, MiniChatConfig};
use crate::domain::clock::Clock;
use crate::domain::context::HistoryMessage;
use crate::domain::error::DomainError;
use crate::domain::ports::{
    HandlerOutcome, LlmPort, OutboxPort, PendingWakes, PolicyPort, SummaryHook, SummaryHookInput,
    SummaryTriggerResult, ThreadSummaryRunner,
};
use crate::domain::services::stream_service::history_message;
use crate::infra::db::entity::thread_summary;
use crate::infra::db::repos::{MessageRepo, OrderKey, ThreadSummaryRepo};
use crate::infra::db::tx::with_retry;
use crate::infra::llm::provider_resolver::ProviderResolver;
use crate::infra::llm::sanitize::provider_user_field;
use crate::infra::llm::types::{InputMessage, LlmRequest, RequestMetadata, Role};
use crate::infra::metrics::MiniChatMetrics;
use crate::infra::outbox::payloads::ThreadSummaryPayload;

/// Opening of the first summary request (D B.5.5).
const FIRST_SUMMARY_OPENING: &str = "Summarize the following conversation:";

/// Opening of a summary request that merges an existing summary (D B.5.5).
const MERGE_OPENING: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";

/// Heading of the messages after the existing summary block (D B.5.5).
const NEW_MESSAGES_HEADING: &str = "New messages to incorporate:";

/// Analysis instruction at the end of the summary request (D B.5.5, verbatim).
pub const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n\
1. Chronologically review each exchange, identifying:\n   \
- The user's requests and questions\n   \
- Key decisions, answers, and information shared\n   \
- Any follow-up actions or commitments\n   \
- Specific names, dates, numbers, URLs, or references mentioned\n\
2. Verify accuracy and completeness.\n\
\n\
Your summary MUST include these sections:\n\
\n\
1. Conversation Purpose: The user's primary goals and recurring themes\n\
2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n\
3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n\
4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n\
5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\
\n\
Respond with an <analysis> block followed by a <summary> block.";

/// Separator between prompt parts and between message entries.
const BLANK_LINE: &str = "\n\n";

#[allow(clippy::expect_used, reason = "constant pattern")]
static ANALYSIS_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<analysis>.*?</analysis>").expect("valid regex"));
#[allow(clippy::expect_used, reason = "constant pattern")]
static SUMMARY_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<summary>(.*?)</summary>").expect("valid regex"));

/// User prompt of the summary request (D B.5.5): the opening (with the
/// `<existing_summary>` block when a summary exists), one `User:` /
/// `Assistant:` entry per message (content cut to `limit` characters plus
/// `...`; `0` = no limit), then the analysis instruction.
#[must_use]
pub fn build_summary_prompt(
    existing: Option<&str>,
    msgs: &[HistoryMessage],
    limit: usize,
) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(msgs.len() + 4);
    match existing {
        None => parts.push(FIRST_SUMMARY_OPENING.to_owned()),
        Some(summary) => {
            parts.push(MERGE_OPENING.to_owned());
            parts.push(format!(
                "<existing_summary>\n{summary}\n</existing_summary>"
            ));
            parts.push(NEW_MESSAGES_HEADING.to_owned());
        }
    }
    parts.extend(msgs.iter().map(|m| {
        let speaker = match m.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
        };
        format!("{speaker}: {}", truncate_chars(&m.content, limit))
    }));
    parts.push(ANALYSIS_INSTRUCTION.to_owned());
    parts.join(BLANK_LINE)
}

/// `text` cut to `limit` characters followed by `...` when longer (`0` =
/// no limit).
fn truncate_chars(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((cut, _)) if limit > 0 => format!("{}...", &text[..cut]),
        _ => text.to_owned(),
    }
}

/// Stored summary text of a provider response (D "Response parsing"): the
/// `<analysis>` block is removed; the content of `<summary>` is kept, or
/// without it the whole remaining text unless it still contains
/// `<analysis` / `<summary` markup (then empty). Runs of blank lines are
/// collapsed and the result is trimmed.
#[must_use]
pub fn parse_summary(text: &str) -> String {
    let without_analysis = ANALYSIS_BLOCK.replace_all(text, "");
    let body = match SUMMARY_BLOCK.captures(&without_analysis) {
        Some(c) => c.get(1).map_or("", |m| m.as_str()).to_owned(),
        None if without_analysis.contains("<analysis") || without_analysis.contains("<summary") => {
            return String::new();
        }
        None => without_analysis.into_owned(),
    };
    collapse_blank_lines(&body)
}

/// Collapse runs of blank (whitespace-only) lines into one empty line and
/// trim the result.
fn collapse_blank_lines(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in text.trim().lines() {
        let blank = line.trim().is_empty();
        if blank && out.last().is_some_and(|l| l.is_empty()) {
            continue;
        }
        out.push(if blank { "" } else { line });
    }
    out.join("\n")
}

/// Stored `token_estimate` of a summary: `output_tokens - reasoning_tokens`
/// of the call, or `ceil(summary bytes / 4)` when that is not positive.
#[must_use]
pub fn summary_token_estimate(usage: Option<&UsageTokens>, summary: &str) -> i32 {
    let reported = usage.map_or(0, |u| u.output_tokens.saturating_sub(u.reasoning_tokens));
    let estimate = if reported > 0 {
        reported
    } else {
        i64::try_from(summary.len().div_ceil(4)).unwrap_or(i64::MAX)
    };
    i32::try_from(estimate).unwrap_or(i32::MAX)
}

/// User prompt fitted to the summary model's input `budget` (tokens; `None`
/// = no fitting): while the estimated size of system prompt + user prompt
/// (`bytes_per_token` bytes per token) exceeds it, the oldest `ceil(n/5)`
/// messages are dropped, keeping at least two.
#[must_use]
pub fn fit_prompt(
    system_prompt: &str,
    existing: Option<&str>,
    msgs: &[HistoryMessage],
    limit: usize,
    budget: Option<i64>,
    bytes_per_token: u32,
) -> String {
    let bpt = usize::try_from(bytes_per_token.max(1)).unwrap_or(1);
    let mut start = 0;
    loop {
        let prompt = build_summary_prompt(existing, &msgs[start..], limit);
        let size =
            i64::try_from((system_prompt.len() + prompt.len()).div_ceil(bpt)).unwrap_or(i64::MAX);
        let left = msgs.len() - start;
        if budget.is_none_or(|b| size <= b) || left <= 2 {
            return prompt;
        }
        start += left.div_ceil(5).min(left - 2);
    }
}

/// Metric `result` of a failed summary call (`summary_fallback` counts the
/// same cases).
const RESULT_PROVIDER_ERROR: &str = "provider_error";
/// Metric `result` of a response without summary text.
const RESULT_EMPTY_SUMMARY: &str = "empty_summary";
/// Metric `result` of other retryable failures (policy, provider
/// resolution, database).
const RESULT_RETRY: &str = "retry";
/// Metric `result` (and dead-letter reason) of a missing / disabled
/// summary model.
const RESULT_MODEL_UNAVAILABLE: &str = "model_unavailable";

/// Identity of the summary call: the platform default subject
/// (D "Provider Request Metadata").
const SYSTEM_SUBJECT_ID: Uuid = DEFAULT_SUBJECT_ID;

/// Dependencies of [`ThreadSummaryService`].
pub struct ThreadSummaryDeps {
    pub config: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub clock: Arc<dyn Clock>,
    pub policy: Arc<dyn PolicyPort>,
    pub providers: Arc<ProviderResolver>,
    pub llm: Arc<dyn LlmPort>,
    pub outbox: Arc<dyn OutboxPort>,
    pub metrics: Arc<MiniChatMetrics>,
}

/// Thread summary: the trigger in the finalization of completed turns
/// ([`SummaryHook`]) and the outbox task ([`ThreadSummaryRunner`]).
pub struct ThreadSummaryService {
    d: ThreadSummaryDeps,
}

/// How a task attempt ended without a failure (the task is done).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Finished {
    /// Summary saved, frontier advanced, range compressed, usage enqueued.
    Committed,
    /// The stored frontier is no longer the task's base (another commit
    /// advanced it).
    CasConflict,
    /// The frozen target message was deleted (retry / edit / delete).
    FrontierDeleted,
    /// The summary the task was based on no longer exists.
    BaseMissing,
    /// No live, uncompressed message is left in the range.
    NothingToSummarize,
}

/// Why a task attempt failed.
#[derive(Debug)]
enum Failure {
    /// The summary model is missing from the catalog or disabled.
    ModelUnavailable,
    /// A failure a later delivery may not hit (`result` = metric label).
    Retry {
        result: &'static str,
        detail: String,
    },
}

impl Failure {
    fn retry(result: &'static str, detail: impl std::fmt::Display) -> Self {
        Self::Retry {
            result,
            detail: detail.to_string(),
        }
    }

    fn db(e: impl Into<DomainError>) -> Self {
        Self::retry(RESULT_RETRY, e.into())
    }
}

/// Values of one CAS commit (cloned per transaction attempt).
#[derive(Clone)]
struct CommitInput {
    scope: AccessScope,
    tenant_id: Uuid,
    chat_id: Uuid,
    base: Option<OrderKey>,
    target: OrderKey,
    token_estimate: i32,
    summary_text: String,
    /// The messages of the summarized range (marked compressed).
    range_ids: Vec<Uuid>,
    usage_event: UsageEvent,
    now: OffsetDateTime,
}

impl ThreadSummaryService {
    #[must_use]
    pub fn new(d: ThreadSummaryDeps) -> Self {
        Self { d }
    }

    /// Gear-start check: when summaries are enabled, log an error if the
    /// summary model is missing from the catalog or disabled (startup
    /// continues; a dynamic policy plugin can add it later). Returns `false`
    /// only in that case (disabled summaries or an unavailable catalog are
    /// not a missing model).
    pub async fn check_summary_model(&self) -> bool {
        let cfg = &self.d.config.thread_summary_worker;
        if !cfg.enabled {
            return true;
        }
        let model = cfg.effective_summary_model_id();
        match self.d.policy.current_snapshot(SYSTEM_SUBJECT_ID).await {
            Ok(snap) if enabled_entry(&snap, model).is_none() => {
                error!(
                    model,
                    "thread summary model is missing from the catalog or disabled; summary tasks will be rejected"
                );
                false
            }
            Ok(_) => true,
            Err(e) => {
                warn!(model, error = %e, "thread summary model not checked: policy catalog unavailable");
                true
            }
        }
    }

    /// One attempt of a summary task (D "Execution stages and invariants").
    async fn execute(&self, p: &ThreadSummaryPayload) -> Result<Finished, Failure> {
        let cfg = &self.d.config.thread_summary_worker;
        let snap = self
            .d
            .policy
            .current_snapshot(SYSTEM_SUBJECT_ID)
            .await
            .map_err(|e| Failure::retry(RESULT_RETRY, e))?;
        let entry = enabled_entry(&snap, cfg.effective_summary_model_id())
            .cloned()
            .ok_or(Failure::ModelUnavailable)?;
        let target = self
            .d
            .providers
            .resolve(&entry.provider_id, p.tenant_id)
            .map_err(|e| Failure::retry(RESULT_RETRY, e))?;

        let scope = AccessScope::for_tenant(p.tenant_id);
        let backend = self.d.db.db().backend();
        let conn = self.d.db.conn().map_err(Failure::db)?;
        let current = ThreadSummaryRepo
            .find_by_chat(&conn, &scope, p.chat_id)
            .await
            .map_err(Failure::db)?;
        if let Some(done) = base_check(p.base_frontier(), current.as_ref()) {
            return Ok(done);
        }
        let rows = MessageRepo
            .summary_range(
                &conn,
                &scope,
                p.chat_id,
                p.base_frontier(),
                p.frozen_target(),
                backend,
            )
            .await
            .map_err(Failure::db)?;
        let range_ids: Vec<Uuid> = rows.iter().map(|m| m.id).collect();
        let msgs: Vec<HistoryMessage> = rows.into_iter().filter_map(history_message).collect();
        if msgs.is_empty() {
            return Ok(Finished::NothingToSummarize);
        }

        let system_prompt = summary_system_prompt(&entry, &cfg.summary_system_prompt);
        let existing = current.as_ref().map(|s| s.summary_text.as_str());
        let prompt = fit_prompt(
            system_prompt,
            existing,
            &msgs,
            cfg.message_content_limit,
            summary_input_budget(&entry),
            entry.estimation_budgets.bytes_per_token_conservative,
        );
        let request = LlmRequest {
            model: entry.provider_model_id.clone(),
            instructions: system_prompt.to_owned(),
            input: vec![InputMessage::text(Role::User, prompt)],
            max_output_tokens: entry.max_output_tokens,
            tools: Vec::new(),
            max_tool_calls: entry.max_tool_calls,
            api_params: entry.general_config.api_params.clone(),
            user: provider_user_field(p.tenant_id, SYSTEM_SUBJECT_ID),
            metadata: RequestMetadata::summary(p.tenant_id, SYSTEM_SUBJECT_ID, p.chat_id),
            stream: false,
        };
        let completion = self
            .d
            .llm
            .complete(&target, request)
            .await
            .map_err(|f| Failure::retry(RESULT_PROVIDER_ERROR, f.message))?;
        let summary_text = parse_summary(&completion.text);
        if summary_text.is_empty() {
            return Err(Failure::retry(
                RESULT_EMPTY_SUMMARY,
                "the response contains no summary text",
            ));
        }

        let now = self.d.clock.now();
        self.commit(CommitInput {
            scope,
            tenant_id: p.tenant_id,
            chat_id: p.chat_id,
            base: p.base_frontier(),
            target: p.frozen_target(),
            token_estimate: summary_token_estimate(completion.usage.as_ref(), &summary_text),
            summary_text,
            range_ids,
            usage_event: system_usage_event(
                p,
                &entry.id,
                snap.policy_version,
                completion.usage,
                now,
            ),
            now,
        })
        .await
    }

    /// The atomic commit (D "CAS-protected commit"): CAS on the base
    /// frontier, target message not deleted (row locked), upsert of the
    /// summary, the range marked compressed, the system usage event.
    async fn commit(&self, input: CommitInput) -> Result<Finished, Failure> {
        let outbox = Arc::clone(&self.d.outbox);
        let result = with_retry(&self.d.db, move |tx| {
            let (outbox, c) = (Arc::clone(&outbox), input.clone());
            Box::pin(async move {
                let current = ThreadSummaryRepo
                    .find_by_chat(tx, &c.scope, c.chat_id)
                    .await?;
                if let Some(done) = base_check(c.base, current.as_ref()) {
                    return Ok((done, None));
                }
                if MessageRepo
                    .lock_live(tx, &c.scope, c.chat_id, c.target.1)
                    .await?
                    == 0
                {
                    return Ok((Finished::FrontierDeleted, None));
                }
                match c.base {
                    None => {
                        ThreadSummaryRepo
                            .insert(tx, &c.scope, new_summary_row(&c))
                            .await?;
                    }
                    Some((_, base_id)) => {
                        let rows = ThreadSummaryRepo
                            .advance(
                                tx,
                                &c.scope,
                                c.chat_id,
                                base_id,
                                &c.summary_text,
                                c.target,
                                c.token_estimate,
                                c.now,
                            )
                            .await?;
                        if rows == 0 {
                            return Ok((Finished::CasConflict, None));
                        }
                    }
                }
                MessageRepo
                    .mark_compressed(tx, &c.scope, c.chat_id, &c.range_ids)
                    .await?;
                let mut wakes = PendingWakes::new();
                outbox.enqueue_usage(tx, &c.usage_event, &mut wakes).await?;
                Ok((Finished::Committed, Some(wakes)))
            })
        })
        .await;
        match result {
            Ok((done, wakes)) => {
                if let Some(wakes) = wakes {
                    wakes.fire_all();
                }
                Ok(done)
            }
            // A concurrent first summary of the chat won the insert.
            Err(e) if e.is_unique_violation() => Ok(Finished::CasConflict),
            Err(e) => Err(Failure::db(e)),
        }
    }
}

#[async_trait]
impl SummaryHook for ThreadSummaryService {
    /// Trigger (D "Trigger timing"): urgent when the context was truncated,
    /// proactive when no summary exists and the assembled context reached
    /// `compression_threshold_pct` of the effective budget. The frozen
    /// target is the latest non-deleted message outside the causing turn.
    async fn maybe_enqueue(
        &self,
        tx: &DbTx<'_>,
        input: &SummaryHookInput,
        wakes: &mut PendingWakes,
    ) -> Result<SummaryTriggerResult, DomainError> {
        let cfg = &self.d.config.thread_summary_worker;
        let t = &input.trigger;
        let threshold_reached = t.assembled_context_tokens.saturating_mul(100)
            >= t.effective_budget
                .saturating_mul(i64::from(cfg.compression_threshold_pct));
        if !cfg.enabled || !(t.messages_truncated || threshold_reached) {
            return Ok(SummaryTriggerResult::NotEvaluated);
        }
        let scope = AccessScope::for_tenant(input.tenant_id);
        let summary = ThreadSummaryRepo
            .find_by_chat(tx, &scope, input.chat_id)
            .await?;
        if summary.is_some() && !t.messages_truncated {
            return Ok(SummaryTriggerResult::NotNeeded);
        }
        let base = summary.map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        let Some(target) = MessageRepo
            .latest_outside_turn(tx, &scope, input.chat_id, input.request_id)
            .await?
        else {
            return Ok(SummaryTriggerResult::NotNeeded);
        };
        let target = (target.created_at, target.id);
        if base.is_some_and(|b| target <= b) {
            return Ok(SummaryTriggerResult::NotNeeded);
        }
        let payload = ThreadSummaryPayload::new(input.tenant_id, input.chat_id, base, target);
        self.d
            .outbox
            .enqueue_thread_summary(tx, &payload, wakes)
            .await?;
        Ok(SummaryTriggerResult::Scheduled)
    }
}

#[async_trait]
impl ThreadSummaryRunner for ThreadSummaryService {
    /// A `Retry` on the `max_attempts`-th delivery becomes `Reject`.
    async fn run(&self, payload: ThreadSummaryPayload, attempt: u32) -> HandlerOutcome {
        let chat_id = payload.chat_id;
        let system_request_id = payload.system_request_id;
        match self.execute(&payload).await {
            Ok(done) => {
                match done {
                    Finished::Committed => {
                        self.d.metrics.summary_execution("success");
                        info!(%chat_id, %system_request_id, result = "success", "thread summary committed");
                    }
                    Finished::CasConflict => {
                        self.d.metrics.summary_cas_conflict();
                        info!(%chat_id, %system_request_id, "thread summary frontier already advanced; nothing committed");
                    }
                    Finished::FrontierDeleted => {
                        self.d.metrics.summary_execution("frontier_deleted");
                        info!(%chat_id, %system_request_id, result = "frontier_deleted", "thread summary target deleted; commit skipped");
                    }
                    Finished::BaseMissing => {
                        self.d.metrics.summary_execution("base_missing");
                        info!(%chat_id, %system_request_id, result = "base_missing", "thread summary base no longer exists; task dropped");
                    }
                    Finished::NothingToSummarize => {
                        info!(%chat_id, %system_request_id, "thread summary range is empty; task dropped");
                    }
                }
                HandlerOutcome::Ok
            }
            Err(Failure::ModelUnavailable) => {
                self.d.metrics.summary_execution(RESULT_MODEL_UNAVAILABLE);
                error!(
                    %chat_id,
                    %system_request_id,
                    model = self.d.config.thread_summary_worker.effective_summary_model_id(),
                    result = RESULT_MODEL_UNAVAILABLE,
                    "thread summary model is missing or disabled; task rejected"
                );
                HandlerOutcome::Reject(RESULT_MODEL_UNAVAILABLE.to_owned())
            }
            Err(Failure::Retry { result, detail }) => {
                // A provider error keeps the previous summary (fallback).
                self.d.metrics.summary_execution(result);
                if result == RESULT_PROVIDER_ERROR {
                    self.d.metrics.summary_fallback();
                }
                warn!(%chat_id, %system_request_id, attempt, result, error = %detail, "thread summary attempt failed");
                if attempt >= self.d.config.thread_summary_worker.max_attempts {
                    HandlerOutcome::Reject(result.to_owned())
                } else {
                    HandlerOutcome::Retry
                }
            }
        }
    }
}

/// The enabled catalog entry `id`.
fn enabled_entry<'a>(snap: &'a PolicySnapshot, id: &str) -> Option<&'a ModelCatalogEntry> {
    snap.model_catalog.iter().find(|m| m.enabled && m.id == id)
}

/// `None` when the stored frontier still equals the task's base (a message
/// id identifies its order key), otherwise how the task ends.
fn base_check(base: Option<OrderKey>, current: Option<&thread_summary::Model>) -> Option<Finished> {
    match (base, current) {
        (None, None) => None,
        (Some((_, base_id)), Some(s)) if s.summarized_up_to_message_id == base_id => None,
        (Some(_), None) => Some(Finished::BaseMissing),
        _ => Some(Finished::CasConflict),
    }
}

/// System prompt precedence: catalog `thread_summary_prompt`, then
/// `thread_summary_worker.summary_system_prompt`, then the built-in text.
fn summary_system_prompt<'a>(entry: &'a ModelCatalogEntry, configured: &'a str) -> &'a str {
    if !entry.thread_summary_prompt.is_empty() {
        &entry.thread_summary_prompt
    } else if !configured.is_empty() {
        configured
    } else {
        DEFAULT_SUMMARY_SYSTEM_PROMPT
    }
}

/// Input budget of the summary model: `context_window - max_output_tokens`,
/// capped by `max_input_tokens` when > 0; `None` (no fitting) when the
/// catalog `context_window` is 0.
fn summary_input_budget(entry: &ModelCatalogEntry) -> Option<i64> {
    if entry.context_window == 0 {
        return None;
    }
    let window = i64::from(entry.context_window) - i64::from(entry.max_output_tokens);
    Some(match entry.max_input_tokens {
        0 => window,
        max_in => window.min(i64::from(max_in)),
    })
}

/// First summary row of a chat.
fn new_summary_row(c: &CommitInput) -> thread_summary::Model {
    thread_summary::Model {
        id: Uuid::new_v4(),
        tenant_id: c.tenant_id,
        chat_id: c.chat_id,
        summary_text: c.summary_text.clone(),
        summarized_up_to_created_at: c.target.0,
        summarized_up_to_message_id: c.target.1,
        token_estimate: c.token_estimate,
        created_at: c.now,
        updated_at: c.now,
    }
}

/// System usage event of a committed summary (D "System Task Attribution
/// Rules"): no user / turn, zero credits, token counts only.
fn system_usage_event(
    p: &ThreadSummaryPayload,
    model: &str,
    policy_version: u64,
    usage: Option<UsageTokens>,
    now: OffsetDateTime,
) -> UsageEvent {
    UsageEvent {
        tenant_id: p.tenant_id,
        user_id: None,
        chat_id: p.chat_id,
        turn_id: None,
        request_id: p.system_request_id,
        effective_model: model.to_owned(),
        selected_model: model.to_owned(),
        terminal_state: TerminalState::Completed,
        billing_outcome: BillingOutcome::SystemTask,
        usage,
        actual_credits_micro: 0,
        settlement_method: SettlementMethod::None,
        policy_version_applied: policy_version,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: now,
        requester_type: RequesterType::System,
        dedupe_key: format!(
            "{}/{}/{}",
            p.tenant_id.simple(),
            p.system_task_type,
            p.system_request_id.simple()
        ),
        system_task_type: Some(p.system_task_type.clone()),
    }
}

#[cfg(test)]
#[path = "thread_summary_tests.rs"]
mod tests;
