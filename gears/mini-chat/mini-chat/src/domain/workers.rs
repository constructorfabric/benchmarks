//! Background work: outbox handlers (usage, audit, attachment cleanup, chat
//! cleanup, thread summary), the orphan watchdog and the upload reaper.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mini_chat_sdk::{
    AuditEvent, AuditLatency, PolicyDecisions, PublishError, QuotaPolicyDecision, ToolCalls,
    TurnAuditEvent, UsageEvent,
};
use time::OffsetDateTime;
use toolkit_db::outbox::{MessageResult, OutboxMessage};
use uuid::Uuid;

use super::quota::{Periods, multipliers};
use super::service::Services;
use super::stream::finalize::{
    ReserveFields, SettleMethod, Settlement, apply_settlement, compute_settlement, dedupe_key,
};
use crate::domain::error::DomainError;
use crate::infra::db::entities::{attachment, chat_turn};
use crate::infra::db::repo::{attachments, chats, messages, summaries, turns, vector_stores};
use crate::infra::db::{now_ts, system_scope, tenant_scope};
use crate::infra::outbox::{AttachmentCleanupMsg, ChatCleanupMsg, Queue, ThreadSummaryMsg};
use crate::infra::plugins_gateway::AuditDelivery;

/// Platform default subject used as the system identity of summary calls.
pub const SYSTEM_SUBJECT_ID: Uuid = uuid::uuid!("11111111-6a88-4768-9dfc-6bcd5187d9ed");

/// Audit deliveries before an event is dead-lettered.
const AUDIT_MAX_ATTEMPTS: i32 = 120;

fn delivery(msg: &OutboxMessage) -> i32 {
    i32::from(msg.attempts) + 1
}

// ════════════════════════════════════════════════════════════════════════════
// Usage and audit
// ════════════════════════════════════════════════════════════════════════════

pub struct UsageHandler(pub Arc<Services>);

#[async_trait::async_trait]
impl toolkit_db::outbox::LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("corrupt usage payload: {e}")),
        };
        match self.0.policy.publish_usage(ev).await {
            Ok(()) => MessageResult::Ok,
            Err(PublishError::Transient(e)) => {
                tracing::warn!(error = %e, "usage publish failed; retrying");
                MessageResult::Retry
            }
            Err(PublishError::Permanent(e)) => MessageResult::Reject(e),
        }
    }
}

pub struct AuditHandler(pub Arc<Services>);

