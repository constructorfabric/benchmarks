//! Thread summaries (spec §14; DESIGN §3.6 "Thread Summary Update", "Thread
//! Summary - Stable Range and Commit Invariant", §3.2 "System Task Attribution
//! Rules", B.5.5, B.9.4).
//!
//! Scheduling: [`should_trigger`] decides from the turn's [`ContextPlan`];
//! [`ThreadSummaryService::build_payload`] freezes the target frontier before the
//! causing turn is finalized, and the finalization transaction enqueues it.
//!
//! Execution ([`ThreadSummaryService::run`], driven by the outbox handler):
//! resolve the summary model (enabled filter) and its provider, pre-check the
//! stored frontier against the task's base, load `(base, target]`, fit the prompt
//! to the model's input budget, make the non-streaming call (with up to two
//! prompt-too-long retries), parse the `<summary>` block and commit with a CAS on
//! the base frontier: summary upsert, `is_compressed` on the range and the system
//! usage event, in one transaction.

use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};
use mini_chat_sdk::{ModelCatalogEntry, UsageEvent};
use sea_orm::DbBackend;
use toolkit_db::DBProvider;
use toolkit_db::secure::DBRunner;
use toolkit_security::constants::DEFAULT_SUBJECT_ID;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::config::{DEFAULT_SUMMARY_SYSTEM_PROMPT, MiniChatConfig};
use crate::domain::clock::now_utc;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::model::{BillingOutcome, MessageRole, SettlementMethod, TurnState};
use crate::domain::sanitize::user_field;
use crate::domain::services::context::ContextPlan;
use crate::domain::services::finalization::tokens;
use crate::infra::db::repos::message::MessagePosition;
use crate::infra::db::repos::thread_summary::SummaryUpdate;
use crate::infra::db::repos::{MessageRepo, ThreadSummaryRepo};
use crate::infra::db::tx::{TxRetryError, with_tx_retry};
use crate::infra::gateways::model_policy::ModelPolicyGateway;
use crate::infra::llm::CompletionResult;
use crate::infra::llm::{
    LlmClient, LlmError, LlmMessage, LlmRequest, LlmUsage, ProviderResolver, RequestMetadata,
    RequestType, ResolvedProvider,
};
use crate::infra::outbox::payloads::{
    THREAD_SUMMARY_TASK_TYPE, ThreadSummaryPayload, USAGE_PAYLOAD_TYPE,
};
use crate::infra::outbox::{OutboxEnqueuer, QueueKind};

/// Opening of the summary request when the chat has no summary yet (B.5.5).
pub const PLAIN_OPENING: &str = "Summarize the following conversation:";

/// Opening of the summary request when a summary already exists (B.5.5); it is
/// followed by the `<existing_summary>` block and [`NEW_MESSAGES_HEADING`].
pub const MERGE_OPENING: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";

/// Heading of the message entries after an existing summary (B.5.5).
pub const NEW_MESSAGES_HEADING: &str = "New messages to incorporate:";

/// Analysis instruction closing the summary request (B.5.5).
pub const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

/// `requester_type` of system-task usage events.
const REQUESTER_SYSTEM: &str = "system";

/// Prompt-too-long retries after the first call (DESIGN: "up to 2 retries").
const PTL_RETRIES: u32 = 2;

/// Bytes per token of the stored `token_estimate` fallback.
const TOKEN_ESTIMATE_BYTES: usize = 4;

/// Trigger decision for a completed turn (DESIGN "Summary trigger based on token
/// budget"): urgent when context assembly dropped recent messages; proactive
/// when the chat has no summary yet and the assembled context reached
/// `pct` % of the effective budget.
#[must_use]
pub fn should_trigger(plan: &ContextPlan, has_summary: bool, pct: u32) -> bool {
    if plan.messages_truncated {
        return true;
    }
    !has_summary
        && plan.assembled_context_tokens.saturating_mul(100)
            >= plan.effective_budget.saturating_mul(i64::from(pct))
}

