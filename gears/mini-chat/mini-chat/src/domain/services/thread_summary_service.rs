//! Thread summary worker (DESIGN section 3.6 "Thread Summary Update", "Stable
//! Range and Commit Invariant"; section 3.2 system task attribution; B.5.5
//! prompt texts).
//!
//! One run handles one durable `mini-chat.thread_summary` message:
//!
//! 1. Pre-check: the stored frontier must still equal the payload's base
//!    frontier (otherwise `cas conflict` or `base_missing`), and the frozen
//!    target message must still be live (`frontier_deleted`). Each skip ends
//!    the run (`Done`) without a provider call.
//! 2. Load exactly the live, non-compressed messages in `(base, target]`
//!    ordered by `(created_at, id)`.
//! 3. Resolve the summary model with the enabled filter (missing / disabled:
//!    `Reject`), build the prompt, fit it to the model's input budget and call
//!    the provider (non-streaming), dropping the oldest messages on a
//!    context-length error (up to 2 retries).
//! 4. Parse the reply (`<summary>` content, blank-line runs collapsed); an
//!    empty result is `Retry`.
//! 5. Commit (writes first, Ruling R5): guarded no-op `UPDATE` of the target
//!    message (row lock + not-deleted check), CAS upsert of
//!    `thread_summaries`, `is_compressed = true` on exactly the range, system
//!    usage event; one transaction, wake fired after commit.
//!
//! A `Retry` on the `max_attempts`-th delivery becomes `Reject`.
//!
//! The `mini_chat_thread_summary_execution_total{result}` values are recorded
//! as the `result` field of the run's log event.

use std::sync::Arc;

use mini_chat_sdk::{ModelCatalogEntry, UsageEvent, UsageTokens};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::outbox::Wake;
use toolkit_security::constants::DEFAULT_SUBJECT_ID;
use uuid::Uuid;

use crate::config::{DEFAULT_SUMMARY_SYSTEM_PROMPT, MiniChatConfig};
use crate::domain::enums::{MessageRole, RequesterType};
use crate::domain::error::DomainError;
use crate::domain::ports::{
    CompletionResult, ContentPart, InputItem, LlmClient, LlmRequest, ProviderError,
    RequestMetadata, ResolvedProvider, provider_user,
};
use crate::domain::services::model_service::ModelService;
use crate::domain::time::db_now;
use crate::infra::db::entities::{message, thread_summary};
use crate::infra::db::repos::message_repo::{self, SummaryWrite};
use crate::infra::llm::ProviderResolver;
use crate::infra::outbox::MiniChatOutbox;
use crate::infra::outbox::payloads::ThreadSummaryPayload;

/// `system_task_type` of the thread summary.
pub const THREAD_SUMMARY_TASK: &str = "thread_summary_update";
const BILLING_SYSTEM_TASK: &str = "system_task";
const SETTLEMENT_NONE: &str = "none";
/// Retries after a context-length-exceeded provider error.
const PTL_RETRIES: usize = 2;
/// Fitting and PTL retries never go below this many messages.
const MIN_MESSAGES: usize = 2;

/// Opening of the summary request without an existing summary (B.5.5).
pub const SUMMARY_OPENING: &str = "Summarize the following conversation:";
/// Opening of the summary request with an existing summary (B.5.5).
pub const SUMMARY_MERGE_OPENING: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";
const NEW_MESSAGES_HEADER: &str = "New messages to incorporate:";
/// Analysis instruction closing the summary request (B.5.5).
pub const SUMMARY_ANALYSIS_INSTRUCTION: &str =
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

/// Outcome of one run, mapped 1:1 to the outbox `MessageResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryRunResult {
    /// Committed, or skipped for good (CAS conflict, `frontier_deleted`,
    /// `base_missing`, nothing to summarize).
    Done,
    Retry(String),
    Reject(String),
}

pub struct ThreadSummaryDeps {
    pub db: Arc<DBProvider<DomainError>>,
    pub cfg: Arc<MiniChatConfig>,
    pub models: Arc<ModelService>,
    pub llm: Arc<dyn LlmClient>,
    pub resolver: Arc<ProviderResolver>,
    pub outbox: Arc<MiniChatOutbox>,
}

pub struct ThreadSummaryService {
    deps: ThreadSummaryDeps,
}

type Frontier = (OffsetDateTime, Uuid);

/// Result of the commit transaction.
enum Commit {
    Done(Wake),
    FrontierDeleted,
    CasConflict,
    BaseMissing,
}

