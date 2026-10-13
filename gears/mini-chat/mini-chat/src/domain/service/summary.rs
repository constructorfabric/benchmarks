//! Thread summary: trigger evaluated in the finalization transaction and the outbox handler
//! that generates and CAS-commits the summary (OWNER: streaming core).
//!
//! DESIGN "Thread Summary Update", "Thread Summary - Stable Range and Commit Invariant",
//! B.5.5 and B.9.4.

use std::sync::{Arc, LazyLock};

use mini_chat_sdk::{ModelCatalogEntry, UsageEvent, UsageTokens};
use regex::Regex;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::outbox::{MessageResult, Wake};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use toolkit_security::constants::DEFAULT_SUBJECT_ID;
use uuid::Uuid;

use crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT;
use crate::domain::error::{DomainError, reasons};
use crate::domain::outbox_payloads::{ThreadSummaryTask, simple_uuid};
use crate::domain::service::Deps;
use crate::domain::service::context::{msg_key_gt, msg_key_le};
use crate::domain::service::finalization::retry_locked;
use crate::infra::db::entity::{message, thread_summary};
use crate::infra::llm::{InputMessage, InputRole, LlmRequest, RequestType};

/// `system_task_type` of the thread-summary work item.
pub const SYSTEM_TASK_TYPE: &str = "thread_summary_update";

/// Opening of the summary request without an existing summary (B.5.5).
pub const OPENING_NEW: &str = "Summarize the following conversation:";

/// Opening of the summary request with an existing summary (B.5.5).
pub const OPENING_MERGE: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";

/// Analysis instruction at the end of the summary request (B.5.5).
pub const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

// ── Trigger ────────────────────────────────────────────────────────────────

/// Trigger inputs computed from the context assembly of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryTrigger {
    pub assembled_tokens: i64,
    /// `min(max_input_tokens (0 = none), context_window - max_output_tokens_applied)`.
    pub effective_budget: i64,
    pub threshold_pct: u32,
    pub messages_truncated: bool,
}

impl SummaryTrigger {
    /// Proactive condition (`assembled >= pct% of budget`).
    #[must_use]
    pub fn over_threshold(&self) -> bool {
        i128::from(self.assembled_tokens) * 100
            >= i128::from(self.effective_budget) * i128::from(self.threshold_pct)
    }
}

/// Evaluates the trigger inside the finalization transaction of a completed turn and enqueues
/// the thread-summary task when it fires.
///
/// # Errors
/// Database / outbox failure.
pub async fn maybe_enqueue(
    tx: &DbTx<'_>,
    deps: &Deps,
    tenant_id: Uuid,
    chat_id: Uuid,
    causing_request_id: Uuid,
    trigger: &SummaryTrigger,
) -> Result<Option<Wake>, DomainError> {
    let scope = AccessScope::for_tenant(tenant_id);
    let row = thread_summary::Entity::find()
        .filter(thread_summary::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&scope)
        .one(tx)
        .await?;
    let fire = (row.is_none() && trigger.over_threshold()) || trigger.messages_truncated;
    if !fire {
        return Ok(None);
    }
    let target = message::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::DeletedAt.is_null())
                .add(message::Column::RequestId.ne(causing_request_id)),
        )
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(tx)
        .await?;
    let Some(target) = target else {
        return Ok(None);
    };
    let base = row
        .as_ref()
        .map(|r| (r.summarized_up_to_created_at, r.summarized_up_to_message_id));
    if base == Some((target.created_at, target.id)) {
        return Ok(None);
    }
    let task = ThreadSummaryTask {
        tenant_id,
        chat_id,
        system_request_id: Uuid::new_v4(),
        base_frontier_created_at: base.map(|b| b.0),
        base_frontier_message_id: base.map(|b| b.1),
        frozen_target_created_at: target.created_at,
        frozen_target_message_id: target.id,
        system_task_type: SYSTEM_TASK_TYPE.to_owned(),
    };
    let wake = deps.outbox.enqueue_thread_summary(tx, &task).await?;
    Ok(Some(wake))
}

// ── Prompt ─────────────────────────────────────────────────────────────────

/// Cuts `content` to `limit` characters with a `...` suffix (`0` = no limit).
#[must_use]
pub fn cut_content(content: &str, limit: usize) -> String {
    if limit == 0 || content.chars().count() <= limit {
        return content.to_owned();
    }
    let mut s: String = content.chars().take(limit).collect();
    s.push_str("...");
    s
}