/// `content` cut to `limit` characters plus `...` when longer (`0` = no limit).
fn truncate_content(content: &str, limit: usize) -> String {
    if limit == 0 {
        return content.to_owned();
    }
    match content.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}...", &content[..cut]),
        None => content.to_owned(),
    }
}

/// User prompt of the summary request (DESIGN "Request format", B.5.5): the
/// opening (with the `<existing_summary>` block when a summary exists), one
/// `User: ` / `Assistant: ` entry per message (system messages are skipped;
/// content cut to `limit` characters plus `...`), then the analysis
/// instruction; parts separated by a blank line.
#[must_use]
pub fn build_summary_prompt(
    existing: Option<&str>,
    msgs: &[(MessageRole, String)],
    limit: usize,
) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(msgs.len() + 4);
    match existing {
        Some(summary) => {
            parts.push(MERGE_OPENING.to_owned());
            parts.push(format!(
                "<existing_summary>\n{summary}\n</existing_summary>"
            ));
            parts.push(NEW_MESSAGES_HEADING.to_owned());
        }
        None => parts.push(PLAIN_OPENING.to_owned()),
    }
    for (role, content) in msgs {
        let label = match role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::System => continue,
        };
        parts.push(format!("{label}: {}", truncate_content(content, limit)));
    }
    parts.push(ANALYSIS_INSTRUCTION.to_owned());
    parts.join("\n\n")
}

/// `text` without its complete `open ... close` blocks.
fn remove_blocks(text: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        let Some(len) = rest[start..].find(close) else {
            break;
        };
        out.push_str(&rest[..start]);
        rest = &rest[start + len + close.len()..];
    }
    out.push_str(rest);
    out
}

/// Inner text of the first complete `open ... close` block.
fn block<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let len = text[start..].find(close)?;
    Some(&text[start..start + len])
}

/// Trailing whitespace removed per line, runs of blank lines collapsed into one,
/// leading and trailing blank space removed.
fn collapse_blank_lines(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    let mut pending_blank = false;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            pending_blank = !lines.is_empty();
            continue;
        }
        if pending_blank {
            lines.push("");
            pending_blank = false;
        }
        lines.push(line);
    }
    lines.join("\n").trim().to_owned()
}

/// Stored text of a summary response (DESIGN "Response parsing"): the
/// `<analysis>` block is removed and the inner text of `<summary>` kept; without
/// a `<summary>` block the remaining text is kept unless it still contains
/// `<analysis` / `<summary` markup (then empty). Runs of blank lines collapsed.
#[must_use]
pub fn parse_summary(text: &str) -> String {
    let text = remove_blocks(text, "<analysis>", "</analysis>");
    if let Some(inner) = block(&text, "<summary>", "</summary>") {
        return collapse_blank_lines(inner);
    }
    if text.contains("<analysis") || text.contains("<summary") {
        return String::new();
    }
    collapse_blank_lines(&text)
}

/// Stored `token_estimate` (DESIGN §3.7 `thread_summaries`): the call's
/// `output_tokens - reasoning_tokens` when positive, else `ceil(bytes / 4)` of
/// the stored summary.
#[must_use]
pub fn summary_token_estimate(usage: Option<&LlmUsage>, summary: &str) -> i64 {
    let diff = usage.map_or(0, |u| u.output_tokens.saturating_sub(u.reasoning_tokens));
    if diff > 0 {
        diff
    } else {
        i64::try_from(summary.len().div_ceil(TOKEN_ESTIMATE_BYTES)).unwrap_or(i64::MAX)
    }
}