/// The frozen range to summarize.
struct Work {
    base: Option<Frontier>,
    target: Frontier,
    /// Text of the stored summary the range extends.
    existing: Option<String>,
    entries: Vec<(MessageRole, String)>,
}

/// The summary model and everything the request needs from it.
struct Target {
    entry: ModelCatalogEntry,
    policy_version: u64,
    provider: ResolvedProvider,
}

impl ThreadSummaryService {
    #[must_use]
    pub fn new(deps: ThreadSummaryDeps) -> Self {
        Self { deps }
    }

    /// Runs one delivery. `attempts` is the outbox message's previous
    /// delivery count: a `Retry` on the `max_attempts`-th delivery
    /// (`attempts + 1 >= max_attempts`) becomes `Reject`.
    pub async fn run(&self, p: &ThreadSummaryPayload, attempts: i16) -> SummaryRunResult {
        let res = match self.execute(p).await {
            Ok(res) => res,
            Err(e) => {
                log_result(p, "retry", &e.to_string());
                SummaryRunResult::Retry(format!("thread summary failed: {e}"))
            }
        };
        let max = self.deps.cfg.thread_summary_worker.max_attempts;
        match res {
            SummaryRunResult::Retry(reason) if i64::from(attempts) + 1 >= i64::from(max) => {
                tracing::error!(
                    chat_id = %p.chat_id,
                    system_request_id = %p.system_request_id,
                    attempts,
                    %reason,
                    "thread summary: max attempts reached; dead-lettering"
                );
                SummaryRunResult::Reject(format!(
                    "thread summary: max attempts ({max}) reached: {reason}"
                ))
            }
            other => other,
        }
    }

