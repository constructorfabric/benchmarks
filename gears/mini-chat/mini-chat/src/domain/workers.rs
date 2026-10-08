//! Leader-only background workers: orphan watchdog and upload reaper (DESIGN B.9.1, B.9.5).

use std::sync::Arc;
use std::time::{Duration, Instant};

use mini_chat_sdk::{
    AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, MiniChatAuditEvent, ModelTier, TurnAuditEvent,
};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use tokio_util::sync::CancellationToken;

use crate::infra::db::WriteTransaction as _;
use crate::domain::error::DomainError;
use crate::domain::finalization::{Settlement, settlement, usage_event};
use crate::domain::quota::{self, PeriodStarts};
use crate::domain::repo::state;
use crate::domain::service::Svc;
use crate::infra::db::entities::{attachments, chat_turns};
use crate::infra::db::now;
use crate::infra::outbox::AttachmentCleanupEvent;

/// Maximum rows per upload-reaper scan.
pub const REAPER_BATCH: u64 = 100;

/// Leader elector abstraction (no-op without Kubernetes: always leader).
pub trait LeaderElector: Send + Sync {
    /// Whether this process currently leads `role`.
    fn is_leader(&self, role: &str) -> bool;
}

/// Single-process elector.
pub struct NoopElector;

impl LeaderElector for NoopElector {
    fn is_leader(&self, _role: &str) -> bool {
        true
    }
}

fn secs(n: u64) -> time::Duration {
    time::Duration::seconds(i64::try_from(n).unwrap_or(i64::MAX))
}