/// Fit the prompt to the summary model's input budget (DESIGN "Before the first
/// call..."): `context_window - max_output_tokens`, capped by `max_input_tokens`
/// when > 0, no fitting when `context_window` is 0. The prompt (system prompt +
/// user prompt) is sized at `bytes_per_token_conservative`; while it is over the
/// budget the oldest `ceil(n/5)` messages are dropped, keeping at least two.
#[must_use]
pub fn fit_messages(
    entry: &ModelCatalogEntry,
    system_prompt: &str,
    existing: Option<&str>,
    mut msgs: Vec<(MessageRole, String)>,
    limit: usize,
) -> Vec<(MessageRole, String)> {
    if entry.context_window == 0 {
        return msgs;
    }
    let mut budget = i64::from(entry.context_window) - i64::from(entry.max_output_tokens);
    if entry.max_input_tokens > 0 {
        budget = budget.min(i64::from(entry.max_input_tokens));
    }
    let bpt =
        usize::try_from(entry.estimation_budgets.bytes_per_token_conservative.max(1)).unwrap_or(1);
    let size = |m: &[(MessageRole, String)]| {
        let bytes = system_prompt.len() + build_summary_prompt(existing, m, limit).len();
        i64::try_from(bytes.div_ceil(bpt)).unwrap_or(i64::MAX)
    };
    let mut start = 0;
    while msgs.len() - start > 2 && size(&msgs[start..]) > budget {
        let n = msgs.len() - start;
        start += n.div_ceil(5).min(n - 2);
    }
    msgs.drain(..start);
    msgs
}

/// A provider error saying the prompt exceeds the model's context length.
#[must_use]
pub fn is_context_length_error(e: &LlmError) -> bool {
    let LlmError::Provider { message } = e else {
        return false;
    };
    let m = message.to_ascii_lowercase();
    // OpenAI / Azure ("maximum context length", code `context_length_exceeded`)
    // and Anthropic ("prompt is too long").
    m.contains("context_length_exceeded")
        || m.contains("context length")
        || m.contains("prompt is too long")
}

/// Messages to drop before a prompt-too-long retry of `n` messages: the oldest
/// `ceil(n/5)` (~20 %), keeping at least two (like the budget fitting); `None`
/// when only two or fewer remain.
#[must_use]
pub fn ptl_drop_count(n: usize) -> Option<usize> {
    (n > 2).then(|| n.div_ceil(5).min(n - 2))
}

/// Infrastructure of [`ThreadSummaryService`].
pub struct ThreadSummaryDeps {
    pub config: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub policy: Arc<dyn ModelPolicyGateway>,
    pub llm: Arc<LlmClient>,
    pub providers: Arc<ProviderResolver>,
    pub outbox: Arc<OutboxEnqueuer>,
}

/// Outcome of one task attempt, mapped by the outbox handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryOutcome {
    /// Committed, or nothing (more) to do for this task.
    Done,
    /// Transient failure; the handler retries (or dead-letters on the last delivery).
    Retry(String),
    /// Permanent failure (dead letter).
    Reject(String),
}

/// Why a commit (or the pre-check) ended without a new summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Skip {
    /// Another commit already advanced the frontier.
    CasConflict,
    /// The summary the task was based on no longer exists.
    BaseMissing,
    /// The target frontier message was deleted.
    FrontierDeleted,
}

impl Skip {
    const fn result(self) -> &'static str {
        match self {
            Self::CasConflict => "cas_conflict",
            Self::BaseMissing => "base_missing",
            Self::FrontierDeleted => "frontier_deleted",
        }
    }
}

/// Rollback reason of the commit transaction.
#[derive(Debug)]
enum CommitError {
    Skip(Skip),
    Domain(DomainError),
}

impl From<DomainError> for CommitError {
    fn from(e: DomainError) -> Self {
        Self::Domain(e)
    }
}

impl From<toolkit_db::DbError> for CommitError {
    fn from(e: toolkit_db::DbError) -> Self {
        Self::Domain(DomainError::from(e))
    }
}

impl TxRetryError for CommitError {
    fn is_contention(&self) -> bool {
        matches!(self, Self::Domain(e) if e.is_contention())
    }
}

/// A task past its pre-check: frontiers, existing summary text and the range.
struct TaskInput {
    base: Option<MessagePosition>,
    target: MessagePosition,
    existing: Option<String>,
    msgs: Vec<(MessageRole, String)>,
}

/// Everything the commit transaction writes (cloned per transaction attempt).
#[derive(Clone)]
struct CommitPlan {
    base: Option<MessagePosition>,
    update: SummaryUpdate,
    usage_event: UsageEvent,
}

