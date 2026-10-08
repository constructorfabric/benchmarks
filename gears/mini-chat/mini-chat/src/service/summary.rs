//! Thread summary outbox handler (DESIGN "Thread Summary Update").

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{ModelCatalogEntry, UsageEvent, UsageTokens};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, Set};
use time::OffsetDateTime;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::AppState;
use super::outbox::ThreadSummaryTask;
use crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT;
use crate::domain::error::{DomainError, DomainResult};
use crate::infra::db::entity::{chat, message, thread_summary};
use crate::infra::llm::{
    ChatMessage, ChatRequest, RequestMetadata, ToolsSpec, provider_user_field,
};
use crate::infra::repo::{self, now_utc};

/// Platform default subject id used as the system identity.
pub const SYSTEM_SUBJECT_ID: Uuid = toolkit_security::constants::DEFAULT_SUBJECT_ID;

const OPENING_NEW: &str = "Summarize the following conversation:";
const OPENING_MERGE: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";
const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

/// Build the user prompt of the summary request.
#[must_use]
pub fn build_prompt(
    existing: Option<&str>,
    msgs: &[(String, String)],
    content_limit: usize,
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
    let entries: Vec<String> = msgs
        .iter()
        .map(|(role, content)| {
            let label = if role == "assistant" {
                "Assistant"
            } else {
                "User"
            };
            let text = if content_limit > 0 && content.chars().count() > content_limit {
                let mut t: String = content.chars().take(content_limit).collect();
                t.push_str("...");
                t
            } else {
                content.clone()
            };
            format!("{label}: {text}")
        })
        .collect();
    out.push_str("\n\n");
    out.push_str(&entries.join("\n\n"));
    out.push_str("\n\n");
    out.push_str(ANALYSIS_INSTRUCTION);
    out
}

