//! Outbox handler logic (usage, audit, attachment / chat cleanup) and the
//! leader-only scans (orphan watchdog, upload reaper).

use std::sync::Arc;

use mini_chat_sdk::{AuditEvent, LatencyMs, PolicyDecisions, QuotaDecisionAudit, ToolCalls, TurnAuditEvent, UsageEvent, UsageTokens};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::finalize::{SettleCtx, settle_and_enqueue};
use super::{MiniChatService, now};
use crate::domain::billing::{ReserveFields, Terminal};
use crate::domain::error::DomainResult;
use crate::domain::ports::{AuditDelivery, UsagePublishError};
use crate::infra::db::entities::{attachments, chat_turns, chat_vector_stores, chats};
use crate::infra::outbox::{AttachmentCleanupEvent, ChatCleanupEvent, fire};

/// Outcome of one outbox message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerOutcome {
    Ok,
    Retry(String),
    Reject(String),
}

/// Audit delivery attempts before dead-lettering.
pub const AUDIT_MAX_ATTEMPTS: u32 = 120;

impl MiniChatService {
    /// Usage queue: publish through the model policy plugin.
    pub async fn handle_usage(&self, payload: &[u8]) -> HandlerOutcome {
        let ev: UsageEvent = match serde_json::from_slice(payload) {
            Ok(e) => e,
            Err(e) => return HandlerOutcome::Reject(format!("malformed usage payload: {e}")),
        };
        match self.policy.publish_usage(ev).await {
            Ok(()) => HandlerOutcome::Ok,
            Err(UsagePublishError::Resolve(m) | UsagePublishError::Transient(m)) => HandlerOutcome::Retry(m),
            Err(UsagePublishError::Permanent(m)) => HandlerOutcome::Reject(m),
        }
    }

    /// Audit queue: deliver to the audit plugin (payload checked first).
    pub async fn handle_audit(&self, payload: &[u8], attempts: u32) -> HandlerOutcome {
        let ev: AuditEvent = match serde_json::from_slice(payload) {
            Ok(e) => e,
            Err(e) => return HandlerOutcome::Reject(format!("malformed audit payload: {e}")),
        };
        match self.audit.deliver(ev).await {
            AuditDelivery::Ok | AuditDelivery::Dropped => HandlerOutcome::Ok,
            AuditDelivery::Retry(m) => {
                if attempts + 1 >= AUDIT_MAX_ATTEMPTS {
                    HandlerOutcome::Reject(format!("audit delivery: max attempts reached: {m}"))
                } else {
                    HandlerOutcome::Retry(m)
                }
            }
            AuditDelivery::Reject(m) => HandlerOutcome::Reject(m),
        }
    }

