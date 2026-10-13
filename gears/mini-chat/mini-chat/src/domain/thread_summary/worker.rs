//! The thread-summary worker: the logic of one delivery of the
//! `outbox.thread_summary_queue_name` queue (DESIGN 3.6 "Thread Summary Update").
//!
//! One delivery, in this order: parse the payload (malformed: reject) -> skip
//! when the chat is gone or the stored frontier is not the task's base (the
//! pre-check) -> resolve the summary model (disabled or missing: reject) ->
//! load the range -> fit the prompt -> call the model (a context-length error is
//! retried with the oldest messages dropped) -> parse the answer -> one commit
//! transaction. Every failure of the call or of the commit is a retry; the
//! delivery that would retry on the `max_attempts`-th attempt rejects.
//!
//! A system task: no `chat_turns` row, no quota, no audit, no message; the only
//! outputs are the `thread_summaries` row, `messages.is_compressed` and one
//! `system_task` usage event, all committed together.

use std::sync::{Arc, OnceLock};

use futures::StreamExt;
use mini_chat_sdk::{
    BillingOutcome, ModelCatalogEntry, RequesterType, SettlementMethod, SystemTaskType,
    TerminalState, UsageEvent, UsageTokens, system_task_dedupe_key,
};
use toolkit_db::outbox::Wake;
use toolkit_db::secure::TxConfig;
use toolkit_db::{DBProvider, DbTx};
use toolkit_security::constants::DEFAULT_SUBJECT_ID;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::prompt::{Entry, FitBudget, build_prompt, fit, oldest_fifth, parse_summary};
use super::{NewSummary, ThreadSummaryTask, bytes_to_tokens, summary_model_id};
use crate::config::MiniChatConfig;
use crate::config::ThreadSummaryWorkerConfig;
use crate::domain::clock::now_utc;
use crate::domain::context::HistoryRole;
use crate::domain::error::DomainError;
use crate::domain::messages::MessageRow;
use crate::domain::ports::{
    ChatRepository, LlmGateway, MessageRepository, ModelPolicyPort, ThreadSummaryRepository,
    UsagePort,
};
use crate::domain::provider::{ChatInputItem, ChatRequest, Completion, ProviderEvent};

/// Context-length retries inside one delivery (3 calls at most).
const CONTEXT_LENGTH_RETRIES: u32 = 2;

/// The result of one delivery, as the outbox needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryOutcome {
    /// Acknowledge the message (committed, or skipped as stale).
    Done,
    /// Deliver it again later (outbox backoff).
    Retry,
    /// Dead-letter it, for this reason.
    Reject(String),
}

/// Why a delivery ended without a commit.
enum Stop {
    /// Nothing to do any more; the message is acknowledged.
    Skip(&'static str),
    /// A permanent problem: dead-letter at once.
    Reject(String),
    /// A transient failure: retry, bounded by `max_attempts`.
    Retry(String),
    /// A database, policy or provider-resolution failure: transient as well.
    Error(DomainError),
}

impl From<DomainError> for Stop {
    fn from(error: DomainError) -> Self {
        Self::Error(error)
    }
}

/// A failed provider call.
enum CallError {
    /// The prompt does not fit the model's context window.
    ContextLength,
    /// Any other failure.
    Failed(String),
}

/// What the commit transaction decided.
enum Commit {
    /// The summary is stored; fire the wake once the transaction is durable.
    Stored(Wake),
    /// The compare-and-set or a precondition no longer holds.
    Skipped(&'static str),
}

/// What the commit transaction needs (cloned per attempt).
#[derive(Clone)]
struct CommitInput {
    scope: AccessScope,
    task: ThreadSummaryTask,
    text: String,
    token_estimate: i32,
    usage: UsageEvent,
}

/// The collected answer of the summary model.
struct Answer {
    text: String,
    completion: Completion,
}

/// Runs the deliveries of the thread-summary queue.
pub struct ThreadSummaryService {
    db: Arc<DBProvider<DomainError>>,
    policy: Arc<dyn ModelPolicyPort>,
    llm: Arc<dyn LlmGateway>,
    config: Arc<MiniChatConfig>,
    /// The usage outbox only exists once the pipeline runs: bound afterwards.
    usage: OnceLock<Arc<dyn UsagePort>>,
}

impl ThreadSummaryService {
    /// Creates the service; it retries every delivery until
    /// [`Self::bind_usage`] was called.
    #[must_use]
    pub fn new<P: ModelPolicyPort + 'static, L: LlmGateway + 'static>(
        db: Arc<DBProvider<DomainError>>,
        policy: Arc<P>,
        llm: Arc<L>,
        config: Arc<MiniChatConfig>,
    ) -> Self {
        Self {
            db,
            policy,
            llm,
            config,
            usage: OnceLock::new(),
        }
    }