/// Extract the stored summary from the model output.
#[must_use]
pub fn parse_summary(raw: &str) -> String {
    let mut text = raw.to_owned();
    // Drop the analysis block(s).
    while let Some(start) = text.find("<analysis>") {
        match text[start..].find("</analysis>") {
            Some(end) => text.replace_range(start..start + end + "</analysis>".len(), ""),
            None => break,
        }
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
    collapse_blank_lines(inner.trim())
}

fn collapse_blank_lines(s: &str) -> String {
    let mut out = Vec::new();
    let mut blank = false;
    for line in s.lines() {
        if line.trim().is_empty() {
            if !blank {
                out.push("");
            }
            blank = true;
        } else {
            out.push(line);
            blank = false;
        }
    }
    out.join("\n")
}

/// Stored token estimate of a summary.
#[must_use]
pub fn summary_token_estimate(usage: Option<&UsageTokens>, summary: &str) -> i32 {
    let diff = usage.map_or(0, |u| u.output_tokens - u.reasoning_tokens);
    let v = if diff > 0 {
        diff
    } else {
        i64::try_from(summary.len().div_ceil(4)).unwrap_or(i64::MAX)
    };
    i32::try_from(v).unwrap_or(i32::MAX)
}

/// Input budget of the summary model (`None`: no fitting).
fn summary_budget(m: &ModelCatalogEntry) -> Option<i64> {
    if m.context_window == 0 {
        return None;
    }
    let mut b = i64::from(m.context_window) - i64::from(m.max_output_tokens);
    if m.max_input_tokens > 0 {
        b = b.min(i64::from(m.max_input_tokens));
    }
    Some(b)
}

pub struct ThreadSummaryHandler {
    pub state: Arc<AppState>,
}

enum Step {
    Ok,
    Retry(String),
    Reject(String),
}

impl ThreadSummaryHandler {
    #[allow(clippy::too_many_lines)]
    #[allow(
        clippy::cognitive_complexity,
        reason = "sequential orchestration steps; splitting would obscure the flow"
    )]
    #[allow(clippy::similar_names, reason = "conventional names (cond/res/rest)")]
    async fn run(&self, task: &ThreadSummaryTask) -> Step {
        let st = &self.state;
        let scope = AccessScope::for_tenant(task.tenant_id);
        let Ok(conn) = st.conn() else {
            return Step::Retry("db unavailable".into());
        };
        let chat = match chat::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(chat::Column::Id.eq(task.chat_id)))
            .one(&conn)
            .await
        {
            Ok(Some(c)) => c,
            Ok(None) => return Step::Ok,
            Err(e) => return Step::Retry(e.to_string()),
        };
        if chat.deleted_at.is_some() {
            return Step::Ok;
        }
        let base = task
            .base_frontier_created_at
            .zip(task.base_frontier_message_id);
        let existing = match repo::find_summary(&conn, &scope, task.chat_id).await {
            Ok(s) => s,
            Err(e) => return Step::Retry(e.to_string()),
        };
        match (&existing, base) {
            (None, Some(_)) => {
                tracing::info!(chat_id = %task.chat_id, "thread summary base no longer exists");
                return Step::Ok;
            }
            (Some(s), b) => {
                if b != Some((s.summarized_up_to_created_at, s.summarized_up_to_message_id)) {
                    tracing::info!(chat_id = %task.chat_id, "thread summary frontier already advanced");
                    return Step::Ok;
                }
            }
            (None, None) => {}
        }
        // Summary model.
        let snapshot = match st.policy.current_snapshot(chat.user_id).await {
            Ok(s) => s,
            Err(e) => return Step::Retry(e.to_string()),
        };
        let model_id = st.cfg.thread_summary_worker.effective_summary_model_id();
        let Some(model) = snapshot.find_enabled(model_id).cloned() else {
            tracing::error!(model = %model_id, "thread summary model is missing or disabled");
            return Step::Reject(format!("summary model '{model_id}' unavailable"));
        };
        let Some(provider) = st
            .llm
            .registry
            .resolve(&model.provider_id, Some(task.tenant_id))
        else {
            return Step::Retry(format!("no provider for summary model '{model_id}'"));
        };
        // Messages of the frozen range.
        let mut cond = Condition::all()
            .add(message::Column::ChatId.eq(task.chat_id))
            .add(message::Column::DeletedAt.is_null())
            .add(message::Column::IsCompressed.eq(false))
            .add(
                Condition::any()
                    .add(message::Column::CreatedAt.lt(task.frozen_target_created_at))
                    .add(
                        Condition::all()
                            .add(message::Column::CreatedAt.eq(task.frozen_target_created_at))
                            .add(message::Column::Id.lte(task.frozen_target_message_id)),
                    ),
            );
        if let Some((bc, bid)) = base {
            cond = cond.add(
                Condition::any().add(message::Column::CreatedAt.gt(bc)).add(
                    Condition::all()
                        .add(message::Column::CreatedAt.eq(bc))
                        .add(message::Column::Id.gt(bid)),
                ),
            );
        }
        let msgs = match message::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(cond)
            .order_by(message::Column::CreatedAt, Order::Asc)
            .order_by(message::Column::Id, Order::Asc)
            .all(&conn)
            .await
        {
            Ok(m) => m,
            Err(e) => return Step::Retry(e.to_string()),
        };
        let mut entries: Vec<(String, String)> = msgs
            .iter()
            .filter(|m| m.role == "user" || m.role == "assistant")
            .map(|m| (m.role.clone(), m.content.clone()))
            .collect();
        if entries.is_empty() && existing.is_none() {
            return Step::Ok;
        }
        let system_prompt = if !model.thread_summary_prompt.trim().is_empty() {
            model.thread_summary_prompt.clone()
        } else if !st
            .cfg
            .thread_summary_worker
            .summary_system_prompt
            .trim()
            .is_empty()
        {
            st.cfg.thread_summary_worker.summary_system_prompt.clone()
        } else {
            DEFAULT_SUMMARY_SYSTEM_PROMPT.to_owned()
        };
        let limit = st.cfg.thread_summary_worker.message_content_limit;
        let existing_text = existing.as_ref().map(|s| s.summary_text.clone());
        let bpt = u64::from(model.estimation_budgets.bytes_per_token_conservative.max(1));
        if let Some(budget) = summary_budget(&model) {
            loop {
                let prompt = build_prompt(existing_text.as_deref(), &entries, limit);
                let est =
                    i64::try_from(((system_prompt.len() + prompt.len()) as u64).div_ceil(bpt))
                        .unwrap_or(i64::MAX);
                if est <= budget || entries.len() <= 2 {
                    break;
                }
                let drop_n = entries.len().div_ceil(5).min(entries.len() - 2);
                entries.drain(..drop_n);
            }
        }
        let ctx = match SecurityContext::builder()
            .subject_id(SYSTEM_SUBJECT_ID)
            .subject_tenant_id(task.tenant_id)
            .build()
        {
            Ok(c) => c,
            Err(e) => return Step::Retry(e.to_string()),
        };
        let mut attempt = 0;
        let completion = loop {
            let prompt = build_prompt(existing_text.as_deref(), &entries, limit);
            let req = ChatRequest {
                model: model.provider_model_id.clone(),
                instructions: system_prompt.clone(),
                messages: vec![ChatMessage {
                    role: "user",
                    text: prompt,
                    image_file_ids: vec![],
                }],
                max_output_tokens: i64::from(model.max_output_tokens),
                tools: ToolsSpec::default(),
                max_tool_calls: 0,
                api_params: model.general_config.api_params.clone(),
                user: provider_user_field(task.tenant_id, SYSTEM_SUBJECT_ID),
                metadata: RequestMetadata {
                    tenant_id: task.tenant_id.to_string(),
                    user_id: SYSTEM_SUBJECT_ID.to_string(),
                    chat_id: task.chat_id.to_string(),
                    request_type: "summary",
                    feature: "none".into(),
                },
                stream: false,
                extra_input: vec![],
            };
            match st.llm.complete(&ctx, &provider, &req).await {
                Ok(c) => break c,
                Err(e) => {
                    let ctx_len = e.provider_code.as_deref() == Some("context_length_exceeded")
                        || e.message.to_ascii_lowercase().contains("context length");
                    if ctx_len && attempt < 2 && entries.len() > 2 {
                        attempt += 1;
                        let drop_n = entries.len().div_ceil(5).min(entries.len() - 2);
                        entries.drain(..drop_n);
                        continue;
                    }
                    tracing::warn!(error = %e.message, chat_id = %task.chat_id, "thread summary call failed");
                    return Step::Retry(format!("provider error: {}", e.message));
                }
            }
        };
        let summary = parse_summary(&completion.text);
        if summary.is_empty() {
            return Step::Retry("empty summary".into());
        }
        let token_estimate = summary_token_estimate(completion.usage.as_ref(), &summary);
        match self
            .commit(
                task,
                base,
                &model,
                summary,
                token_estimate,
                completion.usage,
            )
            .await
        {
            Ok(()) => Step::Ok,
            Err(e) => Step::Retry(format!("commit failed: {e}")),
        }
    }

    async fn commit(
        &self,
        task: &ThreadSummaryTask,
        base: Option<(OffsetDateTime, Uuid)>,
        model: &ModelCatalogEntry,
        summary: String,
        token_estimate: i32,
        usage: Option<UsageTokens>,
    ) -> DomainResult<()> {
        let st = &self.state;
        let outbox = st.outbox.get().await?;
        let task = task.clone();
        let model_id = model.id.clone();
        let usage_queue = st.cfg.outbox.queue_name.clone();
        let partitions = st.cfg.outbox.num_partitions;
        let wake = st
            .write_tx(move |tx| {
                let task = task.clone();
                let summary = summary.clone();
                let model_id = model_id.clone();
                let outbox = Arc::clone(&outbox);
                let usage_queue = usage_queue.clone();
                Box::pin(async move {
                    let scope = AccessScope::for_tenant(task.tenant_id);
                    // The target frontier message must still be live.
                    let target = message::Entity::find()
                        .secure()
                        .scope_with(&scope)
                        .filter(
                            Condition::all()
                                .add(message::Column::Id.eq(task.frozen_target_message_id))
                                .add(message::Column::ChatId.eq(task.chat_id)),
                        )
                        .one(tx)
                        .await?;
                    if target.is_none_or(|m| m.deleted_at.is_some()) {
                        tracing::info!(chat_id = %task.chat_id, "summary target frontier deleted; skipping commit");
                        return Ok(None);
                    }
                    let now = now_utc();
                    let won = cas_upsert(tx, &scope, &task, base, &summary, token_estimate, now).await?;
                    if !won {
                        tracing::info!(chat_id = %task.chat_id, "thread summary CAS conflict");
                        return Ok(None);
                    }
                    mark_compressed(tx, &scope, &task, base).await?;
                    let ev = UsageEvent {
                        tenant_id: task.tenant_id,
                        user_id: None,
                        chat_id: task.chat_id,
                        turn_id: None,
                        request_id: task.system_request_id,
                        effective_model: model_id.clone(),
                        selected_model: model_id.clone(),
                        terminal_state: "completed".into(),
                        billing_outcome: "system_task".into(),
                        usage: Some(usage.unwrap_or_default()),
                        actual_credits_micro: 0,
                        settlement_method: "none".into(),
                        policy_version_applied: 0,
                        web_search_calls: 0,
                        code_interpreter_calls: 0,
                        file_search_calls: 0,
                        timestamp: OffsetDateTime::now_utc(),
                        requester_type: "system".into(),
                        dedupe_key: format!(
                            "{}/{}/{}",
                            task.tenant_id.as_simple(),
                            task.system_task_type,
                            task.system_request_id.as_simple()
                        ),
                        system_task_type: Some(task.system_task_type.clone()),
                    };
                    let w = super::outbox::enqueue_json(
                        &outbox,
                        tx,
                        &usage_queue,
                        super::outbox::partition_for(task.tenant_id, partitions),
                        super::outbox::PT_USAGE,
                        &ev,
                    )
                    .await?;
                    Ok(Some(w))
                })
            })
            .await?;
        if let Some(w) = wake {
            w.fire();
        }
        Ok(())
    }
}