    async fn mark_cleanup(&self, tenant_id: Uuid, id: Uuid, status: &str, error: Option<String>, inc_attempt: bool) -> DomainResult<()> {
        use sea_orm::sea_query::ExprTrait as _;
        let conn = self.db.conn()?;
        let mut q = attachments::Entity::update_many()
            .col_expr(attachments::Column::CleanupStatus, Expr::value(status))
            .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(now()));
        if let Some(e) = error {
            q = q.col_expr(attachments::Column::LastCleanupError, Expr::value(e));
        }
        if inc_attempt {
            q = q.col_expr(attachments::Column::CleanupAttempts, Expr::col(attachments::Column::CleanupAttempts).add(1));
        }
        q.filter(attachments::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant_id))
            .exec(&conn)
            .await?;
        Ok(())
    }

    /// Delete one attachment's provider file and record the outcome; returns the new cleanup status.
    async fn cleanup_one(&self, a: &attachments::Model) -> DomainResult<String> {
        let max = i32::try_from(self.cfg.cleanup_worker.max_attempts).unwrap_or(i32::MAX);
        let Some(file_id) = &a.provider_file_id else {
            self.mark_cleanup(a.tenant_id, a.id, "done", None, false).await?;
            return Ok("done".into());
        };
        let target = match self.llm.resolver.rag_target_by_backend(&a.storage_backend, a.tenant_id) {
            Ok(t) => t,
            Err(e) => {
                let failed = a.cleanup_attempts + 1 >= max;
                let st = if failed { "failed" } else { "pending" };
                self.mark_cleanup(a.tenant_id, a.id, st, Some(e), true).await?;
                return Ok(st.into());
            }
        };
        match self.llm.delete_file(&target, file_id).await {
            Ok(_) => {
                self.mark_cleanup(a.tenant_id, a.id, "done", None, false).await?;
                Ok("done".into())
            }
            Err(e) => {
                let failed = a.cleanup_attempts + 1 >= max;
                let st = if failed { "failed" } else { "pending" };
                self.mark_cleanup(a.tenant_id, a.id, st, Some(e.to_string()), true).await?;
                Ok(st.into())
            }
        }
    }

    /// Attachment cleanup queue.
    pub async fn handle_attachment_cleanup(&self, payload: &[u8]) -> HandlerOutcome {
        let ev: AttachmentCleanupEvent = match serde_json::from_slice(payload) {
            Ok(e) => e,
            Err(e) => return HandlerOutcome::Reject(format!("malformed attachment cleanup payload: {e}")),
        };
        let scope = AccessScope::for_tenant(ev.tenant_id);
        let Ok(conn) = self.db.conn() else { return HandlerOutcome::Retry("db".into()) };
        let chat = match chats::Entity::find().filter(chats::Column::Id.eq(ev.chat_id)).secure().scope_with(&scope).one(&conn).await {
            Ok(c) => c,
            Err(e) => return HandlerOutcome::Retry(e.to_string()),
        };
        if chat.as_ref().is_none_or(|c| c.deleted_at.is_some()) {
            return HandlerOutcome::Ok;
        }
        let row = match attachments::Entity::find()
            .filter(attachments::Column::Id.eq(ev.attachment_id))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await
        {
            Ok(r) => r,
            Err(e) => return HandlerOutcome::Retry(e.to_string()),
        };
        let Some(mut a) = row else { return HandlerOutcome::Ok };
        if a.cleanup_status.as_deref().is_some_and(|s| s != "pending") {
            return HandlerOutcome::Ok;
        }
        if a.provider_file_id.is_none() {
            a.provider_file_id.clone_from(&ev.provider_file_id);
        }
        if a.storage_backend.is_empty() {
            a.storage_backend.clone_from(&ev.storage_backend);
        }
        match self.cleanup_one(&a).await {
            Ok(s) if s == "done" => HandlerOutcome::Ok,
            Ok(s) if s == "failed" => HandlerOutcome::Reject("attachment cleanup: max attempts reached".into()),
            Ok(_) => HandlerOutcome::Retry("provider file delete failed".into()),
            Err(e) => HandlerOutcome::Retry(e.to_string()),
        }
    }

    /// Chat cleanup queue.
    #[allow(clippy::cognitive_complexity)] // linear handler with early-return outcomes per step
    pub async fn handle_chat_cleanup(&self, payload: &[u8], attempts: u32) -> HandlerOutcome {
        let ev: ChatCleanupEvent = match serde_json::from_slice(payload) {
            Ok(e) => e,
            Err(e) => return HandlerOutcome::Reject(format!("malformed chat cleanup payload: {e}")),
        };
        let scope = AccessScope::for_tenant(ev.tenant_id);
        let Ok(conn) = self.db.conn() else { return HandlerOutcome::Retry("db".into()) };
        let chat = match chats::Entity::find().filter(chats::Column::Id.eq(ev.chat_id)).secure().scope_with(&scope).one(&conn).await {
            Ok(c) => c,
            Err(e) => return HandlerOutcome::Retry(e.to_string()),
        };
        if chat.as_ref().is_none_or(|c| c.deleted_at.is_none()) {
            return HandlerOutcome::Reject("chat is not soft-deleted".into());
        }
        let pending = match attachments::Entity::find()
            .filter(Condition::all().add(attachments::Column::ChatId.eq(ev.chat_id)).add(attachments::Column::CleanupStatus.eq("pending")))
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await
        {
            Ok(r) => r,
            Err(e) => return HandlerOutcome::Retry(e.to_string()),
        };
        let mut still_pending = false;
        for a in &pending {
            match self.cleanup_one(a).await {
                Ok(s) if s == "pending" => still_pending = true,
                Ok(_) => {}
                Err(e) => return HandlerOutcome::Retry(e.to_string()),
            }
        }
        let max = self.cfg.cleanup_worker.max_attempts;
        if still_pending {
            return HandlerOutcome::Retry("attachment cleanup pending".into());
        }
        let Ok(conn) = self.db.conn() else { return HandlerOutcome::Retry("db".into()) };
        let vs = match chat_vector_stores::Entity::find()
            .filter(chat_vector_stores::Column::ChatId.eq(ev.chat_id))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await
        {
            Ok(v) => v,
            Err(e) => return HandlerOutcome::Retry(e.to_string()),
        };
        let Some(vs) = vs else { return HandlerOutcome::Ok };
        if let Some(vs_id) = &vs.vector_store_id {
            let failed_any = match attachments::Entity::find()
                .filter(Condition::all().add(attachments::Column::ChatId.eq(ev.chat_id)).add(attachments::Column::CleanupStatus.eq("failed")))
                .secure()
                .scope_with(&scope)
                .count(&conn)
                .await
            {
                Ok(n) => n > 0,
                Err(e) => return HandlerOutcome::Retry(e.to_string()),
            };
            if failed_any {
                tracing::warn!(chat_id = %ev.chat_id, "deleting vector store with failed attachment cleanup");
            }
            let target = match self.llm.resolver.rag_target_by_backend(&vs.provider, ev.tenant_id) {
                Ok(t) => t,
                Err(e) => return HandlerOutcome::Retry(e),
            };
            if let Err(e) = self.llm.delete_vector_store(&target, vs_id).await {
                if attempts + 1 >= max {
                    return HandlerOutcome::Reject(format!("vector store delete: max attempts ({max}) reached"));
                }
                return HandlerOutcome::Retry(format!("vector store delete failed: {e}"));
            }
        }
        match chat_vector_stores::Entity::delete_many()
            .filter(chat_vector_stores::Column::Id.eq(vs.id))
            .secure()
            .scope_with(&scope)
            .exec(&conn)
            .await
        {
            Ok(_) => HandlerOutcome::Ok,
            Err(e) => HandlerOutcome::Retry(e.to_string()),
        }
    }

    /// One orphan watchdog scan; returns the number of finalized turns.
    ///
    /// # Errors
    /// Database failure of the scan query.
    pub async fn orphan_scan(self: &Arc<Self>) -> DomainResult<usize> {
        let cutoff = OffsetDateTime::now_utc() - time::Duration::seconds(i64::try_from(self.cfg.orphan_watchdog.timeout_secs).unwrap_or(300));
        let stale = Condition::any()
            .add(chat_turns::Column::LastProgressAt.lte(cutoff))
            .add(Condition::all().add(chat_turns::Column::LastProgressAt.is_null()).add(chat_turns::Column::StartedAt.lte(cutoff)));
        let base = Condition::all()
            .add(chat_turns::Column::State.eq("running"))
            .add(chat_turns::Column::DeletedAt.is_null())
            .add(stale);
        let conn = self.db.conn()?;
        let candidates = chat_turns::Entity::find()
            .filter(base.clone())
            .limit(100)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await?;
        let mut finalized = 0;
        for t in candidates {
            match self.finalize_orphan(&t, base.clone()).await {
                Ok(true) => finalized += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, turn_id = %t.id, "orphan finalization failed"),
            }
        }
        Ok(finalized)
    }

    #[allow(clippy::many_single_char_names)] // short pattern bindings mirror the turn's reserve columns
    async fn finalize_orphan(&self, t: &chat_turns::Model, guard: Condition) -> DomainResult<bool> {
        let effective = t.effective_model.clone().unwrap_or_default();
        let (tier, in_mult, out_mult) = match (t.policy_version_applied, t.requester_user_id) {
            (Some(v), Some(u)) => {
                let snap = self.policy.snapshot(u, u64::try_from(v).unwrap_or(0)).await?;
                snap.find(&effective).map_or((None, 1, 1), |m| {
                    (Some(m.tier), m.input_tokens_credit_multiplier_micro, m.output_tokens_credit_multiplier_micro)
                })
            }
            _ => (None, 1, 1),
        };
        let reserve = match (t.reserve_tokens, t.max_output_tokens_applied, t.reserved_credits_micro, t.minimal_generation_floor_applied) {
            (Some(r), Some(m), Some(c), Some(f)) => Some(ReserveFields {
                reserve_tokens: r,
                max_output_tokens_applied: i64::from(m),
                reserved_credits_micro: c,
                minimal_generation_floor_applied: i64::from(f),
            }),
            _ => None,
        };
        let sctx = SettleCtx {
            tenant_id: t.tenant_id,
            user_id: t.requester_user_id,
            chat_id: t.chat_id,
            turn_id: t.id,
            request_id: t.request_id,
            selected_model: effective.clone(),
            effective_model: effective,
            tier,
            reserve,
            policy_version: t.policy_version_applied.and_then(|v| u64::try_from(v).ok()).unwrap_or(0),
            period_anchor: t.started_at,
            in_mult,
            out_mult,
            web_search_calls: u32::try_from(t.web_search_completed_count).unwrap_or(0),
            code_interpreter_calls: u32::try_from(t.code_interpreter_completed_count).unwrap_or(0),
            file_search_calls: u32::try_from(t.file_search_completed_count).unwrap_or(0),
            overshoot_tolerance: self.cfg.quota.overshoot_tolerance_factor,
        };
        let outbox = self.outbox.clone();
        let turn = t.clone();
        let wakes = self
            .tx(move |tx| {
                let outbox = outbox.clone();
                let sctx = sctx.clone();
                let guard = guard.clone();
                let turn = turn.clone();
                Box::pin(async move {
                    let ts = now();
                    let r = chat_turns::Entity::update_many()
                        .col_expr(chat_turns::Column::State, Expr::value("failed"))
                        .col_expr(chat_turns::Column::ErrorCode, Expr::value("orphan_timeout"))
                        .col_expr(chat_turns::Column::CompletedAt, Expr::value(ts))
                        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
                        .filter(guard.add(chat_turns::Column::Id.eq(turn.id)))
                        .secure()
                        .scope_with(&AccessScope::for_tenant(turn.tenant_id))
                        .exec(tx)
                        .await?;
                    if r.rows_affected == 0 {
                        return Ok(None);
                    }
                    let (_s, wake) = settle_and_enqueue(tx, &outbox, &sctx, &Terminal::Orphan, None).await?;
                    let audit = AuditEvent::Turn(TurnAuditEvent {
                        event_type: mini_chat_sdk::audit::event_types::TURN_FAILED.to_owned(),
                        timestamp: OffsetDateTime::now_utc(),
                        tenant_id: turn.tenant_id,
                        requester_type: turn.requester_type.clone(),
                        user_id: turn.requester_user_id,
                        chat_id: turn.chat_id,
                        turn_id: turn.id,
                        request_id: turn.request_id,
                        selected_model: sctx.selected_model.clone(),
                        effective_model: sctx.effective_model.clone(),
                        usage: UsageTokens::default(),
                        latency_ms: LatencyMs::default(),
                        tool_calls: ToolCalls { web_search_calls: sctx.web_search_calls, file_search_calls: sctx.file_search_calls },
                        policy_decisions: PolicyDecisions {
                            quota: QuotaDecisionAudit { decision: "unknown".into(), downgrade_from: None, downgrade_reason: None },
                            license: None,
                            quota_scope: None,
                        },
                        error_code: Some("orphan_timeout".into()),
                        prompt: String::new(),
                        response: String::new(),
                        attachments: Vec::new(),
                        trace_id: None,
                    });
                    let w2 = outbox.audit(tx, &audit).await?;
                    Ok(Some(vec![wake, w2]))
                })
            })
            .await?;
        match wakes {
            Some(w) => {
                fire(w);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// One upload-reaper scan; returns the number of reaped rows.
    ///
    /// # Errors
    /// Database failure of the scan query.
    pub async fn reaper_scan(&self) -> DomainResult<usize> {
        let cutoff = OffsetDateTime::now_utc() - time::Duration::seconds(i64::try_from(self.cfg.upload_reaper.stale_after_secs).unwrap_or(300));
        let conn = self.db.conn()?;
        let rows = attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(attachments::Column::Status.is_in(["pending", "uploaded"]))
                    .add(attachments::Column::DeletedAt.is_null())
                    .add(attachments::Column::CleanupStatus.is_null())
                    .add(attachments::Column::UpdatedAt.lt(cutoff)),
            )
            .order_by(attachments::Column::UpdatedAt, sea_orm::Order::Asc)
            .limit(100)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await?;
        let mut reaped = 0;
        for a in rows {
            let outbox = self.outbox.clone();
            let row = a.clone();
            let res = self
            .tx(move |tx| {
                    let outbox = outbox.clone();
                    let a = row.clone();
                    Box::pin(async move {
                        let ts = now();
                        let mut q = attachments::Entity::update_many()
                            .col_expr(attachments::Column::Status, Expr::value("failed"))
                            .col_expr(attachments::Column::ErrorCode, Expr::value("upload_abandoned"))
                            .col_expr(attachments::Column::UpdatedAt, Expr::value(ts));
                        if a.provider_file_id.is_some() {
                            q = q
                                .col_expr(attachments::Column::CleanupStatus, Expr::value("pending"))
                                .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(ts));
                        }
                        let r = q
                            .filter(
                                Condition::all()
                                    .add(attachments::Column::Id.eq(a.id))
                                    .add(attachments::Column::Status.eq(a.status.clone()))
                                    .add(attachments::Column::DeletedAt.is_null())
                                    .add(attachments::Column::CleanupStatus.is_null())
                                    .add(attachments::Column::UpdatedAt.lt(cutoff)),
                            )
                            .secure()
                            .scope_with(&AccessScope::for_tenant(a.tenant_id))
                            .exec(tx)
                            .await?;
                        if r.rows_affected == 0 {
                            return Ok(None);
                        }
                        let mut wakes = Vec::new();
                        if a.provider_file_id.is_some() {
                            if let Some(sf) = &a.secondary_file_id {
                                tracing::warn!(secondary_file_id = %sf, "reaped upload keeps its secondary copy");
                            }
                            let ev = AttachmentCleanupEvent {
                                event_type: "attachment_upload_abandoned".into(),
                                tenant_id: a.tenant_id,
                                chat_id: a.chat_id,
                                attachment_id: a.id,
                                provider_file_id: a.provider_file_id.clone(),
                                vector_store_id: None,
                                storage_backend: a.storage_backend.clone(),
                                attachment_kind: a.attachment_kind.clone(),
                                deleted_at: ts,
                                secondary_ref: None,
                            };
                            wakes.push(outbox.attachment_cleanup(tx, &ev).await?);
                        }
                        Ok(Some(wakes))
                    })
                })
                .await;
            match res {
                Ok(Some(w)) => {
                    fire(w);
                    reaped += 1;
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, attachment_id = %a.id, "upload reaper update failed"),
            }
        }
        Ok(reaped)
    }
}

