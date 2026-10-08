//! Thread summary worker logic (DESIGN §3.6 "Thread Summary Update").

use mini_chat_sdk::UsageEvent;
use sea_orm::EntityTrait;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, Set};
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT;
use crate::domain::errors::{DomainError, DomainResult};
use crate::domain::finalize::{ThreadSummaryPayload, usage_tokens};
use crate::domain::state::AppState;
use crate::infra::db::entities::{chats, messages, thread_summaries};
use crate::infra::db::repo;
use crate::infra::llm::{ChatItem, ItemRole, LlmRequest, RequestMetadata, ToolsSpec, user_field};
use crate::infra::outbox::{Queue, Wakes};

pub const OPENING_NEW: &str = "Summarize the following conversation:";
pub const OPENING_MERGE: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";
pub const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

/// Outcome of one handler attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryOutcome {
    Done(&'static str),
    Retry(String),
    Reject(String),
}

/// Extract the stored summary from a model response.
#[must_use]
pub fn parse_summary(raw: &str) -> String {
    let mut s = raw.to_owned();
    while let (Some(a), Some(b)) = (s.find("<analysis>"), s.find("</analysis>")) {
        if b < a {
            break;
        }
        s.replace_range(a..b + "</analysis>".len(), "");
    }
    let body = match (s.find("<summary>"), s.find("</summary>")) {
        (Some(a), Some(b)) if b > a => s[a + "<summary>".len()..b].to_owned(),
        (Some(a), None) => s[a + "<summary>".len()..].to_owned(),
        _ => {
            if s.contains("<analysis") || s.contains("<summary") {
                return String::new();
            }
            s
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

fn truncate_content(s: &str, limit: usize) -> String {
    if limit == 0 || s.chars().count() <= limit {
        return s.to_owned();
    }
    let mut t: String = s.chars().take(limit).collect();
    t.push_str("...");
    t
}

/// Build the user prompt of the summary request.
#[must_use]
pub fn build_prompt(existing: Option<&str>, msgs: &[(String, String)], limit: usize) -> String {
    let mut p = String::new();
    match existing {
        Some(e) => {
            p.push_str(OPENING_MERGE);
            p.push_str("\n\n<existing_summary>\n");
            p.push_str(e);
            p.push_str("\n</existing_summary>\n\nNew messages to incorporate:\n\n");
        }
        None => {
            p.push_str(OPENING_NEW);
            p.push_str("\n\n");
        }
    }
    let entries: Vec<String> = msgs
        .iter()
        .map(|(role, content)| {
            let who = if role == "assistant" { "Assistant" } else { "User" };
            format!("{who}: {}", truncate_content(content, limit))
        })
        .collect();
    p.push_str(&entries.join("\n\n"));
    p.push_str("\n\n");
    p.push_str(ANALYSIS_INSTRUCTION);
    p
}

impl AppState {
    #[allow(clippy::too_many_lines)]
    pub async fn run_thread_summary(&self, p: &ThreadSummaryPayload) -> DomainResult<SummaryOutcome> {
        let scope = AccessScope::for_tenant(p.tenant_id);
        let conn = self.db.conn()?;
        let chat = chats::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(chats::Column::Id.eq(p.chat_id)))
            .one(&conn)
            .await?;
        let Some(chat) = chat else { return Ok(SummaryOutcome::Done("chat_missing")) };
        if chat.deleted_at.is_some() {
            return Ok(SummaryOutcome::Done("chat_deleted"));
        }
        let current = repo::find_summary(&conn, &scope, p.chat_id).await?;
        match (&current, p.base_frontier_message_id) {
            (Some(s), Some(base)) if s.summarized_up_to_message_id != base => {
                return Ok(SummaryOutcome::Done("cas_conflict"));
            }
            (None, Some(_)) => return Ok(SummaryOutcome::Done("base_missing")),
            (Some(_), None) => return Ok(SummaryOutcome::Done("cas_conflict")),
            _ => {}
        }
        let base = current.as_ref().map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        let target = (p.frozen_target_created_at, p.frozen_target_message_id);
        let all = repo::list_live_messages(&conn, &scope, p.chat_id).await?;
        let range: Vec<&messages::Model> = all
            .iter()
            .filter(|m| !m.is_compressed && m.role != "system")
            .filter(|m| base.is_none_or(|b| (m.created_at, m.id) > b))
            .filter(|m| (m.created_at, m.id) <= target)
            .collect();
        drop(conn);
        if range.is_empty() {
            return Ok(SummaryOutcome::Done("not_needed"));
        }
        let model_id = self.cfg.thread_summary_worker.effective_model_id().to_owned();
        let system_user = toolkit_security::constants::DEFAULT_SUBJECT_ID;
        let snap = match self.policy.current_snapshot(system_user).await {
            Ok(s) => s,
            Err(e) => return Ok(SummaryOutcome::Retry(format!("policy: {e}"))),
        };
        let Some(model) = snap.find_enabled_model(&model_id).cloned() else {
            tracing::error!(model = %model_id, "thread summary model is missing or disabled");
            return Ok(SummaryOutcome::Reject("model_unavailable".to_owned()));
        };
        let system = if model.thread_summary_prompt.trim().is_empty() {
            if self.cfg.thread_summary_worker.summary_system_prompt.trim().is_empty() {
                DEFAULT_SUMMARY_SYSTEM_PROMPT.to_owned()
            } else {
                self.cfg.thread_summary_worker.summary_system_prompt.clone()
            }
        } else {
            model.thread_summary_prompt.clone()
        };
        let mut entries: Vec<(String, String)> = range.iter().map(|m| (m.role.clone(), m.content.clone())).collect();
        let limit = self.cfg.thread_summary_worker.message_content_limit;
        let existing = current.as_ref().map(|s| s.summary_text.clone());
        let mut prompt = build_prompt(existing.as_deref(), &entries, limit);
        // Fit the prompt to the summary model's input budget.
        if model.context_window > 0 {
            let mut budget = i64::from(model.context_window) - i64::from(model.max_output_tokens);
            if model.max_input_tokens > 0 {
                budget = budget.min(i64::from(model.max_input_tokens));
            }
            let bpt = i64::from(model.estimation_budgets.bytes_per_token_conservative.max(1));
            let est = |s: &str, p: &str| i64::try_from(s.len() + p.len()).unwrap_or(i64::MAX) / bpt;
            while est(&system, &prompt) > budget && entries.len() > 2 {
                let drop_n = entries.len().div_ceil(5).min(entries.len() - 2);
                entries.drain(..drop_n);
                prompt = build_prompt(existing.as_deref(), &entries, limit);
            }
        }
        let target_route = match self
            .resolver
            .chat_target(&model.provider_id, chat.tenant_id, &model.provider_model_id)
        {
            Ok(t) => t,
            Err(e) => return Ok(SummaryOutcome::Retry(format!("provider: {e}"))),
        };
        let req = LlmRequest {
            model: if model.provider_model_id.is_empty() { model.id.clone() } else { model.provider_model_id.clone() },
            instructions: system,
            items: vec![ChatItem {
                role: ItemRole::User,
                text: prompt,
                images: Vec::new(),
            }],
            max_output_tokens: model.max_output_tokens.max(1),
            tools: ToolsSpec::default(),
            max_tool_calls: model.max_tool_calls,
            api_params: model.general_config.api_params.clone(),
            user: user_field(chat.tenant_id, system_user),
            metadata: RequestMetadata {
                tenant_id: chat.tenant_id.to_string(),
                user_id: system_user.to_string(),
                chat_id: chat.id.to_string(),
                request_type: "summary",
                feature: "none".to_owned(),
            },
            stream: false,
            extra_input: Vec::new(),
        };
        let (raw, usage) = match self.llm.complete(&target_route, &req).await {
            Ok(r) => r,
            Err(f) => {
                tracing::warn!(code = f.code, message = %f.message, "thread summary call failed");
                return Ok(SummaryOutcome::Retry("provider_error".to_owned()));
            }
        };
        let text = parse_summary(&raw);
        if text.is_empty() {
            return Ok(SummaryOutcome::Retry("empty_summary".to_owned()));
        }
        let u = usage.unwrap_or_default();
        let diff = u.output_tokens - u.reasoning_tokens;
        let token_estimate = if diff > 0 {
            diff
        } else {
            i64::try_from(text.len().div_ceil(4)).unwrap_or(i64::MAX)
        };
        let ids: Vec<Uuid> = range.iter().map(|m| m.id).collect();
        let outbox = self.outbox.clone();
        let p2 = p.clone();
        let model_id2 = model.id.clone();
        let res = self
            .write_tx(move |tx| {
                let outbox = outbox.clone();
                let p = p2.clone();
                let ids = ids.clone();
                let text = text.clone();
                let model_id = model_id2.clone();
                Box::pin(async move {
                    let scope = AccessScope::for_tenant(p.tenant_id);
                    let target_alive = repo::find_message(tx, &scope, p.chat_id, p.frozen_target_message_id)
                        .await?
                        .is_some();
                    if !target_alive {
                        return Ok((SummaryOutcome::Done("frontier_deleted"), Wakes::default()));
                    }
                    let now = repo::now();
                    let est = i32::try_from(token_estimate).unwrap_or(i32::MAX);
                    match p.base_frontier_message_id {
                        None => {
                            let am = thread_summaries::ActiveModel {
                                id: Set(Uuid::new_v4()),
                                tenant_id: Set(p.tenant_id),
                                chat_id: Set(p.chat_id),
                                summary_text: Set(text),
                                summarized_up_to_created_at: Set(p.frozen_target_created_at),
                                summarized_up_to_message_id: Set(p.frozen_target_message_id),
                                token_estimate: Set(est),
                                created_at: Set(now),
                                updated_at: Set(now),
                            };
                            match secure_insert::<thread_summaries::Entity>(am, &scope, tx).await {
                                Ok(_) => {}
                                Err(e) if e.is_unique_violation() => {
                                    return Ok((SummaryOutcome::Done("cas_conflict"), Wakes::default()));
                                }
                                Err(e) => return Err(e.into()),
                            }
                        }
                        Some(base) => {
                            let r = thread_summaries::Entity::update_many()
                                .secure()
                                .col_expr(thread_summaries::Column::SummaryText, Expr::value(text))
                                .col_expr(thread_summaries::Column::SummarizedUpToCreatedAt, Expr::value(p.frozen_target_created_at))
                                .col_expr(thread_summaries::Column::SummarizedUpToMessageId, Expr::value(p.frozen_target_message_id))
                                .col_expr(thread_summaries::Column::TokenEstimate, Expr::value(est))
                                .col_expr(thread_summaries::Column::UpdatedAt, Expr::value(now))
                                .filter(
                                    Condition::all()
                                        .add(thread_summaries::Column::ChatId.eq(p.chat_id))
                                        .add(thread_summaries::Column::SummarizedUpToMessageId.eq(base)),
                                )
                                .scope_with(&scope)
                                .exec(tx)
                                .await?;
                            if r.rows_affected == 0 {
                                return Ok((SummaryOutcome::Done("cas_conflict"), Wakes::default()));
                            }
                        }
                    }
                    repo::set_compressed_ids(tx, &scope, &ids).await?;
                    let ev = UsageEvent {
                        tenant_id: p.tenant_id,
                        user_id: None,
                        chat_id: p.chat_id,
                        turn_id: None,
                        request_id: p.system_request_id,
                        effective_model: model_id.clone(),
                        selected_model: model_id,
                        terminal_state: "completed".to_owned(),
                        billing_outcome: "system_task".to_owned(),
                        usage: Some(usage_tokens(&u)),
                        actual_credits_micro: 0,
                        settlement_method: "none".to_owned(),
                        policy_version_applied: 0,
                        web_search_calls: 0,
                        code_interpreter_calls: 0,
                        file_search_calls: 0,
                        timestamp: now,
                        requester_type: "system".to_owned(),
                        dedupe_key: format!(
                            "{}/thread_summary_update/{}",
                            p.tenant_id.simple(),
                            p.system_request_id.simple()
                        ),
                        system_task_type: Some("thread_summary_update".to_owned()),
                    };
                    let mut wakes = Wakes::default();
                    wakes.push(outbox.enqueue(tx, Queue::Usage, p.tenant_id, &ev).await?);
                    Ok((SummaryOutcome::Done("success"), wakes))
                })
            })
            .await;
        match res {
            Ok((o, w)) => {
                w.fire();
                Ok(o)
            }
            Err(e) => Ok(SummaryOutcome::Retry(format!("commit failed: {e}"))),
        }
    }
}

#[allow(dead_code)]
fn _unused(_: DomainError) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_variants() {
        assert_eq!(parse_summary("<analysis>x</analysis>\n<summary>\nA\n\n\n\nB\n</summary>"), "A\n\nB");
        assert_eq!(parse_summary("plain text"), "plain text");
        assert_eq!(parse_summary("<analysis>only"), "");
    }

    #[test]
    fn prompt_shape() {
        let p = build_prompt(None, &[("user".into(), "hi".into()), ("assistant".into(), "x".repeat(10))], 5);
        assert!(p.starts_with(OPENING_NEW));
        assert!(p.contains("User: hi\n\nAssistant: xxxxx..."));
        let p = build_prompt(Some("old"), &[], 0);
        assert!(p.contains("<existing_summary>\nold\n</existing_summary>"));
    }
}