    async fn execute(&self, p: &ThreadSummaryPayload) -> Result<SummaryRunResult, DomainError> {
        let Some(work) = self.load(p).await? else {
            return Ok(SummaryRunResult::Done);
        };
        let t = match self.resolve_target(p).await {
            Ok(t) => t,
            Err(res) => return Ok(res),
        };
        let completion = match self
            .summarize(p, &t, work.existing.as_deref(), work.entries)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                log_result(p, "provider_error", &e.to_string());
                tracing::warn!(chat_id = %p.chat_id, "mini_chat_summary_fallback: previous summary kept");
                return Ok(SummaryRunResult::Retry(format!("summary call failed: {e}")));
            }
        };
        let summary = parse_summary(&completion.text);
        if summary.is_empty() {
            log_result(p, "empty_summary", "the provider returned no summary");
            return Ok(SummaryRunResult::Retry("empty summary".to_owned()));
        }
        let estimate = token_estimate(completion.usage.as_ref(), &summary);
        let commit = self
            .commit(
                p,
                work.base,
                work.target,
                &t,
                summary,
                estimate,
                completion.usage,
            )
            .await?;
        Ok(finish(p, commit))
    }

    /// Pre-check and range load; `None` when the run ends without a
    /// provider call (frontier moved, base missing, target deleted, nothing
    /// to summarize).
    async fn load(&self, p: &ThreadSummaryPayload) -> Result<Option<Work>, DomainError> {
        let base = base_frontier(p);
        let target = (p.frozen_target_created_at, p.frozen_target_message_id);
        let conn = self.deps.db.conn()?;
        let existing = message_repo::thread_summary(&conn, p.tenant_id, p.chat_id).await?;
        if let Some(skip) = precheck(base, existing.as_ref()) {
            log_result(p, skip, "skipped before the provider call");
            return Ok(None);
        }
        if message_repo::find_live(&conn, p.tenant_id, p.chat_id, target.1)
            .await?
            .is_none()
        {
            log_result(p, "frontier_deleted", "target frontier message deleted");
            return Ok(None);
        }
        let range =
            message_repo::summary_range(&conn, p.tenant_id, p.chat_id, base, target).await?;
        let entries = prompt_entries(range);
        if entries.is_empty() {
            tracing::info!(chat_id = %p.chat_id, "thread summary: no message to summarize");
            return Ok(None);
        }
        Ok(Some(Work {
            base,
            target,
            existing: existing.map(|s| s.summary_text),
            entries,
        }))
    }

    /// Summary model (enabled filter, system subject) and its provider.
    async fn resolve_target(&self, p: &ThreadSummaryPayload) -> Result<Target, SummaryRunResult> {
        let model_id = self
            .deps
            .cfg
            .thread_summary_worker
            .effective_summary_model_id();
        let (snapshot, entry) = match self
            .deps
            .models
            .resolve_for_chat(DEFAULT_SUBJECT_ID, model_id, true)
            .await
        {
            Ok(v) => v,
            Err(DomainError::InvalidModel) => {
                tracing::error!(
                    chat_id = %p.chat_id,
                    model = model_id,
                    result = "model_unavailable",
                    "thread summary: summary model missing or disabled"
                );
                return Err(SummaryRunResult::Reject(format!(
                    "summary model '{model_id}' is missing or disabled"
                )));
            }
            Err(e) => {
                log_result(p, "retry", &e.to_string());
                return Err(SummaryRunResult::Retry(format!(
                    "summary model resolution: {e}"
                )));
            }
        };
        let provider = self
            .deps
            .resolver
            .resolve(&entry.provider_id, p.tenant_id)
            .map_err(|e| {
                log_result(p, "retry", &e.to_string());
                SummaryRunResult::Retry(format!("summary provider resolution: {e}"))
            })?;
        Ok(Target {
            entry,
            policy_version: snapshot.policy_version,
            provider,
        })
    }

    /// Fits the prompt, then calls the provider; on a context-length error
    /// drops the oldest `ceil(n/5)` messages and retries (up to 2 times).
    async fn summarize(
        &self,
        p: &ThreadSummaryPayload,
        t: &Target,
        existing: Option<&str>,
        mut entries: Vec<(MessageRole, String)>,
    ) -> Result<CompletionResult, ProviderError> {
        let cfg = &self.deps.cfg.thread_summary_worker;
        let system_prompt = system_prompt(&t.entry, &cfg.summary_system_prompt);
        let limit = cfg.message_content_limit;
        let mut prompt = fit_prompt(&t.entry, system_prompt, existing, &mut entries, limit);
        let mut retries = 0;
        loop {
            let req = summary_request(p, &t.entry, system_prompt, prompt);
            match self.deps.llm.complete(&t.provider, req).await {
                Err(e)
                    if e.context_length_exceeded
                        && retries < PTL_RETRIES
                        && entries.len() > MIN_MESSAGES =>
                {
                    retries += 1;
                    drop_oldest(&mut entries);
                    tracing::info!(
                        chat_id = %p.chat_id,
                        retries,
                        kept = entries.len(),
                        "thread summary: prompt too long; retrying with fewer messages"
                    );
                    prompt = build_user_prompt(existing, &entries, limit);
                }
                other => return other,
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit(
        &self,
        p: &ThreadSummaryPayload,
        base: Option<Frontier>,
        target: Frontier,
        t: &Target,
        summary: String,
        estimate: i32,
        usage: Option<UsageTokens>,
    ) -> Result<Commit, DomainError> {
        let now = db_now();
        let write = SummaryWrite {
            tenant_id: p.tenant_id,
            chat_id: p.chat_id,
            summary_text: summary,
            frontier: target,
            token_estimate: estimate,
            now,
        };
        let ev = system_usage_event(p, &t.entry.id, t.policy_version, usage, now);
        let outbox = Arc::clone(&self.deps.outbox);
        self.deps
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let (tenant, chat) = (write.tenant_id, write.chat_id);
                    // First statement is a write (Ruling R5): row lock on the
                    // target and the not-deleted check.
                    if message_repo::touch_live(tx, tenant, chat, target.1).await? == 0 {
                        return Ok(Commit::FrontierDeleted);
                    }
                    let won = match base {
                        None => message_repo::insert_summary_if_absent(tx, &write).await?,
                        Some(b) => message_repo::cas_update_summary(tx, &write, b).await? == 1,
                    };
                    if !won {
                        let missing = base.is_some()
                            && message_repo::thread_summary(tx, tenant, chat)
                                .await?
                                .is_none();
                        return Ok(if missing {
                            Commit::BaseMissing
                        } else {
                            Commit::CasConflict
                        });
                    }
                    message_repo::mark_range_compressed(tx, tenant, chat, base, target).await?;
                    let wake = outbox.enqueue_usage(tx, &ev).await?;
                    Ok(Commit::Done(wake))
                })
            })
            .await
    }
}

/// `(role, content)` of the non-system messages, in order.
fn prompt_entries(range: Vec<message::Model>) -> Vec<(MessageRole, String)> {
    range
        .into_iter()
        .filter_map(|m| {
            MessageRole::parse(&m.role)
                .filter(|r| *r != MessageRole::System)
                .map(|r| (r, m.content))
        })
        .collect()
}

