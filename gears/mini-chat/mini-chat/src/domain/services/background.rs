//! Background processing: outbox handler logic (usage, audit, attachment
//! cleanup, chat cleanup, thread summary), orphan watchdog, upload reaper.

use std::sync::Arc;

use mini_chat_sdk::{AuditEvent, ModelTier, UsageEvent, UsageTokens};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::MiniChatService;
use super::finalize::{FinalizeInput, TurnContext, to_time, turn_audit, usage_event};
use super::stream::provider_user;
use crate::domain::clock;
use crate::domain::error::DomainError;
use crate::domain::quota::{SettlementMethod, TurnReserve};
use crate::infra::audit::AuditOutcome;
use crate::infra::db::entities::{attachments, chat_turns, thread_summaries};
use crate::infra::db::repo;
use crate::infra::db::repo::turns::state;
use crate::infra::llm::types::{InputMessage, InputPart, InputRole, LlmRequest};
use crate::infra::outbox::payloads::{AttachmentCleanupPayload, ChatCleanupPayload, ThreadSummaryPayload};
use crate::infra::policy::PublishOutcome;

/// Handler outcome (maps to the outbox `MessageResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerOutcome {
    Ok,
    Retry(String),
    Reject(String),
}

/// Maximum audit delivery attempts before dead-lettering.
pub const AUDIT_MAX_ATTEMPTS: u32 = 120;

const SUMMARY_INTRO: &str = "Summarize the following conversation:";
const SUMMARY_MERGE: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";
const SUMMARY_ANALYSIS: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

