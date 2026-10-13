//! Thread summary outbox handler (DESIGN §3.6 Thread Summary Update, B.5.5).

use std::sync::LazyLock;

use mini_chat_sdk::UsageEvent;
use regex::Regex;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, Set};
use time::OffsetDateTime;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use toolkit_security::constants::DEFAULT_SUBJECT_ID;
use uuid::Uuid;

use super::cleanup::HandlerOutcome;
use super::{MiniChatService, now};
use crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT;
use crate::domain::error::DomainError;
use crate::infra::db::entities::{chats, messages, thread_summaries};
use crate::infra::llm::types::{ChatRequest, InputMessage, Role};
use crate::infra::outbox::{ThreadSummaryTask, fire};

pub const OPENING_NEW: &str = "Summarize the following conversation:";
pub const OPENING_MERGE: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";
pub const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

#[allow(clippy::expect_used)] // constant pattern, compiled once; covered by unit tests
static ANALYSIS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<analysis>.*?</analysis>").expect("regex"));
#[allow(clippy::expect_used)] // constant pattern, compiled once; covered by unit tests
static SUMMARY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<summary>(.*?)</summary>").expect("regex"));
#[allow(clippy::expect_used)] // constant pattern, compiled once; covered by unit tests
static BLANKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\s*\n(\s*\n)+").expect("regex"));

/// Extract the stored summary from the model response.
#[must_use]
pub fn parse_summary(response: &str) -> String {
    let without = ANALYSIS.replace_all(response, "");
    let text = if let Some(c) = SUMMARY.captures(&without) {
        c.get(1).map_or("", |m| m.as_str()).to_owned()
    } else {
        let rest = without.trim().to_owned();
        if rest.contains("<analysis") || rest.contains("<summary") {
            String::new()
        } else {
            rest
        }
    };
    BLANKS.replace_all(text.trim(), "\n\n").into_owned()
}

/// Build the user prompt of the summary request.
#[must_use]
pub fn build_prompt(existing: Option<&str>, msgs: &[(String, String)], limit: usize) -> String {
    let mut p = String::new();
    if let Some(s) = existing {
        p.push_str(OPENING_MERGE);
        p.push_str("\n\n<existing_summary>\n");
        p.push_str(s);
        p.push_str("\n</existing_summary>\n\nNew messages to incorporate:\n\n");
    } else {
        p.push_str(OPENING_NEW);
        p.push_str("\n\n");
    }
    let entries: Vec<String> = msgs
        .iter()
        .map(|(role, content)| {
            let label = if role == "assistant" { "Assistant" } else { "User" };
            let body = if limit > 0 && content.chars().count() > limit {
                format!("{}...", content.chars().take(limit).collect::<String>())
            } else {
                content.clone()
            };
            format!("{label}: {body}")
        })
        .collect();
    p.push_str(&entries.join("\n\n"));
    p.push_str("\n\n");
    p.push_str(ANALYSIS_INSTRUCTION);
    p
}

fn order_gt(created: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any()
        .add(messages::Column::CreatedAt.gt(created))
        .add(Condition::all().add(messages::Column::CreatedAt.eq(created)).add(messages::Column::Id.gt(id)))
}

fn order_le(created: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any()
        .add(messages::Column::CreatedAt.lt(created))
        .add(Condition::all().add(messages::Column::CreatedAt.eq(created)).add(messages::Column::Id.lte(id)))
}