/// Log a skipped task; skips are final (`Done`).
fn skipped(p: &ThreadSummaryPayload, skip: Skip) -> SummaryOutcome {
    info!(chat_id = %p.chat_id, system_request_id = %p.system_request_id, result = skip.result(), "thread summary task skipped without commit");
    SummaryOutcome::Done
}

fn position(at: Option<DateTime<Utc>>, id: Option<Uuid>) -> Option<MessagePosition> {
    Some((at?, id?))
}

/// Thread-summary scheduling and execution.
pub struct ThreadSummaryService {
    cfg: Arc<MiniChatConfig>,
    db: Arc<DBProvider<DomainError>>,
    policy: Arc<dyn ModelPolicyGateway>,
    llm: Arc<LlmClient>,
    providers: Arc<ProviderResolver>,
    outbox: Arc<OutboxEnqueuer>,
}

impl ThreadSummaryService {
    #[must_use]
    pub fn new(deps: ThreadSummaryDeps) -> Self {
        let ThreadSummaryDeps {
            config,
            db,
            policy,
            llm,
            providers,
            outbox,
        } = deps;
        Self {
            cfg: config,
            db,
            policy,
            llm,
            providers,
            outbox,
        }
    }

    /// `thread_summary_worker.max_attempts`: deliveries before dead-lettering.
    #[must_use]
    pub fn max_attempts(&self) -> u32 {
        self.cfg.thread_summary_worker.max_attempts
    }

    /// Payload of the task a completed turn `causing_request_id` enqueues: base
    /// = the current frontier (none without a summary), frozen target = the
    /// latest non-deleted message outside the causing turn. `None` when there is
    /// no such message or it is not after the frontier.
    ///
    /// # Errors
    /// Database failures.
    pub async fn build_payload(
        conn: &impl DBRunner,
        tenant_id: Uuid,
        chat_id: Uuid,
        causing_request_id: Uuid,
    ) -> DomainResult<Option<ThreadSummaryPayload>> {
        let Some(target) =
            MessageRepo::latest_live_outside_turn(conn, tenant_id, chat_id, causing_request_id)
                .await?
        else {
            return Ok(None);
        };
        let base = ThreadSummaryRepo::get_for_chat(conn, tenant_id, chat_id)
            .await?
            .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        if base.is_some_and(|b| target <= b) {
            return Ok(None);
        }
        Ok(Some(ThreadSummaryPayload {
            tenant_id,
            chat_id,
            system_request_id: Uuid::new_v4(),
            base_frontier_created_at: base.map(|b| b.0),
            base_frontier_message_id: base.map(|b| b.1),
            frozen_target_created_at: target.0,
            frozen_target_message_id: target.1,
            system_task_type: THREAD_SUMMARY_TASK_TYPE.to_owned(),
        }))
    }

    /// Run one attempt of the task `p`. Unexpected failures (database, provider
    /// resolution) are `Retry`.
    pub async fn run(&self, p: &ThreadSummaryPayload) -> SummaryOutcome {
        match self.try_run(p).await {
            Ok(outcome) => outcome,
            Err(err) => {
                warn!(chat_id = %p.chat_id, system_request_id = %p.system_request_id, %err, result = "retry", "thread summary attempt failed");
                SummaryOutcome::Retry(err.to_string())
            }
        }
    }

    /// Gear-start check (DESIGN B.9.4): `true` when the summary model is an
    /// enabled catalog entry. A missing or disabled model, or an unavailable
    /// catalog, is logged as an error; startup continues (a dynamic policy plugin
    /// can add the model later).
    pub async fn check_summary_model(&self) -> bool {
        let model = self.cfg.thread_summary_worker.effective_model_id();
        match self.summary_model().await {
            Ok(Some(_)) => true,
            Ok(None) => {
                error!(
                    model,
                    "thread summary model is missing from the catalog or disabled; summary tasks will be rejected until it is available"
                );
                false
            }
            Err(err) => {
                error!(model, %err, "thread summary model could not be checked");
                false
            }
        }
    }

