//! Background work: attachment / chat cleanup handlers, the thread-summary
//! handler, the orphan watchdog and the upload reaper.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mini_chat_sdk::audit::QuotaDecisionAudit;
use mini_chat_sdk::usage::{UsageTokens, billing_outcome, requester_type, settlement_method, system_task_dedupe_key};
use mini_chat_sdk::{ModelTier, UsageEvent};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage, Wake};
use toolkit_db::secure::{AccessScope, SecureDeleteExt, SecureEntityExt, SecureUpdateExt, secure_insert};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::attachments::{AttachmentCleanupPayload, event_types};
use super::billing::{TurnState, codes};
use super::chats::ChatCleanupPayload;
use super::clock;
use super::error::DomainError;
use super::finalize::{RunStats, SettleInputs, THREAD_SUMMARY_TASK, ThreadSummaryPayload, settle_and_emit};
use super::periods::{PeriodType, period_start};
use super::service::MiniChat;
use super::summary::{RangeMessage, build_user_prompt, drop_fifth, fit_messages, parse_response, token_estimate};
use crate::config::ProviderKind;
use crate::infra::llm::types::{LlmMessage, LlmMetadata, LlmRequest, LlmRole};
use crate::infra::outbox::{OutboxKind, enqueue_json};
use crate::infra::storage::entity::{attachment, chat, chat_turn, chat_vector_store, message, thread_summary};

/// Rows per watchdog / reaper scan.
const SCAN_BATCH: u64 = 100;

fn tenant_scope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}

/// Attachment-cleanup outbox handler.
pub struct AttachmentCleanupHandler(pub Arc<MiniChat>);

/// Chat-cleanup outbox handler.
pub struct ChatCleanupHandler(pub Arc<MiniChat>);

/// Thread-summary outbox handler.
pub struct ThreadSummaryHandler(pub Arc<MiniChat>);

/// Result of one provider file delete attempt bookkeeping.
enum FileCleanup {
    Done,
    Pending,
    Failed,
}

impl MiniChat {
    fn secondary_alias(&self) -> Option<String> {
        self.llm
            .providers()
            .iter()
            .find(|(_, e)| e.kind == ProviderKind::AnthropicMessages)
            .and_then(|(id, _)| self.llm.resolve(id, Uuid::nil()).ok())
            .map(|p| p.alias)
    }

