//! Thread summary handler (`mini-chat.thread_summary`, DESIGN "Thread Summary Update").

use std::sync::Arc;

use mini_chat_sdk::{ModelCatalogEntry, UsageEvent, UsageTokens};
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QueryOrder};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage, Wake};
use toolkit_db::secure::{ScopeError, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::{ThreadSummaryTask, system_dedupe_key};
use crate::infra::db::WriteTransaction as _;
use crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT;
use crate::domain::error::DomainError;
use crate::domain::repo;
use crate::domain::service::Svc;
use crate::infra::db::entities::{chats, messages, thread_summaries};
use crate::infra::db::now;
use crate::infra::llm::{InputMessage, LlmRequest, Role};

/// Platform default subject id used as the system identity of summary requests.
pub const SYSTEM_USER_ID: Uuid = Uuid::from_u128(0x1111_1111_6a88_4768_9dfc_6bcd_5187_d9ed);

/// Opening of the request when no summary exists.
pub const OPENING_FIRST: &str = "Summarize the following conversation:";

/// Opening of the request when a summary exists (B.5.5).
pub const OPENING_MERGE: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";

/// Analysis instruction at the end of the request (B.5.5).
pub const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

/// A message of the summarized range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeMessage {
    /// `user` or `assistant`.
    pub role: String,
    /// Content.
    pub content: String,
}

/// System prompt of the summary request.
#[must_use]
pub fn system_prompt(entry: &ModelCatalogEntry, configured: &str) -> String {
    if !entry.thread_summary_prompt.trim().is_empty() {
        entry.thread_summary_prompt.clone()
    } else if !configured.trim().is_empty() {
        configured.to_owned()
    } else {
        DEFAULT_SUMMARY_SYSTEM_PROMPT.to_owned()
    }
}

fn truncate_content(content: &str, limit: usize) -> String {
    if limit == 0 || content.chars().count() <= limit {
        return content.to_owned();
    }
    let cut: String = content.chars().take(limit).collect();
    format!("{cut}...")
}

/// Builds the user prompt of the summary request.
#[must_use]
pub fn user_prompt(existing: Option<&str>, msgs: &[RangeMessage], content_limit: usize) -> String {
    let mut out = String::new();
    if let Some(s) = existing {
        out.push_str(OPENING_MERGE);
        out.push_str("\n\n<existing_summary>\n");
        out.push_str(s);
        out.push_str("\n</existing_summary>\n\nNew messages to incorporate:\n\n");
    } else {
        out.push_str(OPENING_FIRST);
        out.push_str("\n\n");
    }
    let entries: Vec<String> = msgs
        .iter()
        .filter(|m| m.role != "system")
        .map(|m| {
            let who = if m.role == "user" { "User" } else { "Assistant" };
            format!("{who}: {}", truncate_content(&m.content, content_limit))
        })
        .collect();
    out.push_str(&entries.join("\n\n"));
    out.push_str("\n\n");
    out.push_str(ANALYSIS_INSTRUCTION);
    out
}

/// Input budget of the summary model (`None` = no fitting).
#[must_use]
pub fn input_budget(entry: &ModelCatalogEntry) -> Option<u64> {
    if entry.context_window == 0 {
        return None;
    }
    let mut b = u64::from(entry.context_window.saturating_sub(entry.max_output_tokens));
    if entry.max_input_tokens > 0 {
        b = b.min(u64::from(entry.max_input_tokens));
    }
    Some(b)
}

/// Drops the oldest `ceil(n/5)` messages per step while the prompt is over budget (keeps ≥ 2).
#[must_use]
pub fn fit_messages(
    system: &str,
    existing: Option<&str>,
    msgs: &[RangeMessage],
    content_limit: usize,
    budget: Option<u64>,
    bytes_per_token: u32,
) -> usize {
    let Some(budget) = budget else { return 0 };
    let bpt = u64::from(bytes_per_token.max(1));
    let mut skip = 0usize;
    loop {
        let prompt = user_prompt(existing, &msgs[skip..], content_limit);
        let est = (system.len() as u64 + prompt.len() as u64).div_ceil(bpt);
        let n = msgs.len() - skip;
        if est <= budget || n <= 2 {
            return skip;
        }
        skip += n.div_ceil(5).min(n - 2);
    }
}