#[async_trait::async_trait]
impl toolkit_db::outbox::LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let m = &self.0.metrics;
        let ev: AuditEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => {
                m.inc("audit_emit", &[("result", "reject")]);
                return MessageResult::Reject(format!("corrupt audit payload: {e}"));
            }
        };
        match self.0.audit.deliver(ev).await {
            AuditDelivery::Ok => {
                m.inc("audit_emit", &[("result", "ok")]);
                MessageResult::Ok
            }
            AuditDelivery::Dropped => {
                m.inc("audit_emit", &[("result", "dropped")]);
                MessageResult::Ok
            }
            AuditDelivery::Reject(e) => {
                m.inc("audit_emit", &[("result", "reject")]);
                MessageResult::Reject(e)
            }
            AuditDelivery::Retry(e) => {
                if delivery(msg) >= AUDIT_MAX_ATTEMPTS {
                    m.inc("audit_emit", &[("result", "reject")]);
                    return MessageResult::Reject(format!(
                        "audit delivery failed after {AUDIT_MAX_ATTEMPTS} attempts: {e}"
                    ));
                }
                m.inc("audit_emit", &[("result", "retry")]);
                tracing::warn!(error = %e, "audit delivery failed; retrying");
                MessageResult::Retry
            }
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Cleanup
// ════════════════════════════════════════════════════════════════════════════

enum FileDelete {
    Done,
    Failed(String),
}

impl Services {
    async fn delete_provider_file(&self, a: &attachment::Model) -> FileDelete {
        let Some(fid) = &a.provider_file_id else {
            return FileDelete::Done;
        };
        let Some(storage) = self
            .providers
            .storage_by_backend(&a.storage_backend, a.tenant_id)
        else {
            return FileDelete::Failed(format!("unknown storage backend '{}'", a.storage_backend));
        };
        match self.llm.delete_file(&storage, fid).await {
            Ok(()) => FileDelete::Done,
            Err(e) => FileDelete::Failed(e.to_string()),
        }
    }

    async fn delete_secondary(&self, file_id: &str, alias: Option<&str>) {
        match alias {
            Some(alias) => {
                if let Err(e) = self.llm.anthropic_delete(alias, file_id).await {
                    tracing::warn!(error = %e, "secondary file delete failed");
                }
            }
            None => self.metrics.inc(
                "secondary_cleanup_skipped",
                &[("provider_kind", "anthropic")],
            ),
        }
    }

    fn anthropic_alias_for_tenant(&self, tenant: Uuid) -> Option<String> {
        self.providers
            .entries()
            .iter()
            .find(|(_, e)| e.kind == crate::config::ProviderKind::AnthropicMessages)
            .and_then(|(id, _)| self.providers.anthropic_alias(id, tenant))
    }

    /// Record a failed provider delete; returns `true` when the attachment
    /// reached the attempt limit and was marked `failed`.
    async fn record_cleanup_failure(
        &self,
        a: &attachment::Model,
        err: &str,
    ) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let scope = tenant_scope(a.tenant_id);
        attachments::record_cleanup_attempt(&conn, &scope, a.id, err, now_ts()).await?;
        self.metrics.inc(
            "cleanup_retry",
            &[("resource_type", "file"), ("reason", "provider_error")],
        );
        let attempts = a.cleanup_attempts + 1;
        if u32::try_from(attempts).unwrap_or(0) >= self.cfg.cleanup_worker.max_attempts {
            attachments::set_cleanup_outcome(
                &conn,
                &scope,
                a.id,
                attachments::CLEANUP_FAILED,
                Some(err),
                now_ts(),
            )
            .await?;
            self.metrics
                .inc("cleanup_failed", &[("resource_type", "file")]);
            return Ok(true);
        }
        Ok(false)
    }

    async fn mark_cleanup_done(&self, a: &attachment::Model) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        attachments::set_cleanup_outcome(
            &conn,
            &tenant_scope(a.tenant_id),
            a.id,
            attachments::CLEANUP_DONE,
            None,
            now_ts(),
        )
        .await?;
        self.metrics
            .inc("cleanup_completed", &[("resource_type", "file")]);
        Ok(())
    }
}

pub struct AttachmentCleanupHandler(pub Arc<Services>);

#[async_trait::async_trait]
impl toolkit_db::outbox::LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let svc = &self.0;
        let m: AttachmentCleanupMsg = match serde_json::from_slice(&msg.payload) {
            Ok(m) => m,
            Err(e) => return MessageResult::Reject(format!("corrupt cleanup payload: {e}")),
        };
        let Ok(conn) = svc.db.conn() else {
            return MessageResult::Retry;
        };
        let scope = tenant_scope(m.tenant_id);
        match chats::find_any(&conn, &scope, m.chat_id).await {
            Ok(Some(c)) if c.deleted_at.is_some() => return MessageResult::Ok,
            Ok(_) => {}
            Err(_) => return MessageResult::Retry,
        }
        let a = match attachments::find_by_id(&conn, &scope, m.attachment_id).await {
            Ok(Some(a)) => a,
            Ok(None) => return MessageResult::Ok,
            Err(_) => return MessageResult::Retry,
        };
        if a.cleanup_status.as_deref() != Some(attachments::CLEANUP_PENDING) {
            return MessageResult::Ok;
        }
        let mut row = a.clone();
        if row.provider_file_id.is_none() {
            row.provider_file_id.clone_from(&m.provider_file_id);
        }
        match svc.delete_provider_file(&row).await {
            FileDelete::Done => {
                if let Some(sec) = &m.secondary_ref {
                    svc.delete_secondary(&sec.file_id, Some(&sec.upstream_alias))
                        .await;
                }
                match svc.mark_cleanup_done(&row).await {
                    Ok(()) => MessageResult::Ok,
                    Err(_) => MessageResult::Retry,
                }
            }
            FileDelete::Failed(e) => {
                tracing::warn!(error = %e, attachment_id = %row.id, "attachment cleanup failed");
                match svc.record_cleanup_failure(&row, &e).await {
                    Ok(true) => MessageResult::Reject(format!(
                        "attachment cleanup: max attempts ({}) reached",
                        svc.cfg.cleanup_worker.max_attempts
                    )),
                    Ok(false) | Err(_) => MessageResult::Retry,
                }
            }
        }
    }
}

