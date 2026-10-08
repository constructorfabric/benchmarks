//! Thread summary outbox handler (DESIGN §3.6 "Thread Summary Update").

#[allow(unused_imports)]
use sea_orm::{EntityTrait as _, QueryFilter as _};
use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{ModelCatalogEntry, UsageEvent, UsageTokens};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, Set};
use time::OffsetDateTime;
use toolkit_db::outbox::{MessageResult, OutboxMessage};
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use toolkit_security::constants::DEFAULT_SUBJECT_ID;
use uuid::Uuid;

use super::{Service, provider_user};
use crate::domain::clock;
use crate::domain::events::{PAYLOAD_USAGE, THREAD_SUMMARY_TASK, ThreadSummaryTask};
use crate::infra::llm::types::{InputMessage, InputRole, LlmRequest, RequestMetadata};
use crate::infra::outbox::{QueueHandler, fire};
use crate::infra::storage::entity::{message, thread_summary};

/// Built-in default system prompt of the summary request.
pub const DEFAULT_SUMMARY_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";

const OPENING_NEW: &str = "Summarize the following conversation:";
const OPENING_MERGE: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";
const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

/// Thread summary queue handler.
pub struct ThreadSummaryHandler(pub Arc<Service>);

#[async_trait]
impl QueueHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let task: ThreadSummaryTask = match serde_json::from_slice(&msg.payload) {
            Ok(t) => t,
            Err(e) => {
                return MessageResult::Reject(format!("malformed thread summary payload: {e}"));
            }
        };
        let max = i16::try_from(self.0.cfg.thread_summary_worker.max_attempts).unwrap_or(i16::MAX);
        let result = self.0.run_summary(&task).await;
        let label = match &result {
            SummaryResult::Done => "success",
            SummaryResult::Reject(_) => "model_unavailable",
            SummaryResult::Retry(r) if r.starts_with("summary provider error") => {
                self.0.metrics.summary_fallback.add(1, &[]);
                "provider_error"
            }
            SummaryResult::Retry(r) if r == "empty summary" => "empty_summary",
            SummaryResult::Retry(_) => "retry",
        };
        self.0
            .metrics
            .thread_summary_execution
            .add(1, &crate::infra::metrics::labels(&[("result", label)]));
        match result {
            SummaryResult::Done => MessageResult::Ok,
            SummaryResult::Reject(r) => MessageResult::Reject(r),
            SummaryResult::Retry(r) => {
                tracing::warn!(reason = %r, chat_id = %task.chat_id, "mini-chat: thread summary retry");
                if msg.attempts.saturating_add(1) >= max {
                    MessageResult::Reject(format!("thread summary: max attempts reached: {r}"))
                } else {
                    MessageResult::Retry
                }
            }
        }
    }
}

/// Outcome of one summary attempt.
pub enum SummaryResult {
    Done,
    Retry(String),
    Reject(String),
}

/// Build the user prompt of a summary request.
#[must_use]
pub fn build_user_prompt(
    existing: Option<&str>,
    messages: &[(String, String)],
    limit: usize,
) -> String {
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
    out.push_str("\n\n");
    let entries: Vec<String> = messages
        .iter()
        .map(|(role, content)| {
            let label = if role == "assistant" {
                "Assistant"
            } else {
                "User"
            };
            let text = if limit > 0 && content.chars().count() > limit {
                let mut t: String = content.chars().take(limit).collect();
                t.push_str("...");
                t
            } else {
                content.clone()
            };
            format!("{label}: {text}")
        })
        .collect();
    out.push_str(&entries.join("\n\n"));
    out.push_str("\n\n");
    out.push_str(ANALYSIS_INSTRUCTION);
    out
}

/// Extract the stored summary from a model response.
#[must_use]
pub fn parse_summary(raw: &str) -> String {
    let mut text = raw.to_owned();
    while let (Some(s), Some(e)) = (text.find("<analysis>"), text.find("</analysis>")) {
        if e < s {
            break;
        }
        text.replace_range(s..e + "</analysis>".len(), "");
    }
    let body = match (text.find("<summary>"), text.find("</summary>")) {
        (Some(s), Some(e)) if e > s => text[s + "<summary>".len()..e].to_owned(),
        _ => {
            if text.contains("<analysis") || text.contains("<summary") {
                return String::new();
            }
            text
        }
    };
    // collapse runs of blank lines
    let mut out = String::new();
    let mut blank = 0;
    for line in body.trim().lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.trim().to_owned()
}