    /// Binds the usage port the commit transaction enqueues the system usage
    /// event through. The first binding wins.
    pub fn bind_usage<U: UsagePort + 'static>(&self, usage: Arc<U>) {
        if self.usage.set(usage).is_err() {
            tracing::warn!("thread summary: the usage port is already bound");
        }
    }

    /// Handles one message; `attempts` is the delivery count so far (`0` on the
    /// first delivery).
    pub async fn handle(&self, payload: &[u8], attempts: u32) -> SummaryOutcome {
        let task = match ThreadSummaryTask::from_payload(payload) {
            Ok(task) => task,
            Err(reason) => {
                tracing::error!(%reason, "thread summary: corrupt payload rejected");
                return SummaryOutcome::Reject(format!(
                    "payload is not a thread summary work item: {reason}"
                ));
            }
        };
        let Some(usage) = self.usage.get().map(Arc::clone) else {
            tracing::warn!(chat_id = %task.chat_id, "thread summary: the usage port is not bound yet");
            return SummaryOutcome::Retry;
        };
        let result = self.run(&task, usage).await;
        self.conclude(&task, attempts, result)
    }

    /// Maps how a delivery ended to the outcome (and logs it).
    fn conclude(
        &self,
        task: &ThreadSummaryTask,
        attempts: u32,
        result: Result<(), Stop>,
    ) -> SummaryOutcome {
        let Err(stop) = result else {
            tracing::info!(chat_id = %task.chat_id, result = "success", "thread summary committed");
            return SummaryOutcome::Done;
        };
        match stop {
            Stop::Skip(result) => {
                log_skip(task, result);
                SummaryOutcome::Done
            }
            Stop::Reject(reason) => {
                log_reject(task, &reason);
                SummaryOutcome::Reject(reason)
            }
            Stop::Retry(reason) => self.retry_or_reject(task, &reason, attempts),
            Stop::Error(error) => self.retry_or_reject(task, &error.to_string(), attempts),
        }
    }

    /// A delivery that would retry on the `max_attempts`-th attempt rejects, so
    /// a persistent failure does not block the partition.
    fn retry_or_reject(
        &self,
        task: &ThreadSummaryTask,
        reason: &str,
        attempts: u32,
    ) -> SummaryOutcome {
        let worker = &self.config.thread_summary_worker;
        if attempts.saturating_add(1) >= worker.max_attempts {
            tracing::error!(chat_id = %task.chat_id, attempts, %reason,
                "thread summary: attempts exhausted, work item dead-lettered");
            SummaryOutcome::Reject(format!(
                "thread summary failed after {} attempts: {reason}",
                worker.max_attempts
            ))
        } else {
            tracing::warn!(chat_id = %task.chat_id, attempts, %reason,
                "thread summary failed; retrying");
            SummaryOutcome::Retry
        }
    }

    async fn run(&self, task: &ThreadSummaryTask, usage: Arc<dyn UsagePort>) -> Result<(), Stop> {
        // The tenant scope of the payload: a chat of another tenant is not found.
        let scope = AccessScope::for_tenant(task.tenant_id);
        let existing = self.precheck(&scope, task).await?;
        let (entry, policy_version) = self.summary_model().await?;
        self.llm
            .check_provider(task.tenant_id, &entry.provider_id)
            .map_err(Stop::from)?;

        let conn = self.db.conn()?;
        let range = conn
            .summary_range(&scope, task.chat_id, task.base, task.target)
            .await?;
        if range.is_empty() {
            return Err(Stop::Skip("empty_range"));
        }
        let answer = self
            .summarize(task, &entry, existing.as_deref(), range)
            .await?;

        let stored = parse_summary(&answer.text);
        if stored.is_empty() {
            return Err(Stop::Retry(
                "the summary model returned no summary".to_owned(),
            ));
        }
        let input = CommitInput {
            scope,
            task: task.clone(),
            text: stored.clone(),
            token_estimate: token_estimate(&answer.completion, &stored),
            usage: usage_event(task, &entry, policy_version, &answer.completion),
        };
        match self.commit(&input, usage).await? {
            Commit::Stored(wake) => {
                // The rows are durable now: wake the outbox sequencer.
                wake.fire();
                Ok(())
            }
            Commit::Skipped(result) => Err(Stop::Skip(result)),
        }
    }

