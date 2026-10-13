//! Thread summary execution (DESIGN §3.6 "Thread Summary Update").

use std::sync::Arc;

use chrono::{DateTime, Utc};
use mini_chat_sdk::{UsageEvent, UsageTokens, system_task_dedupe_key};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use serde_json::json;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use toolkit_security::constants::DEFAULT_SUBJECT_ID;
use uuid::Uuid;

use super::finalize::ThreadSummaryPayload;
use super::{AppServices, now, policy};
use crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT;
use crate::domain::error::DomainError;
use crate::infra::db::entity::{messages, thread_summaries};
use crate::infra::llm::transport::resolve_provider;
use crate::infra::llm::{self, InputMessage, LlmRequest};
use crate::infra::outbox::Wakes;

pub const OPENING_NEW: &str = "Summarize the following conversation:";
pub const OPENING_MERGE: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";
pub const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

/// Outcome of a summary handler attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryOutcome {
    Ok(&'static str),
    Retry(&'static str),
    Reject(String),
}

/// Builds the user prompt of the summary request.
#[must_use]
pub fn build_prompt(existing: Option<&str>, msgs: &[(String, String)], content_limit: usize) -> String {
    let mut p = String::new();
    match existing {
        Some(s) => {
            p.push_str(OPENING_MERGE);
            p.push_str("\n\n<existing_summary>\n");
            p.push_str(s);
            p.push_str("\n</existing_summary>\n\nNew messages to incorporate:\n\n");
        }
        None => {
            p.push_str(OPENING_NEW);
            p.push_str("\n\n");
        }
    }
    let entries: Vec<String> = msgs
        .iter()
        .filter(|(role, _)| role != "system")
        .map(|(role, content)| {
            let who = if role == "assistant" { "Assistant" } else { "User" };
            let text = if content_limit > 0 && content.chars().count() > content_limit {
                let mut t: String = content.chars().take(content_limit).collect();
                t.push_str("...");
                t
            } else {
                content.clone()
            };
            format!("{who}: {text}")
        })
        .collect();
    p.push_str(&entries.join("\n\n"));
    p.push_str("\n\n");
    p.push_str(ANALYSIS_INSTRUCTION);
    p
}

/// Extracts the stored summary from a model response.
#[must_use]
pub fn parse_summary(response: &str) -> String {
    let mut text = response.to_owned();
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

fn after(frontier: Option<(DateTime<Utc>, Uuid)>) -> Condition {
    match frontier {
        None => Condition::all(),
        Some((ts, id)) => Condition::any().add(messages::Column::CreatedAt.gt(ts)).add(
            Condition::all()
                .add(messages::Column::CreatedAt.eq(ts))
                .add(messages::Column::Id.gt(id)),
        ),
    }
}

fn up_to(ts: DateTime<Utc>, id: Uuid) -> Condition {
    Condition::any().add(messages::Column::CreatedAt.lt(ts)).add(
        Condition::all()
            .add(messages::Column::CreatedAt.eq(ts))
            .add(messages::Column::Id.lte(id)),
    )
}

async fn range_messages(
    runner: &impl DBRunner,
    p: &ThreadSummaryPayload,
    base: Option<(DateTime<Utc>, Uuid)>,
) -> Result<Vec<messages::Model>, DomainError> {
    Ok(messages::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(p.tenant_id))
        .filter(
            Condition::all()
                .add(messages::Column::ChatId.eq(p.chat_id))
                .add(messages::Column::DeletedAt.is_null())
                .add(messages::Column::IsCompressed.eq(false))
                .add(after(base))
                .add(up_to(p.frozen_target_created_at, p.frozen_target_message_id)),
        )
        .order_by(messages::Column::CreatedAt, sea_orm::Order::Asc)
        .order_by(messages::Column::Id, sea_orm::Order::Asc)
        .all(runner)
        .await?)
}

impl AppServices {
    /// Executes one thread-summary task delivery.
    #[allow(clippy::too_many_lines)]
    pub async fn run_thread_summary(self: &Arc<Self>, p: ThreadSummaryPayload, attempts: i16) -> SummaryOutcome {
        let cfgw = &self.cfg.thread_summary_worker;
        let last_attempt = i64::from(attempts) + 1 >= i64::from(cfgw.max_attempts);
        let retry = |why: &'static str| {
            if last_attempt {
                SummaryOutcome::Reject(format!("thread summary: max attempts reached ({why})"))
            } else {
                SummaryOutcome::Retry(why)
            }
        };
        let snapshot = match policy::current_snapshot(self.policy.as_ref(), DEFAULT_SUBJECT_ID).await {
            Ok(s) => s,
            Err(_) => return retry("retry"),
        };
        let Some(model) = snapshot.enabled_model(cfgw.summary_model()).cloned() else {
            tracing::error!(model = cfgw.summary_model(), "thread summary model missing or disabled");
            return SummaryOutcome::Reject("model_unavailable".to_owned());
        };
        let Ok(conn) = self.conn() else {
            return retry("retry");
        };
        let current = match super::stream::load_summary(&conn, p.tenant_id, p.chat_id).await {
            Ok(c) => c,
            Err(_) => return retry("retry"),
        };
        let base = p.base_frontier_created_at.zip(p.base_frontier_message_id);
        let cur = current
            .as_ref()
            .map(|c| (c.summarized_up_to_created_at, c.summarized_up_to_message_id));
        if cur != base {
            if base.is_some() && cur.is_none() {
                return SummaryOutcome::Ok("base_missing");
            }
            return SummaryOutcome::Ok("cas_conflict");
        }
        let msgs = match range_messages(&conn, &p, base).await {
            Ok(m) => m,
            Err(_) => return retry("retry"),
        };
        if msgs.is_empty() {
            return SummaryOutcome::Ok("not_needed");
        }
        drop(conn);
        let system_prompt = if !model.thread_summary_prompt.trim().is_empty() {
            model.thread_summary_prompt.clone()
        } else if !cfgw.summary_system_prompt.trim().is_empty() {
            cfgw.summary_system_prompt.clone()
        } else {
            DEFAULT_SUMMARY_SYSTEM_PROMPT.to_owned()
        };
        let mut pairs: Vec<(String, String)> = msgs.iter().map(|m| (m.role.clone(), m.content.clone())).collect();
        // Fit the prompt into the summary model's input budget.
        if model.context_window > 0 {
            let mut budget = i64::from(model.context_window) - i64::from(model.max_output_tokens);
            if model.max_input_tokens > 0 {
                budget = budget.min(i64::from(model.max_input_tokens));
            }
            let bpt = i64::from(model.estimation_budgets.bytes_per_token_conservative.max(1));
            loop {
                let prompt = build_prompt(current.as_ref().map(|c| c.summary_text.as_str()), &pairs, cfgw.message_content_limit);
                let size = i64::try_from(prompt.len() + system_prompt.len()).unwrap_or(i64::MAX) / bpt;
                if size <= budget || pairs.len() <= 2 {
                    break;
                }
                let drop_n = pairs.len().div_ceil(5).min(pairs.len() - 2);
                pairs.drain(..drop_n);
            }
        }
        let prompt = build_prompt(current.as_ref().map(|c| c.summary_text.as_str()), &pairs, cfgw.message_content_limit);
        let Some(provider) = resolve_provider(&self.cfg, &model.provider_id, p.tenant_id) else {
            return retry("retry");
        };
        let mut metadata = serde_json::Map::new();
        metadata.insert("tenant_id".into(), json!(p.tenant_id.to_string()));
        metadata.insert("user_id".into(), json!(DEFAULT_SUBJECT_ID.to_string()));
        metadata.insert("chat_id".into(), json!(p.chat_id.to_string()));
        metadata.insert("request_type".into(), json!("summary"));
        metadata.insert("feature".into(), json!("none"));
        let req = LlmRequest {
            provider_model_id: if model.provider_model_id.is_empty() {
                model.id.clone()
            } else {
                model.provider_model_id.clone()
            },
            instructions: system_prompt,
            input: vec![InputMessage::text("user", prompt)],
            max_output_tokens: model.max_output_tokens.max(1),
            tools: Vec::new(),
            max_tool_calls: model.max_tool_calls,
            user: llm::provider_user(p.tenant_id, DEFAULT_SUBJECT_ID),
            metadata,
            api_params: model.general_config.api_params.clone(),
            stream: false,
        };
        let body = llm::build_body(provider.kind, &req);
        let http_req = http::Request::builder()
            .method("POST")
            .uri(provider.chat_uri(&req.provider_model_id))
            .header("content-type", "application/json")
            .body(oagw_sdk::Body::Bytes(bytes::Bytes::from(serde_json::to_vec(&body).unwrap_or_default())));
        let Ok(http_req) = http_req else {
            return retry("provider_error");
        };
        let resp = match self.transport.send(http_req).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, chat_id = %p.chat_id, "thread summary call failed");
                return retry("provider_error");
            }
        };
        let status = resp.status();
        let bytes = resp.into_body().into_bytes().await.unwrap_or_default();
        if !status.is_success() {
            tracing::warn!(status = status.as_u16(), chat_id = %p.chat_id, "thread summary provider error");
            return retry("provider_error");
        }
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        let (text, usage) = llm::parse_completion(provider.kind, &v);
        let summary = parse_summary(&text);
        if summary.is_empty() {
            return retry("empty_summary");
        }
        let u = usage.unwrap_or_default();
        let diff = u.output_tokens - u.reasoning_tokens;
        let token_estimate = if diff > 0 {
            diff
        } else {
            i64::try_from(summary.len().div_ceil(4)).unwrap_or(i64::MAX)
        };
        match self.commit_summary(p.clone(), base, summary, token_estimate, &model.id, u).await {
            Ok(r) => SummaryOutcome::Ok(r),
            Err(e) => {
                tracing::warn!(error = %e, "thread summary commit failed");
                retry("retry")
            }
        }
    }

    async fn commit_summary(
        &self,
        p: ThreadSummaryPayload,
        base: Option<(DateTime<Utc>, Uuid)>,
        summary: String,
        token_estimate: i64,
        model_id: &str,
        usage: UsageTokens,
    ) -> Result<&'static str, DomainError> {
        let outbox = Arc::clone(&self.outbox);
        let model_id = model_id.to_owned();
        let (result, wakes) = self
            .db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    crate::domain::service::lock_for_write(tx).await?;
                    let scope = AccessScope::for_tenant(p.tenant_id);
                    let target = messages::Entity::find()
                        .secure()
                        .scope_with(&scope)
                        .filter(
                            Condition::all()
                                .add(messages::Column::Id.eq(p.frozen_target_message_id))
                                .add(messages::Column::DeletedAt.is_null()),
                        )
                        .one(tx)
                        .await?;
                    if target.is_none() {
                        return Ok::<_, DomainError>(("frontier_deleted", Wakes::default()));
                    }
                    let ts = now();
                    let estimate = i32::try_from(token_estimate).unwrap_or(i32::MAX);
                    match base {
                        None => {
                            let am = thread_summaries::ActiveModel {
                                id: sea_orm::Set(Uuid::new_v4()),
                                tenant_id: sea_orm::Set(p.tenant_id),
                                chat_id: sea_orm::Set(p.chat_id),
                                summary_text: sea_orm::Set(summary.clone()),
                                summarized_up_to_created_at: sea_orm::Set(p.frozen_target_created_at),
                                summarized_up_to_message_id: sea_orm::Set(p.frozen_target_message_id),
                                token_estimate: sea_orm::Set(estimate),
                                created_at: sea_orm::Set(ts),
                                updated_at: sea_orm::Set(ts),
                            };
                            match thread_summaries::Entity::insert(am)
                                .secure()
                                .scope_unchecked(&scope)?
                                .exec(tx)
                                .await
                            {
                                Ok(_) => {}
                                Err(e) => {
                                    let e = DomainError::from(e);
                                    if matches!(e, DomainError::UniqueViolation) {
                                        return Ok(("cas_conflict", Wakes::default()));
                                    }
                                    return Err(e);
                                }
                            }
                        }
                        Some((bts, bid)) => {
                            let rows = thread_summaries::Entity::update_many()
                                .col_expr(thread_summaries::Column::SummaryText, Expr::value(summary.clone()))
                                .col_expr(
                                    thread_summaries::Column::SummarizedUpToCreatedAt,
                                    Expr::value(p.frozen_target_created_at),
                                )
                                .col_expr(
                                    thread_summaries::Column::SummarizedUpToMessageId,
                                    Expr::value(p.frozen_target_message_id),
                                )
                                .col_expr(thread_summaries::Column::TokenEstimate, Expr::value(estimate))
                                .col_expr(thread_summaries::Column::UpdatedAt, Expr::value(ts))
                                .filter(
                                    Condition::all()
                                        .add(thread_summaries::Column::ChatId.eq(p.chat_id))
                                        .add(thread_summaries::Column::SummarizedUpToCreatedAt.eq(bts))
                                        .add(thread_summaries::Column::SummarizedUpToMessageId.eq(bid)),
                                )
                                .secure()
                                .scope_with(&scope)
                                .exec(tx)
                                .await?
                                .rows_affected;
                            if rows == 0 {
                                return Ok(("cas_conflict", Wakes::default()));
                            }
                        }
                    }
                    messages::Entity::update_many()
                        .col_expr(messages::Column::IsCompressed, Expr::value(true))
                        .filter(
                            Condition::all()
                                .add(messages::Column::ChatId.eq(p.chat_id))
                                .add(messages::Column::DeletedAt.is_null())
                                .add(after(base))
                                .add(up_to(p.frozen_target_created_at, p.frozen_target_message_id)),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    let event = UsageEvent {
                        tenant_id: p.tenant_id,
                        user_id: None,
                        chat_id: p.chat_id,
                        turn_id: None,
                        request_id: p.system_request_id,
                        effective_model: model_id.clone(),
                        selected_model: model_id.clone(),
                        terminal_state: "completed".to_owned(),
                        billing_outcome: "system_task".to_owned(),
                        usage: Some(usage),
                        actual_credits_micro: 0,
                        settlement_method: "none".to_owned(),
                        policy_version_applied: 0,
                        web_search_calls: 0,
                        code_interpreter_calls: 0,
                        file_search_calls: 0,
                        timestamp: time::OffsetDateTime::now_utc(),
                        requester_type: "system".to_owned(),
                        dedupe_key: system_task_dedupe_key(p.tenant_id, "thread_summary_update", p.system_request_id),
                        system_task_type: Some("thread_summary_update".to_owned()),
                    };
                    let mut w = Wakes::default();
                    w.push(outbox.usage(tx, p.tenant_id, &event).await?);
                    Ok(("success", w))
                })
            })
            .await?;
        wakes.fire();
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_entries_and_existing_summary() {
        let msgs = vec![
            ("user".to_owned(), "hello".to_owned()),
            ("assistant".to_owned(), "x".repeat(10)),
        ];
        let p = build_prompt(None, &msgs, 5);
        assert!(p.starts_with(OPENING_NEW));
        assert!(p.contains("User: hello"));
        assert!(p.contains("Assistant: xxxxx..."));
        assert!(p.ends_with(ANALYSIS_INSTRUCTION));
        let p = build_prompt(Some("old"), &msgs, 0);
        assert!(p.contains("<existing_summary>\nold\n</existing_summary>"));
        assert!(p.contains("New messages to incorporate:"));
    }

    #[test]
    fn parses_summary_blocks() {
        assert_eq!(parse_summary("<analysis>think</analysis>\n<summary>\nA\n\n\n\nB\n</summary>"), "A\n\nB");
        assert_eq!(parse_summary("plain summary"), "plain summary");
        assert_eq!(parse_summary("<analysis>unterminated"), "");
        assert_eq!(parse_summary("<summary></summary>"), "");
    }
}
