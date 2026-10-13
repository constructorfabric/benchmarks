//! Thread summary outbox handler (DESIGN §3.6 "Thread Summary Update").

use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{ModelCatalogEntry, UsageEvent};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, Set};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::{SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::ServiceSlot;
use crate::domain::error::DomainError;
use crate::domain::service::MiniChat;
use crate::infra::db::entity::{messages, thread_summaries};
use crate::infra::db::now;
use crate::infra::llm::{InputMessage, LlmRequest, RequestMetadata, Role, provider_user_field};
use crate::infra::outbox::{Queue, ThreadSummaryTask};

/// Platform default subject id used as the system identity of summary calls.
pub const SYSTEM_USER_ID: &str = "11111111-6a88-4768-9dfc-6bcd5187d9ed";

const MERGE_INTRO: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";

const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

/// One summarized message.
#[derive(Debug, Clone)]
pub struct SummaryInput {
    pub role: String,
    pub content: String,
}

/// Build the user prompt of the summary request.
#[must_use]
pub fn build_user_prompt(existing: Option<&str>, msgs: &[SummaryInput], content_limit: usize) -> String {
    let mut out = String::new();
    match existing {
        None => out.push_str("Summarize the following conversation:\n\n"),
        Some(s) => {
            out.push_str(MERGE_INTRO);
            out.push_str("\n\n<existing_summary>\n");
            out.push_str(s);
            out.push_str("\n</existing_summary>\n\nNew messages to incorporate:\n\n");
        }
    }
    let entries: Vec<String> = msgs
        .iter()
        .map(|m| {
            let who = if m.role == "assistant" { "Assistant" } else { "User" };
            let content = if content_limit > 0 && m.content.chars().count() > content_limit {
                format!("{}...", m.content.chars().take(content_limit).collect::<String>())
            } else {
                m.content.clone()
            };
            format!("{who}: {content}")
        })
        .collect();
    out.push_str(&entries.join("\n\n"));
    out.push_str("\n\n");
    out.push_str(ANALYSIS_INSTRUCTION);
    out
}

fn strip_block(text: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = text.to_owned();
    while let Some(s) = out.find(&open) {
        if let Some(e) = out[s..].find(&close) {
            out.replace_range(s..s + e + close.len(), "");
        } else {
            out.truncate(s);
            break;
        }
    }
    out
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
            out.push(line);
            blank = false;
        }
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// Extract the stored summary from the model response.
#[must_use]
pub fn parse_summary(text: &str) -> String {
    let without_analysis = strip_block(text, "analysis");
    if let Some(s) = without_analysis.find("<summary>") {
        let rest = &without_analysis[s + "<summary>".len()..];
        let inner = rest.find("</summary>").map_or(rest, |e| &rest[..e]);
        return collapse_blank_lines(inner.trim());
    }
    if without_analysis.contains("<analysis") || without_analysis.contains("<summary") {
        return String::new();
    }
    collapse_blank_lines(without_analysis.trim())
}

/// Estimated tokens of `s` at `bpt` bytes per token.
#[allow(clippy::integer_division)] // reason: deliberate integer ceiling division for token estimates
fn est(s: &str, bpt: u32) -> i64 {
    let bpt = i64::from(bpt.max(1));
    (i64::try_from(s.len()).unwrap_or(i64::MAX / 2) + bpt - 1) / bpt
}

/// Drop the oldest `ceil(n/5)` messages per step while over budget (keep >= 2).
#[must_use]
pub fn fit_messages(
    model: &ModelCatalogEntry,
    system: &str,
    existing: Option<&str>,
    mut msgs: Vec<SummaryInput>,
    content_limit: usize,
) -> Vec<SummaryInput> {
    if model.context_window == 0 {
        return msgs;
    }
    let mut budget = i64::from(model.context_window) - i64::from(model.max_output_tokens);
    if model.max_input_tokens > 0 {
        budget = budget.min(i64::from(model.max_input_tokens));
    }
    let bpt = model.estimation_budgets.bytes_per_token_conservative;
    while msgs.len() > 2 {
        let size = est(system, bpt) + est(&build_user_prompt(existing, &msgs, content_limit), bpt);
        if size <= budget {
            break;
        }
        let drop = msgs.len().div_ceil(5).min(msgs.len() - 2);
        msgs.drain(..drop);
    }
    msgs
}

fn after(created: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any()
        .add(messages::Column::CreatedAt.gt(created))
        .add(
            Condition::all()
                .add(messages::Column::CreatedAt.eq(created))
                .add(messages::Column::Id.gt(id)),
        )
}

fn at_or_before(created: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any()
        .add(messages::Column::CreatedAt.lt(created))
        .add(
            Condition::all()
                .add(messages::Column::CreatedAt.eq(created))
                .add(messages::Column::Id.lte(id)),
        )
}

enum Step {
    Done(&'static str),
    Retry(&'static str),
    Reject(String),
}

/// `mini-chat.thread_summary` handler.
pub struct ThreadSummaryHandler {
    pub slot: ServiceSlot,
}

#[async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let task: ThreadSummaryTask = match serde_json::from_slice(&msg.payload) {
            Ok(t) => t,
            Err(e) => return MessageResult::Reject(format!("malformed thread summary payload: {e}")),
        };
        let Some(svc) = self.slot.get() else {
            return MessageResult::Retry;
        };
        let max = i16::try_from(svc.cfg.thread_summary_worker.max_attempts).unwrap_or(i16::MAX);
        match svc.run_summary(&task).await {
            Step::Done(result) => {
                tracing::info!(chat_id = %task.chat_id, result, "thread summary task finished");
                MessageResult::Ok
            }
            Step::Reject(why) => MessageResult::Reject(why),
            Step::Retry(result) => {
                tracing::warn!(chat_id = %task.chat_id, result, attempt = msg.attempts + 1, "thread summary retry");
                if msg.attempts + 1 >= max {
                    MessageResult::Reject(format!("thread summary gave up after {max} attempts ({result})"))
                } else {
                    MessageResult::Retry
                }
            }
        }
    }
}

