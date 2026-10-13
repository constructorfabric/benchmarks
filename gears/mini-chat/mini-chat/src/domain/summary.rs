//! Thread summary execution (DESIGN §3.6 "Thread Summary Update").

use std::sync::Arc;

use mini_chat_sdk::usage::system_task_dedupe_key;
use mini_chat_sdk::{ModelCatalogEntry, UsageEvent, UsageTokens};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use toolkit_db::secure::{SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use uuid::Uuid;

use super::app::{App, fire, now, tenant_scope};
use super::quota::estimate_item_tokens;
use crate::infra::db::entity::{messages, thread_summaries};
use crate::infra::llm::client::complete;
use crate::infra::llm::{ChatRequest, InputMessage, RequestTools, provider_user};
use crate::infra::outbox::ThreadSummaryPayload;

pub const MERGE_INTRO: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";

pub const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

/// Outcome of one handler attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryResult {
    Ok(&'static str),
    Retry(String),
    Reject(String),
}

/// Builds the user prompt of the summary request.
#[must_use]
pub fn build_prompt(existing: Option<&str>, msgs: &[(String, String)], content_limit: usize) -> String {
    let mut out = String::new();
    match existing {
        Some(s) => {
            out.push_str(MERGE_INTRO);
            out.push_str("\n\n<existing_summary>\n");
            out.push_str(s);
            out.push_str("\n</existing_summary>\n\nNew messages to incorporate:\n\n");
        }
        None => out.push_str("Summarize the following conversation:\n\n"),
    }
    let entries: Vec<String> = msgs
        .iter()
        .map(|(role, content)| {
            let label = if role == "assistant" { "Assistant" } else { "User" };
            let text = if content_limit > 0 && content.chars().count() > content_limit {
                format!("{}...", content.chars().take(content_limit).collect::<String>())
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

/// Extracts the stored summary from the model output.
#[must_use]
pub fn parse_summary(text: &str) -> String {
    let mut t = text.to_owned();
    while let (Some(s), Some(e)) = (t.find("<analysis>"), t.find("</analysis>")) {
        if e < s {
            break;
        }
        t.replace_range(s..e + "</analysis>".len(), "");
    }
    let body = match (t.find("<summary>"), t.find("</summary>")) {
        (Some(s), Some(e)) if e > s => t[s + "<summary>".len()..e].to_owned(),
        _ => {
            if t.contains("<analysis") || t.contains("<summary") {
                return String::new();
            }
            t
        }
    };
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

/// Drops the oldest `ceil(n/5)` messages per step while the prompt is over budget, keeping at least two.
fn fit_prompt(model: &ModelCatalogEntry, system: &str, existing: Option<&str>, mut msgs: Vec<(String, String)>, limit: usize) -> Vec<(String, String)> {
    if model.context_window == 0 {
        return msgs;
    }
    let mut budget = i64::from(model.context_window) - i64::from(model.max_output_tokens);
    if model.max_input_tokens > 0 {
        budget = budget.min(i64::from(model.max_input_tokens));
    }
    loop {
        let prompt = build_prompt(existing, &msgs, limit);
        let est = estimate_item_tokens(system.len() + prompt.len(), &model.estimation_budgets);
        if est <= budget || msgs.len() <= 2 {
            return msgs;
        }
        let drop = msgs.len().div_ceil(5).min(msgs.len() - 2);
        msgs.drain(..drop.max(1));
    }
}

impl App {
    /// Runs one thread-summary work item.
    #[allow(clippy::too_many_lines)]
    pub async fn run_thread_summary(self: &Arc<Self>, p: ThreadSummaryPayload, attempt: u32) -> SummaryResult {
        let cfg = &self.cfg.thread_summary_worker;
        let retry = |m: String| {
            if attempt >= cfg.max_attempts {
                SummaryResult::Reject(m)
            } else {
                SummaryResult::Retry(m)
            }
        };
        let system_user = toolkit_security::constants::DEFAULT_SUBJECT_ID;
        let snapshot = match self.policy.current_snapshot(system_user).await {
            Ok(s) => s,
            Err(e) => return retry(e.to_string()),
        };
        let Some(model) = snapshot.find_enabled(cfg.summary_model()).cloned() else {
            tracing::error!(model = %cfg.summary_model(), "summary model missing or disabled");
            return SummaryResult::Reject("model_unavailable".into());
        };
        let scope = tenant_scope(p.tenant_id);
        let Ok(conn) = self.db.conn() else { return retry("db".into()) };
        let stored = match thread_summaries::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(thread_summaries::Column::ChatId.eq(p.chat_id)))
            .one(&conn)
            .await
        {
            Ok(s) => s,
            Err(e) => return retry(e.to_string()),
        };
        let base = p.base_frontier_created_at.zip(p.base_frontier_message_id);
        match (&base, &stored) {
            (Some(_), None) => return SummaryResult::Ok("base_missing"),
            (Some((bc, bm)), Some(s)) if (s.summarized_up_to_created_at, s.summarized_up_to_message_id) != (*bc, *bm) => {
                return SummaryResult::Ok("cas_conflict");
            }
            (None, Some(_)) => return SummaryResult::Ok("cas_conflict"),
            _ => {}
        }
        let mut cond = Condition::all()
            .add(messages::Column::ChatId.eq(p.chat_id))
            .add(messages::Column::DeletedAt.is_null())
            .add(messages::Column::IsCompressed.eq(false))
            .add(
                Condition::any()
                    .add(messages::Column::CreatedAt.lt(p.frozen_target_created_at))
                    .add(
                        Condition::all()
                            .add(messages::Column::CreatedAt.eq(p.frozen_target_created_at))
                            .add(messages::Column::Id.lte(p.frozen_target_message_id)),
                    ),
            );
        if let Some((bc, bm)) = base {
            cond = cond.add(
                Condition::any()
                    .add(messages::Column::CreatedAt.gt(bc))
                    .add(Condition::all().add(messages::Column::CreatedAt.eq(bc)).add(messages::Column::Id.gt(bm))),
            );
        }
        let rows = match messages::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(cond)
            .order_by(messages::Column::CreatedAt, Order::Asc)
            .order_by(messages::Column::Id, Order::Asc)
            .all(&conn)
            .await
        {
            Ok(r) => r,
            Err(e) => return retry(e.to_string()),
        };
        drop(conn);
        if rows.is_empty() {
            return SummaryResult::Ok("nothing_to_summarize");
        }
        let range_ids: Vec<Uuid> = rows.iter().map(|m| m.id).collect();
        let system_prompt = if !model.thread_summary_prompt.trim().is_empty() {
            model.thread_summary_prompt.clone()
        } else if !cfg.summary_system_prompt.trim().is_empty() {
            cfg.summary_system_prompt.clone()
        } else {
            crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT.to_owned()
        };
        let msgs: Vec<(String, String)> = rows
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| (m.role.clone(), m.content.clone()))
            .collect();
        let existing = stored.as_ref().map(|s| s.summary_text.clone());
        let mut msgs = fit_prompt(&model, &system_prompt, existing.as_deref(), msgs, cfg.message_content_limit);
        let Some(provider) = self.resolver.resolve(&model.provider_id, p.tenant_id) else {
            return retry(format!("provider '{}' is not configured", model.provider_id));
        };
        let mut metadata = serde_json::Map::new();
        metadata.insert("tenant_id".into(), p.tenant_id.to_string().into());
        metadata.insert("user_id".into(), system_user.to_string().into());
        metadata.insert("chat_id".into(), p.chat_id.to_string().into());
        metadata.insert("request_type".into(), "summary".into());
        metadata.insert("feature".into(), "none".into());
        let mut completion = None;
        for _ptl in 0..3 {
            let req = ChatRequest {
                model: model.provider_model_id.clone(),
                instructions: system_prompt.clone(),
                input: vec![InputMessage {
                    role: "user",
                    text: build_prompt(existing.as_deref(), &msgs, cfg.message_content_limit),
                    image_file_ids: Vec::new(),
                }],
                tools: RequestTools::default(),
                max_output_tokens: model.max_output_tokens,
                user: provider_user(p.tenant_id, system_user),
                metadata: metadata.clone(),
                api_params: model.general_config.api_params.clone(),
                stream: false,
            };
            match complete(&self.transport, &provider, &req).await {
                Ok(c) => {
                    completion = Some(c);
                    break;
                }
                Err((_, msg)) if msg.to_lowercase().contains("context") && msgs.len() > 2 => {
                    let drop = msgs.len().div_ceil(5).min(msgs.len() - 2).max(1);
                    msgs.drain(..drop);
                }
                Err((_, msg)) => {
                    tracing::warn!(chat_id = %p.chat_id, error = %msg, "summary call failed");
                    return retry(format!("provider_error: {msg}"));
                }
            }
        }
        let Some(completion) = completion else {
            return retry("provider_error: context length".into());
        };
        let summary = parse_summary(&completion.text);
        if summary.is_empty() {
            return retry("empty_summary".into());
        }
        let usage = completion.usage.unwrap_or_default();
        let net = usage.output_tokens - usage.reasoning_tokens;
        let token_estimate = if net > 0 {
            net
        } else {
            i64::try_from(summary.len().div_ceil(4)).unwrap_or(i64::MAX)
        };
        let app = Arc::clone(self);
        let model_id = model.id.clone();
        let payload = p.clone();
        let res = self
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let at = now();
                    let scope = tenant_scope(payload.tenant_id);
                    let target_live = messages::Entity::find()
                        .secure()
                        .scope_with(&scope)
                        .filter(
                            Condition::all()
                                .add(messages::Column::Id.eq(payload.frozen_target_message_id))
                                .add(messages::Column::DeletedAt.is_null()),
                        )
                        .one(tx)
                        .await?;
                    if target_live.is_none() {
                        return Ok((SummaryResult::Ok("frontier_deleted"), Vec::new()));
                    }
                    let est = i32::try_from(token_estimate).unwrap_or(i32::MAX);
                    match base {
                        None => {
                            let am = thread_summaries::ActiveModel {
                                id: ActiveValue::Set(Uuid::new_v4()),
                                tenant_id: ActiveValue::Set(payload.tenant_id),
                                chat_id: ActiveValue::Set(payload.chat_id),
                                summary_text: ActiveValue::Set(summary.clone()),
                                summarized_up_to_created_at: ActiveValue::Set(payload.frozen_target_created_at),
                                summarized_up_to_message_id: ActiveValue::Set(payload.frozen_target_message_id),
                                token_estimate: ActiveValue::Set(est),
                                created_at: ActiveValue::Set(at),
                                updated_at: ActiveValue::Set(at),
                            };
                            let r = thread_summaries::Entity::insert(am)
                                .secure()
                                .scope_unchecked(&scope)?
                                .on_conflict_raw(OnConflict::column(thread_summaries::Column::ChatId).do_nothing().to_owned())
                                .exec(tx)
                                .await;
                            match r {
                                Ok(_) => {}
                                Err(toolkit_db::secure::ScopeError::Db(sea_orm::DbErr::RecordNotInserted)) => {
                                    return Ok((SummaryResult::Ok("cas_conflict"), Vec::new()));
                                }
                                Err(e) => return Err(e.into()),
                            }
                        }
                        Some((bc, bm)) => {
                            let rows = thread_summaries::Entity::update_many()
                                .col_expr(thread_summaries::Column::SummaryText, Expr::value(summary.clone()))
                                .col_expr(
                                    thread_summaries::Column::SummarizedUpToCreatedAt,
                                    Expr::value(payload.frozen_target_created_at),
                                )
                                .col_expr(
                                    thread_summaries::Column::SummarizedUpToMessageId,
                                    Expr::value(payload.frozen_target_message_id),
                                )
                                .col_expr(thread_summaries::Column::TokenEstimate, Expr::value(est))
                                .col_expr(thread_summaries::Column::UpdatedAt, Expr::value(at))
                                .filter(
                                    Condition::all()
                                        .add(thread_summaries::Column::ChatId.eq(payload.chat_id))
                                        .add(thread_summaries::Column::SummarizedUpToCreatedAt.eq(bc))
                                        .add(thread_summaries::Column::SummarizedUpToMessageId.eq(bm)),
                                )
                                .secure()
                                .scope_with(&scope)
                                .exec(tx)
                                .await?
                                .rows_affected;
                            if rows == 0 {
                                return Ok((SummaryResult::Ok("cas_conflict"), Vec::new()));
                            }
                        }
                    }
                    messages::Entity::update_many()
                        .col_expr(messages::Column::IsCompressed, Expr::value(true))
                        .filter(Condition::all().add(messages::Column::Id.is_in(range_ids)))
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    let ev = UsageEvent {
                        tenant_id: payload.tenant_id,
                        user_id: None,
                        chat_id: payload.chat_id,
                        turn_id: None,
                        request_id: payload.system_request_id,
                        effective_model: model_id.clone(),
                        selected_model: model_id,
                        terminal_state: "completed".into(),
                        billing_outcome: "system_task".into(),
                        usage: Some(UsageTokens {
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cache_read_input_tokens: usage.cache_read_input_tokens,
                            cache_write_input_tokens: usage.cache_write_input_tokens,
                            reasoning_tokens: usage.reasoning_tokens,
                        }),
                        actual_credits_micro: 0,
                        settlement_method: "none".into(),
                        policy_version_applied: 0,
                        web_search_calls: 0,
                        code_interpreter_calls: 0,
                        file_search_calls: 0,
                        timestamp: at,
                        requester_type: "system".into(),
                        dedupe_key: system_task_dedupe_key(payload.tenant_id, "thread_summary_update", payload.system_request_id),
                        system_task_type: Some("thread_summary_update".into()),
                    };
                    let wake = app.outbox.usage(tx, &ev).await?;
                    Ok((SummaryResult::Ok("success"), vec![wake]))
                })
            })
            .await;
        match res {
            Ok((r, wakes)) => {
                fire(wakes);
                r
            }
            Err(e) => retry(format!("commit failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_and_parsing() {
        let p = build_prompt(None, &[("user".into(), "hello".into()), ("assistant".into(), "x".repeat(10))], 5);
        assert!(p.starts_with("Summarize the following conversation:"));
        assert!(p.contains("User: hello\n\nAssistant: xxxxx..."));
        assert!(p.ends_with("followed by a <summary> block."));
        let p = build_prompt(Some("old"), &[("user".into(), "q".into())], 0);
        assert!(p.contains("<existing_summary>\nold\n</existing_summary>"));
        assert_eq!(parse_summary("<analysis>think</analysis>\n<summary>\nA\n\n\n\nB\n</summary>"), "A\n\nB");
        assert_eq!(parse_summary("plain text"), "plain text");
        assert_eq!(parse_summary("<analysis>unterminated"), "");
    }
}