    /// The enabled summary model of the current catalog (system identity).
    async fn summary_model(&self) -> DomainResult<Option<ModelCatalogEntry>> {
        let snapshot = self.policy.current_snapshot(DEFAULT_SUBJECT_ID).await?;
        Ok(snapshot
            .find(self.cfg.thread_summary_worker.effective_model_id())
            .filter(|m| m.enabled)
            .cloned())
    }

    /// System prompt: the model's `thread_summary_prompt`, else the configured
    /// `summary_system_prompt`, else the built-in default.
    fn system_prompt<'a>(&'a self, entry: &'a ModelCatalogEntry) -> &'a str {
        [
            entry.thread_summary_prompt.as_str(),
            self.cfg
                .thread_summary_worker
                .summary_system_prompt
                .as_str(),
        ]
        .into_iter()
        .find(|s| !s.trim().is_empty())
        .unwrap_or(DEFAULT_SUMMARY_SYSTEM_PROMPT)
    }

    async fn try_run(&self, p: &ThreadSummaryPayload) -> DomainResult<SummaryOutcome> {
        let model_id = self.cfg.thread_summary_worker.effective_model_id();
        let Some(entry) = self.summary_model().await? else {
            error!(chat_id = %p.chat_id, model = model_id, result = "model_unavailable", "thread summary model is missing from the catalog or disabled");
            return Ok(SummaryOutcome::Reject(format!(
                "model_unavailable: summary model `{model_id}` is missing or disabled"
            )));
        };
        let provider = self.providers.resolve(&entry.provider_id, p.tenant_id)?;
        let task = match self.load_task(p).await? {
            Ok(task) => task,
            Err(outcome) => return Ok(outcome),
        };
        let (summary, usage) = match self.generate(&entry, &provider, p, &task).await {
            Ok(generated) => generated,
            Err(outcome) => return Ok(outcome),
        };
        let estimate = summary_token_estimate(usage.as_ref(), &summary);
        let now = now_utc();
        let plan = CommitPlan {
            base: task.base,
            update: SummaryUpdate {
                tenant_id: p.tenant_id,
                chat_id: p.chat_id,
                summary_text: summary,
                frontier: task.target,
                token_estimate: i32::try_from(estimate).unwrap_or(i32::MAX),
                now,
            },
            usage_event: usage_event(p, &entry.id, usage.as_ref(), now),
        };
        match self.commit(plan).await {
            Ok(()) => {
                info!(chat_id = %p.chat_id, system_request_id = %p.system_request_id, result = "success", "thread summary committed");
                Ok(SummaryOutcome::Done)
            }
            Err(CommitError::Skip(skip)) => Ok(skipped(p, skip)),
            Err(CommitError::Domain(e)) => Err(e),
        }
    }

    /// Pre-check (stored frontier == base, target still live) and the range
    /// `(base, target]`; `Err(outcome)` ends the attempt without a call.
    async fn load_task(
        &self,
        p: &ThreadSummaryPayload,
    ) -> DomainResult<Result<TaskInput, SummaryOutcome>> {
        let base = position(p.base_frontier_created_at, p.base_frontier_message_id);
        let target = (p.frozen_target_created_at, p.frozen_target_message_id);
        let conn = self.db.conn()?;

        let current = ThreadSummaryRepo::get_for_chat(&conn, p.tenant_id, p.chat_id).await?;
        let stored = current
            .as_ref()
            .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        let skip = match (base, stored) {
            (Some(_), None) => Some(Skip::BaseMissing),
            (b, s) if b != s => Some(Skip::CasConflict),
            _ => None,
        };
        if let Some(skip) = skip {
            return Ok(Err(skipped(p, skip)));
        }
        if MessageRepo::find_live(&conn, p.tenant_id, p.chat_id, target.1)
            .await?
            .is_none()
        {
            return Ok(Err(skipped(p, Skip::FrontierDeleted)));
        }

        let msgs: Vec<(MessageRole, String)> =
            MessageRepo::summary_range(&conn, p.tenant_id, p.chat_id, base, target)
                .await?
                .into_iter()
                .filter_map(|m| match MessageRole::parse(&m.role) {
                    Some(role @ (MessageRole::User | MessageRole::Assistant)) => {
                        Some((role, m.content))
                    }
                    _ => None,
                })
                .collect();
        if msgs.is_empty() {
            info!(chat_id = %p.chat_id, system_request_id = %p.system_request_id, "thread summary range is empty; nothing to summarize");
            return Ok(Err(SummaryOutcome::Done));
        }
        Ok(Ok(TaskInput {
            base,
            target,
            existing: current
                .and_then(|s| s.summary_text)
                .filter(|t| !t.trim().is_empty()),
            msgs,
        }))
    }

