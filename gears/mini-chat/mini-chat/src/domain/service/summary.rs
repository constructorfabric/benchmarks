//! Thread summary outbox task (DESIGN §3.6 "Thread Summary Update", B.5.5).

use crate::infra::db::WriteTransaction;
use std::sync::Arc;

use mini_chat_sdk::{ModelCatalogEntry, UsageEvent};
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QueryOrder};
use serde_json::Map;
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use uuid::Uuid;

use super::chats::tenant_scope;
use super::finalize::ThreadSummaryPayload;
use super::stream::load_summary;
use super::{Core, SYSTEM_SUBJECT_ID, now, provider_user_field};
use crate::domain::error::DomainError;
use crate::infra::db::entities::{message, thread_summary};
use crate::infra::llm::responses::{ChatRequest, InputMessage, InputRole, ToolSet, complete_chat};
use crate::infra::outbox::QueueKind;

pub const OPENING_NEW: &str = "Summarize the following conversation:";
pub const OPENING_MERGE: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";
pub const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

/// Handler outcome (mapped to the outbox `MessageResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOutcome {
    Ok,
    Retry(String),
    Reject(String),
}

/// Extracts the stored summary text from the model output.
#[must_use]
pub fn parse_summary_output(raw: &str) -> String {
    let mut text = raw.to_owned();
    while let (Some(s), Some(e)) = (text.find("<analysis>"), text.find("</analysis>")) {
        if e < s {
            break;
        }
        text.replace_range(s..e + "</analysis>".len(), "");
    }
    let inner = match (text.find("<summary>"), text.find("</summary>")) {
        (Some(s), Some(e)) if e > s => text[s + "<summary>".len()..e].to_owned(),
        _ => {
            if text.contains("<analysis") || text.contains("<summary") {
                return String::new();
            }
            text
        }
    };
    // Collapse runs of blank lines.
    let mut out = String::new();
    let mut blank = 0;
    for line in inner.trim().lines() {
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

/// Builds the summary user prompt.
#[must_use]
pub fn build_summary_prompt(
    existing: Option<&str>,
    messages: &[(String, String)],
    content_limit: usize,
) -> String {
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
    let entries: Vec<String> = messages
        .iter()
        .map(|(role, content)| {
            let label = if role == "assistant" {
                "Assistant"
            } else {
                "User"
            };
            let c = if content_limit > 0 && content.chars().count() > content_limit {
                format!(
                    "{}...",
                    content.chars().take(content_limit).collect::<String>()
                )
            } else {
                content.clone()
            };
            format!("{label}: {c}")
        })
        .collect();
    p.push_str(&entries.join("\n\n"));
    p.push_str("\n\n");
    p.push_str(ANALYSIS_INSTRUCTION);
    p
}

impl Core {
    fn summary_system_prompt(&self, model: &ModelCatalogEntry) -> String {
        if !model.thread_summary_prompt.trim().is_empty() {
            return model.thread_summary_prompt.clone();
        }
        if !self
            .cfg
            .thread_summary_worker
            .summary_system_prompt
            .trim()
            .is_empty()
        {
            return self.cfg.thread_summary_worker.summary_system_prompt.clone();
        }
        crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT.to_owned()
    }

    /// Processes one thread-summary task.
    #[allow(
        clippy::cognitive_complexity,
        clippy::too_many_lines,
        reason = "load, fit-to-budget, provider call and CAS commit form one task with uniform retry handling"
    )]
    pub async fn process_thread_summary(
        self: &Arc<Self>,
        p: &ThreadSummaryPayload,
        delivery: u32,
    ) -> TaskOutcome {
        let max = self.cfg.thread_summary_worker.max_attempts;
        let retry = |why: String| {
            if delivery >= max {
                TaskOutcome::Reject(why)
            } else {
                TaskOutcome::Retry(why)
            }
        };
        let snapshot = match self.policy.current_snapshot(SYSTEM_SUBJECT_ID).await {
            Ok(s) => s,
            Err(e) => return retry(e.to_string()),
        };
        let model_id = self.cfg.thread_summary_worker.effective_model_id();
        let Some(model) = snapshot.find_enabled_model(model_id).cloned() else {
            tracing::error!(model = %model_id, "mini-chat: summary model missing or disabled");
            return TaskOutcome::Reject("summary model unavailable".to_owned());
        };
        let Ok(conn) = self.db.conn() else {
            return retry("db".to_owned());
        };
        let summary = match load_summary(&conn, p.tenant_id, p.chat_id).await {
            Ok(s) => s,
            Err(e) => return retry(e.to_string()),
        };
        let base = match (p.base_frontier_created_at, p.base_frontier_message_id) {
            (Some(t), Some(id)) => Some((t, id)),
            _ => None,
        };
        let current = summary
            .as_ref()
            .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        if current != base {
            // Another commit advanced the frontier, or the base summary is gone.
            return TaskOutcome::Ok;
        }
        let target = (p.frozen_target_created_at, p.frozen_target_message_id);
        let msgs = match load_range(&conn, p.tenant_id, p.chat_id, base, target).await {
            Ok(m) => m,
            Err(e) => return retry(e.to_string()),
        };
        if msgs.is_empty() {
            return TaskOutcome::Ok;
        }
        let mut entries: Vec<(String, String)> = msgs
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| (m.role.clone(), m.content.clone()))
            .collect();
        let system_prompt = self.summary_system_prompt(&model);
        let limit = self.cfg.thread_summary_worker.message_content_limit;
        let existing = summary.as_ref().map(|s| s.summary_text.clone());
        // Fit the prompt to the summary model's input budget.
        if model.context_window > 0 {
            let mut budget = i64::from(model.context_window) - i64::from(model.max_output_tokens);
            if model.max_input_tokens > 0 {
                budget = budget.min(i64::from(model.max_input_tokens));
            }
            let bpt = i64::from(model.estimation_budgets.bytes_per_token_conservative.max(1));
            loop {
                let prompt = build_summary_prompt(existing.as_deref(), &entries, limit);
                let tokens = i64::try_from(prompt.len() + system_prompt.len())
                    .unwrap_or(i64::MAX)
                    .div_euclid(bpt);
                if tokens <= budget || entries.len() <= 2 {
                    break;
                }
                let drop_n = entries.len().div_ceil(5).min(entries.len() - 2);
                entries.drain(..drop_n);
            }
        }
        let prompt = build_summary_prompt(existing.as_deref(), &entries, limit);
        let provider = match self.providers.resolve(&model.provider_id, p.tenant_id) {
            Ok(pr) => pr,
            Err(e) => return retry(e.to_string()),
        };
        let mut metadata = Map::new();
        metadata.insert("tenant_id".into(), p.tenant_id.to_string().into());
        metadata.insert("user_id".into(), SYSTEM_SUBJECT_ID.to_string().into());
        metadata.insert("chat_id".into(), p.chat_id.to_string().into());
        metadata.insert("request_type".into(), "summary".into());
        metadata.insert("feature".into(), "none".into());
        let req = ChatRequest {
            provider_model_id: model.provider_model_id.clone(),
            instructions: system_prompt,
            input: vec![InputMessage {
                role: InputRole::User,
                text: prompt,
                image_file_ids: Vec::new(),
            }],
            tools: ToolSet::default(),
            max_output_tokens: model.max_output_tokens,
            max_tool_calls: model.max_tool_calls,
            user: provider_user_field(p.tenant_id, SYSTEM_SUBJECT_ID),
            metadata,
            api_params: model.general_config.api_params.clone(),
            stream: false,
        };
        let ctx = self.system_ctx.get_or_tenant(p.tenant_id);
        let (raw, usage) = match complete_chat(self.transport.as_ref(), ctx, &provider, &req).await
        {
            Ok(r) => r,
            Err((_, msg)) => {
                tracing::warn!(error = %msg, chat_id = %p.chat_id, "mini-chat: summary provider call failed");
                return retry(msg);
            }
        };
        let text = parse_summary_output(&raw);
        if text.is_empty() {
            return retry("empty summary".to_owned());
        }
        let token_estimate = usage
            .map(|u| u.output_tokens - u.reasoning_tokens)
            .filter(|t| *t > 0)
            .unwrap_or_else(|| i64::try_from(text.len().div_ceil(4)).unwrap_or(i64::MAX));
        let core = Arc::clone(self);
        let payload = p.clone();
        let model_id = model.id.clone();
        let res = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    let scope = tenant_scope(payload.tenant_id);
                    let target_msg = message::Entity::find()
                        .filter(message::Column::Id.eq(payload.frozen_target_message_id))
                        .secure()
                        .scope_with(&scope)
                        .one(tx)
                        .await?;
                    if target_msg.is_none_or(|m| m.deleted_at.is_some()) {
                        return Ok(Wake::empty());
                    }
                    let current = load_summary(tx, payload.tenant_id, payload.chat_id).await?;
                    let cur = current
                        .as_ref()
                        .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
                    let base = match (
                        payload.base_frontier_created_at,
                        payload.base_frontier_message_id,
                    ) {
                        (Some(t), Some(id)) => Some((t, id)),
                        _ => None,
                    };
                    if cur != base {
                        return Ok(Wake::empty());
                    }
                    let ts = now();
                    let te = i32::try_from(token_estimate).unwrap_or(i32::MAX);
                    if let Some(s) = current {
                        thread_summary::Entity::update_many()
                            .col_expr(
                                thread_summary::Column::SummaryText,
                                Expr::value(text.clone()),
                            )
                            .col_expr(
                                thread_summary::Column::SummarizedUpToCreatedAt,
                                Expr::value(payload.frozen_target_created_at),
                            )
                            .col_expr(
                                thread_summary::Column::SummarizedUpToMessageId,
                                Expr::value(payload.frozen_target_message_id),
                            )
                            .col_expr(thread_summary::Column::TokenEstimate, Expr::value(te))
                            .col_expr(thread_summary::Column::UpdatedAt, Expr::value(ts))
                            .filter(Condition::all().add(thread_summary::Column::Id.eq(s.id)))
                            .secure()
                            .scope_with(&scope)
                            .exec(tx)
                            .await?;
                    } else {
                        let am = thread_summary::ActiveModel {
                            id: ActiveValue::Set(Uuid::new_v4()),
                            tenant_id: ActiveValue::Set(payload.tenant_id),
                            chat_id: ActiveValue::Set(payload.chat_id),
                            summary_text: ActiveValue::Set(text.clone()),
                            summarized_up_to_created_at: ActiveValue::Set(
                                payload.frozen_target_created_at,
                            ),
                            summarized_up_to_message_id: ActiveValue::Set(
                                payload.frozen_target_message_id,
                            ),
                            token_estimate: ActiveValue::Set(te),
                            created_at: ActiveValue::Set(ts),
                            updated_at: ActiveValue::Set(ts),
                        };
                        thread_summary::Entity::insert(am)
                            .secure()
                            .scope_unchecked(&scope)?
                            .exec(tx)
                            .await?;
                    }
                    let range = range_condition(
                        payload.chat_id,
                        base,
                        (
                            payload.frozen_target_created_at,
                            payload.frozen_target_message_id,
                        ),
                    );
                    message::Entity::update_many()
                        .col_expr(message::Column::IsCompressed, Expr::value(true))
                        .filter(range)
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    let ev = UsageEvent {
                        tenant_id: payload.tenant_id,
                        user_id: None,
                        chat_id: Some(payload.chat_id),
                        turn_id: None,
                        request_id: payload.system_request_id,
                        effective_model: model_id.clone(),
                        selected_model: model_id,
                        terminal_state: "completed".to_owned(),
                        billing_outcome: "system_task".to_owned(),
                        usage,
                        actual_credits_micro: 0,
                        settlement_method: "none".to_owned(),
                        policy_version_applied: 0,
                        web_search_calls: 0,
                        code_interpreter_calls: 0,
                        file_search_calls: 0,
                        timestamp: ts,
                        requester_type: "system".to_owned(),
                        dedupe_key: format!(
                            "{}/thread_summary_update/{}",
                            payload.tenant_id.simple(),
                            payload.system_request_id.simple()
                        ),
                        system_task_type: Some("thread_summary_update".to_owned()),
                    };
                    core.outbox
                        .enqueue(tx, QueueKind::Usage, payload.tenant_id, &ev)
                        .await
                })
            })
            .await;
        match res {
            Ok(w) => {
                w.fire();
                TaskOutcome::Ok
            }
            Err(e) => retry(e.to_string()),
        }
    }
}