impl MiniChat {
    // reason: sequential worker pipeline with early-exit steps; splitting risks behaviour drift
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    async fn run_summary(&self, task: &ThreadSummaryTask) -> Step {
        let cfg = &self.cfg.thread_summary_worker;
        let model_id = cfg.summary_model().to_owned();
        let system_user = Uuid::parse_str(SYSTEM_USER_ID).unwrap_or_default();
        let Ok(snap) = self.policy.current_snapshot(system_user).await else {
            return Step::Retry("retry");
        };
        let Some(model) = snap.find_enabled(&model_id).cloned() else {
            tracing::error!(model = %model_id, "thread summary model is missing or disabled");
            return Step::Reject(format!("summary model '{model_id}' unavailable (model_unavailable)"));
        };
        let Ok(target_created) = OffsetDateTime::parse(&task.frozen_target_created_at, &Rfc3339) else {
            return Step::Reject("invalid frozen target".into());
        };
        let base = match (&task.base_frontier_created_at, task.base_frontier_message_id) {
            (Some(c), Some(id)) => match OffsetDateTime::parse(c, &Rfc3339) {
                Ok(t) => Some((t, id)),
                Err(_) => return Step::Reject("invalid base frontier".into()),
            },
            _ => None,
        };
        let scope = AccessScope::for_tenant(task.tenant_id);
        let Ok(conn) = self.db.conn() else {
            return Step::Retry("retry");
        };
        let Ok(current) = thread_summaries::Entity::find()
            .filter(thread_summaries::Column::ChatId.eq(task.chat_id))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await
        else {
            return Step::Retry("retry");
        };
        match (&current, base) {
            (Some(s), None) => {
                let _ = s;
                return Step::Done("cas_conflict");
            }
            (None, Some(_)) => return Step::Done("base_missing"),
            (Some(s), Some((bc, bid))) if (s.summarized_up_to_created_at, s.summarized_up_to_message_id) != (bc, bid) => {
                return Step::Done("cas_conflict");
            }
            _ => {}
        }
        let mut msg_filter = Condition::all()
            .add(messages::Column::ChatId.eq(task.chat_id))
            .add(messages::Column::DeletedAt.is_null())
            .add(messages::Column::IsCompressed.eq(false))
            .add(messages::Column::Role.ne("system"))
            .add(at_or_before(target_created, task.frozen_target_message_id));
        if let Some((bc, bid)) = base {
            msg_filter = msg_filter.add(after(bc, bid));
        }
        let Ok(rows) = messages::Entity::find()
            .filter(msg_filter)
            .order_by_asc(messages::Column::CreatedAt)
            .order_by_asc(messages::Column::Id)
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await
        else {
            return Step::Retry("retry");
        };
        if rows.is_empty() {
            return Step::Done("not_needed");
        }
        let existing = current.as_ref().map(|s| s.summary_text.clone());
        let system = if model.thread_summary_prompt.trim().is_empty() {
            if cfg.summary_system_prompt.trim().is_empty() {
                crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT.to_owned()
            } else {
                cfg.summary_system_prompt.clone()
            }
        } else {
            model.thread_summary_prompt.clone()
        };
        let inputs: Vec<SummaryInput> = rows
            .iter()
            .map(|m| SummaryInput {
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect();
        let mut msgs = fit_messages(&model, &system, existing.as_deref(), inputs, cfg.message_content_limit);
        let Ok(provider) = self.resolver.resolve(&model.provider_id, &task.tenant_id.to_string()) else {
            return Step::Retry("retry");
        };
        let tenant = task.tenant_id.to_string();
        let mut completion = None;
        for _attempt in 0..3 {
            let req = LlmRequest {
                provider_model_id: model.provider_model_id.clone(),
                instructions: system.clone(),
                input: vec![InputMessage::text(
                    Role::User,
                    build_user_prompt(existing.as_deref(), &msgs, cfg.message_content_limit),
                )],
                max_output_tokens: model.max_output_tokens,
                tools: Vec::new(),
                max_tool_calls: model.max_tool_calls,
                api_params: model.general_config.api_params.clone(),
                user: provider_user_field(&tenant, SYSTEM_USER_ID),
                metadata: RequestMetadata {
                    tenant_id: tenant.clone(),
                    user_id: SYSTEM_USER_ID.to_owned(),
                    chat_id: task.chat_id.to_string(),
                    request_type: "summary".into(),
                    feature: "none".into(),
                },
                stream: false,
                tool_exchanges: Vec::new(),
            };
            let timeout = Duration::from_secs(self.cfg.thread_summary_worker.claim_timeout_secs.saturating_sub(5).max(10));
            match self.llm.complete(&provider, &req, timeout).await {
                Ok(c) => {
                    completion = Some(c);
                    break;
                }
                Err(e) => {
                    let ctx_len = e.to_string().contains("context_length");
                    if ctx_len && msgs.len() > 2 {
                        let drop = msgs.len().div_ceil(5).min(msgs.len() - 2);
                        msgs.drain(..drop);
                        continue;
                    }
                    tracing::warn!(error = %e, "thread summary provider call failed");
                    return Step::Retry("provider_error");
                }
            }
        }
        let Some(completion) = completion else {
            return Step::Retry("provider_error");
        };
        let summary = parse_summary(&completion.text);
        if summary.is_empty() {
            return Step::Retry("empty_summary");
        }
        let usage = completion.usage.unwrap_or_default();
        let diff = usage.output_tokens - usage.reasoning_tokens;
        let token_estimate = if diff > 0 {
            diff
        } else {
            i64::try_from(summary.len().div_ceil(4)).unwrap_or(i64::MAX)
        };
        match self
            .commit_summary(task, base, target_created, summary, token_estimate, &model.id, usage)
            .await
        {
            Ok(r) => Step::Done(r),
            Err(e) => {
                tracing::warn!(error = %e, "thread summary commit failed");
                Step::Retry("retry")
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit_summary(
        &self,
        task: &ThreadSummaryTask,
        base: Option<(OffsetDateTime, Uuid)>,
        target_created: OffsetDateTime,
        summary: String,
        token_estimate: i64,
        model_id: &str,
        usage: mini_chat_sdk::UsageTokens,
    ) -> Result<&'static str, DomainError> {
        let task = task.clone();
        let outbox = self.outbox.clone();
        let model_id = model_id.to_owned();
        let res = crate::infra::db::tx_retry(&self.db, move |tx| {
                let model_id = model_id.clone();
                let outbox = outbox.clone();
                let summary = summary.clone();
                Box::pin(async move {
                    let scope = AccessScope::for_tenant(task.tenant_id);
                    let ts = now();
                    let target_alive = messages::Entity::find()
                        .filter(
                            Condition::all()
                                .add(messages::Column::Id.eq(task.frozen_target_message_id))
                                .add(messages::Column::DeletedAt.is_null()),
                        )
                        .secure()
                        .scope_with(&scope)
                        .count(tx)
                        .await?;
                    if target_alive == 0 {
                        return Ok((None, "frontier_deleted"));
                    }
                    let te = i32::try_from(token_estimate).unwrap_or(i32::MAX);
                    match base {
                        None => {
                            let am = thread_summaries::ActiveModel {
                                id: Set(Uuid::new_v4()),
                                tenant_id: Set(task.tenant_id),
                                chat_id: Set(task.chat_id),
                                summary_text: Set(summary),
                                summarized_up_to_created_at: Set(target_created),
                                summarized_up_to_message_id: Set(task.frozen_target_message_id),
                                token_estimate: Set(te),
                                created_at: Set(ts),
                                updated_at: Set(ts),
                            };
                            let ins = thread_summaries::Entity::insert(am)
                                .secure()
                                .scope_unchecked(&scope)?
                                .exec(tx)
                                .await;
                            if let Err(e) = ins {
                                let e: DomainError = e.into();
                                if e.is_unique_violation() {
                                    return Ok((None, "cas_conflict"));
                                }
                                return Err(e);
                            }
                        }
                        Some((bc, bid)) => {
                            let r = thread_summaries::Entity::update_many()
                                .col_expr(thread_summaries::Column::SummaryText, Expr::value(summary))
                                .col_expr(thread_summaries::Column::SummarizedUpToCreatedAt, Expr::value(target_created))
                                .col_expr(
                                    thread_summaries::Column::SummarizedUpToMessageId,
                                    Expr::value(task.frozen_target_message_id),
                                )
                                .col_expr(thread_summaries::Column::TokenEstimate, Expr::value(te))
                                .col_expr(thread_summaries::Column::UpdatedAt, Expr::value(ts))
                                .filter(
                                    Condition::all()
                                        .add(thread_summaries::Column::ChatId.eq(task.chat_id))
                                        .add(thread_summaries::Column::SummarizedUpToCreatedAt.eq(bc))
                                        .add(thread_summaries::Column::SummarizedUpToMessageId.eq(bid)),
                                )
                                .secure()
                                .scope_with(&scope)
                                .exec(tx)
                                .await?;
                            if r.rows_affected == 0 {
                                return Ok((None, "cas_conflict"));
                            }
                        }
                    }
                    let mut cond = Condition::all()
                        .add(messages::Column::ChatId.eq(task.chat_id))
                        .add(messages::Column::DeletedAt.is_null())
                        .add(at_or_before(target_created, task.frozen_target_message_id));
                    if let Some((bc, bid)) = base {
                        cond = cond.add(after(bc, bid));
                    }
                    messages::Entity::update_many()
                        .col_expr(messages::Column::IsCompressed, Expr::value(true))
                        .filter(cond)
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    let tenant_simple = task.tenant_id.as_simple().to_string();
                    let event = UsageEvent {
                        tenant_id: task.tenant_id,
                        user_id: None,
                        chat_id: task.chat_id,
                        turn_id: None,
                        request_id: task.system_request_id,
                        effective_model: model_id.clone(),
                        selected_model: model_id,
                        terminal_state: "completed".into(),
                        billing_outcome: "system_task".into(),
                        usage: Some(usage),
                        actual_credits_micro: 0,
                        settlement_method: "none".into(),
                        policy_version_applied: 0,
                        web_search_calls: 0,
                        code_interpreter_calls: 0,
                        file_search_calls: 0,
                        timestamp: ts.format(&Rfc3339).unwrap_or_default(),
                        requester_type: "system".into(),
                        dedupe_key: format!(
                            "{tenant_simple}/thread_summary_update/{}",
                            task.system_request_id.as_simple()
                        ),
                        system_task_type: Some("thread_summary_update".into()),
                    };
                    let wake = outbox.enqueue(tx, Queue::Usage, task.tenant_id, &event).await?;
                    Ok((Some(wake), "success"))
                })
            })
            .await?;
        let (wake, result) = res;
        if let Some(w) = wake {
            w.fire();
        }
        Ok(result)
    }
}

#[cfg(test)]
#[path = "thread_summary_tests.rs"]
mod tests;