fn base_frontier(p: &ThreadSummaryPayload) -> Option<Frontier> {
    p.base_frontier_created_at.zip(p.base_frontier_message_id)
}

/// `Some(result)` when the stored summary no longer matches the payload's
/// base frontier.
fn precheck(
    base: Option<Frontier>,
    existing: Option<&thread_summary::Model>,
) -> Option<&'static str> {
    let stored = existing.map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
    match (base, stored) {
        (None, None) => None,
        (Some(b), Some(s)) if b == s => None,
        (Some(_), None) => Some("base_missing"),
        _ => {
            tracing::info!("mini_chat_thread_summary_cas_conflicts: frontier already advanced");
            Some("cas_conflict")
        }
    }
}

fn finish(p: &ThreadSummaryPayload, commit: Commit) -> SummaryRunResult {
    match commit {
        Commit::Done(wake) => {
            wake.fire();
            log_result(p, "success", "summary committed");
        }
        Commit::FrontierDeleted => log_result(p, "frontier_deleted", "target deleted at commit"),
        Commit::BaseMissing => log_result(p, "base_missing", "base summary deleted at commit"),
        Commit::CasConflict => {
            tracing::info!("mini_chat_thread_summary_cas_conflicts: CAS lost at commit");
            log_result(p, "cas_conflict", "frontier advanced by another commit");
        }
    }
    SummaryRunResult::Done
}

fn log_result(p: &ThreadSummaryPayload, result: &'static str, detail: &str) {
    tracing::info!(
        chat_id = %p.chat_id,
        system_request_id = %p.system_request_id,
        result,
        detail,
        "mini_chat_thread_summary_execution"
    );
}

/// Catalog `thread_summary_prompt`, else the configured prompt, else the
/// built-in default.
fn system_prompt<'a>(entry: &'a ModelCatalogEntry, configured: &'a str) -> &'a str {
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
fn input_budget(entry: &ModelCatalogEntry) -> Option<i64> {
    if entry.context_window == 0 {
        return None;
    }
    let window = i64::from(entry.context_window) - i64::from(entry.max_output_tokens);
    Some(if entry.max_input_tokens > 0 {
        window.min(i64::from(entry.max_input_tokens))
    } else {
        window
    })
}

/// Drops the oldest `ceil(n/5)` messages, keeping at least [`MIN_MESSAGES`].
fn drop_oldest(entries: &mut Vec<(MessageRole, String)>) {
    let n = entries.len();
    let k = n.div_ceil(5).min(n.saturating_sub(MIN_MESSAGES));
    entries.drain(..k);
}

/// Builds the user prompt, dropping the oldest messages while the estimated
/// size (system + user prompt at `bytes_per_token_conservative`) is over the
/// input budget.
fn fit_prompt(
    entry: &ModelCatalogEntry,
    system_prompt: &str,
    existing: Option<&str>,
    entries: &mut Vec<(MessageRole, String)>,
    limit: usize,
) -> String {
    let mut prompt = build_user_prompt(existing, entries, limit);
    let Some(budget) = input_budget(entry) else {
        return prompt;
    };
    let bpt = u64::from(entry.estimation_budgets.bytes_per_token_conservative.max(1));
    let tokens = |prompt: &str| {
        let bytes = u64::try_from(system_prompt.len() + prompt.len()).unwrap_or(u64::MAX);
        i64::try_from(bytes.div_ceil(bpt)).unwrap_or(i64::MAX)
    };
    while tokens(&prompt) > budget && entries.len() > MIN_MESSAGES {
        drop_oldest(entries);
        prompt = build_user_prompt(existing, entries, limit);
    }
    prompt
}

fn summary_request(
    p: &ThreadSummaryPayload,
    entry: &ModelCatalogEntry,
    system_prompt: &str,
    prompt: String,
) -> LlmRequest {
    let tenant = p.tenant_id.to_string();
    let user = DEFAULT_SUBJECT_ID.to_string();
    LlmRequest {
        model: entry.provider_model_id.clone(),
        instructions: system_prompt.to_owned(),
        input: vec![InputItem::Message {
            role: "user",
            content: vec![ContentPart::InputText(prompt)],
        }],
        tools: Vec::new(),
        max_output_tokens: entry.max_output_tokens,
        api_params: entry.general_config.api_params.clone(),
        max_tool_calls: None,
        user: provider_user(&tenant, &user),
        metadata: RequestMetadata {
            tenant_id: tenant,
            user_id: user,
            chat_id: p.chat_id.to_string(),
            request_type: "summary",
            feature: "none".to_owned(),
        },
        stream: false,
    }
}

/// The system usage event of a committed summary (DESIGN section 3.2).
fn system_usage_event(
    p: &ThreadSummaryPayload,
    model_id: &str,
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
        effective_model: model_id.to_owned(),
        selected_model: model_id.to_owned(),
        terminal_state: "completed".to_owned(),
        billing_outcome: BILLING_SYSTEM_TASK.to_owned(),
        usage,
        actual_credits_micro: 0,
        settlement_method: SETTLEMENT_NONE.to_owned(),
        policy_version_applied: policy_version,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: now,
        requester_type: RequesterType::System.as_str().to_owned(),
        dedupe_key: format!(
            "{}/{THREAD_SUMMARY_TASK}/{}",
            p.tenant_id.as_simple(),
            p.system_request_id.as_simple()
        ),
        system_task_type: Some(THREAD_SUMMARY_TASK.to_owned()),
    }
}