async fn cas_upsert(
    tx: &impl DBRunner,
    scope: &AccessScope,
    task: &ThreadSummaryTask,
    base: Option<(OffsetDateTime, Uuid)>,
    summary: &str,
    token_estimate: i32,
    now: OffsetDateTime,
) -> DomainResult<bool> {
    match base {
        None => {
            if repo::find_summary(tx, scope, task.chat_id).await?.is_some() {
                return Ok(false);
            }
            let am = thread_summary::ActiveModel {
                id: Set(Uuid::now_v7()),
                tenant_id: Set(task.tenant_id),
                chat_id: Set(task.chat_id),
                summary_text: Set(summary.to_owned()),
                summarized_up_to_created_at: Set(task.frozen_target_created_at),
                summarized_up_to_message_id: Set(task.frozen_target_message_id),
                token_estimate: Set(token_estimate),
                created_at: Set(now),
                updated_at: Set(now),
            };
            match secure_insert::<thread_summary::Entity>(am, scope, tx).await {
                Ok(_) => Ok(true),
                Err(e) => {
                    let e = DomainError::from(e);
                    if e.is_unique_violation() {
                        Ok(false)
                    } else {
                        Err(e)
                    }
                }
            }
        }
        Some((bc, bid)) => {
            let res = thread_summary::Entity::update_many()
                .secure()
                .scope_with(scope)
                .col_expr(
                    thread_summary::Column::SummaryText,
                    Expr::value(summary.to_owned()),
                )
                .col_expr(
                    thread_summary::Column::SummarizedUpToCreatedAt,
                    Expr::value(task.frozen_target_created_at),
                )
                .col_expr(
                    thread_summary::Column::SummarizedUpToMessageId,
                    Expr::value(task.frozen_target_message_id),
                )
                .col_expr(
                    thread_summary::Column::TokenEstimate,
                    Expr::value(token_estimate),
                )
                .col_expr(thread_summary::Column::UpdatedAt, Expr::value(now))
                .filter(
                    Condition::all()
                        .add(thread_summary::Column::ChatId.eq(task.chat_id))
                        .add(thread_summary::Column::SummarizedUpToCreatedAt.eq(bc))
                        .add(thread_summary::Column::SummarizedUpToMessageId.eq(bid)),
                )
                .exec(tx)
                .await?;
            Ok(res.rows_affected == 1)
        }
    }
}