impl Svc {
    /// One orphan-watchdog scan; returns the number of finalized turns.
    ///
    /// # Errors
    /// Database errors.
    pub async fn orphan_scan(&self) -> Result<usize, DomainError> {
        let started = Instant::now();
        let cutoff = now() - secs(self.cfg.orphan_watchdog.timeout_secs);
        let stale = Condition::any()
            .add(chat_turns::Column::LastProgressAt.lte(cutoff))
            .add(
                Condition::all()
                    .add(chat_turns::Column::LastProgressAt.is_null())
                    .add(chat_turns::Column::StartedAt.lte(cutoff)),
            );
        let conn = self.db.conn()?;
        let candidates = chat_turns::Entity::find()
            .filter(
                Condition::all()
                    .add(chat_turns::Column::State.eq(state::RUNNING))
                    .add(chat_turns::Column::DeletedAt.is_null())
                    .add(stale.clone()),
            )
            .limit(REAPER_BATCH)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await?;
        let mut finalized = 0;
        for turn in candidates {
            self.metrics.inc("orphan_detected_total", &[("reason", "stale_progress")]);
            match self.finalize_orphan(&turn, cutoff).await {
                Ok(true) => {
                    finalized += 1;
                    self.metrics.inc("orphan_finalized_total", &[("reason", "stale_progress")]);
                    self.metrics.inc("streams_aborted_total", &[("trigger", "orphan_timeout")]);
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(turn_id = %turn.id, error = %e, "orphan finalization failed"),
            }
        }
        self.metrics.record("orphan_scan_duration_seconds", started.elapsed().as_secs_f64(), &[]);
        Ok(finalized)
    }

    async fn finalize_orphan(&self, turn: &chat_turns::Model, cutoff: OffsetDateTime) -> Result<bool, DomainError> {
        let user_id = turn.requester_user_id.unwrap_or_default();
        let effective = turn.effective_model.clone().unwrap_or_default();
        let version = turn.policy_version_applied.and_then(|v| u64::try_from(v).ok()).unwrap_or(0);
        let has_reserve = turn.reserve_tokens.is_some() && turn.reserved_credits_micro.is_some();
        let counts = (
            turn.web_search_completed_count,
            turn.code_interpreter_completed_count,
            turn.file_search_completed_count,
        );
        // Settlement inputs (bounded, estimated).
        let mut tier = ModelTier::Standard;
        let mut s = Settlement { method: "estimated", credits: turn.reserved_credits_micro.unwrap_or(0), telemetry: (0, 0) };
        if has_reserve {
            let snapshot = match self.policy.snapshot_version(user_id, version).await {
                Ok(s) => Some(s),
                Err(_) => self.policy.current_snapshot(user_id).await.ok(),
            };
            if let Some(entry) = snapshot.as_ref().and_then(|s| s.model(&effective)) {
                tier = entry.tier;
                if let Ok((computed, _)) = settlement(
                    turn.reserve_tokens.unwrap_or(0),
                    i64::from(turn.max_output_tokens_applied.unwrap_or(0)),
                    turn.reserved_credits_micro.unwrap_or(0),
                    i64::from(turn.minimal_generation_floor_applied.unwrap_or(0)),
                    entry,
                    None,
                    self.cfg.quota.overshoot_tolerance_factor,
                ) {
                    s = computed;
                }
            }
        }
        let audit = MiniChatAuditEvent::Turn(TurnAuditEvent {
            event_type: "turn_failed".to_owned(),
            tenant_id: turn.tenant_id,
            user_id: turn.requester_user_id,
            chat_id: turn.chat_id,
            turn_id: turn.id,
            request_id: turn.request_id,
            selected_model: effective.clone(),
            effective_model: effective.clone(),
            usage: None,
            latency_ms: None,
            ttft_ms: None,
            tool_calls: AuditToolCalls {
                web_search_calls: u32::try_from(counts.0).unwrap_or(0),
                file_search_calls: u32::try_from(counts.2).unwrap_or(0),
            },
            policy_decisions: AuditPolicyDecisions {
                quota: AuditQuotaDecision { decision: "unknown".to_owned(), downgrade_from: None, downgrade_reason: None },
                license: None,
            },
            error_code: Some("orphan_timeout".to_owned()),
            prompt: None,
            response: None,
            attachments: Vec::new(),
            quota_scope: None,
            timestamp: OffsetDateTime::now_utc(),
        });
        let t = turn.clone();
        let outbox = self.outbox.clone();
        let starts = PeriodStarts::of(turn.started_at);
        let res = self
            .db
            .write_transaction(move |tx| {
                Box::pin(async move {
                    let scope = AccessScope::for_tenant(t.tenant_id);
                    let ts = now();
                    let r = chat_turns::Entity::update_many()
                        .secure()
                        .col_expr(chat_turns::Column::State, Expr::value(state::FAILED))
                        .col_expr(chat_turns::Column::ErrorCode, Expr::value(Some("orphan_timeout".to_owned())))
                        .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(ts)))
                        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
                        .filter(
                            Condition::all()
                                .add(chat_turns::Column::Id.eq(t.id))
                                .add(chat_turns::Column::State.eq(state::RUNNING))
                                .add(chat_turns::Column::DeletedAt.is_null())
                                .add(
                                    Condition::any()
                                        .add(chat_turns::Column::LastProgressAt.lte(cutoff))
                                        .add(
                                            Condition::all()
                                                .add(chat_turns::Column::LastProgressAt.is_null())
                                                .add(chat_turns::Column::StartedAt.lte(cutoff)),
                                        ),
                                ),
                        )
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if r.rows_affected != 1 {
                        return Ok(None);
                    }
                    if has_reserve && let Some(user) = t.requester_user_id {
                        quota::apply_settlement(
                            tx,
                            t.tenant_id,
                            user,
                            &starts,
                            tier,
                            t.reserved_credits_micro.unwrap_or(0),
                            s.credits,
                            s.telemetry,
                            (counts.0, counts.1),
                        )
                        .await?;
                    }
                    let ev = usage_event(
                        t.tenant_id,
                        t.requester_user_id,
                        t.chat_id,
                        t.id,
                        t.request_id,
                        &effective,
                        &effective,
                        "failed",
                        "aborted",
                        None,
                        &s,
                        version,
                        counts,
                    );
                    let mut wake = outbox.usage(tx, &ev).await?;
                    wake += outbox.audit(tx, t.tenant_id, &audit).await?;
                    Ok(Some(wake))
                })
            })
            .await?;
        Ok(match res {
            Some(w) => {
                w.fire();
                true
            }
            None => false,
        })
    }

    /// One upload-reaper scan; returns the number of reaped rows.
    ///
    /// # Errors
    /// Database errors.
    pub async fn upload_reaper_scan(&self) -> Result<usize, DomainError> {
        let started = Instant::now();
        let cutoff = now() - secs(self.cfg.upload_reaper.stale_after_secs);
        let conn = self.db.conn()?;
        let rows = attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(attachments::Column::Status.is_in(["pending", "uploaded"]))
                    .add(attachments::Column::DeletedAt.is_null())
                    .add(attachments::Column::CleanupStatus.is_null())
                    .add(attachments::Column::UpdatedAt.lt(cutoff)),
            )
            .order_by(attachments::Column::UpdatedAt, Order::Asc)
            .limit(REAPER_BATCH)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await?;
        let mut reaped = 0;
        for a in rows {
            let from = a.status.clone();
            let outbox = self.outbox.clone();
            let row = a.clone();
            let res = self
                .db
                .write_transaction(move |tx| {
                    Box::pin(async move {
                        let scope = AccessScope::for_tenant(row.tenant_id);
                        let ts = now();
                        let mut q = attachments::Entity::update_many()
                            .secure()
                            .col_expr(attachments::Column::Status, Expr::value("failed"))
                            .col_expr(attachments::Column::ErrorCode, Expr::value(Some("upload_abandoned".to_owned())))
                            .col_expr(attachments::Column::UpdatedAt, Expr::value(ts));
                        if row.provider_file_id.is_some() {
                            q = q
                                .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending".to_owned())))
                                .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(ts)));
                        }
                        let r = q
                            .filter(
                                Condition::all()
                                    .add(attachments::Column::Id.eq(row.id))
                                    .add(attachments::Column::Status.eq(row.status.clone()))
                                    .add(attachments::Column::DeletedAt.is_null())
                                    .add(attachments::Column::CleanupStatus.is_null())
                                    .add(attachments::Column::UpdatedAt.lt(cutoff)),
                            )
                            .scope_with(&scope)
                            .exec(tx)
                            .await?;
                        if r.rows_affected != 1 {
                            return Ok(None);
                        }
                        let mut wake = Wake::empty();
                        if row.provider_file_id.is_some() {
                            let ev = AttachmentCleanupEvent {
                                event_type: "attachment_upload_abandoned".to_owned(),
                                tenant_id: row.tenant_id,
                                chat_id: row.chat_id,
                                attachment_id: row.id,
                                provider_file_id: row.provider_file_id.clone(),
                                vector_store_id: None,
                                storage_backend: row.storage_backend.clone(),
                                attachment_kind: row.attachment_kind.clone(),
                                deleted_at: ts,
                                secondary_ref: None,
                            };
                            wake += outbox.attachment_cleanup(tx, &ev).await?;
                        }
                        Ok(Some(wake))
                    })
                })
                .await;
            match res {
                Ok(Some(w)) => {
                    w.fire();
                    reaped += 1;
                    self.metrics.inc("attachment_upload_abandoned_total", &[("from_status", from.as_str())]);
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(attachment_id = %a.id, error = %e, "upload reaper failed on row"),
            }
        }
        self.metrics.record("upload_reaper_scan_duration_seconds", started.elapsed().as_secs_f64(), &[]);
        Ok(reaped)
    }
}

/// Spawns a periodic leader-only worker.
pub fn spawn_periodic<F, Fut>(
    role: &'static str,
    interval: Duration,
    elector: Arc<dyn LeaderElector>,
    cancel: CancellationToken,
    f: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = tick.tick() => {
                    if elector.is_leader(role) {
                        f().await;
                    }
                }
            }
        }
    })
}