/// Builds the user prompt of the summary request.
#[must_use]
pub fn build_user_prompt(existing: Option<&str>, msgs: &[message::Model], limit: usize) -> String {
    let mut out = String::new();
    match existing {
        Some(s) => {
            out.push_str(OPENING_MERGE);
            out.push_str("\n\n<existing_summary>\n");
            out.push_str(s);
            out.push_str("\n</existing_summary>\n\nNew messages to incorporate:");
        }
        None => out.push_str(OPENING_NEW),
    }
    let entries: Vec<String> = msgs
        .iter()
        .filter_map(|m| {
            let who = match m.role.as_str() {
                "user" => "User",
                "assistant" => "Assistant",
                _ => return None,
            };
            Some(format!("{who}: {}", cut_content(&m.content, limit)))
        })
        .collect();
    if !entries.is_empty() {
        out.push_str("\n\n");
        out.push_str(&entries.join("\n\n"));
    }
    out.push_str("\n\n");
    out.push_str(ANALYSIS_INSTRUCTION);
    out
}

/// System prompt of the summary request.
#[must_use]
pub fn system_prompt(model: &ModelCatalogEntry, cfg_prompt: &str) -> String {
    if !model.thread_summary_prompt.trim().is_empty() {
        model.thread_summary_prompt.clone()
    } else if !cfg_prompt.trim().is_empty() {
        cfg_prompt.to_owned()
    } else {
        DEFAULT_SUMMARY_SYSTEM_PROMPT.to_owned()
    }
}

/// Input budget of the summary model (`None` = no fitting).
#[must_use]
pub fn summary_input_budget(model: &ModelCatalogEntry) -> Option<i64> {
    if model.context_window == 0 {
        return None;
    }
    let mut b = i64::from(model.context_window) - i64::from(model.max_output_tokens);
    if model.max_input_tokens > 0 {
        b = b.min(i64::from(model.max_input_tokens));
    }
    Some(b)
}

/// Drops the oldest `ceil(n/5)` messages per step while the prompt is over the budget,
/// keeping at least two messages. Returns the index of the first kept message.
#[must_use]
pub fn fit_start(
    system: &str,
    existing: Option<&str>,
    msgs: &[message::Model],
    limit: usize,
    bytes_per_token: u32,
    budget: Option<i64>,
) -> usize {
    let Some(budget) = budget else { return 0 };
    let bpt = u64::from(bytes_per_token.max(1));
    let mut start = 0;
    loop {
        let prompt = build_user_prompt(existing, &msgs[start..], limit);
        let bytes = u64::try_from(system.len() + prompt.len()).unwrap_or(u64::MAX);
        let tokens = i64::try_from(bytes.div_ceil(bpt)).unwrap_or(i64::MAX);
        let n = msgs.len() - start;
        if tokens <= budget || n <= 2 {
            return start;
        }
        let drop = n.div_ceil(5).min(n - 2);
        start += drop.max(1);
    }
}

static ANALYSIS_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::unwrap_used)]
    Regex::new(r"(?s)<analysis>.*?</analysis>").unwrap()
});
static SUMMARY_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::unwrap_used)]
    Regex::new(r"(?s)<summary>(.*?)</summary>").unwrap()
});
static BLANK_RUN_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::unwrap_used)]
    Regex::new(r"\n[ \t]*(?:\n[ \t]*)+\n").unwrap()
});

fn collapse_blank_runs(s: &str) -> String {
    BLANK_RUN_RE.replace_all(s, "\n\n").trim().to_owned()
}

/// Extracts the stored summary from the model output (empty = no usable summary).
#[must_use]
pub fn parse_summary(text: &str) -> String {
    let without = ANALYSIS_RE.replace_all(text, "");
    if let Some(c) = SUMMARY_RE.captures(&without) {
        return collapse_blank_runs(c.get(1).map_or("", |m| m.as_str()));
    }
    if without.contains("<analysis") || without.contains("<summary") {
        return String::new();
    }
    collapse_blank_runs(&without)
}

/// `output_tokens - reasoning_tokens` when positive, else `ceil(bytes / 4)`.
#[must_use]
pub fn token_estimate(usage: Option<&UsageTokens>, summary: &str) -> i32 {
    let from_usage = usage.map_or(0, |u| u.output_tokens - u.reasoning_tokens);
    let v = if from_usage > 0 {
        from_usage
    } else {
        i64::try_from(summary.len().div_ceil(4)).unwrap_or(i64::MAX)
    };
    i32::try_from(v).unwrap_or(i32::MAX)
}

// ── Handler ────────────────────────────────────────────────────────────────

/// Outcome of one handler attempt (before the attempts policy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryResult {
    Success,
    /// Finished without a commit (`conflict`, `base_missing`, `frontier_deleted`, `empty_range`).
    Skipped(&'static str),
    Retry(String),
    Reject(String),
}