    /// Delete the provider file(s) of one attachment row and record the outcome.
    async fn cleanup_attachment_row(
        &self,
        scope: &AccessScope,
        row: &attachment::Model,
        secondary_alias: Option<String>,
    ) -> Result<FileCleanup, DomainError> {
        let now = clock::now();
        let guard = Condition::all()
            .add(attachment::Column::Id.eq(row.id))
            .add(attachment::Column::CleanupStatus.eq("pending"));
        let Some(file_id) = row.provider_file_id.clone() else {
            let conn = self.db.conn()?;
            attachment::Entity::update_many()
                .secure()
                .col_expr(attachment::Column::CleanupStatus, Expr::value(Some("done".to_owned())))
                .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                .filter(guard)
                .scope_with(scope)
                .exec(&conn)
                .await?;
            return Ok(FileCleanup::Done);
        };
        let ctx = self.system_ctx_for(row.tenant_id);
        let result = match self.llm.storage_by_backend(&row.storage_backend, row.tenant_id) {
            Ok(target) => self.llm.delete_file(&ctx, &target, &file_id).await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let conn = self.db.conn()?;
        match result {
            Ok(()) => {
                if let Some(sid) = &row.secondary_file_id {
                    match secondary_alias {
                        Some(alias) => {
                            if let Err(e) = self.llm.delete_anthropic_file(&ctx, &alias, sid).await {
                                tracing::warn!(error = %e, attachment_id = %row.id, "secondary file delete failed");
                            }
                        }
                        None => self.metrics.secondary_cleanup_skipped(row.secondary_provider_kind.as_deref().unwrap_or("unknown")),
                    }
                }
                attachment::Entity::update_many()
                    .secure()
                    .col_expr(attachment::Column::CleanupStatus, Expr::value(Some("done".to_owned())))
                    .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                    .filter(guard)
                    .scope_with(scope)
                    .exec(&conn)
                    .await?;
                self.metrics.cleanup_completed("file");
                Ok(FileCleanup::Done)
            }
            Err(msg) => {
                let attempts = row.cleanup_attempts + 1;
                let terminal = u32::try_from(attempts).unwrap_or(u32::MAX) >= self.cfg.cleanup_worker.max_attempts;
                let status = if terminal { "failed" } else { "pending" };
                attachment::Entity::update_many()
                    .secure()
                    .col_expr(attachment::Column::CleanupAttempts, Expr::value(attempts))
                    .col_expr(attachment::Column::LastCleanupError, Expr::value(Some(msg.chars().take(1000).collect::<String>())))
                    .col_expr(attachment::Column::CleanupStatus, Expr::value(Some(status.to_owned())))
                    .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                    .filter(guard)
                    .scope_with(scope)
                    .exec(&conn)
                    .await?;
                if terminal {
                    self.metrics.cleanup_failed("file");
                    Ok(FileCleanup::Failed)
                } else {
                    self.metrics.cleanup_retry("file", "provider_error");
                    Ok(FileCleanup::Pending)
                }
            }
        }
    }

    async fn handle_attachment_cleanup(&self, p: AttachmentCleanupPayload) -> Result<MessageResult, DomainError> {
        let scope = tenant_scope(p.tenant_id);
        let conn = self.db.conn()?;
        let chat = chat::Entity::find()
            .filter(chat::Column::Id.eq(p.chat_id))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await?;
        if chat.is_none_or(|c| c.deleted_at.is_some()) {
            return Ok(MessageResult::Ok);
        }
        let row = attachment::Entity::find()
            .filter(attachment::Column::Id.eq(p.attachment_id))
            .filter(attachment::Column::ChatId.eq(p.chat_id))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await?;
        #[allow(clippy::drop_non_drop, reason = "explicitly release the DB conn handle before further awaits/transactions")]
        drop(conn);
        let Some(mut row) = row else {
            return Ok(MessageResult::Ok);
        };
        if row.cleanup_status.as_deref() != Some("pending") {
            return Ok(MessageResult::Ok);
        }
        if row.provider_file_id.is_none() {
            row.provider_file_id = p.provider_file_id.clone();
        }
        let alias = p.secondary_ref.as_ref().map(|s| s.upstream_alias.clone()).or_else(|| self.secondary_alias());
        if p.secondary_ref.is_none() {
            row.secondary_file_id = None;
        }
        Ok(match self.cleanup_attachment_row(&scope, &row, alias).await? {
            FileCleanup::Done => MessageResult::Ok,
            FileCleanup::Pending => MessageResult::Retry,
            FileCleanup::Failed => MessageResult::Reject(format!(
                "attachment cleanup: max attempts ({}) reached",
                self.cfg.cleanup_worker.max_attempts
            )),
        })
    }

    async fn handle_chat_cleanup(&self, p: ChatCleanupPayload, attempts: i16) -> Result<MessageResult, DomainError> {
        let scope = tenant_scope(p.tenant_id);
        let conn = self.db.conn()?;
        let chat = chat::Entity::find()
            .filter(chat::Column::Id.eq(p.chat_id))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await?;
        if chat.is_none_or(|c| c.deleted_at.is_none()) {
            return Ok(MessageResult::Reject("chat is not soft-deleted".to_owned()));
        }
        let rows = attachment::Entity::find()
            .filter(attachment::Column::ChatId.eq(p.chat_id))
            .filter(attachment::Column::CleanupStatus.eq("pending"))
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await?;
        #[allow(clippy::drop_non_drop, reason = "explicitly release the DB conn handle before further awaits/transactions")]
        drop(conn);
        let alias = self.secondary_alias();
        let mut pending = false;
        for row in &rows {
            if let FileCleanup::Pending = self.cleanup_attachment_row(&scope, row, alias.clone()).await? {
                pending = true;
            }
        }
        if pending {
            return Ok(MessageResult::Retry);
        }
        let conn = self.db.conn()?;
        let any_failed = attachment::Entity::find()
            .filter(attachment::Column::ChatId.eq(p.chat_id))
            .filter(attachment::Column::CleanupStatus.eq("failed"))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await?
            .is_some();
        let vs_row = chat_vector_store::Entity::find()
            .filter(chat_vector_store::Column::TenantId.eq(p.tenant_id))
            .filter(chat_vector_store::Column::ChatId.eq(p.chat_id))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await?;
        #[allow(clippy::drop_non_drop, reason = "explicitly release the DB conn handle before further awaits/transactions")]
        drop(conn);
        let Some(vs_row) = vs_row else {
            return Ok(MessageResult::Ok);
        };
        if let Some(vs) = &vs_row.vector_store_id {
            if any_failed {
                self.metrics.cleanup_vs_with_failed();
            }
            let ctx = self.system_ctx_for(p.tenant_id);
            let res = match self.llm.storage_by_backend(&vs_row.provider, p.tenant_id) {
                Ok(target) => self.llm.delete_vector_store(&ctx, &target, vs).await.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            if let Err(e) = res {
                tracing::warn!(error = %e, chat_id = %p.chat_id, "vector store delete failed");
                self.metrics.cleanup_retry("vector_store", "vector_store_delete_failed");
                let max = self.cfg.cleanup_worker.max_attempts;
                if u32::try_from(attempts).unwrap_or(0) + 1 >= max {
                    self.metrics.cleanup_failed("vector_store");
                    return Ok(MessageResult::Reject(format!("vector store delete: max attempts ({max}) reached")));
                }
                return Ok(MessageResult::Retry);
            }
            self.metrics.cleanup_completed("vector_store");
        }
        let conn = self.db.conn()?;
        chat_vector_store::Entity::delete_many()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(chat_vector_store::Column::Id.eq(vs_row.id)))
            .exec(&conn)
            .await?;
        Ok(MessageResult::Ok)
    }

    #[allow(clippy::too_many_lines, clippy::cognitive_complexity, reason = "summary task pipeline")]
    async fn handle_thread_summary(&self, p: ThreadSummaryPayload, attempts: i16) -> MessageResult {
        let last_attempt = u32::try_from(attempts).unwrap_or(0) + 1 >= self.cfg.thread_summary_worker.max_attempts;
        let retry = |reason: &str| {
            if last_attempt {
                MessageResult::Reject(format!("thread summary: {reason} (max attempts reached)"))
            } else {
                MessageResult::Retry
            }
        };
        let scope = tenant_scope(p.tenant_id);
        let model_id = self.cfg.thread_summary_worker.effective_summary_model_id().to_owned();
        let policy = match self.policy.current(Uuid::nil()).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "thread summary: policy unavailable");
                self.metrics.thread_summary_execution("retry");
                return retry("policy unavailable");
            }
        };
        let Some(model) = policy.find_enabled(&model_id).cloned() else {
            tracing::error!(model = %model_id, "thread summary model is missing or disabled");
            self.metrics.thread_summary_execution("model_unavailable");
            return MessageResult::Reject(format!("summary model `{model_id}` unavailable"));
        };
        let provider = match self.llm.resolve(&model.provider_id, p.tenant_id) {
            Ok(pr) => pr,
            Err(e) => {
                tracing::warn!(error = %e, "thread summary: provider resolution failed");
                self.metrics.thread_summary_execution("retry");
                return retry("provider resolution");
            }
        };
        let base = match (p.base_frontier_created_at, p.base_frontier_message_id) {
            (Some(t), Some(id)) => Some((t, id)),
            _ => None,
        };
        let loaded = async {
            let conn = self.db.conn()?;
            let summary = thread_summary::Entity::find()
                .filter(thread_summary::Column::ChatId.eq(p.chat_id))
                .secure()
                .scope_with(&scope)
                .one(&conn)
                .await?;
            let mut q = message::Entity::find()
                .filter(message::Column::ChatId.eq(p.chat_id))
                .filter(message::Column::DeletedAt.is_null())
                .filter(message::Column::IsCompressed.eq(false))
                .filter(
                    Condition::any()
                        .add(message::Column::CreatedAt.lt(p.frozen_target_created_at))
                        .add(
                            Condition::all()
                                .add(message::Column::CreatedAt.eq(p.frozen_target_created_at))
                                .add(message::Column::Id.lte(p.frozen_target_message_id)),
                        ),
                );
            if let Some((bt, bid)) = base {
                q = q.filter(
                    Condition::any()
                        .add(message::Column::CreatedAt.gt(bt))
                        .add(Condition::all().add(message::Column::CreatedAt.eq(bt)).add(message::Column::Id.gt(bid))),
                );
            }
            let msgs = q
                .order_by_asc(message::Column::CreatedAt)
                .order_by_asc(message::Column::Id)
                .secure()
                .scope_with(&scope)
                .all(&conn)
                .await?;
            Ok::<_, DomainError>((summary, msgs))
        }
        .await;
        let (summary, msgs) = match loaded {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "thread summary: load failed");
                self.metrics.thread_summary_execution("retry");
                return retry("db");
            }
        };
        match (&summary, base) {
            (None, Some(_)) => {
                self.metrics.thread_summary_execution("base_missing");
                return MessageResult::Ok;
            }
            (Some(s), b) if b != Some((s.summarized_up_to_created_at, s.summarized_up_to_message_id)) => {
                self.metrics.thread_summary_cas_conflict();
                return MessageResult::Ok;
            }
            _ => {}
        }
        let range: Vec<RangeMessage> = msgs
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| RangeMessage { role: m.role.clone(), content: m.content.clone() })
            .collect();
        if range.is_empty() {
            return MessageResult::Ok;
        }
        let system_prompt = if !model.thread_summary_prompt.trim().is_empty() {
            model.thread_summary_prompt.clone()
        } else if !self.cfg.thread_summary_worker.summary_system_prompt.trim().is_empty() {
            self.cfg.thread_summary_worker.summary_system_prompt.clone()
        } else {
            crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT.to_owned()
        };
        let existing = summary.as_ref().map(|s| s.summary_text.clone());
        let limit = self.cfg.thread_summary_worker.message_content_limit;
        let budget = (model.context_window > 0).then(|| {
            let by_window = u64::from(model.context_window.saturating_sub(model.max_output_tokens));
            if model.max_input_tokens > 0 { by_window.min(u64::from(model.max_input_tokens)) } else { by_window }
        });
        let mut range = fit_messages(
            &system_prompt,
            existing.as_deref(),
            range,
            limit,
            budget,
            model.estimation_budgets.bytes_per_token_conservative,
        );
        let ctx = self.system_ctx_for(p.tenant_id);
        let mut result = None;
        for ptl_retry in 0..=2 {
            let req = LlmRequest {
                provider_model_id: model.provider_model_id.clone(),
                instructions: system_prompt.clone(),
                messages: vec![LlmMessage::text(LlmRole::User, build_user_prompt(existing.as_deref(), &range, limit))],
                max_output_tokens: model.max_output_tokens,
                tools: Vec::new(),
                max_tool_calls: 0,
                api_params: model.general_config.api_params.clone(),
                user: super::stream::provider_user_field(p.tenant_id, toolkit_security::constants::DEFAULT_SUBJECT_ID),
                metadata: LlmMetadata {
                    tenant_id: p.tenant_id.to_string(),
                    user_id: toolkit_security::constants::DEFAULT_SUBJECT_ID.to_string(),
                    chat_id: p.chat_id.to_string(),
                    request_type: "summary",
                    feature: "none".to_owned(),
                },
                stream: false,
                extra_input: Vec::new(),
            };
            match self.llm.complete(&ctx, &provider, &req).await {
                Ok(r) => {
                    result = Some(r);
                    break;
                }
                Err(e) if e.context_length && ptl_retry < 2 && range.len() > 2 => {
                    range = drop_fifth(range);
                }
                Err(e) => {
                    tracing::warn!(kind = e.kind.code(), message = %e.message, "thread summary call failed");
                    break;
                }
            }
        }
        let Some((text, usage)) = result else {
            self.metrics.thread_summary_execution("provider_error");
            self.metrics.summary_fallback();
            return retry("provider error");
        };
        let summary_text = parse_response(&text);
        if summary_text.is_empty() {
            self.metrics.thread_summary_execution("empty_summary");
            return retry("empty summary");
        }
        let u = usage.unwrap_or_default();
        let estimate = token_estimate(u.output_tokens, u.reasoning_tokens, &summary_text);
        let slot = self.outbox.clone();
        let tenant_id = p.tenant_id;
        let chat_id = p.chat_id;
        let target = (p.frozen_target_created_at, p.frozen_target_message_id);
        let system_request_id = p.system_request_id;
        let model_id2 = model.id.clone();
        let committed = self
            .tx(move |tx| {
                Box::pin(async move {
                    let target_live = message::Entity::find()
                        .filter(message::Column::ChatId.eq(chat_id))
                        .filter(message::Column::Id.eq(target.1))
                        .filter(message::Column::DeletedAt.is_null())
                        .secure()
                        .scope_with(&scope)
                        .one(tx)
                        .await?
                        .is_some();
                    if !target_live {
                        return Ok(SummaryCommit::FrontierDeleted);
                    }
                    let now = clock::now();
                    if let Some((bt, bid)) = base {
                        let rows = thread_summary::Entity::update_many()
                            .secure()
                            .col_expr(thread_summary::Column::SummaryText, Expr::value(summary_text.clone()))
                            .col_expr(thread_summary::Column::SummarizedUpToCreatedAt, Expr::value(target.0))
                            .col_expr(thread_summary::Column::SummarizedUpToMessageId, Expr::value(target.1))
                            .col_expr(thread_summary::Column::TokenEstimate, Expr::value(estimate))
                            .col_expr(thread_summary::Column::UpdatedAt, Expr::value(now))
                            .filter(
                                Condition::all()
                                    .add(thread_summary::Column::ChatId.eq(chat_id))
                                    .add(thread_summary::Column::SummarizedUpToCreatedAt.eq(bt))
                                    .add(thread_summary::Column::SummarizedUpToMessageId.eq(bid)),
                            )
                            .scope_with(&scope)
                            .exec(tx)
                            .await?
                            .rows_affected;
                        if rows == 0 {
                            return Ok(SummaryCommit::Conflict);
                        }
                    } else {
                        let ins = secure_insert::<thread_summary::Entity>(
                            thread_summary::ActiveModel {
                                id: Set(Uuid::now_v7()),
                                tenant_id: Set(tenant_id),
                                chat_id: Set(chat_id),
                                summary_text: Set(summary_text.clone()),
                                summarized_up_to_created_at: Set(target.0),
                                summarized_up_to_message_id: Set(target.1),
                                token_estimate: Set(estimate),
                                created_at: Set(now),
                                updated_at: Set(now),
                            },
                            &scope,
                            tx,
                        )
                        .await
                        .map_err(DomainError::from);
                        match ins {
                            Ok(_) => {}
                            Err(DomainError::UniqueViolation(_)) => return Ok(SummaryCommit::Conflict),
                            Err(e) => return Err(e),
                        }
                    }
                    let mut range_cond = Condition::all()
                        .add(message::Column::ChatId.eq(chat_id))
                        .add(message::Column::DeletedAt.is_null())
                        .add(message::Column::IsCompressed.eq(false))
                        .add(
                            Condition::any()
                                .add(message::Column::CreatedAt.lt(target.0))
                                .add(Condition::all().add(message::Column::CreatedAt.eq(target.0)).add(message::Column::Id.lte(target.1))),
                        );
                    if let Some((bt, bid)) = base {
                        range_cond = range_cond.add(
                            Condition::any()
                                .add(message::Column::CreatedAt.gt(bt))
                                .add(Condition::all().add(message::Column::CreatedAt.eq(bt)).add(message::Column::Id.gt(bid))),
                        );
                    }
                    message::Entity::update_many()
                        .secure()
                        .col_expr(message::Column::IsCompressed, Expr::value(true))
                        .filter(range_cond)
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    let ev = UsageEvent {
                        tenant_id,
                        user_id: None,
                        chat_id,
                        turn_id: None,
                        request_id: system_request_id,
                        effective_model: model_id2.clone(),
                        selected_model: model_id2,
                        terminal_state: "completed".to_owned(),
                        billing_outcome: billing_outcome::SYSTEM_TASK.to_owned(),
                        usage: Some(UsageTokens {
                            input_tokens: u.input_tokens,
                            output_tokens: u.output_tokens,
                            cache_read_input_tokens: u.cache_read_input_tokens,
                            cache_write_input_tokens: u.cache_write_input_tokens,
                            reasoning_tokens: u.reasoning_tokens,
                        }),
                        actual_credits_micro: 0,
                        settlement_method: settlement_method::NONE.to_owned(),
                        policy_version_applied: 0,
                        web_search_calls: 0,
                        code_interpreter_calls: 0,
                        file_search_calls: 0,
                        timestamp: clock::to_time(now),
                        requester_type: requester_type::SYSTEM.to_owned(),
                        dedupe_key: system_task_dedupe_key(tenant_id, THREAD_SUMMARY_TASK, system_request_id),
                        system_task_type: Some(THREAD_SUMMARY_TASK.to_owned()),
                    };
                    let w = enqueue_json(&slot, tx, OutboxKind::Usage, chat_id, &ev).await?;
                    Ok(SummaryCommit::Done(w))
                })
            })
            .await;
        match committed {
            Ok(SummaryCommit::Done(w)) => {
                w.fire();
                self.metrics.thread_summary_execution("success");
                MessageResult::Ok
            }
            Ok(SummaryCommit::Conflict) => {
                self.metrics.thread_summary_cas_conflict();
                MessageResult::Ok
            }
            Ok(SummaryCommit::FrontierDeleted) => {
                self.metrics.thread_summary_execution("frontier_deleted");
                MessageResult::Ok
            }
            Err(e) => {
                tracing::warn!(error = %e, "thread summary commit failed");
                self.metrics.thread_summary_execution("retry");
                retry("commit failed")
            }
        }
    }

    /// One orphan-watchdog scan.
    ///
    /// # Errors
    /// DB errors of the candidate query.
    pub async fn orphan_scan(&self) -> Result<usize, DomainError> {
        let started = Instant::now();
        let timeout = chrono::Duration::seconds(i64::try_from(self.cfg.orphan_watchdog.timeout_secs).unwrap_or(300));
        let cutoff = clock::now() - timeout;
        let stale = || {
            Condition::any()
                .add(chat_turn::Column::LastProgressAt.lte(cutoff))
                .add(
                    Condition::all()
                        .add(chat_turn::Column::LastProgressAt.is_null())
                        .add(chat_turn::Column::StartedAt.lte(cutoff)),
                )
        };
        let conn = self.db.conn()?;
        let candidates = chat_turn::Entity::find()
            .filter(chat_turn::Column::State.eq("running"))
            .filter(chat_turn::Column::DeletedAt.is_null())
            .filter(stale())
            .order_by_asc(chat_turn::Column::StartedAt)
            .limit(SCAN_BATCH)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await?;
        #[allow(clippy::drop_non_drop, reason = "explicitly release the DB conn handle before further awaits/transactions")]
        drop(conn);
        let mut finalized = 0;
        for t in candidates {
            self.metrics.orphan_detected();
            match self.finalize_orphan(&t, cutoff).await {
                Ok(true) => {
                    finalized += 1;
                    self.metrics.orphan_finalized();
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, turn_id = %t.id, "orphan finalization failed"),
            }
        }
        self.metrics.orphan_scan(started.elapsed().as_secs_f64());
        Ok(finalized)
    }

    #[allow(clippy::cognitive_complexity, reason = "orphan finalization pipeline")]
    async fn finalize_orphan(&self, t: &chat_turn::Model, cutoff: clock::Timestamp) -> Result<bool, DomainError> {
        let reserve = match (t.reserve_tokens, t.max_output_tokens_applied, t.reserved_credits_micro, t.minimal_generation_floor_applied) {
            (Some(tokens), Some(max_out), Some(credits), Some(floor))
                if t.policy_version_applied.is_some() && t.effective_model.is_some() =>
            {
                Some((tokens, i64::from(max_out), credits, i64::from(floor)))
            }
            _ => None,
        };
        let effective = if reserve.is_some() { t.effective_model.clone().unwrap_or_default() } else { String::new() };
        let policy_version = if reserve.is_some() { u64::try_from(t.policy_version_applied.unwrap_or(0)).unwrap_or(0) } else { 0 };
        let (mults, premium) = match (reserve.is_some(), t.requester_user_id) {
            (true, Some(user)) => match self.policy.snapshot(user, policy_version).await {
                Ok(v) => v.find(&effective).map_or((None, false), |m| {
                    (
                        Some((m.input_tokens_credit_multiplier_micro, m.output_tokens_credit_multiplier_micro)),
                        m.tier == ModelTier::Premium,
                    )
                }),
                Err(e) => {
                    tracing::warn!(error = %e, "orphan settlement: policy snapshot unavailable; charging the reserve");
                    (None, false)
                }
            },
            _ => (None, false),
        };
        if reserve.is_none() || t.requester_user_id.is_none() {
            tracing::warn!(turn_id = %t.id, "orphan turn has no reserve fields; quota settlement skipped");
        }
        let settle = SettleInputs {
            tenant_id: t.tenant_id,
            user_id: t.requester_user_id,
            chat_id: t.chat_id,
            turn_id: t.id,
            request_id: t.request_id,
            selected_model: effective.clone(),
            effective_model: effective,
            state: TurnState::Failed,
            error_code: Some(codes::ORPHAN_TIMEOUT.to_owned()),
            usage: None,
            reserve: if t.requester_user_id.is_some() { reserve } else { None },
            multipliers: mults,
            premium,
            daily_start: period_start(PeriodType::Daily, clock::to_time(t.started_at)),
            monthly_start: period_start(PeriodType::Monthly, clock::to_time(t.started_at)),
            policy_version,
            stats: RunStats {
                web_search: u32::try_from(t.web_search_completed_count).unwrap_or(0),
                code_interpreter: u32::try_from(t.code_interpreter_completed_count).unwrap_or(0),
                file_search: u32::try_from(t.file_search_completed_count).unwrap_or(0),
                reported_file_search: u32::try_from(t.file_search_completed_count).unwrap_or(0),
                ttft_ms: None,
            },
            total_ms: u64::try_from((clock::now() - t.started_at).num_milliseconds()).unwrap_or(0),
            quota_decision: QuotaDecisionAudit { decision: "unknown".to_owned(), downgrade_from: None, downgrade_reason: None },
        };
        let slot = self.outbox.clone();
        let quota = self.quota();
        let tolerance = self.cfg.quota.overshoot_tolerance_factor;
        let child = tenant_scope(t.tenant_id);
        let user_scope = t.requester_user_id.map_or_else(|| child.clone(), |u| child.ensure_owner(u));
        let turn_id = t.id;
        let res: Option<Vec<Wake>> = self
            .tx(move |tx| {
                Box::pin(async move {
                    let now = clock::now();
                    let rows = chat_turn::Entity::update_many()
                        .secure()
                        .col_expr(chat_turn::Column::State, Expr::value("failed"))
                        .col_expr(chat_turn::Column::ErrorCode, Expr::value(Some(codes::ORPHAN_TIMEOUT.to_owned())))
                        .col_expr(chat_turn::Column::CompletedAt, Expr::value(Some(now)))
                        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
                        .filter(
                            Condition::all()
                                .add(chat_turn::Column::Id.eq(turn_id))
                                .add(chat_turn::Column::State.eq("running"))
                                .add(chat_turn::Column::DeletedAt.is_null())
                                .add(
                                    Condition::any()
                                        .add(chat_turn::Column::LastProgressAt.lte(cutoff))
                                        .add(
                                            Condition::all()
                                                .add(chat_turn::Column::LastProgressAt.is_null())
                                                .add(chat_turn::Column::StartedAt.lte(cutoff)),
                                        ),
                                ),
                        )
                        .scope_with(&child)
                        .exec(tx)
                        .await?
                        .rows_affected;
                    if rows == 0 {
                        return Ok(None);
                    }
                    let (wakes, _, _) = settle_and_emit(tx, &slot, &quota, tolerance, &user_scope, &settle).await?;
                    Ok(Some(wakes))
                })
            })
            .await?;
        Ok(match res {
            Some(wakes) => {
                for w in wakes {
                    w.fire();
                }
                true
            }
            None => false,
        })
    }

    /// One upload-reaper scan.
    ///
    /// # Errors
    /// DB errors of the candidate query.
    #[allow(clippy::cognitive_complexity, reason = "per-candidate reaping steps")]
    pub async fn upload_reaper_scan(&self) -> Result<usize, DomainError> {
        let started = Instant::now();
        let stale = chrono::Duration::seconds(i64::try_from(self.cfg.upload_reaper.stale_after_secs).unwrap_or(300));
        let cutoff = clock::now() - stale;
        let conn = self.db.conn()?;
        let rows = attachment::Entity::find()
            .filter(attachment::Column::Status.is_in(["pending", "uploaded"]))
            .filter(attachment::Column::DeletedAt.is_null())
            .filter(attachment::Column::CleanupStatus.is_null())
            .filter(attachment::Column::UpdatedAt.lt(cutoff))
            .order_by_asc(attachment::Column::UpdatedAt)
            .limit(SCAN_BATCH)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await?;
        #[allow(clippy::drop_non_drop, reason = "explicitly release the DB conn handle before further awaits/transactions")]
        drop(conn);
        let mut reaped = 0;
        for row in rows {
            let slot = self.outbox.clone();
            let scope = tenant_scope(row.tenant_id);
            let r = row.clone();
            let res = self
                .tx(move |tx| {
                    Box::pin(async move {
                        let now = clock::now();
                        let mut upd = attachment::Entity::update_many()
                            .secure()
                            .col_expr(attachment::Column::Status, Expr::value("failed"))
                            .col_expr(attachment::Column::ErrorCode, Expr::value(Some("upload_abandoned".to_owned())))
                            .col_expr(attachment::Column::UpdatedAt, Expr::value(now));
                        if r.provider_file_id.is_some() {
                            upd = upd
                                .col_expr(attachment::Column::CleanupStatus, Expr::value(Some("pending".to_owned())))
                                .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)));
                        }
                        let n = upd
                            .filter(
                                Condition::all()
                                    .add(attachment::Column::Id.eq(r.id))
                                    .add(attachment::Column::Status.eq(r.status.clone()))
                                    .add(attachment::Column::DeletedAt.is_null())
                                    .add(attachment::Column::CleanupStatus.is_null())
                                    .add(attachment::Column::UpdatedAt.lt(cutoff)),
                            )
                            .scope_with(&scope)
                            .exec(tx)
                            .await?
                            .rows_affected;
                        if n == 0 {
                            return Ok(None);
                        }
                        if r.provider_file_id.is_none() {
                            return Ok(Some(None));
                        }
                        let payload = AttachmentCleanupPayload {
                            event_type: event_types::UPLOAD_ABANDONED.to_owned(),
                            tenant_id: r.tenant_id,
                            chat_id: r.chat_id,
                            attachment_id: r.id,
                            provider_file_id: r.provider_file_id.clone(),
                            vector_store_id: None,
                            storage_backend: r.storage_backend.clone(),
                            attachment_kind: r.attachment_kind.clone(),
                            deleted_at: now,
                            secondary_ref: None,
                        };
                        Ok(Some(Some(enqueue_json(&slot, tx, OutboxKind::AttachmentCleanup, r.tenant_id, &payload).await?)))
                    })
                })
                .await;
            match res {
                Ok(Some(w)) => {
                    if let Some(w) = w {
                        w.fire();
                    }
                    if let Some(sid) = &row.secondary_file_id {
                        tracing::warn!(attachment_id = %row.id, secondary_file_id = %sid, "abandoned upload has a secondary copy that is not deleted");
                    }
                    reaped += 1;
                    self.metrics.upload_abandoned(&row.status);
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, attachment_id = %row.id, "upload reaper update failed"),
            }
        }
        self.metrics.upload_reaper_scan(started.elapsed().as_secs_f64());
        Ok(reaped)
    }
}

