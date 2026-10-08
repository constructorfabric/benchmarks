//! Leader-only background workers: orphan watchdog and upload reaper.
//!
//! Without the `k8s` feature the leader elector is a no-op (every instance
//! is leader); double processing is prevented by the CAS guards.

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::audit::QuotaPolicyDecision;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;

use crate::domain::app::{App, fire, now, tenant_scope};
use crate::domain::error::DomainError;
use crate::domain::finalize::{SettleSpec, ToolCounts, settle_and_emit};
use crate::domain::quota::{self, PeriodStarts, TurnReserve};
use crate::infra::db::entity::{attachments, chat_turns, chats};
use crate::infra::outbox::AttachmentCleanupPayload;

const SCAN_LIMIT: u64 = 100;

fn stale_condition(cutoff: OffsetDateTime) -> Condition {
    Condition::all()
        .add(chat_turns::Column::State.eq("running"))
        .add(chat_turns::Column::DeletedAt.is_null())
        .add(
            Condition::any()
                .add(chat_turns::Column::LastProgressAt.lte(cutoff))
                .add(
                    Condition::all()
                        .add(chat_turns::Column::LastProgressAt.is_null())
                        .add(chat_turns::Column::StartedAt.lte(cutoff)),
                ),
        )
}

impl App {
    /// One orphan watchdog scan; returns the number of finalized turns.
    ///
    /// # Errors
    /// Database errors of the scan.
    pub async fn orphan_scan(self: &Arc<Self>) -> Result<usize, DomainError> {
        let cutoff = now() - time::Duration::seconds(i64::try_from(self.cfg.orphan_watchdog.timeout_secs).unwrap_or(300));
        let candidates = {
            let conn = self.db.conn()?;
            chat_turns::Entity::find()
                .secure()
                .scope_with(&AccessScope::allow_all())
                .filter(stale_condition(cutoff))
                .order_by(chat_turns::Column::StartedAt, Order::Asc)
                .limit(SCAN_LIMIT)
                .all(&conn)
                .await?
        };
        let mut finalized = 0;
        for t in candidates {
            match self.finalize_orphan(t, cutoff).await {
                Ok(true) => finalized += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, "orphan finalization failed"),
            }
        }
        Ok(finalized)
    }

    async fn finalize_orphan(self: &Arc<Self>, t: chat_turns::Model, cutoff: OffsetDateTime) -> Result<bool, DomainError> {
        let selected = {
            let conn = self.db.conn()?;
            chats::Entity::find()
                .secure()
                .scope_with(&tenant_scope(t.tenant_id))
                .filter(Condition::all().add(chats::Column::Id.eq(t.chat_id)))
                .one(&conn)
                .await?
                .map(|c| c.model)
        };
        let reserve = match (t.reserve_tokens, t.max_output_tokens_applied, t.reserved_credits_micro, t.minimal_generation_floor_applied) {
            (Some(r), Some(m), Some(c), Some(f)) => Some(TurnReserve {
                reserve_tokens: r,
                max_output_tokens_applied: i64::from(m),
                reserved_credits_micro: c,
                minimal_generation_floor_applied: i64::from(f),
            }),
            _ => None,
        };
        let version = t.policy_version_applied.and_then(|v| u64::try_from(v).ok()).unwrap_or(0);
        let effective = t.effective_model.clone().unwrap_or_default();
        let mut tier = None;
        let mut mults = (1, 1);
        if let (Some(user), Some(_)) = (t.requester_user_id, reserve)
            && let Ok(snap) = self.policy.snapshot(user, version).await
            && let Some(entry) = snap.find(&effective)
        {
            tier = Some(entry.tier);
            mults = quota::multipliers(entry);
        }
        let _ = selected;
        let app = Arc::clone(self);
        let committed = self
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let at = now();
                    let rows = chat_turns::Entity::update_many()
                        .col_expr(chat_turns::Column::State, Expr::value("failed"))
                        .col_expr(chat_turns::Column::ErrorCode, Expr::value(Some("orphan_timeout")))
                        .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(at)))
                        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(at))
                        .filter(stale_condition(cutoff).add(chat_turns::Column::Id.eq(t.id)))
                        .secure()
                        .scope_with(&tenant_scope(t.tenant_id))
                        .exec(tx)
                        .await?
                        .rows_affected;
                    if rows == 0 {
                        return Ok(None);
                    }
                    let user = t.requester_user_id.unwrap_or_default();
                    let has_reserve = reserve.is_some() && t.requester_user_id.is_some() && tier.is_some();
                    let spec = SettleSpec {
                        tenant_id: t.tenant_id,
                        user_id: user,
                        chat_id: t.chat_id,
                        turn_id: t.id,
                        request_id: t.request_id,
                        selected_model: &effective,
                        effective_model: &effective,
                        policy_version: version,
                        tier: if has_reserve { tier } else { None },
                        mults,
                        reserve: if has_reserve { reserve } else { None },
                        periods: PeriodStarts::at(t.started_at),
                        terminal_state: "failed",
                        error_code: Some("orphan_timeout"),
                        usage: None,
                        counts: ToolCounts {
                            web_search: i64::from(t.web_search_completed_count),
                            code_interpreter: i64::from(t.code_interpreter_completed_count),
                            file_search: i64::from(t.file_search_completed_count),
                        },
                        overshoot_tolerance: app.cfg.quota.overshoot_tolerance_factor,
                        quota_decision: QuotaPolicyDecision {
                            decision: "unknown".into(),
                            downgrade_from: None,
                            downgrade_reason: None,
                        },
                        latency_ms: None,
                    };
                    Ok(Some(settle_and_emit(&app, tx, &spec, at).await?))
                })
            })
            .await?;
        match committed {
            Some(w) => {
                fire(w);
                tracing::info!("orphan turn finalized");
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// One upload reaper scan; returns the number of reaped rows.
    ///
    /// # Errors
    /// Database errors of the scan.
    pub async fn reaper_scan(self: &Arc<Self>) -> Result<usize, DomainError> {
        let cutoff = now() - time::Duration::seconds(i64::try_from(self.cfg.upload_reaper.stale_after_secs).unwrap_or(300));
        let rows = {
            let conn = self.db.conn()?;
            attachments::Entity::find()
                .secure()
                .scope_with(&AccessScope::allow_all())
                .filter(
                    Condition::all()
                        .add(attachments::Column::Status.is_in(["pending", "uploaded"]))
                        .add(attachments::Column::DeletedAt.is_null())
                        .add(attachments::Column::CleanupStatus.is_null())
                        .add(attachments::Column::UpdatedAt.lt(cutoff)),
                )
                .order_by(attachments::Column::UpdatedAt, Order::Asc)
                .limit(SCAN_LIMIT)
                .all(&conn)
                .await?
        };
        let mut reaped = 0;
        for a in rows {
            let app = Arc::clone(self);
            let res = self
                .db
                .transaction(move |tx| {
                    Box::pin(async move {
                        let at = now();
                        let mut upd = attachments::Entity::update_many()
                            .col_expr(attachments::Column::Status, Expr::value("failed"))
                            .col_expr(attachments::Column::ErrorCode, Expr::value(Some("upload_abandoned")))
                            .col_expr(attachments::Column::UpdatedAt, Expr::value(at));
                        if a.provider_file_id.is_some() {
                            upd = upd
                                .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending")))
                                .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(at)));
                        }
                        let n = upd
                            .filter(
                                Condition::all()
                                    .add(attachments::Column::Id.eq(a.id))
                                    .add(attachments::Column::Status.eq(a.status.clone()))
                                    .add(attachments::Column::DeletedAt.is_null())
                                    .add(attachments::Column::CleanupStatus.is_null())
                                    .add(attachments::Column::UpdatedAt.lt(cutoff)),
                            )
                            .secure()
                            .scope_with(&tenant_scope(a.tenant_id))
                            .exec(tx)
                            .await?
                            .rows_affected;
                        if n == 0 {
                            return Ok(None);
                        }
                        let mut wakes = Vec::new();
                        if a.provider_file_id.is_some() {
                            if a.secondary_file_id.is_some() {
                                tracing::warn!(attachment_id = %a.id, "secondary copy of an abandoned upload is not deleted");
                            }
                            let p = AttachmentCleanupPayload {
                                event_type: "attachment_upload_abandoned".into(),
                                tenant_id: a.tenant_id,
                                chat_id: a.chat_id,
                                attachment_id: a.id,
                                provider_file_id: a.provider_file_id.clone(),
                                vector_store_id: None,
                                storage_backend: a.storage_backend.clone(),
                                attachment_kind: a.attachment_kind.clone(),
                                deleted_at: at,
                                secondary_ref: None,
                            };
                            wakes.push(app.outbox.attachment_cleanup(tx, &p).await?);
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
                Err(e) => tracing::warn!(error = %e, "upload reaper failed for a row"),
            }
        }
        Ok(reaped)
    }

    /// Spawns the enabled leader-only workers.
    pub fn spawn_workers(self: &Arc<Self>) -> Vec<tokio::task::JoinHandle<()>> {
        let mut out = Vec::new();
        if self.cfg.orphan_watchdog.enabled {
            let app = Arc::clone(self);
            let every = Duration::from_secs(self.cfg.orphan_watchdog.scan_interval_secs);
            out.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        () = app.shutdown.cancelled() => break,
                        () = tokio::time::sleep(every) => {}
                    }
                    if let Err(e) = app.orphan_scan().await {
                        tracing::warn!(error = %e, "orphan watchdog scan failed");
                    }
                }
            }));
        }
        if self.cfg.upload_reaper.enabled {
            let app = Arc::clone(self);
            let every = Duration::from_secs(self.cfg.upload_reaper.scan_interval_secs);
            out.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        () = app.shutdown.cancelled() => break,
                        () = tokio::time::sleep(every) => {}
                    }
                    if let Err(e) = app.reaper_scan().await {
                        tracing::warn!(error = %e, "upload reaper scan failed");
                    }
                }
            }));
        }
        out
    }
}