pub struct ChatCleanupHandler(pub Arc<Services>);

#[async_trait::async_trait]
impl toolkit_db::outbox::LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let svc = &self.0;
        let m: ChatCleanupMsg = match serde_json::from_slice(&msg.payload) {
            Ok(m) => m,
            Err(e) => return MessageResult::Reject(format!("corrupt chat cleanup payload: {e}")),
        };
        let Ok(conn) = svc.db.conn() else {
            return MessageResult::Retry;
        };
        let scope = tenant_scope(m.tenant_id);
        match chats::find_any(&conn, &scope, m.chat_id).await {
            Ok(Some(c)) if c.deleted_at.is_some() => {}
            Ok(_) => return MessageResult::Reject("chat is not soft-deleted".to_owned()),
            Err(_) => return MessageResult::Retry,
        }
        let Ok(pending) = attachments::cleanup_pending_for_chat(&conn, &scope, m.chat_id).await
        else {
            return MessageResult::Retry;
        };
        let secondary_alias = svc.anthropic_alias_for_tenant(m.tenant_id);
        let mut still_pending = false;
        for a in &pending {
            match svc.delete_provider_file(a).await {
                FileDelete::Done => {
                    if let Some(sid) = &a.secondary_file_id {
                        svc.delete_secondary(sid, secondary_alias.as_deref()).await;
                    }
                    if svc.mark_cleanup_done(a).await.is_err() {
                        return MessageResult::Retry;
                    }
                }
                FileDelete::Failed(e) => {
                    tracing::warn!(error = %e, attachment_id = %a.id, "chat cleanup file delete failed");
                    match svc.record_cleanup_failure(a, &e).await {
                        Ok(true) => {}
                        Ok(false) => still_pending = true,
                        Err(_) => return MessageResult::Retry,
                    }
                }
            }
        }
        let max = i32::try_from(svc.cfg.cleanup_worker.max_attempts).unwrap_or(i32::MAX);
        if still_pending {
            return MessageResult::Retry;
        }
        let Ok(vs_row) = vector_stores::find(&conn, &scope, m.tenant_id, m.chat_id).await else {
            return MessageResult::Retry;
        };
        let Some(vs_row) = vs_row else {
            return MessageResult::Ok;
        };
        if let Ok(n) = attachments::cleanup_failed_count(&conn, &scope, m.chat_id).await
            && n > 0
        {
            svc.metrics
                .inc("cleanup_vector_store_with_failed_attachments", &[]);
        }
        let Some(vs_id) = vs_row.vector_store_id.clone() else {
            if let Err(e) = vector_stores::delete_row(&conn, &scope, vs_row.id).await {
                tracing::warn!(error = %e, "failed to remove the vector store placeholder");
            }
            return MessageResult::Ok;
        };
        let Some(storage) = svc
            .providers
            .storage_by_backend(&vs_row.provider, m.tenant_id)
        else {
            return MessageResult::Reject(format!("unknown storage backend '{}'", vs_row.provider));
        };
        match svc.llm.delete_vector_store(&storage, &vs_id).await {
            Ok(()) => {
                if vector_stores::delete_row(&conn, &scope, vs_row.id)
                    .await
                    .is_err()
                {
                    return MessageResult::Retry;
                }
                svc.metrics
                    .inc("cleanup_completed", &[("resource_type", "vector_store")]);
                MessageResult::Ok
            }
            Err(e) => {
                tracing::warn!(error = %e, "vector store delete failed");
                svc.metrics.inc(
                    "cleanup_retry",
                    &[
                        ("resource_type", "vector_store"),
                        ("reason", "vector_store_delete_failed"),
                    ],
                );
                if delivery(msg) >= max {
                    svc.metrics
                        .inc("cleanup_failed", &[("resource_type", "vector_store")]);
                    MessageResult::Reject(format!(
                        "vector store delete: max attempts ({max}) reached"
                    ))
                } else {
                    MessageResult::Retry
                }
            }
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Thread summary
// ════════════════════════════════════════════════════════════════════════════

const SUMMARY_OPEN: &str = "Summarize the following conversation:";
const SUMMARY_MERGE: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";
const SUMMARY_ANALYSIS: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

/// Build the user prompt of a summary request.
#[must_use]
pub fn summary_prompt(existing: Option<&str>, msgs: &[(String, String)], limit: usize) -> String {
    let mut out = String::new();
    if let Some(s) = existing {
        out.push_str(SUMMARY_MERGE);
        out.push_str("\n\n<existing_summary>\n");
        out.push_str(s);
        out.push_str("\n</existing_summary>\n\nNew messages to incorporate:\n\n");
    } else {
        out.push_str(SUMMARY_OPEN);
        out.push_str("\n\n");
    }
    let entries: Vec<String> = msgs
        .iter()
        .map(|(role, content)| {
            let who = if role == "assistant" {
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
            format!("{who}: {text}")
        })
        .collect();
    out.push_str(&entries.join("\n\n"));
    out.push_str("\n\n");
    out.push_str(SUMMARY_ANALYSIS);
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

pub struct ThreadSummaryHandler(pub Arc<Services>);

enum SummaryResult {
    Ok,
    Retry,
    Reject(String),
}

impl ThreadSummaryHandler {
    fn retry_or_reject(&self, msg: &OutboxMessage) -> MessageResult {
        let max = i32::try_from(self.0.cfg.thread_summary_worker.max_attempts).unwrap_or(i32::MAX);
        if delivery(msg) >= max {
            MessageResult::Reject(format!("thread summary failed after {max} attempts"))
        } else {
            MessageResult::Retry
        }
    }

    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    async fn run(&self, m: &ThreadSummaryMsg) -> SummaryResult {
        let svc = &self.0;
        let metrics = &svc.metrics;
        let cfg = &svc.cfg.thread_summary_worker;
        let model_id = cfg.effective_summary_model_id().to_owned();
        let snapshot = match svc.policy.current_snapshot(SYSTEM_SUBJECT_ID).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "thread summary: policy unavailable");
                metrics.inc("thread_summary_execution", &[("result", "retry")]);
                return SummaryResult::Retry;
            }
        };
        let Some(model) = snapshot.find_enabled_model(&model_id).cloned() else {
            tracing::error!(model = %model_id, "thread summary model is missing or disabled");
            metrics.inc(
                "thread_summary_execution",
                &[("result", "model_unavailable")],
            );
            return SummaryResult::Reject(format!("summary model '{model_id}' unavailable"));
        };
        let Some(target) = svc.providers.resolve(&model.provider_id, m.tenant_id) else {
            metrics.inc("thread_summary_execution", &[("result", "retry")]);
            return SummaryResult::Retry;
        };
        let scope = tenant_scope(m.tenant_id);
        let Ok(conn) = svc.db.conn() else {
            return SummaryResult::Retry;
        };
        let base = match (m.base_frontier_created_at, m.base_frontier_message_id) {
            (Some(c), Some(i)) => Some((c, i)),
            _ => None,
        };
        let target_key = (m.frozen_target_created_at, m.frozen_target_message_id);
        let Ok(current) = summaries::find(&conn, &scope, m.chat_id).await else {
            return SummaryResult::Retry;
        };
        let current_key = current
            .as_ref()
            .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        match (base, current_key) {
            (Some(_), None) => {
                metrics.inc("thread_summary_execution", &[("result", "base_missing")]);
                return SummaryResult::Ok;
            }
            (Some(b), Some(c)) if b != c => {
                metrics.inc("thread_summary_cas_conflicts", &[]);
                return SummaryResult::Ok;
            }
            (None, Some(_)) => {
                metrics.inc("thread_summary_cas_conflicts", &[]);
                return SummaryResult::Ok;
            }
            _ => {}
        }
        let Ok(range) = messages::summary_range(&conn, &scope, m.chat_id, base, target_key).await
        else {
            return SummaryResult::Retry;
        };
        let mut msgs: Vec<(String, String)> = range
            .into_iter()
            .filter(|x| x.role != "system")
            .map(|x| (x.role, x.content))
            .collect();
        if msgs.is_empty() {
            return SummaryResult::Ok;
        }
        let system = if !model.thread_summary_prompt.trim().is_empty() {
            model.thread_summary_prompt.clone()
        } else if !cfg.summary_system_prompt.trim().is_empty() {
            cfg.summary_system_prompt.clone()
        } else {
            crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT.to_owned()
        };
        let existing = current.as_ref().map(|s| s.summary_text.clone());
        // Fit the prompt into the summary model's input budget.
        let ctx_window = i64::from(model.context_window);
        if ctx_window > 0 {
            let mut budget = ctx_window - i64::from(model.max_output_tokens);
            if model.max_input_tokens > 0 {
                budget = budget.min(i64::from(model.max_input_tokens));
            }
            let bpt = usize::try_from(model.estimation_budgets.bytes_per_token_conservative.max(1))
                .unwrap_or(1);
            loop {
                let prompt = summary_prompt(existing.as_deref(), &msgs, cfg.message_content_limit);
                let est =
                    i64::try_from((system.len() + prompt.len()).div_ceil(bpt)).unwrap_or(i64::MAX);
                if est <= budget || msgs.len() <= 2 {
                    break;
                }
                let drop = msgs.len().div_ceil(5).min(msgs.len() - 2);
                msgs.drain(..drop);
            }
        }
        let user = crate::infra::llm::registry::provider_user(m.tenant_id, SYSTEM_SUBJECT_ID);
        let mut attempt_msgs = msgs.clone();
        let mut ptl_retries = 0;
        let (raw, usage) = loop {
            let prompt = summary_prompt(
                existing.as_deref(),
                &attempt_msgs,
                cfg.message_content_limit,
            );
            let req = crate::infra::llm::types::LlmRequest {
                model: model.provider_model_id.clone(),
                instructions: system.clone(),
                input: vec![crate::infra::llm::types::InputItem::text(
                    crate::infra::llm::types::Role::User,
                    prompt,
                )],
                max_output_tokens: model.max_output_tokens,
                tools: vec![],
                max_tool_calls: model.max_tool_calls,
                api_params: model.general_config.api_params.clone(),
                user: user.clone(),
                metadata: crate::infra::llm::types::RequestMetadata {
                    tenant_id: m.tenant_id.to_string(),
                    user_id: SYSTEM_SUBJECT_ID.to_string(),
                    chat_id: m.chat_id.to_string(),
                    request_type: "summary",
                    feature: "none".to_owned(),
                },
                stream: false,
            };
            match svc
                .llm
                .complete(svc.llm.s2s().as_ref(), &target, &req)
                .await
            {
                Ok(r) => break r,
                Err(f) => {
                    let lower = f.message.to_ascii_lowercase();
                    let ptl = lower.contains("context_length")
                        || lower.contains("context length")
                        || lower.contains("too long")
                        || lower.contains("maximum context");
                    if ptl && ptl_retries < 2 && attempt_msgs.len() > 2 {
                        ptl_retries += 1;
                        let drop = attempt_msgs.len().div_ceil(5).min(attempt_msgs.len() - 2);
                        attempt_msgs.drain(..drop);
                        continue;
                    }
                    tracing::warn!(error = %f.message, "thread summary provider call failed");
                    metrics.inc("thread_summary_execution", &[("result", "provider_error")]);
                    metrics.inc("summary_fallback", &[]);
                    return SummaryResult::Retry;
                }
            }
        };
        let text = parse_summary(&raw);
        if text.is_empty() {
            metrics.inc("thread_summary_execution", &[("result", "empty_summary")]);
            return SummaryResult::Retry;
        }
        let u = usage.unwrap_or_default();
        let token_estimate = {
            let t = u.output_tokens - u.reasoning_tokens;
            if t > 0 {
                t
            } else {
                i64::try_from(text.len().div_ceil(4)).unwrap_or(i64::MAX)
            }
        };
        let token_estimate = i32::try_from(token_estimate).unwrap_or(i32::MAX);
        let ob = svc.outbox.clone();
        let mm = m.clone();
        let model_id2 = model.id.clone();
        let res = svc
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let now = now_ts();
                    let scope = tenant_scope(mm.tenant_id);
                    let target_key = (mm.frozen_target_created_at, mm.frozen_target_message_id);
                    if messages::find_live_by_id(
                        tx,
                        &scope,
                        mm.chat_id,
                        mm.frozen_target_message_id,
                    )
                    .await?
                    .is_none()
                    {
                        return Ok(CommitOutcome::FrontierDeleted);
                    }
                    let base = match (mm.base_frontier_created_at, mm.base_frontier_message_id) {
                        (Some(c), Some(i)) => Some((c, i)),
                        _ => None,
                    };
                    match base {
                        Some(b) => {
                            if !summaries::cas_update(
                                tx,
                                &scope,
                                mm.chat_id,
                                b,
                                &text,
                                target_key,
                                token_estimate,
                                now,
                            )
                            .await?
                            {
                                return Ok(CommitOutcome::Conflict);
                            }
                        }
                        None => match summaries::insert(
                            tx,
                            &scope,
                            mm.tenant_id,
                            mm.chat_id,
                            &text,
                            target_key,
                            token_estimate,
                            now,
                        )
                        .await
                        {
                            Ok(()) => {}
                            Err(e) if e.is_unique_violation() => {
                                return Ok(CommitOutcome::Conflict);
                            }
                            Err(e) => return Err(e),
                        },
                    }
                    messages::mark_compressed(tx, &scope, mm.chat_id, base, target_key).await?;
                    let ev = UsageEvent {
                        tenant_id: mm.tenant_id,
                        user_id: None,
                        chat_id: Some(mm.chat_id),
                        turn_id: None,
                        request_id: mm.system_request_id,
                        effective_model: model_id2.clone(),
                        selected_model: model_id2,
                        terminal_state: "completed".to_owned(),
                        billing_outcome: "system_task".to_owned(),
                        usage: Some(u),
                        actual_credits_micro: 0,
                        settlement_method: "none".to_owned(),
                        policy_version_applied: 0,
                        web_search_calls: 0,
                        code_interpreter_calls: 0,
                        file_search_calls: 0,
                        timestamp: OffsetDateTime::now_utc(),
                        requester_type: "system".to_owned(),
                        dedupe_key: format!(
                            "{}/thread_summary_update/{}",
                            mm.tenant_id.as_simple(),
                            mm.system_request_id.as_simple()
                        ),
                        system_task_type: Some("thread_summary_update".to_owned()),
                    };
                    let w = ob.enqueue(tx, Queue::Usage, mm.tenant_id, &ev).await?;
                    Ok(CommitOutcome::Committed(w))
                })
            })
            .await;
        match res {
            Ok(CommitOutcome::Committed(w)) => {
                w.fire();
                metrics.inc("thread_summary_execution", &[("result", "success")]);
                SummaryResult::Ok
            }
            Ok(CommitOutcome::Conflict) => {
                metrics.inc("thread_summary_cas_conflicts", &[]);
                SummaryResult::Ok
            }
            Ok(CommitOutcome::FrontierDeleted) => {
                metrics.inc(
                    "thread_summary_execution",
                    &[("result", "frontier_deleted")],
                );
                SummaryResult::Ok
            }
            Err(e) => {
                tracing::warn!(error = %e, "thread summary commit failed");
                metrics.inc("thread_summary_execution", &[("result", "retry")]);
                SummaryResult::Retry
            }
        }
    }
}