enum SummaryCommit {
    Done(Wake),
    Conflict,
    FrontierDeleted,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: AttachmentCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => return MessageResult::Reject(format!("malformed attachment cleanup payload: {e}")),
        };
        match self.0.handle_attachment_cleanup(p).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "attachment cleanup infrastructure failure");
                MessageResult::Retry
            }
        }
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: ChatCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => return MessageResult::Reject(format!("malformed chat cleanup payload: {e}")),
        };
        match self.0.handle_chat_cleanup(p, msg.attempts).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "chat cleanup infrastructure failure");
                MessageResult::Retry
            }
        }
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: ThreadSummaryPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => return MessageResult::Reject(format!("malformed thread summary payload: {e}")),
        };
        self.0.handle_thread_summary(p, msg.attempts).await
    }
}

/// Run a periodic job until `cancel` fires.
#[allow(clippy::cognitive_complexity, reason = "job loop with per-outcome logging")]
pub async fn run_periodic<F, Fut>(name: &'static str, interval: Duration, cancel: CancellationToken, mut job: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<usize, DomainError>>,
{
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        match job().await {
            Ok(n) if n > 0 => tracing::info!(job = name, processed = n, "background scan"),
            Ok(_) => {}
            Err(e) => tracing::warn!(job = name, error = %e, "background scan failed"),
        }
    }
}