async fn mark_compressed(
    tx: &impl DBRunner,
    scope: &AccessScope,
    task: &ThreadSummaryTask,
    base: Option<(OffsetDateTime, Uuid)>,
) -> DomainResult<()> {
    let mut cond = Condition::all()
        .add(message::Column::ChatId.eq(task.chat_id))
        .add(message::Column::DeletedAt.is_null())
        .add(
            Condition::any()
                .add(message::Column::CreatedAt.lt(task.frozen_target_created_at))
                .add(
                    Condition::all()
                        .add(message::Column::CreatedAt.eq(task.frozen_target_created_at))
                        .add(message::Column::Id.lte(task.frozen_target_message_id)),
                ),
        );
    if let Some((bc, bid)) = base {
        cond = cond.add(
            Condition::any().add(message::Column::CreatedAt.gt(bc)).add(
                Condition::all()
                    .add(message::Column::CreatedAt.eq(bc))
                    .add(message::Column::Id.gt(bid)),
            ),
        );
    }
    message::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(message::Column::IsCompressed, Expr::value(true))
        .filter(cond)
        .exec(tx)
        .await?;
    Ok(())
}

#[async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let task: ThreadSummaryTask = match serde_json::from_slice(&msg.payload) {
            Ok(t) => t,
            Err(e) => {
                return MessageResult::Reject(format!("malformed thread summary payload: {e}"));
            }
        };
        match self.run(&task).await {
            Step::Ok => MessageResult::Ok,
            Step::Reject(why) => MessageResult::Reject(why),
            Step::Retry(why) => {
                let max = i32::try_from(self.state.cfg.thread_summary_worker.max_attempts)
                    .unwrap_or(i32::MAX);
                if i32::from(msg.attempts) + 1 >= max {
                    MessageResult::Reject(format!(
                        "thread summary gave up after {max} attempts: {why}"
                    ))
                } else {
                    tracing::warn!(reason = %why, chat_id = %task.chat_id, "thread summary retry");
                    MessageResult::Retry
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_parsing() {
        assert_eq!(
            parse_summary("<analysis>think</analysis>\n<summary>\nA\n\n\n\nB\n</summary>"),
            "A\n\nB"
        );
        assert_eq!(parse_summary("plain text"), "plain text");
        assert_eq!(parse_summary("<analysis>unterminated"), "");
    }

    #[test]
    fn prompt_shape() {
        let p = build_prompt(
            None,
            &[
                ("user".into(), "hello".into()),
                ("assistant".into(), "x".repeat(10)),
            ],
            5,
        );
        assert!(p.starts_with(OPENING_NEW));
        assert!(p.contains("User: hello\n\nAssistant: xxxxx..."));
        let p = build_prompt(Some("old"), &[], 0);
        assert!(p.contains("<existing_summary>\nold\n</existing_summary>"));
    }

    #[test]
    fn token_estimate_fallback() {
        assert_eq!(summary_token_estimate(None, "abcdefgh1"), 3);
        let u = UsageTokens {
            output_tokens: 10,
            reasoning_tokens: 4,
            ..UsageTokens::default()
        };
        assert_eq!(summary_token_estimate(Some(&u), "x"), 6);
    }

    #[test]
    fn system_subject_id() {
        assert_eq!(
            SYSTEM_SUBJECT_ID.to_string(),
            "11111111-6a88-4768-9dfc-6bcd5187d9ed"
        );
    }
}