impl MiniChatService {
    /// Thread-summary queue handler.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity, clippy::similar_names)] // linear handler; `conn`/`cond` are idiomatic names
    pub async fn handle_thread_summary(&self, payload: &[u8], attempts: u32) -> HandlerOutcome {
        let task: ThreadSummaryTask = match serde_json::from_slice(payload) {
            Ok(t) => t,
            Err(e) => return HandlerOutcome::Reject(format!("malformed thread summary payload: {e}")),
        };
        let max = self.cfg.thread_summary_worker.max_attempts;
        let retry = |reason: String| {
            if attempts + 1 >= max {
                HandlerOutcome::Reject(format!("thread summary: max attempts reached: {reason}"))
            } else {
                HandlerOutcome::Retry(reason)
            }
        };
        let scope = AccessScope::for_tenant(task.tenant_id);
        let Ok(conn) = self.db.conn() else { return retry("db".into()) };
        let chat = match chats::Entity::find().filter(chats::Column::Id.eq(task.chat_id)).secure().scope_with(&scope).one(&conn).await {
            Ok(Some(c)) if c.deleted_at.is_none() => c,
            Ok(_) => return HandlerOutcome::Ok,
            Err(e) => return retry(e.to_string()),
        };
        let current = match thread_summaries::Entity::find()
            .filter(thread_summaries::Column::ChatId.eq(task.chat_id))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await
        {
            Ok(s) => s,
            Err(e) => return retry(e.to_string()),
        };
        let base = task.base_frontier_created_at.zip(task.base_frontier_message_id);
        match (&base, &current) {
            (Some(_), None) => {
                tracing::info!(chat_id = %task.chat_id, "thread summary base missing");
                return HandlerOutcome::Ok;
            }
            (Some((bc, bi)), Some(s)) if (s.summarized_up_to_created_at, s.summarized_up_to_message_id) != (*bc, *bi) => {
                return HandlerOutcome::Ok;
            }
            (None, Some(_)) => return HandlerOutcome::Ok,
            _ => {}
        }
        // Summary model (enabled filter).
        let snap = match self.policy.current_snapshot(DEFAULT_SUBJECT_ID).await {
            Ok(s) => s,
            Err(e) => return retry(e.to_string()),
        };
        let model_id = self.cfg.thread_summary_worker.summary_model().to_owned();
        let Some(model) = snap.find_enabled(&model_id).cloned() else {
            tracing::error!(model = %model_id, "thread summary model unavailable");
            return HandlerOutcome::Reject("summary model unavailable".into());
        };
        let target = match self.llm.resolver.chat_target(&model.provider_id, task.tenant_id) {
            Ok(t) => t,
            Err(e) => return retry(e),
        };
        let mut cond = Condition::all()
            .add(messages::Column::ChatId.eq(task.chat_id))
            .add(messages::Column::DeletedAt.is_null())
            .add(messages::Column::IsCompressed.eq(false))
            .add(messages::Column::Role.is_in(["user", "assistant"]))
            .add(order_le(task.frozen_target_created_at, task.frozen_target_message_id));
        if let Some((bc, bi)) = base {
            cond = cond.add(order_gt(bc, bi));
        }
        let rows = match messages::Entity::find()
            .filter(cond)
            .order_by(messages::Column::CreatedAt, sea_orm::Order::Asc)
            .order_by(messages::Column::Id, sea_orm::Order::Asc)
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await
        {
            Ok(r) => r,
            Err(e) => return retry(e.to_string()),
        };
        if rows.is_empty() {
            return HandlerOutcome::Ok;
        }
        let range_ids: Vec<Uuid> = rows.iter().map(|m| m.id).collect();
        let mut msgs: Vec<(String, String)> = rows.iter().map(|m| (m.role.clone(), m.content.clone())).collect();
        let system = if !model.thread_summary_prompt.trim().is_empty() {
            model.thread_summary_prompt.clone()
        } else if !self.cfg.thread_summary_worker.summary_system_prompt.trim().is_empty() {
            self.cfg.thread_summary_worker.summary_system_prompt.clone()
        } else {
            DEFAULT_SUMMARY_SYSTEM_PROMPT.to_owned()
        };
        let limit = self.cfg.thread_summary_worker.message_content_limit;
        let existing = current.as_ref().map(|s| s.summary_text.clone());
        // Fit the prompt to the summary model budget.
        if model.context_window > 0 {
            let mut budget = i64::from(model.context_window) - i64::from(model.max_output_tokens);
            if model.max_input_tokens > 0 {
                budget = budget.min(i64::from(model.max_input_tokens));
            }
            let bpt = i64::from(model.estimation_budgets.bytes_per_token_conservative.max(1));
            loop {
                let size = i64::try_from(system.len() + build_prompt(existing.as_deref(), &msgs, limit).len()).unwrap_or(i64::MAX);
                if size.div_euclid(bpt) <= budget || msgs.len() <= 2 {
                    break;
                }
                let drop_n = msgs.len().div_ceil(5).min(msgs.len() - 2);
                msgs.drain(..drop_n);
            }
        }
        let user = format!("{}{}", task.tenant_id.as_simple(), DEFAULT_SUBJECT_ID.as_simple());
        let mut metadata = serde_json::Map::new();
        metadata.insert("tenant_id".into(), task.tenant_id.to_string().into());
        metadata.insert("user_id".into(), DEFAULT_SUBJECT_ID.to_string().into());
        metadata.insert("chat_id".into(), task.chat_id.to_string().into());
        metadata.insert("request_type".into(), "summary".into());
        metadata.insert("feature".into(), "none".into());
        let mut ptl_retries = 0;
        let completion = loop {
            let req = ChatRequest {
                model: if model.provider_model_id.is_empty() { model.id.clone() } else { model.provider_model_id.clone() },
                instructions: system.clone(),
                input: vec![InputMessage::text(Role::User, build_prompt(existing.as_deref(), &msgs, limit))],
                max_output_tokens: if model.max_output_tokens == 0 { self.cfg.streaming.max_output_tokens } else { model.max_output_tokens },
                max_tool_calls: None,
                tools: Vec::new(),
                user: user.clone(),
                metadata: metadata.clone(),
                api_params: model.general_config.api_params.clone(),
                stream: false,
            };
            match self.llm.complete(&target, &req).await {
                Ok(c) => break c,
                Err(e) if e.context_length && ptl_retries < 2 && msgs.len() > 2 => {
                    ptl_retries += 1;
                    let drop_n = msgs.len().div_ceil(5).min(msgs.len() - 2);
                    msgs.drain(..drop_n);
                }
                Err(e) => {
                    tracing::warn!(error = %e, chat_id = %task.chat_id, "thread summary provider call failed; keeping previous summary");
                    return retry(e.to_string());
                }
            }
        };
        let summary_text = parse_summary(&completion.text);
        if summary_text.is_empty() {
            return retry("empty summary".into());
        }
        let u = completion.usage.unwrap_or_default();
        let net = u.output_tokens - u.reasoning_tokens;
        let token_estimate = if net > 0 { net } else { i64::try_from(summary_text.len().div_ceil(4)).unwrap_or(i64::MAX) };
        let outbox = self.outbox.clone();
        let model_name = model.id.clone();
        let tenant_id = chat.tenant_id;
        let res = self
            .tx(move |tx| {
                let outbox = outbox.clone();
                let task = task.clone();
                let summary_text = summary_text.clone();
                let range_ids = range_ids.clone();
                let model_name = model_name.clone();
                let scope = AccessScope::for_tenant(tenant_id);
                Box::pin(async move {
                    let target_alive = messages::Entity::find()
                        .filter(
                            Condition::all()
                                .add(messages::Column::Id.eq(task.frozen_target_message_id))
                                .add(messages::Column::DeletedAt.is_null()),
                        )
                        .secure()
                        .scope_with(&scope)
                        .one(tx)
                        .await?;
                    if target_alive.is_none() {
                        return Ok(None);
                    }
                    let ts = now();
                    let te = i32::try_from(token_estimate).unwrap_or(i32::MAX);
                    if let (Some(bc), Some(bi)) = (task.base_frontier_created_at, task.base_frontier_message_id) {
                        let r = thread_summaries::Entity::update_many()
                            .col_expr(thread_summaries::Column::SummaryText, Expr::value(summary_text))
                            .col_expr(thread_summaries::Column::SummarizedUpToCreatedAt, Expr::value(task.frozen_target_created_at))
                            .col_expr(thread_summaries::Column::SummarizedUpToMessageId, Expr::value(task.frozen_target_message_id))
                            .col_expr(thread_summaries::Column::TokenEstimate, Expr::value(te))
                            .col_expr(thread_summaries::Column::UpdatedAt, Expr::value(ts))
                            .filter(
                                Condition::all()
                                    .add(thread_summaries::Column::ChatId.eq(task.chat_id))
                                    .add(thread_summaries::Column::SummarizedUpToCreatedAt.eq(bc))
                                    .add(thread_summaries::Column::SummarizedUpToMessageId.eq(bi)),
                            )
                            .secure()
                            .scope_with(&scope)
                            .exec(tx)
                            .await?;
                        if r.rows_affected == 0 {
                            return Ok(None);
                        }
                    } else {
                        let am = thread_summaries::ActiveModel {
                            id: Set(Uuid::new_v4()),
                            tenant_id: Set(task.tenant_id),
                            chat_id: Set(task.chat_id),
                            summary_text: Set(summary_text),
                            summarized_up_to_created_at: Set(task.frozen_target_created_at),
                            summarized_up_to_message_id: Set(task.frozen_target_message_id),
                            token_estimate: Set(te),
                            created_at: Set(ts),
                            updated_at: Set(ts),
                        };
                        match secure_insert::<thread_summaries::Entity>(am, &scope, tx).await {
                            Ok(_) => {}
                            Err(e) if e.is_unique_violation() => return Ok(None),
                            Err(e) => return Err(DomainError::from(e)),
                        }
                    }
                    messages::Entity::update_many()
                        .col_expr(messages::Column::IsCompressed, Expr::value(true))
                        .filter(Condition::all().add(messages::Column::Id.is_in(range_ids)).add(messages::Column::DeletedAt.is_null()))
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
                        effective_model: model_name.clone(),
                        selected_model: model_name,
                        terminal_state: "completed".into(),
                        billing_outcome: "system_task".into(),
                        usage: Some(u),
                        actual_credits_micro: 0,
                        settlement_method: "none".into(),
                        policy_version_applied: 0,
                        web_search_calls: 0,
                        code_interpreter_calls: 0,
                        file_search_calls: 0,
                        timestamp: OffsetDateTime::now_utc(),
                        requester_type: "system".into(),
                        dedupe_key: format!(
                            "{}/thread_summary_update/{}",
                            task.tenant_id.as_simple(),
                            task.system_request_id.as_simple()
                        ),
                        system_task_type: Some("thread_summary_update".into()),
                    };
                    let w = outbox.usage(tx, &ev).await?;
                    Ok(Some(w))
                })
            })
            .await;
        match res {
            Ok(w) => {
                fire(w.into_iter().collect());
                HandlerOutcome::Ok
            }
            Err(e) => retry(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_strips_analysis() {
        assert_eq!(parse_summary("<analysis>think</analysis>\n<summary>\nA\n\n\n\nB\n</summary>"), "A\n\nB");
        assert_eq!(parse_summary("plain text"), "plain text");
        assert_eq!(parse_summary("<analysis>x</analysis> <summary>broken"), "");
    }

    #[test]
    fn prompt_shapes() {
        let p = build_prompt(None, &[("user".into(), "hi".into()), ("assistant".into(), "yo".into())], 0);
        assert!(p.starts_with(OPENING_NEW));
        assert!(p.contains("User: hi\n\nAssistant: yo"));
        assert!(p.ends_with("Respond with an <analysis> block followed by a <summary> block."));
        let p = build_prompt(Some("old"), &[("user".into(), "abcdef".into())], 3);
        assert!(p.contains("<existing_summary>\nold\n</existing_summary>"));
        assert!(p.contains("User: abc..."));
    }
}