    /// Fit, call and parse: the summary text and the call's usage, or the
    /// `Retry` of a failed call / empty summary.
    async fn generate(
        &self,
        entry: &ModelCatalogEntry,
        provider: &ResolvedProvider,
        p: &ThreadSummaryPayload,
        task: &TaskInput,
    ) -> Result<(String, Option<LlmUsage>), SummaryOutcome> {
        let system_prompt = self.system_prompt(entry);
        let existing = task.existing.as_deref();
        let limit = self.cfg.thread_summary_worker.message_content_limit;
        let msgs = fit_messages(entry, system_prompt, existing, task.msgs.clone(), limit);
        let completion = self
            .call(entry, provider, p, system_prompt, existing, msgs)
            .await
            .map_err(|e| {
                warn!(chat_id = %p.chat_id, system_request_id = %p.system_request_id, error = %e, result = "provider_error", "thread summary call failed; keeping the previous summary");
                SummaryOutcome::Retry(format!("provider_error: {e}"))
            })?;
        let summary = parse_summary(&completion.text);
        if summary.is_empty() {
            warn!(chat_id = %p.chat_id, system_request_id = %p.system_request_id, result = "empty_summary", "thread summary response has no summary text");
            return Err(SummaryOutcome::Retry("empty_summary".to_owned()));
        }
        Ok((summary, completion.usage))
    }

    /// The non-streaming summary call; a context-length error drops the oldest
    /// ~20 % of the messages ([`ptl_drop_count`], at least two kept) and
    /// retries, at most [`PTL_RETRIES`] times.
    async fn call(
        &self,
        entry: &ModelCatalogEntry,
        provider: &ResolvedProvider,
        p: &ThreadSummaryPayload,
        system_prompt: &str,
        existing: Option<&str>,
        mut msgs: Vec<(MessageRole, String)>,
    ) -> Result<CompletionResult, LlmError> {
        let limit = self.cfg.thread_summary_worker.message_content_limit;
        let mut retries = 0;
        loop {
            let prompt = build_summary_prompt(existing, &msgs, limit);
            let req = summary_request(entry, p, system_prompt, prompt);
            let result = self.llm.complete(provider, &req).await;
            let drop = match &result {
                Err(e) if is_context_length_error(e) && retries < PTL_RETRIES => {
                    ptl_drop_count(msgs.len())
                }
                _ => None,
            };
            let Some(drop) = drop else {
                return result;
            };
            debug!(chat_id = %p.chat_id, dropped = drop, "summary prompt too long; dropping the oldest messages");
            msgs.drain(..drop);
            retries += 1;
        }
    }