/// Builds the user prompt of the summary request.
#[must_use]
pub fn summary_user_prompt(existing: Option<&str>, messages: &[(String, String)], limit: usize) -> String {
    let mut out = String::new();
    if let Some(s) = existing {
        out.push_str(SUMMARY_MERGE);
        out.push_str("\n\n<existing_summary>\n");
        out.push_str(s);
        out.push_str("\n</existing_summary>\n\nNew messages to incorporate:\n\n");
    } else {
        out.push_str(SUMMARY_INTRO);
        out.push_str("\n\n");
    }
    let entries: Vec<String> = messages
        .iter()
        .map(|(role, content)| {
            let who = if role == "assistant" { "Assistant" } else { "User" };
            let text = if limit > 0 && content.chars().count() > limit {
                format!("{}...", content.chars().take(limit).collect::<String>())
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

/// Extracts the stored summary from the model output.
#[must_use]
pub fn parse_summary(output: &str) -> String {
    let mut text = output.to_owned();
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
                String::new()
            } else {
                text
            }
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

impl MiniChatService {
    /// Usage outbox handler logic.
    pub async fn handle_usage(&self, payload: &[u8]) -> HandlerOutcome {
        let ev: UsageEvent = match serde_json::from_slice(payload) {
            Ok(e) => e,
            Err(e) => return HandlerOutcome::Reject(format!("invalid usage payload: {e}")),
        };
        match self.policy.publish_usage(ev).await {
            PublishOutcome::Ok => HandlerOutcome::Ok,
            PublishOutcome::Retry(e) => HandlerOutcome::Retry(e),
            PublishOutcome::Reject(e) => HandlerOutcome::Reject(e),
        }
    }

    /// Audit outbox handler logic.
    pub async fn handle_audit(&self, payload: &[u8], attempts: u32) -> HandlerOutcome {
        let ev: AuditEvent = match serde_json::from_slice(payload) {
            Ok(e) => e,
            Err(e) => {
                self.metrics.inc("audit_emit", 1, &[("result", "reject".to_owned())]);
                return HandlerOutcome::Reject(format!("invalid audit payload: {e}"));
            }
        };
        let (result, outcome) = match self.audit.emit(ev).await {
            AuditOutcome::Ok => ("ok", HandlerOutcome::Ok),
            AuditOutcome::Dropped => {
                tracing::warn!("no audit plugin registered; audit event dropped");
                ("dropped", HandlerOutcome::Ok)
            }
            AuditOutcome::Reject(e) => ("reject", HandlerOutcome::Reject(e)),
            AuditOutcome::Retry(e) => {
                if attempts + 1 >= AUDIT_MAX_ATTEMPTS {
                    ("reject", HandlerOutcome::Reject(format!("audit delivery failed after {AUDIT_MAX_ATTEMPTS} attempts: {e}")))
                } else {
                    ("retry", HandlerOutcome::Retry(e))
                }
            }
        };
        self.metrics.inc("audit_emit", 1, &[("result", result.to_owned())]);
        outcome
    }

    fn system_ctx(&self, tenant_id: Uuid) -> SecurityContext {
        self.llm.s2s().get().unwrap_or_else(|| {
            SecurityContext::builder()
                .subject_id(toolkit_security::constants::DEFAULT_SUBJECT_ID)
                .subject_tenant_id(tenant_id)
                .build()
                .unwrap_or_else(|_| SecurityContext::anonymous())
        })
    }

    /// Deletes the provider file of an attachment row; updates its cleanup state.
    /// Returns `Ok(true)` when terminal (`done`/`failed`), `Ok(false)` when still pending.
    async fn cleanup_attachment_file(&self, row: &attachments::Model, provider_file_id: Option<&str>) -> Result<bool, DomainError> {
        let conn = self.db.conn()?;
        let now = clock::now();
        let Some(file_id) = provider_file_id else {
            repo::attachments::update(
                &conn,
                row.tenant_id,
                row.id,
                vec![
                    (attachments::Column::CleanupStatus, Expr::value("done")),
                    (attachments::Column::CleanupUpdatedAt, Expr::value(now)),
                ],
                None,
            )
            .await?;
            return Ok(true);
        };
        let ctx = self.system_ctx(row.tenant_id);
        let result = match self.llm.resolver().storage_by_label(&row.storage_backend, row.tenant_id) {
            Some(target) => self.storage.delete_file(&ctx, &target, file_id).await.map_err(|e| e.message),
            None => Err(format!("unknown storage backend '{}'", row.storage_backend)),
        };
        match result {
            Ok(()) => {
                repo::attachments::update(
                    &conn,
                    row.tenant_id,
                    row.id,
                    vec![
                        (attachments::Column::CleanupStatus, Expr::value("done")),
                        (attachments::Column::CleanupUpdatedAt, Expr::value(now)),
                    ],
                    None,
                )
                .await?;
                self.metrics.inc("cleanup_completed", 1, &[("resource_type", "file".to_owned())]);
                Ok(true)
            }
            Err(e) => {
                let attempts = row.cleanup_attempts + 1;
                let max = i32::try_from(self.cfg.cleanup_worker.max_attempts).unwrap_or(i32::MAX);
                let terminal = attempts >= max;
                repo::attachments::update(
                    &conn,
                    row.tenant_id,
                    row.id,
                    vec![
                        (attachments::Column::CleanupAttempts, Expr::value(attempts)),
                        (attachments::Column::LastCleanupError, Expr::value(e.clone())),
                        (attachments::Column::CleanupUpdatedAt, Expr::value(now)),
                        (
                            attachments::Column::CleanupStatus,
                            Expr::value(if terminal { "failed" } else { "pending" }),
                        ),
                    ],
                    None,
                )
                .await?;
                if terminal {
                    self.metrics.inc("cleanup_failed", 1, &[("resource_type", "file".to_owned())]);
                } else {
                    self.metrics.inc(
                        "cleanup_retry",
                        1,
                        &[("resource_type", "file".to_owned()), ("reason", "provider_error".to_owned())],
                    );
                }
                Ok(terminal)
            }
        }
    }

    /// Attachment cleanup handler logic.
    pub async fn handle_attachment_cleanup(&self, payload: &[u8]) -> HandlerOutcome {
        let p: AttachmentCleanupPayload = match serde_json::from_slice(payload) {
            Ok(p) => p,
            Err(e) => return HandlerOutcome::Reject(format!("invalid payload: {e}")),
        };
        let res: Result<HandlerOutcome, DomainError> = async {
            let conn = self.db.conn()?;
            let chat = repo::chats::find_including_deleted(&conn, p.tenant_id, p.chat_id).await?;
            if chat.is_none_or(|c| c.deleted_at.is_some()) {
                return Ok(HandlerOutcome::Ok);
            }
            let Some(row) = repo::attachments::find_by_id(&conn, p.attachment_id).await? else {
                return Ok(HandlerOutcome::Ok);
            };
            if row.cleanup_status.as_deref() == Some("done") {
                return Ok(HandlerOutcome::Ok);
            }
            let terminal = self
                .cleanup_attachment_file(&row, p.provider_file_id.as_deref())
                .await?;
            if !terminal {
                return Ok(HandlerOutcome::Retry("provider file delete failed".to_owned()));
            }
            let row = repo::attachments::find_by_id(&conn, p.attachment_id).await?;
            if row.and_then(|r| r.cleanup_status).as_deref() == Some("failed") {
                return Ok(HandlerOutcome::Reject("provider file delete: max attempts reached".to_owned()));
            }
            Ok(HandlerOutcome::Ok)
        }
        .await;
        res.unwrap_or_else(|e| HandlerOutcome::Retry(e.to_string()))
    }

    /// Chat cleanup handler logic.
    pub async fn handle_chat_cleanup(&self, payload: &[u8], attempts: u32) -> HandlerOutcome {
        let p: ChatCleanupPayload = match serde_json::from_slice(payload) {
            Ok(p) => p,
            Err(e) => return HandlerOutcome::Reject(format!("invalid payload: {e}")),
        };
        let res: Result<HandlerOutcome, DomainError> = async {
            let conn = self.db.conn()?;
            let chat = repo::chats::find_including_deleted(&conn, p.tenant_id, p.chat_id).await?;
            if chat.is_none_or(|c| c.deleted_at.is_none()) {
                return Ok(HandlerOutcome::Reject("chat is not soft-deleted".to_owned()));
            }
            let rows = repo::attachments::list_all_for_chat(&conn, p.tenant_id, p.chat_id).await?;
            let mut pending = false;
            let mut any_failed = false;
            for row in &rows {
                match row.cleanup_status.as_deref() {
                    Some("pending") => {
                        if !self
                            .cleanup_attachment_file(row, row.provider_file_id.as_deref())
                            .await?
                        {
                            pending = true;
                        }
                    }
                    Some("failed") => any_failed = true,
                    _ => {}
                }
            }
            if pending {
                return Ok(HandlerOutcome::Retry("attachment cleanup pending".to_owned()));
            }
            let rows = repo::attachments::list_all_for_chat(&conn, p.tenant_id, p.chat_id).await?;
            any_failed |= rows.iter().any(|r| r.cleanup_status.as_deref() == Some("failed"));
            if let Some(vs) = repo::vector_stores::find(&conn, p.tenant_id, p.chat_id).await? {
                if any_failed {
                    self.metrics.inc("cleanup_vector_store_with_failed_attachments", 1, &[]);
                }
                let deleted = match &vs.vector_store_id {
                    None => Ok(()),
                    Some(vs_id) => {
                        let ctx = self.system_ctx(p.tenant_id);
                        match self.llm.resolver().storage_by_label(&vs.provider, p.tenant_id) {
                            Some(target) => self
                                .storage
                                .delete_vector_store(&ctx, &target, vs_id)
                                .await
                                .map_err(|e| e.message),
                            None => Err(format!("unknown storage backend '{}'", vs.provider)),
                        }
                    }
                };
                match deleted {
                    Ok(()) => {
                        repo::vector_stores::delete_row(&conn, p.tenant_id, vs.id).await?;
                        self.metrics.inc("cleanup_completed", 1, &[("resource_type", "vector_store".to_owned())]);
                    }
                    Err(e) => {
                        self.metrics.inc(
                            "cleanup_retry",
                            1,
                            &[("resource_type", "vector_store".to_owned()), ("reason", "vector_store_delete_failed".to_owned())],
                        );
                        if attempts + 1 >= self.cfg.cleanup_worker.max_attempts {
                            self.metrics.inc("cleanup_failed", 1, &[("resource_type", "vector_store".to_owned())]);
                            return Ok(HandlerOutcome::Reject(format!(
                                "vector store delete: max attempts ({}) reached",
                                self.cfg.cleanup_worker.max_attempts
                            )));
                        }
                        return Ok(HandlerOutcome::Retry(e));
                    }
                }
            }
            Ok(HandlerOutcome::Ok)
        }
        .await;
        res.unwrap_or_else(|e| HandlerOutcome::Retry(e.to_string()))
    }

    /// Thread summary handler logic.
    // `integer_division`: floored bytes-per-token estimate is intended.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity, clippy::integer_division)]
    pub async fn handle_thread_summary(self: &Arc<Self>, payload: &[u8], attempts: u32) -> HandlerOutcome {
        let p: ThreadSummaryPayload = match serde_json::from_slice(payload) {
            Ok(p) => p,
            Err(e) => return HandlerOutcome::Reject(format!("invalid payload: {e}")),
        };
        let max = self.cfg.thread_summary_worker.max_attempts;
        let retry = |reason: String| {
            if attempts + 1 >= max {
                HandlerOutcome::Reject(reason)
            } else {
                HandlerOutcome::Retry(reason)
            }
        };
        let exec = |result: &str| {
            self.metrics
                .inc("thread_summary_execution", 1, &[("result", result.to_owned())]);
        };
        let cfg = &self.cfg.thread_summary_worker;
        let model_id = cfg.effective_summary_model_id().to_owned();
        let system_user = toolkit_security::constants::DEFAULT_SUBJECT_ID;
        let snapshot = match self.policy.current_snapshot(system_user).await {
            Ok(s) => s,
            Err(e) => {
                exec("retry");
                return retry(e.to_string());
            }
        };
        let Some(model) = snapshot.find(&model_id).filter(|m| m.enabled).cloned() else {
            tracing::error!(model = %model_id, "thread summary model is missing or disabled");
            exec("model_unavailable");
            return HandlerOutcome::Reject("summary model unavailable".to_owned());
        };
        let conn = match self.db.conn() {
            Ok(c) => c,
            Err(e) => return HandlerOutcome::Retry(e.to_string()),
        };
        let current = match repo::summaries::find(&conn, p.tenant_id, p.chat_id).await {
            Ok(c) => c,
            Err(e) => return HandlerOutcome::Retry(e.to_string()),
        };
        let base = p.base_frontier_created_at.zip(p.base_frontier_message_id);
        match (&current, base) {
            (None, Some(_)) => {
                exec("base_missing");
                return HandlerOutcome::Ok;
            }
            (Some(c), b) if b != Some((c.summarized_up_to_created_at, c.summarized_up_to_message_id)) => {
                self.metrics.inc("thread_summary_cas_conflicts", 1, &[]);
                return HandlerOutcome::Ok;
            }
            _ => {}
        }
        let target = (p.frozen_target_created_at, p.frozen_target_message_id);
        let msgs = match repo::messages::summary_range(&conn, p.tenant_id, p.chat_id, base, target).await {
            Ok(m) => m,
            Err(e) => return HandlerOutcome::Retry(e.to_string()),
        };
        let mut entries: Vec<(String, String)> = msgs
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| (m.role.clone(), m.content.clone()))
            .collect();
        if entries.is_empty() {
            return HandlerOutcome::Ok;
        }
        let system = if model.thread_summary_prompt.trim().is_empty() {
            if cfg.summary_system_prompt.trim().is_empty() {
                crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT.to_owned()
            } else {
                cfg.summary_system_prompt.clone()
            }
        } else {
            model.thread_summary_prompt.clone()
        };
        let existing = current.as_ref().map(|c| c.summary_text.clone());
        // Fit the prompt to the summary model's input budget.
        let mut budget = i64::from(model.context_window) - i64::from(model.max_output_tokens);
        if model.max_input_tokens > 0 {
            budget = budget.min(i64::from(model.max_input_tokens));
        }
        let bpt = i64::from(model.estimation_budgets.bytes_per_token_conservative.max(1));
        let mut prompt = summary_user_prompt(existing.as_deref(), &entries, cfg.message_content_limit);
        if model.context_window > 0 {
            while i64::try_from(prompt.len() + system.len()).unwrap_or(i64::MAX) / bpt > budget && entries.len() > 2 {
                let drop_n = entries.len().div_ceil(5).min(entries.len() - 2);
                entries.drain(..drop_n);
                prompt = summary_user_prompt(existing.as_deref(), &entries, cfg.message_content_limit);
            }
        }
        let Some(target_provider) = self.llm.resolver().resolve(&model.provider_id, p.tenant_id) else {
            exec("retry");
            return retry(format!("unknown provider '{}'", model.provider_id));
        };
        let mut metadata = serde_json::Map::new();
        metadata.insert("tenant_id".to_owned(), p.tenant_id.to_string().into());
        metadata.insert("user_id".to_owned(), system_user.to_string().into());
        metadata.insert("chat_id".to_owned(), p.chat_id.to_string().into());
        metadata.insert("request_type".to_owned(), "summary".into());
        metadata.insert("feature".to_owned(), "none".into());
        let req = LlmRequest {
            model: model.provider_model_id.clone(),
            instructions: system,
            input: vec![InputMessage {
                role: InputRole::User,
                parts: vec![InputPart::Text(prompt)],
                is_current: true,
            }],
            tools: Vec::new(),
            max_output_tokens: model.max_output_tokens,
            max_tool_calls: None,
            user: provider_user(p.tenant_id, system_user),
            metadata,
            api_params: model.general_config.api_params.clone(),
            stream: false,
        };
        let ctx = self.system_ctx(p.tenant_id);
        let completion = match self.llm.complete(&target_provider, &req, &ctx).await {
            Ok(c) => c,
            Err(f) => {
                tracing::warn!(error = %f.message, "thread summary call failed");
                exec("provider_error");
                self.metrics.inc("summary_fallback", 1, &[]);
                return retry(f.message);
            }
        };
        let summary = parse_summary(&completion.text);
        if summary.is_empty() {
            exec("empty_summary");
            return retry("empty summary".to_owned());
        }
        let usage = completion.usage.unwrap_or_default();
        let est = usage.output_tokens - usage.reasoning_tokens;
        let token_estimate = if est > 0 {
            i32::try_from(est).unwrap_or(i32::MAX)
        } else {
            i32::try_from(summary.len().div_ceil(4)).unwrap_or(i32::MAX)
        };
        let ids: Vec<Uuid> = msgs.iter().map(|m| m.id).collect();
        let svc = Arc::clone(self);
        let pp = p.clone();
        let text = summary.clone();
        let model_id2 = model.id.clone();
        let result: Result<&'static str, DomainError> = self
            .transact(move |tx| {
                Box::pin(async move {
                    let now = clock::now();
                    let target_msg = repo::messages::find_by_id(tx, pp.frozen_target_message_id).await?;
                    if target_msg.is_none_or(|m| m.deleted_at.is_some()) {
                        return Ok(("frontier_deleted", toolkit_db::outbox::Wake::empty()));
                    }
                    let target = (pp.frozen_target_created_at, pp.frozen_target_message_id);
                    match pp.base_frontier_created_at.zip(pp.base_frontier_message_id) {
                        None => {
                            if repo::summaries::find(tx, pp.tenant_id, pp.chat_id).await?.is_some() {
                                return Ok(("cas_conflict", toolkit_db::outbox::Wake::empty()));
                            }
                            let am: thread_summaries::ActiveModel = thread_summaries::Model {
                                id: Uuid::new_v4(),
                                tenant_id: pp.tenant_id,
                                chat_id: pp.chat_id,
                                summary_text: text.clone(),
                                summarized_up_to_created_at: target.0,
                                summarized_up_to_message_id: target.1,
                                token_estimate,
                                created_at: now,
                                updated_at: now,
                            }
                            .into();
                            repo::summaries::insert(tx, pp.tenant_id, am).await?;
                        }
                        Some(base) => {
                            let n = repo::summaries::cas_update(tx, pp.tenant_id, pp.chat_id, base, &text, target, token_estimate, now).await?;
                            if n == 0 {
                                return Ok(("cas_conflict", toolkit_db::outbox::Wake::empty()));
                            }
                        }
                    }
                    repo::messages::mark_compressed(tx, pp.tenant_id, &ids).await?;
                    let ev = UsageEvent {
                        tenant_id: pp.tenant_id,
                        user_id: None,
                        chat_id: pp.chat_id,
                        turn_id: None,
                        request_id: pp.system_request_id,
                        effective_model: model_id2.clone(),
                        selected_model: model_id2.clone(),
                        terminal_state: "completed".to_owned(),
                        billing_outcome: "system_task".to_owned(),
                        usage: Some(usage),
                        actual_credits_micro: 0,
                        settlement_method: "none".to_owned(),
                        policy_version_applied: 0,
                        web_search_calls: 0,
                        code_interpreter_calls: 0,
                        file_search_calls: 0,
                        timestamp: to_time(now),
                        requester_type: "system".to_owned(),
                        dedupe_key: format!(
                            "{}/thread_summary_update/{}",
                            pp.tenant_id.simple(),
                            pp.system_request_id.simple()
                        ),
                        system_task_type: Some("thread_summary_update".to_owned()),
                    };
                    let wake = svc.outbox.usage(tx, &ev).await?;
                    Ok(("success", wake))
                })
            })
            .await;
        match result {
            Ok("cas_conflict") | Err(DomainError::UniqueViolation) => {
                self.metrics.inc("thread_summary_cas_conflicts", 1, &[]);
                HandlerOutcome::Ok
            }
            Ok(r) => {
                exec(r);
                HandlerOutcome::Ok
            }
            Err(e) => {
                exec("retry");
                retry(e.to_string())
            }
        }
    }

    /// One orphan watchdog scan; returns the number of finalized turns.
    pub async fn orphan_scan(self: &Arc<Self>) -> usize {
        let started = std::time::Instant::now();
        let cutoff = clock::now() - chrono::Duration::seconds(i64::try_from(self.cfg.orphan_watchdog.timeout_secs).unwrap_or(300));
        let Ok(conn) = self.db.conn() else { return 0 };
        let candidates = repo::turns::orphan_candidates(&conn, cutoff, 100).await.unwrap_or_default();
        let mut finalized = 0;
        for turn in candidates {
            self.metrics.inc("orphan_detected", 1, &[("reason", "stale_progress".to_owned())]);
            match self.finalize_orphan(&turn, cutoff).await {
                Ok(true) => {
                    finalized += 1;
                    self.metrics.inc("orphan_finalized", 1, &[("reason", "stale_progress".to_owned())]);
                    self.metrics.inc("streams_aborted", 1, &[("trigger", "orphan_timeout".to_owned())]);
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, turn_id = %turn.id, "orphan finalization failed"),
            }
        }
        self.metrics
            .record("orphan_scan_duration_seconds", started.elapsed().as_secs_f64(), &[]);
        finalized
    }

    async fn finalize_orphan(self: &Arc<Self>, turn: &chat_turns::Model, cutoff: chrono::DateTime<chrono::Utc>) -> Result<bool, DomainError> {
        let user = turn.requester_user_id.unwrap_or_default();
        let effective = turn.effective_model.clone().unwrap_or_default();
        let version = turn.policy_version_applied.and_then(|v| u64::try_from(v).ok()).unwrap_or(0);
        let mut reserve = None;
        if let (Some(rt), Some(mo), Some(rc), Some(floor), Some(_)) = (
            turn.reserve_tokens,
            turn.max_output_tokens_applied,
            turn.reserved_credits_micro,
            turn.minimal_generation_floor_applied,
            turn.requester_user_id,
        ) {
            let snap = self.policy.snapshot_version(user, version).await?;
            if let Some(m) = snap.find(&effective) {
                reserve = Some(TurnReserve {
                    tenant_id: turn.tenant_id,
                    user_id: user,
                    reserve_tokens: rt,
                    max_output_tokens_applied: i64::from(mo),
                    reserved_credits_micro: rc,
                    minimal_generation_floor_applied: i64::from(floor),
                    premium: m.tier == ModelTier::Premium,
                    in_mult: m.input_tokens_credit_multiplier_micro,
                    out_mult: m.output_tokens_credit_multiplier_micro,
                    daily_start: clock::day_start(turn.started_at),
                    monthly_start: clock::month_start(turn.started_at),
                });
            } else {
                tracing::warn!(turn_id = %turn.id, "orphan settlement skipped: model not in snapshot");
            }
        } else {
            tracing::warn!(turn_id = %turn.id, "orphan settlement skipped: reserve fields are NULL");
        }
        let t = TurnContext {
            tenant_id: turn.tenant_id,
            user_id: user,
            chat_id: turn.chat_id,
            turn_id: turn.id,
            request_id: turn.request_id,
            message_id: Uuid::nil(),
            selected_model: effective.clone(),
            effective_model: effective,
            downgrade: false,
            downgrade_reason: None,
            policy_version: version,
            reserve: reserve.clone(),
            requester_type: "user",
        };
        let input = FinalizeInput {
            text: String::new(),
            counters: (turn.web_search_completed_count, turn.code_interpreter_completed_count, turn.file_search_completed_count),
            ttft_ms: None,
            total_ms: u64::try_from((clock::now() - turn.started_at).num_milliseconds()).unwrap_or(0),
            plan: None,
            has_summary: false,
        };
        let svc = Arc::clone(self);
        let turn_id = turn.id;
        self.transact(move |tx| {
            Box::pin(async move {
                let now = clock::now();
                let n = repo::turns::orphan_cas(tx, turn_id, cutoff, now).await?;
                if n == 0 {
                    return Ok((false, toolkit_db::outbox::Wake::empty()));
                }
                let (credits, method) = match &reserve {
                    Some(r) => {
                        let res = svc.quota.settle(tx, r, SettlementMethod::Estimated, input.counters.0, input.counters.1, now).await?;
                        (res.committed_credits_micro, SettlementMethod::Estimated)
                    }
                    None => (0, SettlementMethod::Estimated),
                };
                let mut ev = usage_event(&t, state::FAILED, "aborted", method, credits, input.counters, None);
                if reserve.is_none() {
                    ev.effective_model = String::new();
                    ev.selected_model = String::new();
                    ev.policy_version_applied = 0;
                }
                let mut wake = svc.outbox.usage(tx, &ev).await?;
                let audit = turn_audit(&t, state::FAILED, Some("orphan_timeout"), None::<UsageTokens>, &input, "unknown");
                wake += svc.outbox.audit(tx, t.tenant_id, &audit).await?;
                Ok((true, wake))
            })
        })
        .await
    }

    /// One upload reaper scan; returns the number of reaped rows.
    pub async fn upload_reaper_scan(self: &Arc<Self>) -> usize {
        let started = std::time::Instant::now();
        let cutoff = clock::now() - chrono::Duration::seconds(i64::try_from(self.cfg.upload_reaper.stale_after_secs).unwrap_or(300));
        let Ok(conn) = self.db.conn() else { return 0 };
        let rows = repo::attachments::stale_uploads(&conn, cutoff, 100).await.unwrap_or_default();
        let mut reaped = 0;
        for row in rows {
            let svc = Arc::clone(self);
            let r = row.clone();
            let res: Result<bool, DomainError> = self
                .transact(move |tx| {
                    Box::pin(async move {
                        let now = clock::now();
                        let mut sets = vec![
                            (attachments::Column::Status, Expr::value("failed")),
                            (attachments::Column::ErrorCode, Expr::value("upload_abandoned")),
                            (attachments::Column::UpdatedAt, Expr::value(now)),
                        ];
                        if r.provider_file_id.is_some() {
                            sets.push((attachments::Column::CleanupStatus, Expr::value("pending")));
                            sets.push((attachments::Column::CleanupUpdatedAt, Expr::value(now)));
                        }
                        let n = repo::attachments::update(
                            tx,
                            r.tenant_id,
                            r.id,
                            sets,
                            Some(
                                Condition::all()
                                    .add(attachments::Column::Status.eq(r.status.clone()))
                                    .add(attachments::Column::DeletedAt.is_null())
                                    .add(attachments::Column::CleanupStatus.is_null())
                                    .add(attachments::Column::UpdatedAt.lt(cutoff)),
                            ),
                        )
                        .await?;
                        if n == 0 {
                            return Ok((false, toolkit_db::outbox::Wake::empty()));
                        }
                        let wake = if r.provider_file_id.is_some() {
                            if r.secondary_file_id.is_some() {
                                tracing::warn!(attachment_id = %r.id, "secondary file of an abandoned upload is not deleted");
                            }
                            svc.outbox
                                .attachment_cleanup(
                                    tx,
                                    &AttachmentCleanupPayload {
                                        event_type: "attachment_upload_abandoned".to_owned(),
                                        tenant_id: r.tenant_id,
                                        chat_id: r.chat_id,
                                        attachment_id: r.id,
                                        provider_file_id: r.provider_file_id.clone(),
                                        vector_store_id: None,
                                        storage_backend: r.storage_backend.clone(),
                                        attachment_kind: r.attachment_kind.clone(),
                                        deleted_at: now,
                                        secondary_ref: None,
                                    },
                                )
                                .await?
                        } else {
                            toolkit_db::outbox::Wake::empty()
                        };
                        Ok((true, wake))
                    })
                })
                .await;
            match res {
                Ok(true) => {
                    reaped += 1;
                    self.metrics.inc("attachment_upload_abandoned", 1, &[("from_status", row.status.clone())]);
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, attachment_id = %row.id, "upload reaper failed"),
            }
        }
        self.metrics
            .record("upload_reaper_scan_duration_seconds", started.elapsed().as_secs_f64(), &[]);
        reaped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_parsing() {
        let out = "<analysis>thinking</analysis>\n<summary>\nA\n\n\n\nB\n</summary>";
        assert_eq!(parse_summary(out), "A\n\nB");
        assert_eq!(parse_summary("plain summary"), "plain summary");
        assert_eq!(parse_summary("<analysis>unterminated"), "");
        assert_eq!(parse_summary("<summary>open only"), "");
    }

    #[test]
    fn summary_prompt_shapes() {
        let msgs = vec![
            ("user".to_owned(), "hello".to_owned()),
            ("assistant".to_owned(), "x".repeat(10)),
        ];
        let p = summary_user_prompt(None, &msgs, 5);
        assert!(p.starts_with(SUMMARY_INTRO));
        assert!(p.contains("User: hello"));
        assert!(p.contains("Assistant: xxxxx..."));
        assert!(p.ends_with("Respond with an <analysis> block followed by a <summary> block."));
        let p = summary_user_prompt(Some("old"), &msgs, 0);
        assert!(p.contains("<existing_summary>\nold\n</existing_summary>"));
        assert!(p.contains("New messages to incorporate:"));
    }
}