enum CommitOutcome {
    Committed(toolkit_db::outbox::Wake),
    Conflict,
    FrontierDeleted,
}

#[async_trait::async_trait]
impl toolkit_db::outbox::LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let m: ThreadSummaryMsg = match serde_json::from_slice(&msg.payload) {
            Ok(m) => m,
            Err(e) => return MessageResult::Reject(format!("corrupt summary payload: {e}")),
        };
        match self.run(&m).await {
            SummaryResult::Ok => MessageResult::Ok,
            SummaryResult::Reject(r) => MessageResult::Reject(r),
            SummaryResult::Retry => self.retry_or_reject(msg),
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Orphan watchdog and upload reaper
// ════════════════════════════════════════════════════════════════════════════

impl Services {
    /// One orphan watchdog scan.
    pub async fn orphan_scan(&self) {
        let started = Instant::now();
        let timeout = time::Duration::seconds(
            i64::try_from(self.cfg.orphan_watchdog.timeout_secs).unwrap_or(300),
        );
        let cutoff = OffsetDateTime::now_utc() - timeout;
        let candidates = match self.db.conn() {
            Ok(conn) => turns::orphan_candidates(&conn, &system_scope(), cutoff, 100)
                .await
                .unwrap_or_default(),
            Err(_) => vec![],
        };
        for t in candidates {
            self.metrics
                .inc("orphan_detected", &[("reason", "stale_progress")]);
            if let Err(e) = self.finalize_orphan(&t, cutoff).await {
                tracing::warn!(error = %e, turn_id = %t.id, "orphan finalization failed");
            }
        }
        self.metrics.record(
            "orphan_scan_duration_seconds",
            started.elapsed().as_secs_f64(),
            &[],
        );
    }

    async fn finalize_orphan(
        &self,
        t: &chat_turn::Model,
        cutoff: OffsetDateTime,
    ) -> Result<(), DomainError> {
        let effective = t.effective_model.clone().unwrap_or_default();
        let user = t.requester_user_id;
        let has_reserve = t.reserve_tokens.is_some()
            && t.max_output_tokens_applied.is_some()
            && t.reserved_credits_micro.is_some()
            && t.policy_version_applied.is_some()
            && t.minimal_generation_floor_applied.is_some();
        let settlement = if has_reserve && let Some(user) = user {
            {
                let version = u64::try_from(t.policy_version_applied.unwrap_or(0)).unwrap_or(0);
                let snapshot = self.policy.snapshot(user, version).await?;
                let model = snapshot.find_model(&effective).ok_or_else(|| {
                    DomainError::internal(format!(
                        "model '{effective}' missing from policy {version}"
                    ))
                })?;
                let (in_mult, out_mult) = multipliers(model);
                Some(Settlement {
                    tenant: t.tenant_id,
                    user,
                    periods: Periods::at(t.started_at),
                    tier: model.tier,
                    in_mult,
                    out_mult,
                    reserve: ReserveFields {
                        reserve_tokens: t.reserve_tokens.unwrap_or(0),
                        max_output_tokens_applied: i64::from(
                            t.max_output_tokens_applied.unwrap_or(0),
                        ),
                        reserved_credits_micro: t.reserved_credits_micro.unwrap_or(0),
                        floor_applied: i64::from(t.minimal_generation_floor_applied.unwrap_or(0)),
                    },
                    method: SettleMethod::Estimated,
                    web_search_calls: t.web_search_completed_count,
                    code_interpreter_calls: t.code_interpreter_completed_count,
                    tolerance: self.cfg.quota.overshoot_tolerance_factor,
                })
            }
        } else {
            tracing::warn!(turn_id = %t.id, "orphan turn has no reserve; settlement skipped");
            None
        };
        let settled = settlement
            .as_ref()
            .map(compute_settlement)
            .transpose()
            .map_err(|e| DomainError::internal(e.to_string()))?;
        let usage_ev = UsageEvent {
            tenant_id: t.tenant_id,
            user_id: user,
            chat_id: Some(t.chat_id),
            turn_id: Some(t.id),
            request_id: t.request_id,
            effective_model: if settlement.is_some() {
                effective.clone()
            } else {
                String::new()
            },
            selected_model: if settlement.is_some() {
                effective.clone()
            } else {
                String::new()
            },
            terminal_state: turns::STATE_FAILED.to_owned(),
            billing_outcome: "aborted".to_owned(),
            usage: None,
            actual_credits_micro: settled.map_or(0, |s| s.credits),
            settlement_method: "estimated".to_owned(),
            policy_version_applied: if settlement.is_some() {
                u64::try_from(t.policy_version_applied.unwrap_or(0)).unwrap_or(0)
            } else {
                0
            },
            web_search_calls: u32::try_from(t.web_search_completed_count).unwrap_or(0),
            code_interpreter_calls: u32::try_from(t.code_interpreter_completed_count).unwrap_or(0),
            file_search_calls: u32::try_from(t.file_search_completed_count).unwrap_or(0),
            timestamp: OffsetDateTime::now_utc(),
            requester_type: t.requester_type.clone(),
            dedupe_key: dedupe_key(t.tenant_id, t.id, t.request_id),
            system_task_type: None,
        };
        let audit_ev = AuditEvent::Turn(Box::new(TurnAuditEvent {
            event_type: "turn_failed".to_owned(),
            timestamp: OffsetDateTime::now_utc(),
            tenant_id: t.tenant_id,
            requester_type: t.requester_type.clone(),
            user_id: user,
            chat_id: t.chat_id,
            turn_id: t.id,
            request_id: t.request_id,
            selected_model: effective.clone(),
            effective_model: effective,
            terminal_state: turns::STATE_FAILED.to_owned(),
            error_code: Some("orphan_timeout".to_owned()),
            usage: None,
            latency: AuditLatency {
                ttft_ms: None,
                total_ms: u64::try_from(
                    (OffsetDateTime::now_utc() - t.started_at).whole_milliseconds(),
                )
                .unwrap_or(0),
            },
            tool_calls: ToolCalls {
                web_search_calls: u64::try_from(t.web_search_completed_count).unwrap_or(0),
                file_search_calls: u64::try_from(t.file_search_completed_count).unwrap_or(0),
            },
            policy_decisions: PolicyDecisions {
                quota: QuotaPolicyDecision {
                    decision: "unknown".to_owned(),
                    downgrade_from: None,
                    downgrade_reason: None,
                },
                license: None,
            },
            prompt: None,
            response: None,
            attachments: vec![],
            quota_scope: None,
            trace_id: None,
        }));
        let ob = self.outbox.clone();
        let turn_id = t.id;
        let tenant = t.tenant_id;
        let won = self
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let now = now_ts();
                    if !turns::orphan_cas(tx, &system_scope(), turn_id, cutoff, now).await? {
                        return Ok(None);
                    }
                    if let (Some(s), Some(st)) = (&settlement, &settled) {
                        apply_settlement(tx, s, st, now).await?;
                    }
                    let w1 = ob.enqueue(tx, Queue::Usage, tenant, &usage_ev).await?;
                    let w2 = ob.enqueue(tx, Queue::Audit, tenant, &audit_ev).await?;
                    Ok(Some(vec![w1, w2]))
                })
            })
            .await?;
        if let Some(w) = won {
            crate::infra::outbox::fire(w);
            self.metrics
                .inc("orphan_finalized", &[("reason", "stale_progress")]);
            self.metrics
                .inc("streams_aborted", &[("trigger", "orphan_timeout")]);
        }
        Ok(())
    }

    /// One upload reaper scan.
    #[allow(clippy::cognitive_complexity)]
    pub async fn reap_uploads(&self) {
        let started = Instant::now();
        let stale = time::Duration::seconds(
            i64::try_from(self.cfg.upload_reaper.stale_after_secs).unwrap_or(300),
        );
        let cutoff = OffsetDateTime::now_utc() - stale;
        let rows = match self.db.conn() {
            Ok(conn) => attachments::stale_uploads(&conn, &system_scope(), cutoff, 100)
                .await
                .unwrap_or_default(),
            Err(_) => vec![],
        };
        for a in rows {
            let ob = self.outbox.clone();
            let row = a.clone();
            let res = self
                .db
                .transaction(move |tx| {
                    Box::pin(async move {
                        let now = now_ts();
                        let with_cleanup = row.provider_file_id.is_some();
                        if !attachments::reap_cas(
                            tx,
                            &system_scope(),
                            row.id,
                            &row.status,
                            with_cleanup,
                            cutoff,
                            now,
                        )
                        .await?
                        {
                            return Ok(None);
                        }
                        if with_cleanup {
                            let msg = AttachmentCleanupMsg {
                                event_type: "attachment_upload_abandoned".to_owned(),
                                tenant_id: row.tenant_id,
                                chat_id: row.chat_id,
                                attachment_id: row.id,
                                provider_file_id: row.provider_file_id.clone(),
                                vector_store_id: None,
                                storage_backend: row.storage_backend.clone(),
                                attachment_kind: row.attachment_kind.clone(),
                                deleted_at: now,
                                secondary_ref: None,
                            };
                            return Ok(Some(Some(
                                ob.enqueue(tx, Queue::AttachmentCleanup, row.tenant_id, &msg)
                                    .await?,
                            )));
                        }
                        Ok(Some(None))
                    })
                })
                .await;
            match res {
                Ok(Some(w)) => {
                    if let Some(w) = w {
                        w.fire();
                    }
                    if let Some(sid) = &a.secondary_file_id {
                        tracing::warn!(attachment_id = %a.id, secondary_file_id = %sid, "abandoned upload keeps its secondary copy");
                    }
                    let from = a.status.clone();
                    self.metrics.inc(
                        "attachment_upload_abandoned",
                        &[("from_status", from.as_str())],
                    );
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, attachment_id = %a.id, "upload reaper failed"),
            }
        }
        self.metrics.record(
            "upload_reaper_scan_duration_seconds",
            started.elapsed().as_secs_f64(),
            &[],
        );
    }
}

/// Periodic loop helper.
pub async fn periodic<F, Fut>(
    interval: Duration,
    cancel: tokio_util::sync::CancellationToken,
    mut f: F,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => f().await,
        }
    }
}