    /// Skips what is stale before spending a provider call: the chat is gone,
    /// the stored frontier is not the base of the task (a newer summary was
    /// committed, or a mutation dropped the one the task builds on), the target
    /// message was deleted. Returns the text of the existing summary (the base).
    async fn precheck(
        &self,
        scope: &AccessScope,
        task: &ThreadSummaryTask,
    ) -> Result<Option<String>, Stop> {
        let conn = self.db.conn()?;
        if conn.chat_is_deleted(scope, task.chat_id).await? != Some(false) {
            return Err(Stop::Skip("chat_unavailable"));
        }
        let stored = conn.find_summary(scope, task.chat_id).await?;
        if stored.as_ref().map(|row| row.frontier) != task.base {
            return Err(Stop::Skip(if task.base.is_some() && stored.is_none() {
                "base_missing"
            } else {
                "cas_conflict"
            }));
        }
        if conn
            .find_live_message(scope, task.chat_id, task.target.message_id)
            .await?
            .is_none()
        {
            return Err(Stop::Skip("frontier_deleted"));
        }
        Ok(stored.map(|row| row.text.unwrap_or_default()))
    }

    /// The catalog entry of the summary model, resolved with the enabled filter
    /// (a disabled or missing one is rejected), and the policy version of the
    /// snapshot it came from.
    async fn summary_model(&self) -> Result<(ModelCatalogEntry, u64), Stop> {
        let model_id = summary_model_id(&self.config.thread_summary_worker);
        let snapshot = self.policy.current_snapshot(DEFAULT_SUBJECT_ID).await?;
        let entry = snapshot
            .model_catalog
            .into_iter()
            .find(|entry| entry.id == model_id && entry.enabled)
            .ok_or_else(|| {
                Stop::Reject(format!(
                    "summary model `{model_id}` is disabled or not in the catalog"
                ))
            })?;
        Ok((entry, snapshot.policy_version))
    }

    /// Fits the prompt, calls the model and retries a context-length error with
    /// the oldest ~20 % of the messages dropped.
    async fn summarize(
        &self,
        task: &ThreadSummaryTask,
        entry: &ModelCatalogEntry,
        existing: Option<&str>,
        range: Vec<MessageRow>,
    ) -> Result<Answer, Stop> {
        let worker = &self.config.thread_summary_worker;
        let system = system_prompt(entry, worker);
        let ctx = system_context(task.tenant_id)?;
        let mut entries: Vec<Entry> = range
            .into_iter()
            .filter_map(|message| {
                let role = match message.role.as_str() {
                    "user" => HistoryRole::User,
                    "assistant" => HistoryRole::Assistant,
                    _ => return None,
                };
                Some(Entry {
                    role,
                    content: message.content,
                })
            })
            .collect();
        if let Some(budget) = fit_budget(entry) {
            entries = fit(
                entries,
                existing,
                &system,
                worker.message_content_limit,
                budget,
            );
        }
        let mut retries = 0;
        loop {
            let prompt = build_prompt(existing, &entries, worker.message_content_limit);
            let request = summary_request(task, entry, &system, prompt);
            match self.call(&ctx, &request).await {
                Ok(answer) => return Ok(answer),
                Err(CallError::ContextLength)
                    if retries < CONTEXT_LENGTH_RETRIES && entries.len() > 2 =>
                {
                    retries += 1;
                    entries.drain(..oldest_fifth(entries.len()));
                }
                Err(CallError::ContextLength) => {
                    return Err(Stop::Retry(
                        "the prompt exceeds the context window of the summary model".to_owned(),
                    ));
                }
                Err(CallError::Failed(reason)) => return Err(Stop::Retry(reason)),
            }
        }
    }