/// The summary request's user prompt (DESIGN section 3.6 "Request format",
/// B.5.5). System messages are skipped; content longer than `limit`
/// characters (0 = no limit) is cut to `limit` characters plus `...`.
#[must_use]
pub(crate) fn build_user_prompt(
    existing: Option<&str>,
    entries: &[(MessageRole, String)],
    limit: usize,
) -> String {
    let mut out = String::new();
    match existing {
        Some(summary) => {
            out.push_str(SUMMARY_MERGE_OPENING);
            out.push_str("\n\n<existing_summary>\n");
            out.push_str(summary);
            out.push_str("\n</existing_summary>\n\n");
            out.push_str(NEW_MESSAGES_HEADER);
        }
        None => out.push_str(SUMMARY_OPENING),
    }
    out.push_str("\n\n");
    for (role, content) in entries {
        let label = match role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::System => continue,
        };
        out.push_str(label);
        out.push_str(": ");
        match content.char_indices().nth(limit) {
            Some((cut, _)) if limit > 0 => {
                out.push_str(&content[..cut]);
                out.push_str("...");
            }
            _ => out.push_str(content),
        }
        out.push_str("\n\n");
    }
    out.push_str(SUMMARY_ANALYSIS_INSTRUCTION);
    out
}

/// Text between the first `open` and the next `close` after it, with the
/// byte range of the whole block.
fn block<'a>(text: &'a str, open: &str, close: &str) -> Option<(usize, usize, &'a str)> {
    let start = text.find(open)?;
    let inner = start + open.len();
    let end = inner + text[inner..].find(close)?;
    Some((start, end + close.len(), &text[inner..end]))
}

/// DESIGN section 3.6 "Response parsing": `<analysis>` blocks are removed;
/// the `<summary>` content is kept (else the remaining text, unless it still
/// has `<analysis` / `<summary` markup); runs of blank lines collapse to
/// one; the result is trimmed.
#[must_use]
pub(crate) fn parse_summary(text: &str) -> String {
    let mut rest = text.to_owned();
    while let Some((start, end, _)) = block(&rest, "<analysis>", "</analysis>") {
        rest.replace_range(start..end, "");
    }
    let body = match block(&rest, "<summary>", "</summary>") {
        Some((_, _, inner)) => inner.to_owned(),
        None if rest.contains("<analysis") || rest.contains("<summary") => String::new(),
        None => rest,
    };
    collapse_blank_lines(&body)
}

fn collapse_blank_lines(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    let mut blank = false;
    for line in text.trim().lines() {
        if line.trim().is_empty() {
            if !blank {
                lines.push("");
            }
            blank = true;
        } else {
            lines.push(line);
            blank = false;
        }
    }
    lines.join("\n")
}

/// Stored `token_estimate`: `output_tokens - reasoning_tokens` when
/// positive, else `ceil(summary bytes / 4)`.
#[must_use]
pub(crate) fn token_estimate(usage: Option<&UsageTokens>, summary: &str) -> i32 {
    let reported = usage.map_or(0, |u| u.output_tokens.saturating_sub(u.reasoning_tokens));
    let est = if reported > 0 {
        reported
    } else {
        i64::try_from(summary.len().div_ceil(4)).unwrap_or(i64::MAX)
    };
    i32::try_from(est).unwrap_or(i32::MAX)
}

#[cfg(test)]
#[path = "thread_summary_service_tests.rs"]
mod thread_summary_service_tests;