fn range_condition(
    chat_id: Uuid,
    base: Option<(OffsetDateTime, Uuid)>,
    target: (OffsetDateTime, Uuid),
) -> Condition {
    let mut c = Condition::all()
        .add(message::Column::ChatId.eq(chat_id))
        .add(message::Column::DeletedAt.is_null())
        .add(message::Column::IsCompressed.eq(false))
        .add(
            Condition::any()
                .add(message::Column::CreatedAt.lt(target.0))
                .add(
                    Condition::all()
                        .add(message::Column::CreatedAt.eq(target.0))
                        .add(message::Column::Id.lte(target.1)),
                ),
        );
    if let Some((t, id)) = base {
        c = c.add(
            Condition::any().add(message::Column::CreatedAt.gt(t)).add(
                Condition::all()
                    .add(message::Column::CreatedAt.eq(t))
                    .add(message::Column::Id.gt(id)),
            ),
        );
    }
    c
}

async fn load_range(
    db: &impl DBRunner,
    tenant: Uuid,
    chat_id: Uuid,
    base: Option<(OffsetDateTime, Uuid)>,
    target: (OffsetDateTime, Uuid),
) -> Result<Vec<message::Model>, DomainError> {
    Ok(message::Entity::find()
        .filter(range_condition(chat_id, base, target))
        .order_by(message::Column::CreatedAt, Order::Asc)
        .order_by(message::Column::Id, Order::Asc)
        .secure()
        .scope_with(&tenant_scope(tenant))
        .all(db)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_strips_analysis_and_takes_summary() {
        let raw = "<analysis>thinking\n</analysis>\n<summary>\nA\n\n\n\nB\n</summary>";
        assert_eq!(parse_summary_output(raw), "A\n\nB");
        assert_eq!(parse_summary_output("plain text"), "plain text");
        assert_eq!(parse_summary_output("<summary>unterminated"), "");
    }

    #[test]
    fn prompt_shapes() {
        let msgs = vec![
            ("user".to_owned(), "hello".to_owned()),
            ("assistant".to_owned(), "x".repeat(10)),
        ];
        let p = build_summary_prompt(None, &msgs, 5);
        assert!(p.starts_with(OPENING_NEW));
        assert!(p.contains("User: hello\n\nAssistant: xxxxx..."));
        assert!(p.ends_with("Respond with an <analysis> block followed by a <summary> block."));
        let p2 = build_summary_prompt(Some("OLD"), &msgs, 0);
        assert!(p2.contains("<existing_summary>\nOLD\n</existing_summary>"));
        assert!(p2.contains("New messages to incorporate:"));
    }
}