    /// One provider call; the answer is the concatenation of the text deltas.
    async fn call(
        &self,
        ctx: &SecurityContext,
        request: &ChatRequest,
    ) -> Result<Answer, CallError> {
        let mut stream = self
            .llm
            .stream_chat(ctx, request)
            .await
            .map_err(|failure| {
                if failure.context_length_exceeded {
                    CallError::ContextLength
                } else {
                    CallError::Failed(format!("provider call failed: {}", failure.message))
                }
            })?;
        let mut text = String::new();
        loop {
            match stream.next().await {
                None => {
                    return Err(CallError::Failed(
                        "the provider stream ended without a terminal event".to_owned(),
                    ));
                }
                Some(Err(failure)) => {
                    return Err(CallError::Failed(format!(
                        "provider stream failed: {}",
                        failure.message
                    )));
                }
                Some(Ok(ProviderEvent::TextDelta(delta))) => text.push_str(&delta),
                Some(Ok(ProviderEvent::Completed(completion))) => {
                    return Ok(Answer { text, completion });
                }
                Some(Ok(ProviderEvent::Failed(failure))) => {
                    return Err(CallError::Failed(format!(
                        "the provider reported a failure: {}",
                        failure.message
                    )));
                }
                Some(Ok(_)) => {}
            }
        }
    }