    /// Commit transaction (DESIGN "CAS-protected commit").
    async fn commit(&self, plan: CommitPlan) -> Result<(), CommitError> {
        let pg = self.db.db().backend() == DbBackend::Postgres;
        let outbox = Arc::clone(&self.outbox);
        let wake = with_tx_retry(&self.db, "thread summary commit", move |tx| {
            let (plan, outbox) = (plan.clone(), Arc::clone(&outbox));
            Box::pin(async move {
                let u = &plan.update;
                let (tenant_id, chat_id, target) = (u.tenant_id, u.chat_id, u.frontier);
                // PostgreSQL: lock the target first; it orders this commit
                // against a concurrent turn mutation.
                if pg
                    && MessageRepo::lock_live(tx, tenant_id, chat_id, target.1)
                        .await?
                        .is_none()
                {
                    return Err(CommitError::Skip(Skip::FrontierDeleted));
                }
                let written = match plan.base {
                    None => ThreadSummaryRepo::insert_first(tx, u).await?,
                    Some(base) => ThreadSummaryRepo::cas_update(tx, base, u).await?,
                };
                if !written {
                    let exists = ThreadSummaryRepo::get_for_chat(tx, tenant_id, chat_id)
                        .await?
                        .is_some();
                    return Err(CommitError::Skip(if plan.base.is_some() && !exists {
                        Skip::BaseMissing
                    } else {
                        Skip::CasConflict
                    }));
                }
                // SQLite: the write above took the database lock, so this
                // read sees every committed mutation.
                if !pg
                    && MessageRepo::find_live(tx, tenant_id, chat_id, target.1)
                        .await?
                        .is_none()
                {
                    return Err(CommitError::Skip(Skip::FrontierDeleted));
                }
                MessageRepo::mark_compressed(tx, tenant_id, chat_id, plan.base, target).await?;
                let wake = outbox
                    .enqueue_json(
                        tx,
                        QueueKind::Usage,
                        tenant_id,
                        USAGE_PAYLOAD_TYPE,
                        &plan.usage_event,
                    )
                    .await?;
                Ok(wake)
            })
        })
        .await?;
        wake.fire();
        Ok(())
    }
}

/// The non-streaming summary request (spec §11.6, DESIGN §4 "Provider Request
/// Metadata"): system identity (chat tenant + platform default subject),
/// `request_type = summary`, `feature = none`, no tools, `max_output_tokens` =
/// the summary model's catalog value.
fn summary_request(
    entry: &ModelCatalogEntry,
    p: &ThreadSummaryPayload,
    system_prompt: &str,
    prompt: String,
) -> LlmRequest {
    LlmRequest {
        model: entry.provider_model_id.clone(),
        instructions: system_prompt.to_owned(),
        input: vec![LlmMessage::text(MessageRole::User, prompt)],
        max_output_tokens: entry.max_output_tokens,
        tools: Vec::new(),
        max_tool_calls: None,
        api_params: entry.general_config.api_params.clone(),
        user: user_field(&p.tenant_id.to_string(), &DEFAULT_SUBJECT_ID.to_string()),
        metadata: RequestMetadata::new(
            p.tenant_id,
            DEFAULT_SUBJECT_ID,
            p.chat_id,
            RequestType::Summary,
            &[],
        ),
        stream: false,
        tool_rounds: Vec::new(),
    }
}

/// System-task usage event (DESIGN §3.2 "System Task Attribution Rules").
fn usage_event(
    p: &ThreadSummaryPayload,
    model_id: &str,
    usage: Option<&LlmUsage>,
    now: DateTime<Utc>,
) -> UsageEvent {
    UsageEvent {
        tenant_id: p.tenant_id,
        user_id: None,
        chat_id: p.chat_id,
        turn_id: None,
        request_id: p.system_request_id,
        effective_model: model_id.to_owned(),
        selected_model: model_id.to_owned(),
        terminal_state: TurnState::Completed.as_str().to_owned(),
        billing_outcome: BillingOutcome::SystemTask.as_str().to_owned(),
        usage: usage.map(tokens),
        actual_credits_micro: 0,
        settlement_method: SettlementMethod::None.as_str().to_owned(),
        policy_version_applied: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: now.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        requester_type: REQUESTER_SYSTEM.to_owned(),
        dedupe_key: format!(
            "{}/{THREAD_SUMMARY_TASK_TYPE}/{}",
            p.tenant_id.simple(),
            p.system_request_id.simple()
        ),
        system_task_type: Some(THREAD_SUMMARY_TASK_TYPE.to_owned()),
    }
}

#[cfg(test)]
#[path = "thread_summary_tests.rs"]
mod thread_summary_tests;