/// Thread summary queue handler.
pub struct ThreadSummaryHandler {
    deps: Arc<Deps>,
}

impl ThreadSummaryHandler {
    #[must_use]
    pub fn new(deps: Arc<Deps>) -> Self {
        Self { deps }
    }

    /// Runs one attempt for a decoded task.
    pub async fn run(&self, task: &ThreadSummaryTask) -> SummaryResult {
        match self.run_inner(task).await {
            Ok(r) => r,
            Err(e) => SummaryResult::Retry(format!("thread summary failed: {e}")),
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn run_inner(&self, task: &ThreadSummaryTask) -> Result<SummaryResult, DomainError> {
        let deps = &self.deps;
        let cfg = &deps.cfg.thread_summary_worker;
        let model_id = cfg.effective_model_id();
        let model = match deps
            .policy
            .resolve_model(DEFAULT_SUBJECT_ID, model_id, true)
            .await
        {
            Ok((_, m)) => m,
            Err(DomainError::InvalidArgument { reason, .. })
                if reason == reasons::INVALID_MODEL =>
            {
                tracing::error!(model = %model_id, "thread summary model is missing or disabled");
                return Ok(SummaryResult::Reject("model_unavailable".to_owned()));
            }
            Err(e) => {
                return Ok(SummaryResult::Retry(format!(
                    "summary model resolution failed: {e}"
                )));
            }
        };
        let scope = AccessScope::for_tenant(task.tenant_id);
        let base = task
            .base_frontier_created_at
            .zip(task.base_frontier_message_id);
        let target = (task.frozen_target_created_at, task.frozen_target_message_id);

        let (row, range) = {
            let conn = deps.db.conn()?;
            let row = find_summary(&conn, &scope, task.chat_id).await?;
            match (&base, &row) {
                (None, Some(_)) => return Ok(SummaryResult::Skipped("conflict")),
                (Some(_), None) => return Ok(SummaryResult::Skipped("base_missing")),
                (Some(b), Some(r))
                    if (r.summarized_up_to_created_at, r.summarized_up_to_message_id) != *b =>
                {
                    return Ok(SummaryResult::Skipped("conflict"));
                }
                _ => {}
            }
            let range = load_range(&conn, &scope, task.chat_id, base, target).await?;
            (row, range)
        };
        if range.is_empty() {
            return Ok(SummaryResult::Skipped("empty_range"));
        }

        let system = system_prompt(&model, &cfg.summary_system_prompt);
        let existing = row.as_ref().map(|r| r.summary_text.clone());
        let limit = cfg.message_content_limit;
        let mut start = fit_start(
            &system,
            existing.as_deref(),
            &range,
            limit,
            model.estimation_budgets.bytes_per_token_conservative,
            summary_input_budget(&model),
        );

        let mut ptl_retries = 0;
        let result = loop {
            let prompt = build_user_prompt(existing.as_deref(), &range[start..], limit);
            let req = summary_request(&model, task, &system, prompt);
            match deps.llm.complete(req).await {
                Ok(r) => break r,
                Err(f)
                    if f.context_length_exceeded && ptl_retries < 2 && range.len() - start > 2 =>
                {
                    ptl_retries += 1;
                    let n = range.len() - start;
                    start += n.div_ceil(5).min(n - 2).max(1);
                }
                Err(f) => {
                    tracing::warn!(chat_id = %task.chat_id, code = f.code, "thread summary provider call failed");
                    return Ok(SummaryResult::Retry(format!("provider_error: {}", f.code)));
                }
            }
        };
        let summary_text = parse_summary(&result.text);
        if summary_text.is_empty() {
            return Ok(SummaryResult::Retry("empty_summary".to_owned()));
        }
        let estimate = token_estimate(result.usage.as_ref(), &summary_text);

        let commit = CommitInput {
            task: task.clone(),
            base,
            target,
            summary_text,
            token_estimate: estimate,
            model_id: model.id.clone(),
            usage: result.usage,
        };
        let commit = Arc::new(commit);
        let res = retry_locked(|| {
            let deps2 = Arc::clone(deps);
            let commit = Arc::clone(&commit);
            deps.db.transaction(move |tx| {
                Box::pin(async move { commit_in_tx(tx, &deps2, &commit).await })
            })
        })
        .await;
        match res {
            Ok(Ok(wake)) => {
                wake.fire();
                Ok(SummaryResult::Success)
            }
            Ok(Err(skip)) => Ok(SummaryResult::Skipped(skip)),
            Err(e) if e.is_unique_violation() => Ok(SummaryResult::Skipped("conflict")),
            Err(e) => Ok(SummaryResult::Retry(format!("summary commit failed: {e}"))),
        }
    }
}

struct CommitInput {
    task: ThreadSummaryTask,
    base: Option<(OffsetDateTime, Uuid)>,
    target: (OffsetDateTime, Uuid),
    summary_text: String,
    token_estimate: i32,
    model_id: String,
    usage: Option<UsageTokens>,
}

fn summary_request(
    model: &ModelCatalogEntry,
    task: &ThreadSummaryTask,
    system: &str,
    prompt: String,
) -> LlmRequest {
    let mut metadata = std::collections::BTreeMap::new();
    metadata.insert("tenant_id".to_owned(), task.tenant_id.to_string());
    metadata.insert("user_id".to_owned(), DEFAULT_SUBJECT_ID.to_string());
    metadata.insert("chat_id".to_owned(), task.chat_id.to_string());
    metadata.insert("request_type".to_owned(), "summary".to_owned());
    metadata.insert("feature".to_owned(), "none".to_owned());
    LlmRequest {
        provider_id: model.provider_id.clone(),
        tenant_id: task.tenant_id,
        model: model.provider_model_id.clone(),
        instructions: system.to_owned(),
        input: vec![InputMessage::text(InputRole::User, prompt)],
        tool_exchanges: Vec::new(),
        tools: Vec::new(),
        max_output_tokens: model.max_output_tokens,
        max_tool_calls: None,
        api_params: model.general_config.api_params.clone(),
        user: format!(
            "{}{}",
            simple_uuid(task.tenant_id),
            simple_uuid(DEFAULT_SUBJECT_ID)
        ),
        metadata,
        request_type: RequestType::Summary,
        stream: false,
    }
}

async fn find_summary(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<Option<thread_summary::Model>, DomainError> {
    Ok(thread_summary::Entity::find()
        .filter(thread_summary::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?)
}

fn range_cond(
    chat_id: Uuid,
    base: Option<(OffsetDateTime, Uuid)>,
    target: (OffsetDateTime, Uuid),
) -> Condition {
    let mut c = Condition::all()
        .add(message::Column::ChatId.eq(chat_id))
        .add(message::Column::DeletedAt.is_null())
        .add(message::Column::IsCompressed.eq(false))
        .add(msg_key_le(target.0, target.1));
    if let Some((at, id)) = base {
        c = c.add(msg_key_gt(at, id));
    }
    c
}

async fn load_range(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    base: Option<(OffsetDateTime, Uuid)>,
    target: (OffsetDateTime, Uuid),
) -> Result<Vec<message::Model>, DomainError> {
    Ok(message::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(range_cond(chat_id, base, target))
        .order_by(message::Column::CreatedAt, Order::Asc)
        .order_by(message::Column::Id, Order::Asc)
        .all(runner)
        .await?)
}

/// Commit transaction: `Ok(Err(reason))` = finished without commit.
async fn commit_in_tx(
    tx: &DbTx<'_>,
    deps: &Deps,
    c: &CommitInput,
) -> Result<Result<Wake, &'static str>, DomainError> {
    let task = &c.task;
    let scope = AccessScope::for_tenant(task.tenant_id);
    let now = OffsetDateTime::now_utc();

    let target_msg = message::Entity::find()
        .filter(message::Column::ChatId.eq(task.chat_id))
        .secure()
        .scope_with(&scope)
        .and_id(c.target.1)?
        .one(tx)
        .await?;
    if target_msg.is_none_or(|m| m.deleted_at.is_some()) {
        return Ok(Err("frontier_deleted"));
    }

    match c.base {
        None => {
            if find_summary(tx, &scope, task.chat_id).await?.is_some() {
                return Ok(Err("conflict"));
            }
            let am = thread_summary::ActiveModel {
                id: Set(Uuid::new_v4()),
                tenant_id: Set(task.tenant_id),
                chat_id: Set(task.chat_id),
                summary_text: Set(c.summary_text.clone()),
                summarized_up_to_created_at: Set(c.target.0),
                summarized_up_to_message_id: Set(c.target.1),
                token_estimate: Set(c.token_estimate),
                created_at: Set(now),
                updated_at: Set(now),
            };
            secure_insert::<thread_summary::Entity>(am, &scope, tx).await?;
        }
        Some((at, id)) => {
            let rows = thread_summary::Entity::update_many()
                .col_expr(
                    thread_summary::Column::SummaryText,
                    Expr::value(c.summary_text.clone()),
                )
                .col_expr(
                    thread_summary::Column::SummarizedUpToCreatedAt,
                    Expr::value(c.target.0),
                )
                .col_expr(
                    thread_summary::Column::SummarizedUpToMessageId,
                    Expr::value(c.target.1),
                )
                .col_expr(
                    thread_summary::Column::TokenEstimate,
                    Expr::value(c.token_estimate),
                )
                .col_expr(thread_summary::Column::UpdatedAt, Expr::value(now))
                .filter(
                    Condition::all()
                        .add(thread_summary::Column::ChatId.eq(task.chat_id))
                        .add(thread_summary::Column::SummarizedUpToCreatedAt.eq(at))
                        .add(thread_summary::Column::SummarizedUpToMessageId.eq(id)),
                )
                .secure()
                .scope_with(&scope)
                .exec(tx)
                .await?
                .rows_affected;
            if rows == 0 {
                return Ok(Err("conflict"));
            }
        }
    }

    message::Entity::update_many()
        .col_expr(message::Column::IsCompressed, Expr::value(true))
        .filter(range_cond(task.chat_id, c.base, c.target))
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await?;

    let ev = UsageEvent {
        tenant_id: task.tenant_id,
        user_id: None,
        chat_id: task.chat_id,
        turn_id: None,
        request_id: task.system_request_id,
        effective_model: c.model_id.clone(),
        selected_model: c.model_id.clone(),
        terminal_state: "completed".to_owned(),
        billing_outcome: "system_task".to_owned(),
        // `null` when the provider reported no usage.
        usage: c.usage,
        actual_credits_micro: 0,
        settlement_method: "none".to_owned(),
        policy_version_applied: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: now,
        requester_type: "system".to_owned(),
        dedupe_key: format!(
            "{}/{SYSTEM_TASK_TYPE}/{}",
            simple_uuid(task.tenant_id),
            simple_uuid(task.system_request_id)
        ),
        system_task_type: Some(SYSTEM_TASK_TYPE.to_owned()),
    };
    let wake = deps.outbox.enqueue_usage(tx, &ev).await?;
    Ok(Ok(wake))
}

/// Outcome of the startup check of the thread-summary model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryModelCheck {
    /// Thread summaries are disabled; nothing checked.
    Disabled,
    /// The summary model exists and is enabled.
    Ok,
    /// The summary model is missing from the catalog or disabled (logged at `error`).
    Unavailable,
    /// The policy plugin could not be resolved (logged at `warn`).
    Unknown,
}

/// Startup check (gear `serve`): when thread summaries are enabled, resolves the summary
/// model (enabled models only) and logs an error when it is missing or disabled. Startup
/// continues in every case.
pub async fn check_summary_model(deps: &Deps) -> SummaryModelCheck {
    let cfg = &deps.cfg.thread_summary_worker;
    if !cfg.enabled {
        return SummaryModelCheck::Disabled;
    }
    let model_id = cfg.effective_model_id();
    match deps
        .policy
        .resolve_model(DEFAULT_SUBJECT_ID, model_id, true)
        .await
    {
        Ok(_) => SummaryModelCheck::Ok,
        Err(DomainError::InvalidArgument { reason, .. }) if reason == reasons::INVALID_MODEL => {
            tracing::error!(
                model = %model_id,
                "thread summary model is missing from the catalog or disabled; thread summaries will fail"
            );
            SummaryModelCheck::Unavailable
        }
        Err(e) => {
            tracing::warn!(model = %model_id, error = %e, "thread summary model could not be checked at startup");
            SummaryModelCheck::Unknown
        }
    }
}

#[async_trait::async_trait]
impl toolkit_db::outbox::LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &toolkit_db::outbox::OutboxMessage) -> MessageResult {
        let task: ThreadSummaryTask = match serde_json::from_slice(&msg.payload) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(error = %e, "invalid thread summary payload");
                return MessageResult::Reject(format!("invalid payload: {e}"));
            }
        };
        let max_attempts = i64::from(self.deps.cfg.thread_summary_worker.max_attempts);
        match self.run(&task).await {
            SummaryResult::Success => MessageResult::Ok,
            SummaryResult::Skipped(reason) => {
                tracing::debug!(chat_id = %task.chat_id, reason, "thread summary skipped");
                MessageResult::Ok
            }
            SummaryResult::Reject(r) => MessageResult::Reject(r),
            SummaryResult::Retry(r) => {
                if i64::from(msg.attempts) + 1 >= max_attempts {
                    tracing::error!(chat_id = %task.chat_id, reason = %r, "thread summary dead-lettered");
                    MessageResult::Reject(r)
                } else {
                    tracing::warn!(chat_id = %task.chat_id, reason = %r, "thread summary retry");
                    MessageResult::Retry
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "summary_tests.rs"]
mod summary_tests;