    /// The commit transaction (DESIGN "CAS-protected commit"): the first
    /// statement is a write (the chat lock; it also refuses a deleted chat),
    /// then the target message is locked and checked (a mutation that deleted
    /// it wins), the stored frontier is compared with the base, the
    /// range is re-read, and the summary row, the compression flags and the
    /// usage message are written together.
    async fn commit(&self, input: &CommitInput, usage: Arc<dyn UsagePort>) -> Result<Commit, Stop> {
        let committed = self
            .db
            .db()
            .transaction_with_retry(TxConfig::default(), DomainError::db_cause, |tx| {
                let (input, usage) = (input.clone(), Arc::clone(&usage));
                Box::pin(async move { commit_tx(tx, &input, usage.as_ref()).await })
            })
            .await;
        committed.map_err(|error| Stop::Retry(format!("the commit failed: {error}")))
    }
}

async fn commit_tx(
    tx: &DbTx<'_>,
    input: &CommitInput,
    usage: &dyn UsagePort,
) -> Result<Commit, DomainError> {
    let (scope, task) = (&input.scope, &input.task);
    // A no-op write that takes the chat's write lock first; false when the chat
    // was deleted meanwhile.
    if !tx.lock_live_chat(scope, task.chat_id).await? {
        return Ok(Commit::Skipped("chat_unavailable"));
    }
    // A retry, edit or delete removed the frozen target: no summary, no flags.
    // The check locks the row (after the chat lock), so a mutation that deletes
    // it is ordered against this commit.
    if !tx
        .lock_live_message(scope, task.chat_id, task.target.message_id)
        .await?
    {
        return Ok(Commit::Skipped("frontier_deleted"));
    }
    // Compare-and-set on the base frontier.
    let stored = tx.find_summary_frontier(scope, task.chat_id).await?;
    if stored != task.base {
        return Ok(Commit::Skipped("cas_conflict"));
    }
    // Exactly the messages of the range, as they are now.
    let range = tx
        .summary_range(scope, task.chat_id, task.base, task.target)
        .await?;
    if range.is_empty() {
        return Ok(Commit::Skipped("empty_range"));
    }
    let ids: Vec<Uuid> = range.iter().map(|message| message.id).collect();
    tx.replace_summary(
        scope,
        NewSummary {
            id: Uuid::new_v4(),
            tenant_id: task.tenant_id,
            chat_id: task.chat_id,
            text: input.text.clone(),
            frontier: task.target,
            token_estimate: input.token_estimate,
            now: now_utc(),
        },
    )
    .await?;
    tx.mark_compressed(scope, task.chat_id, &ids).await?;
    let wake = usage.enqueue_usage(tx, &input.usage).await?;
    Ok(Commit::Stored(wake))
}

fn log_skip(task: &ThreadSummaryTask, result: &str) {
    tracing::info!(chat_id = %task.chat_id, result, "thread summary skipped");
}

fn log_reject(task: &ThreadSummaryTask, reason: &str) {
    tracing::error!(chat_id = %task.chat_id, %reason, "thread summary rejected");
}

/// The system prompt: the catalog entry's, else the configured one, else the
/// built-in text (the default of the configuration key).
fn system_prompt(entry: &ModelCatalogEntry, worker: &ThreadSummaryWorkerConfig) -> String {
    [
        entry.thread_summary_prompt.as_str(),
        worker.summary_system_prompt.as_str(),
    ]
    .into_iter()
    .find(|prompt| !prompt.is_empty())
    .map_or_else(
        || ThreadSummaryWorkerConfig::default().summary_system_prompt,
        str::to_owned,
    )
}

/// Input budget of the summary model: `context_window - max_output_tokens`,
/// capped by `max_input_tokens` when it is positive. `None` (no fitting) when
/// the catalog does not know the window.
fn fit_budget(entry: &ModelCatalogEntry) -> Option<FitBudget> {
    if entry.context_window == 0 {
        return None;
    }
    let mut tokens = u64::from(entry.context_window.saturating_sub(entry.max_output_tokens));
    if entry.max_input_tokens > 0 {
        tokens = tokens.min(u64::from(entry.max_input_tokens));
    }
    Some(FitBudget {
        tokens,
        bytes_per_token: u64::from(entry.estimation_budgets.bytes_per_token_conservative),
    })
}

/// The identity of the summary call: the platform system subject in the tenant
/// of the chat.
fn system_context(tenant_id: Uuid) -> Result<SecurityContext, Stop> {
    SecurityContext::builder()
        .subject_id(DEFAULT_SUBJECT_ID)
        .subject_type("service")
        .subject_tenant_id(tenant_id)
        .build()
        .map_err(|error| Stop::Retry(format!("summary security context: {error}")))
}

/// The summary request: one user item, no tools, the catalog output budget of
/// the summary model (not capped by `streaming.max_output_tokens`).
fn summary_request(
    task: &ThreadSummaryTask,
    entry: &ModelCatalogEntry,
    system: &str,
    prompt: String,
) -> ChatRequest {
    ChatRequest {
        tenant_id: task.tenant_id,
        user_id: DEFAULT_SUBJECT_ID,
        chat_id: task.chat_id,
        provider_id: entry.provider_id.clone(),
        provider_model_id: entry.provider_model_id.clone(),
        instructions: system.to_owned(),
        input: vec![ChatInputItem {
            role: HistoryRole::User,
            content: prompt,
            images: Vec::new(),
        }],
        tools: Vec::new(),
        max_output_tokens: entry.max_output_tokens,
        max_tool_calls: entry.max_tool_calls,
        api_params: entry.general_config.api_params.clone(),
        is_summary: true,
    }
}

/// `output_tokens - reasoning_tokens` of the call when positive, else
/// `ceil(bytes / 4)` of the stored text.
fn token_estimate(completion: &Completion, stored: &str) -> i32 {
    let visible = completion
        .output_tokens
        .saturating_sub(completion.breakdown.reasoning);
    let tokens = if visible > 0 {
        visible
    } else {
        bytes_to_tokens(stored)
    };
    i32::try_from(tokens).unwrap_or(i32::MAX)
}

/// The usage event of the system task (DESIGN 3.2 "System Task Attribution
/// Rules"): no user, no turn, no credits, `system_task`.
fn usage_event(
    task: &ThreadSummaryTask,
    entry: &ModelCatalogEntry,
    policy_version: u64,
    completion: &Completion,
) -> UsageEvent {
    UsageEvent {
        tenant_id: task.tenant_id,
        user_id: None,
        chat_id: task.chat_id,
        turn_id: None,
        request_id: task.system_request_id,
        effective_model: entry.id.clone(),
        selected_model: entry.id.clone(),
        terminal_state: TerminalState::Completed,
        billing_outcome: BillingOutcome::SystemTask,
        usage: Some(UsageTokens {
            input_tokens: completion.input_tokens,
            output_tokens: completion.output_tokens,
            cache_read_input_tokens: completion.breakdown.cache_read,
            cache_write_input_tokens: completion.breakdown.cache_write,
            reasoning_tokens: completion.breakdown.reasoning,
        }),
        actual_credits_micro: 0,
        settlement_method: SettlementMethod::None,
        policy_version_applied: policy_version,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: now_utc(),
        requester_type: RequesterType::System,
        dedupe_key: system_task_dedupe_key(
            task.tenant_id,
            SystemTaskType::ThreadSummaryUpdate,
            task.system_request_id,
        ),
        system_task_type: Some(SystemTaskType::ThreadSummaryUpdate),
    }
}