/// Parses the model output: removes `<analysis>`, extracts `<summary>`, collapses blank lines.
#[must_use]
pub fn parse_summary(text: &str) -> String {
    let mut rest = text.to_owned();
    while let Some(start) = rest.find("<analysis>") {
        match rest[start..].find("</analysis>") {
            Some(end) => rest.replace_range(start..start + end + "</analysis>".len(), ""),
            None => break,
        }
    }
    let body = if let Some(start) = rest.find("<summary>") {
        let inner = &rest[start + "<summary>".len()..];
        match inner.find("</summary>") {
            Some(end) => inner[..end].to_owned(),
            None => inner.to_owned(),
        }
    } else {
        if rest.contains("<analysis") || rest.contains("<summary") {
            return String::new();
        }
        rest
    };
    collapse_blank_lines(body.trim())
}

fn collapse_blank_lines(s: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut blank = false;
    for line in s.lines() {
        if line.trim().is_empty() {
            if !blank && !out.is_empty() {
                out.push("");
            }
            blank = true;
        } else {
            out.push(line.trim_end());
            blank = false;
        }
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// Stored token estimate of a summary.
#[must_use]
pub fn token_estimate(usage: Option<&UsageTokens>, summary: &str) -> i32 {
    let diff = usage.map_or(0, |u| u.output_tokens - u.reasoning_tokens);
    let v = if diff > 0 { diff } else { i64::try_from(summary.len().div_ceil(4)).unwrap_or(i64::MAX) };
    i32::try_from(v).unwrap_or(i32::MAX)
}

fn is_context_length_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("context_length") || m.contains("context length") || m.contains("maximum context")
}

/// Leased handler of the thread summary queue.
pub struct ThreadSummaryHandler {
    /// Services.
    pub svc: Arc<Svc>,
}

enum Step {
    Done(MessageResult),
}

impl ThreadSummaryHandler {
    fn exec(&self, result: &str) {
        self.svc.metrics.inc("thread_summary_execution_total", &[("result", result)]);
    }

    fn retry_or_reject(&self, attempts: i16, why: &str) -> MessageResult {
        if u32::try_from(attempts + 1).unwrap_or(u32::MAX) >= self.svc.cfg.thread_summary_worker.max_attempts {
            MessageResult::Reject(format!("thread summary: {why} (max attempts reached)"))
        } else {
            MessageResult::Retry
        }
    }

    #[allow(
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        reason = "thread summary worker orchestration: frontier checks, range load, LLM call and CAS commit"
    )]
    async fn run(&self, task: &ThreadSummaryTask, attempts: i16) -> Result<Step, DomainError> {
        let svc = &self.svc;
        let cfg = &svc.cfg.thread_summary_worker;
        let scope = AccessScope::for_tenant(task.tenant_id);
        let conn = svc.db.conn()?;

        let chat = chats::Entity::find()
            .filter(Condition::all().add(chats::Column::Id.eq(task.chat_id)))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await?;
        if chat.as_ref().is_none_or(|c| c.deleted_at.is_some()) {
            return Ok(Step::Done(MessageResult::Ok));
        }

        // Pre-check of the base frontier.
        let current = repo::thread_summary(&conn, &scope, task.chat_id).await?;
        let base = task.base_frontier_created_at.zip(task.base_frontier_message_id);
        match (&current, base) {
            (None, Some(_)) => {
                self.exec("base_missing");
                return Ok(Step::Done(MessageResult::Ok));
            }
            (Some(s), b) if b != Some((s.summarized_up_to_created_at, s.summarized_up_to_message_id)) => {
                svc.metrics.inc("thread_summary_cas_conflicts_total", &[]);
                return Ok(Step::Done(MessageResult::Ok));
            }
            _ => {}
        }

        // Summary model (enabled filter).
        let snapshot = match svc.policy.current_snapshot(SYSTEM_USER_ID).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "thread summary: policy snapshot unavailable");
                self.exec("retry");
                return Ok(Step::Done(self.retry_or_reject(attempts, "policy unavailable")));
            }
        };
        let model_id = cfg.effective_model_id().to_owned();
        let Some(entry) = snapshot.enabled_model(&model_id).cloned() else {
            tracing::error!(model = %model_id, "thread summary model is missing or disabled");
            self.exec("model_unavailable");
            return Ok(Step::Done(MessageResult::Reject(format!("summary model '{model_id}' unavailable"))));
        };
        let target = match svc.llm.resolver.chat_target(&entry.provider_id, task.tenant_id) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "thread summary: provider resolution failed");
                self.exec("retry");
                return Ok(Step::Done(self.retry_or_reject(attempts, "provider resolution failed")));
            }
        };

        // Range (base, target].
        let mut range_filter = Condition::all()
            .add(messages::Column::ChatId.eq(task.chat_id))
            .add(messages::Column::DeletedAt.is_null())
            .add(messages::Column::IsCompressed.eq(false))
            .add(repo::at_or_before(task.frozen_target_created_at, task.frozen_target_message_id));
        if let Some((c, id)) = base {
            range_filter = range_filter.add(repo::after(c, id));
        }
        let rows = messages::Entity::find()
            .filter(range_filter)
            .order_by(messages::Column::CreatedAt, Order::Asc)
            .order_by(messages::Column::Id, Order::Asc)
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await?;
        let msgs: Vec<RangeMessage> = rows
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| RangeMessage { role: m.role.clone(), content: m.content.clone() })
            .collect();

        let system = system_prompt(&entry, &cfg.summary_system_prompt);
        let existing = current.as_ref().map(|s| s.summary_text.as_str());
        let mut skip = fit_messages(
            &system,
            existing,
            &msgs,
            cfg.message_content_limit,
            input_budget(&entry),
            entry.estimation_budgets.bytes_per_token_conservative,
        );
        let mut metadata = serde_json::Map::new();
        metadata.insert("tenant_id".into(), task.tenant_id.to_string().into());
        metadata.insert("user_id".into(), SYSTEM_USER_ID.to_string().into());
        metadata.insert("chat_id".into(), task.chat_id.to_string().into());
        metadata.insert("request_type".into(), "summary".into());
        metadata.insert("feature".into(), "none".into());

        let mut ptl_retries = 0;
        let completion = loop {
            let req = LlmRequest {
                model: entry.provider_model_id.clone(),
                instructions: system.clone(),
                input: vec![InputMessage::text(
                    Role::User,
                    user_prompt(existing, &msgs[skip..], cfg.message_content_limit),
                )],
                max_output_tokens: entry.max_output_tokens,
                tools: Vec::new(),
                max_tool_calls: 0,
                user: format!("{}{}", task.tenant_id.simple(), SYSTEM_USER_ID.simple()),
                metadata: metadata.clone(),
                api_params: entry.general_config.api_params.clone(),
                stream: false,
            };
            match svc.llm.complete(&target, &req).await {
                Ok(c) => break c,
                Err(e) => {
                    let remaining = msgs.len() - skip;
                    if is_context_length_error(&e.message) && ptl_retries < 2 && remaining > 2 {
                        ptl_retries += 1;
                        skip += remaining.div_ceil(5).min(remaining - 2);
                        continue;
                    }
                    tracing::warn!(error = %e.message, "thread summary provider call failed");
                    self.exec("provider_error");
                    svc.metrics.inc("summary_fallback_total", &[]);
                    return Ok(Step::Done(self.retry_or_reject(attempts, "provider error")));
                }
            }
        };

        let summary = parse_summary(&completion.text);
        if summary.is_empty() {
            self.exec("empty_summary");
            return Ok(Step::Done(self.retry_or_reject(attempts, "empty summary")));
        }
        let estimate = token_estimate(completion.usage.as_ref(), &summary);
        let usage = completion.usage.unwrap_or_default();

        let t = task.clone();
        let outbox = svc.outbox.clone();
        let policy_version = snapshot.policy_version;
        let model = entry.id.clone();
        let res = svc
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    let scope = AccessScope::for_tenant(t.tenant_id);
                    let target_msg = messages::Entity::find()
                        .filter(Condition::all().add(messages::Column::Id.eq(t.frozen_target_message_id)))
                        .secure()
                        .scope_with(&scope)
                        .one(tx)
                        .await?;
                    if target_msg.is_none_or(|m| m.deleted_at.is_some()) {
                        return Ok((Some("frontier_deleted"), Wake::empty()));
                    }
                    let ts = now();
                    let applied = if let Some((bc, bid)) = t.base_frontier_created_at.zip(t.base_frontier_message_id) {
                        let r = thread_summaries::Entity::update_many()
                            .secure()
                            .col_expr(thread_summaries::Column::SummaryText, Expr::value(summary.clone()))
                            .col_expr(
                                thread_summaries::Column::SummarizedUpToCreatedAt,
                                Expr::value(t.frozen_target_created_at),
                            )
                            .col_expr(
                                thread_summaries::Column::SummarizedUpToMessageId,
                                Expr::value(t.frozen_target_message_id),
                            )
                            .col_expr(thread_summaries::Column::TokenEstimate, Expr::value(estimate))
                            .col_expr(thread_summaries::Column::UpdatedAt, Expr::value(ts))
                            .filter(
                                Condition::all()
                                    .add(thread_summaries::Column::ChatId.eq(t.chat_id))
                                    .add(thread_summaries::Column::SummarizedUpToCreatedAt.eq(bc))
                                    .add(thread_summaries::Column::SummarizedUpToMessageId.eq(bid)),
                            )
                            .scope_with(&scope)
                            .exec(tx)
                            .await?;
                        r.rows_affected == 1
                    } else {
                        let am = thread_summaries::ActiveModel {
                            id: Set(Uuid::now_v7()),
                            tenant_id: Set(t.tenant_id),
                            chat_id: Set(t.chat_id),
                            summary_text: Set(summary.clone()),
                            summarized_up_to_created_at: Set(t.frozen_target_created_at),
                            summarized_up_to_message_id: Set(t.frozen_target_message_id),
                            token_estimate: Set(estimate),
                            created_at: Set(ts),
                            updated_at: Set(ts),
                        };
                        let r = thread_summaries::Entity::insert(am.clone())
                            .secure()
                            .scope_with_model(&scope, &am)?
                            .on_conflict_raw(
                                OnConflict::column(thread_summaries::Column::ChatId).do_nothing().to_owned(),
                            )
                            .exec(tx)
                            .await;
                        match r {
                            Ok(_) => true,
                            Err(ScopeError::Db(sea_orm::DbErr::RecordNotInserted)) => false,
                            Err(e) => return Err(e.into()),
                        }
                    };
                    if !applied {
                        return Ok((Some("cas_conflict"), Wake::empty()));
                    }
                    let mut range = Condition::all()
                        .add(messages::Column::ChatId.eq(t.chat_id))
                        .add(messages::Column::DeletedAt.is_null())
                        .add(repo::at_or_before(t.frozen_target_created_at, t.frozen_target_message_id));
                    if let Some((c, id)) = t.base_frontier_created_at.zip(t.base_frontier_message_id) {
                        range = range.add(repo::after(c, id));
                    }
                    messages::Entity::update_many()
                        .secure()
                        .col_expr(messages::Column::IsCompressed, Expr::value(true))
                        .filter(range)
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    let ev = UsageEvent {
                        tenant_id: t.tenant_id,
                        user_id: None,
                        chat_id: t.chat_id,
                        turn_id: None,
                        request_id: t.system_request_id,
                        effective_model: model.clone(),
                        selected_model: model.clone(),
                        terminal_state: "completed".into(),
                        billing_outcome: "system_task".into(),
                        usage: Some(usage),
                        actual_credits_micro: 0,
                        settlement_method: "none".into(),
                        policy_version_applied: policy_version,
                        web_search_calls: 0,
                        code_interpreter_calls: 0,
                        file_search_calls: 0,
                        timestamp: ts,
                        requester_type: "system".into(),
                        dedupe_key: system_dedupe_key(t.tenant_id, &t.system_task_type, t.system_request_id),
                        system_task_type: Some(t.system_task_type.clone()),
                    };
                    let wake = outbox.usage(tx, &ev).await?;
                    Ok((None, wake))
                })
            })
            .await;
        match res {
            Ok((None, wake)) => {
                wake.fire();
                self.exec("success");
                Ok(Step::Done(MessageResult::Ok))
            }
            Ok((Some("cas_conflict"), _)) => {
                svc.metrics.inc("thread_summary_cas_conflicts_total", &[]);
                Ok(Step::Done(MessageResult::Ok))
            }
            Ok((Some(r), _)) => {
                self.exec(r);
                Ok(Step::Done(MessageResult::Ok))
            }
            Err(e) => {
                tracing::warn!(error = %e, "thread summary commit failed");
                self.exec("retry");
                Ok(Step::Done(self.retry_or_reject(attempts, "commit failed")))
            }
        }
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let task: ThreadSummaryTask = match serde_json::from_slice(&msg.payload) {
            Ok(t) => t,
            Err(e) => return MessageResult::Reject(format!("malformed thread summary payload: {e}")),
        };
        match self.run(&task, msg.attempts).await {
            Ok(Step::Done(r)) => r,
            Err(e) => {
                tracing::warn!(error = %e, "thread summary infrastructure failure");
                self.exec("retry");
                self.retry_or_reject(msg.attempts, "infrastructure failure")
            }
        }
    }
}

#[cfg(test)]
#[path = "thread_summary_tests.rs"]
mod thread_summary_tests;