fn est_tokens(bytes: usize, bpt: u32) -> i64 {
    let bpt = i64::from(bpt.max(1));
    (i64::try_from(bytes).unwrap_or(i64::MAX >> 2) + bpt - 1).div_euclid(bpt)
}

impl Service {
    fn summary_model(&self, snapshot: &mini_chat_sdk::PolicySnapshot) -> Option<ModelCatalogEntry> {
        let id = self.cfg.thread_summary_worker.summary_model();
        snapshot.find_enabled_model(id.as_ref()).cloned()
    }

    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    async fn run_summary(&self, task: &ThreadSummaryTask) -> SummaryResult {
        let scope = AccessScope::for_tenant(task.tenant_id);
        let snapshot = match self.policy.current_snapshot(DEFAULT_SUBJECT_ID).await {
            Ok(s) => s,
            Err(e) => return SummaryResult::Retry(e.to_string()),
        };
        let Some(model) = self.summary_model(&snapshot) else {
            tracing::error!(
                model = %self.cfg.thread_summary_worker.summary_model(),
                "mini-chat: summary model missing or disabled"
            );
            return SummaryResult::Reject("summary model unavailable".to_owned());
        };
        let base = match (task.base_frontier_created_at, task.base_frontier_message_id) {
            (Some(t), Some(i)) => Some((t, i)),
            _ => None,
        };
        let target = (task.frozen_target_created_at, task.frozen_target_message_id);
        let Ok(conn) = self.db.conn() else {
            return SummaryResult::Retry("db".to_owned());
        };
        let current = match thread_summary::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(thread_summary::Column::ChatId.eq(task.chat_id)))
            .one(&conn)
            .await
        {
            Ok(c) => c,
            Err(e) => return SummaryResult::Retry(e.to_string()),
        };
        let current_frontier = current
            .as_ref()
            .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        if current_frontier != base {
            if base.is_some() && current.is_none() {
                return SummaryResult::Done; // base_missing
            }
            return SummaryResult::Done; // CAS conflict
        }
        let mut filter = Condition::all()
            .add(message::Column::ChatId.eq(task.chat_id))
            .add(message::Column::DeletedAt.is_null())
            .add(message::Column::IsCompressed.eq(false))
            .add(key_le(target));
        if let Some(b) = base {
            filter = filter.add(key_gt(b));
        }
        let msgs = match message::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(filter)
            .order_by(message::Column::CreatedAt, sea_orm::Order::Asc)
            .order_by(message::Column::Id, sea_orm::Order::Asc)
            .all(&conn)
            .await
        {
            Ok(m) => m,
            Err(e) => return SummaryResult::Retry(e.to_string()),
        };
        let mut entries: Vec<(String, String)> = msgs
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| (m.role.clone(), m.content.clone()))
            .collect();
        if entries.is_empty() {
            return SummaryResult::Done;
        }
        let system = if !model.thread_summary_prompt.trim().is_empty() {
            model.thread_summary_prompt.clone()
        } else if !self
            .cfg
            .thread_summary_worker
            .summary_system_prompt
            .trim()
            .is_empty()
        {
            self.cfg.thread_summary_worker.summary_system_prompt.clone()
        } else {
            DEFAULT_SUMMARY_PROMPT.to_owned()
        };
        let existing = current.as_ref().map(|s| s.summary_text.clone());
        let limit = self.cfg.thread_summary_worker.message_content_limit;
        let bpt = model.estimation_budgets.bytes_per_token_conservative;
        if model.context_window > 0 {
            let mut budget = i64::from(model.context_window) - i64::from(model.max_output_tokens);
            if model.max_input_tokens > 0 {
                budget = budget.min(i64::from(model.max_input_tokens));
            }
            loop {
                let prompt = build_user_prompt(existing.as_deref(), &entries, limit);
                if est_tokens(prompt.len() + system.len(), bpt) <= budget || entries.len() <= 2 {
                    break;
                }
                let drop_n = entries.len().div_ceil(5).min(entries.len() - 2);
                entries.drain(..drop_n);
            }
        }
        let target_entry = match self
            .providers
            .chat_target(&model.provider_id, task.tenant_id)
        {
            Ok(t) => t,
            Err(e) => return SummaryResult::Retry(e),
        };
        let mut attempt = 0;
        let response = loop {
            let prompt = build_user_prompt(existing.as_deref(), &entries, limit);
            let req = LlmRequest {
                model: model.provider_model_id.clone(),
                instructions: system.clone(),
                input: vec![InputMessage::text(InputRole::User, prompt)],
                function_items: Vec::new(),
                tools: Vec::new(),
                max_output_tokens: model.max_output_tokens.max(1),
                max_tool_calls: None,
                api_params: model.general_config.api_params.clone(),
                user: provider_user(task.tenant_id, DEFAULT_SUBJECT_ID),
                metadata: RequestMetadata {
                    tenant_id: task.tenant_id.to_string(),
                    user_id: DEFAULT_SUBJECT_ID.to_string(),
                    chat_id: task.chat_id.to_string(),
                    request_type: "summary".to_owned(),
                    feature: "none".to_owned(),
                },
                stream: false,
            };
            match self.llm.complete(&target_entry, &req).await {
                Ok(r) => break r,
                Err(f) => {
                    let ctx_error = f
                        .provider_code
                        .as_deref()
                        .is_some_and(|c| c.contains("context_length"))
                        || f.message.contains("context length")
                        || f.message.contains("context_length");
                    if ctx_error && attempt < 2 && entries.len() > 2 {
                        attempt += 1;
                        let drop_n = entries.len().div_ceil(5).min(entries.len() - 2);
                        entries.drain(..drop_n);
                        continue;
                    }
                    return SummaryResult::Retry(format!("summary provider error: {}", f.message));
                }
            }
        };
        let text = parse_summary(&response.text);
        if text.is_empty() {
            return SummaryResult::Retry("empty summary".to_owned());
        }
        let diff = response.usage.output_tokens - response.usage.reasoning_tokens;
        let token_estimate = if diff > 0 {
            diff
        } else {
            est_tokens(text.len(), 4)
        };
        let token_estimate = i32::try_from(token_estimate).unwrap_or(i32::MAX);
        self.commit_summary(
            task,
            base,
            target,
            text,
            token_estimate,
            &model,
            response.usage,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit_summary(
        &self,
        task: &ThreadSummaryTask,
        base: Option<(OffsetDateTime, Uuid)>,
        target: (OffsetDateTime, Uuid),
        text: String,
        token_estimate: i32,
        model: &ModelCatalogEntry,
        usage: crate::infra::llm::types::Usage,
    ) -> SummaryResult {
        let outbox = Arc::clone(&self.outbox);
        let task = task.clone();
        let model_id = model.id.clone();
        let res = self
            .tx(move |tx| {
                let outbox = Arc::clone(&outbox);
                let task = task.clone();
                let text = text.clone();
                let model_id = model_id.clone();
                Box::pin(async move {
                    let scope = AccessScope::for_tenant(task.tenant_id);
                    let now = clock::now();
                    // Target frontier message must still be live.
                    let live = message::Entity::find()
                        .secure()
                        .scope_with(&scope)
                        .filter(
                            Condition::all()
                                .add(message::Column::Id.eq(target.1))
                                .add(message::Column::DeletedAt.is_null()),
                        )
                        .one(tx)
                        .await?;
                    if live.is_none() {
                        return Ok(None);
                    }
                    match base {
                        None => {
                            let am = thread_summary::ActiveModel {
                                id: Set(Uuid::new_v4()),
                                tenant_id: Set(task.tenant_id),
                                chat_id: Set(task.chat_id),
                                summary_text: Set(text.clone()),
                                summarized_up_to_created_at: Set(target.0),
                                summarized_up_to_message_id: Set(target.1),
                                token_estimate: Set(token_estimate),
                                created_at: Set(now),
                                updated_at: Set(now),
                            };
                            match secure_insert::<thread_summary::Entity>(am, &scope, tx).await {
                                Ok(_) => {}
                                Err(e) if e.is_unique_violation() => return Ok(None),
                                Err(e) => return Err(e.into()),
                            }
                        }
                        Some((bt, bid)) => {
                            let r = thread_summary::Entity::update_many()
                                .col_expr(
                                    thread_summary::Column::SummaryText,
                                    Expr::value(text.clone()),
                                )
                                .col_expr(
                                    thread_summary::Column::SummarizedUpToCreatedAt,
                                    Expr::value(target.0),
                                )
                                .col_expr(
                                    thread_summary::Column::SummarizedUpToMessageId,
                                    Expr::value(target.1),
                                )
                                .col_expr(
                                    thread_summary::Column::TokenEstimate,
                                    Expr::value(token_estimate),
                                )
                                .col_expr(thread_summary::Column::UpdatedAt, Expr::value(now))
                                .filter(
                                    Condition::all()
                                        .add(thread_summary::Column::ChatId.eq(task.chat_id))
                                        .add(thread_summary::Column::SummarizedUpToCreatedAt.eq(bt))
                                        .add(
                                            thread_summary::Column::SummarizedUpToMessageId.eq(bid),
                                        ),
                                )
                                .secure()
                                .scope_with(&scope)
                                .exec(tx)
                                .await?;
                            if r.rows_affected == 0 {
                                return Ok(None);
                            }
                        }
                    }
                    let mut cond = Condition::all()
                        .add(message::Column::ChatId.eq(task.chat_id))
                        .add(message::Column::DeletedAt.is_null())
                        .add(key_le(target));
                    if let Some(b) = base {
                        cond = cond.add(key_gt(b));
                    }
                    message::Entity::update_many()
                        .col_expr(message::Column::IsCompressed, Expr::value(true))
                        .filter(cond)
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    let event = UsageEvent {
                        tenant_id: task.tenant_id,
                        user_id: None,
                        chat_id: task.chat_id,
                        turn_id: None,
                        request_id: task.system_request_id,
                        effective_model: model_id.clone(),
                        selected_model: model_id.clone(),
                        terminal_state: "completed".to_owned(),
                        billing_outcome: "system_task".to_owned(),
                        usage: Some(UsageTokens {
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cache_read_input_tokens: usage.cache_read_input_tokens,
                            cache_write_input_tokens: usage.cache_write_input_tokens,
                            reasoning_tokens: usage.reasoning_tokens,
                        }),
                        actual_credits_micro: 0,
                        settlement_method: "none".to_owned(),
                        policy_version_applied: 0,
                        web_search_calls: 0,
                        code_interpreter_calls: 0,
                        file_search_calls: 0,
                        timestamp: now,
                        requester_type: "system".to_owned(),
                        dedupe_key: UsageEvent::system_task_dedupe_key(
                            task.tenant_id,
                            THREAD_SUMMARY_TASK,
                            task.system_request_id,
                        ),
                        system_task_type: Some(THREAD_SUMMARY_TASK.to_owned()),
                    };
                    let wake = outbox
                        .enqueue_json(
                            tx,
                            &outbox.queues.queue_name,
                            task.tenant_id,
                            PAYLOAD_USAGE,
                            &event,
                        )
                        .await?;
                    Ok(Some(vec![wake]))
                })
            })
            .await;
        match res {
            Ok(Some(w)) => {
                fire(w);
                SummaryResult::Done
            }
            Ok(None) => SummaryResult::Done,
            Err(e) => SummaryResult::Retry(e.to_string()),
        }
    }
}

fn key_le(key: (OffsetDateTime, Uuid)) -> Condition {
    Condition::any()
        .add(message::Column::CreatedAt.lt(key.0))
        .add(
            Condition::all()
                .add(message::Column::CreatedAt.eq(key.0))
                .add(message::Column::Id.lte(key.1)),
        )
}

fn key_gt(key: (OffsetDateTime, Uuid)) -> Condition {
    Condition::any()
        .add(message::Column::CreatedAt.gt(key.0))
        .add(
            Condition::all()
                .add(message::Column::CreatedAt.eq(key.0))
                .add(message::Column::Id.gt(key.1)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_summary_variants() {
        assert_eq!(
            parse_summary("<analysis>think</analysis>\n<summary>\nA\n\n\n\nB\n</summary>"),
            "A\n\nB"
        );
        assert_eq!(parse_summary("plain text"), "plain text");
        assert_eq!(parse_summary("<analysis>unterminated"), "");
        assert_eq!(parse_summary("<summary>no end"), "");
    }

    #[test]
    fn prompt_shapes() {
        let msgs = vec![
            ("user".to_owned(), "hello there".to_owned()),
            ("assistant".to_owned(), "hi".to_owned()),
        ];
        let p = build_user_prompt(None, &msgs, 5);
        assert!(p.starts_with("Summarize the following conversation:"));
        assert!(p.contains("User: hello..."));
        assert!(p.contains("\n\nAssistant: hi"));
        assert!(p.ends_with("Respond with an <analysis> block followed by a <summary> block."));
        let p = build_user_prompt(Some("old"), &msgs, 0);
        assert!(p.contains("<existing_summary>\nold\n</existing_summary>"));
        assert!(p.contains("New messages to incorporate:"));
        assert!(p.contains("User: hello there"));
    }
}
