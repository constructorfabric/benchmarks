//! Leader-only background workers: orphan turn watchdog and upload reaper.

#[allow(unused_imports)]
use sea_orm::{EntityTrait as _, QueryFilter as _};
use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{AuditLatency, AuditQuotaDecision, ModelTier};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition};
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use toolkit_security::constants::DEFAULT_SUBJECT_ID;

use super::Service;
use super::finalize::{SettlementInput, settle_and_emit};
use super::quota::Periods;
use crate::domain::billing::PersistedReserve;
use crate::domain::clock;
use crate::domain::error::DomainResult;
use crate::domain::events::{AttachmentCleanupEvent, PAYLOAD_ATTACHMENT_CLEANUP};
use crate::infra::outbox::fire;
use crate::infra::storage::entity::{attachment, chat, chat_turn};

/// Candidates per scan.
pub const SCAN_LIMIT: u64 = 100;

/// Leadership of a background role.
#[async_trait::async_trait]
pub trait LeaderElector: Send + Sync {
    /// `true` while this process leads the role.
    async fn is_leader(&self, role: &str) -> bool;
}

/// Single-process elector: always the leader.
pub struct NoopElector;

#[async_trait::async_trait]
impl LeaderElector for NoopElector {
    async fn is_leader(&self, _role: &str) -> bool {
        true
    }
}

fn stale_condition(cutoff: time::OffsetDateTime) -> Condition {
    Condition::any()
        .add(chat_turn::Column::LastProgressAt.lte(cutoff))
        .add(
            Condition::all()
                .add(chat_turn::Column::LastProgressAt.is_null())
                .add(chat_turn::Column::StartedAt.lte(cutoff)),
        )
}

impl Service {
    /// Run a periodic worker until `cancel`.
    pub async fn run_periodic<F, Fut>(
        self: Arc<Self>,
        role: &'static str,
        interval: Duration,
        elector: Arc<dyn LeaderElector>,
        cancel: CancellationToken,
        mut tick: F,
    ) where
        F: FnMut(Arc<Self>) -> Fut + Send,
        Fut: std::future::Future<Output = ()> + Send,
    {
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(interval) => {}
            }
            if elector.is_leader(role).await {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tick(Arc::clone(&self)) => {}
                }
            }
        }
    }

    /// One orphan-watchdog scan. Returns the number of finalized turns.
    pub async fn orphan_scan(&self) -> DomainResult<usize> {
        let started = std::time::Instant::now();
        let r = self.orphan_scan_inner().await;
        self.metrics
            .orphan_scan_duration_seconds
            .record(started.elapsed().as_secs_f64(), &[]);
        r
    }

    async fn orphan_scan_inner(&self) -> DomainResult<usize> {
        let timeout = time::Duration::seconds(
            i64::try_from(self.cfg.orphan_watchdog.timeout_secs).unwrap_or(300),
        );
        let cutoff = clock::now() - timeout;
        let conn = self.db.conn()?;
        let candidates = chat_turn::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(
                Condition::all()
                    .add(chat_turn::Column::State.eq("running"))
                    .add(chat_turn::Column::DeletedAt.is_null())
                    .add(stale_condition(cutoff)),
            )
            .order_by(chat_turn::Column::StartedAt, sea_orm::Order::Asc)
            .limit(SCAN_LIMIT)
            .all(&conn)
            .await?;
        let mut finalized = 0;
        let reason = crate::infra::metrics::labels(&[("reason", "stale_progress")]);
        for turn in candidates {
            self.metrics.orphan_detected.add(1, &reason);
            match self.finalize_orphan(turn, cutoff).await {
                Ok(true) => {
                    finalized += 1;
                    self.metrics.orphan_finalized.add(1, &reason);
                    self.metrics.streams_aborted.add(
                        1,
                        &crate::infra::metrics::labels(&[("trigger", "orphan_timeout")]),
                    );
                }
                Ok(false) => {}
                Err(e) => tracing::error!(error = %e, "mini-chat: orphan finalization failed"),
            }
        }
        Ok(finalized)
    }

    async fn finalize_orphan(
        &self,
        turn: chat_turn::Model,
        cutoff: time::OffsetDateTime,
    ) -> DomainResult<bool> {
        let user = turn.requester_user_id.unwrap_or(DEFAULT_SUBJECT_ID);
        let has_reserve = turn.reserve_tokens.is_some()
            && turn.max_output_tokens_applied.is_some()
            && turn.reserved_credits_micro.is_some()
            && turn.policy_version_applied.is_some()
            && turn.minimal_generation_floor_applied.is_some()
            && turn.requester_user_id.is_some();
        let version = turn
            .policy_version_applied
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or_default();
        let effective_id = turn.effective_model.clone().unwrap_or_default();
        let (premium, multipliers) = if has_reserve {
            let snapshot = self.policy.snapshot(user, version).await?;
            match snapshot.find_model(&effective_id) {
                Some(m) => (
                    m.tier == ModelTier::Premium,
                    (
                        m.input_tokens_credit_multiplier_micro,
                        m.output_tokens_credit_multiplier_micro,
                    ),
                ),
                None => (false, (0, 0)),
            }
        } else {
            (false, (0, 0))
        };
        let outbox = Arc::clone(&self.outbox);
        let metrics = Arc::clone(&self.metrics);
        let tolerance = self.cfg.quota.overshoot_tolerance_factor;
        let won = self
            .tx(move |tx| {
                let outbox = Arc::clone(&outbox);
                let metrics = Arc::clone(&metrics);
                let turn = turn.clone();
                let effective_id = effective_id.clone();
                Box::pin(async move {
                    let now = clock::now();
                    let scope = AccessScope::for_tenant(turn.tenant_id);
                    let r = chat_turn::Entity::update_many()
                        .col_expr(chat_turn::Column::State, Expr::value("failed"))
                        .col_expr(
                            chat_turn::Column::ErrorCode,
                            Expr::value(Some("orphan_timeout")),
                        )
                        .col_expr(chat_turn::Column::CompletedAt, Expr::value(Some(now)))
                        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
                        .filter(
                            Condition::all()
                                .add(chat_turn::Column::Id.eq(turn.id))
                                .add(chat_turn::Column::State.eq("running"))
                                .add(chat_turn::Column::DeletedAt.is_null())
                                .add(stale_condition(cutoff)),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if r.rows_affected == 0 {
                        return Ok(None);
                    }
                    let reserve = has_reserve.then(|| PersistedReserve {
                        reserve_tokens: turn.reserve_tokens.unwrap_or_default(),
                        max_output_tokens_applied: i64::from(
                            turn.max_output_tokens_applied.unwrap_or_default(),
                        ),
                        reserved_credits_micro: turn.reserved_credits_micro.unwrap_or_default(),
                        minimal_generation_floor_applied: i64::from(
                            turn.minimal_generation_floor_applied.unwrap_or_default(),
                        ),
                    });
                    let (eff, sel, ver) = if has_reserve {
                        (effective_id.as_str(), effective_id.as_str(), version)
                    } else {
                        ("", "", 0)
                    };
                    let total_ms = u64::try_from((now - turn.started_at).whole_milliseconds())
                        .unwrap_or_default();
                    let wakes = settle_and_emit(
                        tx,
                        &outbox,
                        &metrics,
                        SettlementInput {
                            tenant_id: turn.tenant_id,
                            user_id: turn.requester_user_id,
                            chat_id: turn.chat_id,
                            turn_id: turn.id,
                            request_id: turn.request_id,
                            selected_model: sel,
                            effective_model: eff,
                            premium,
                            state: "failed",
                            error_code: Some("orphan_timeout"),
                            usage: None,
                            reserve,
                            multipliers,
                            policy_version: ver,
                            periods: Periods::at(turn.started_at),
                            counts: (
                                u32::try_from(turn.web_search_completed_count).unwrap_or_default(),
                                u32::try_from(turn.code_interpreter_completed_count)
                                    .unwrap_or_default(),
                                u32::try_from(turn.file_search_completed_count).unwrap_or_default(),
                            ),
                            quota_decision: AuditQuotaDecision {
                                decision: "unknown".to_owned(),
                                downgrade_from: None,
                                downgrade_reason: None,
                                quota_scope: String::new(),
                            },
                            latency: AuditLatency {
                                ttft_ms: None,
                                total_ms,
                            },
                            tolerance,
                        },
                    )
                    .await?;
                    Ok(Some(wakes))
                })
            })
            .await?;
        match won {
            Some(w) => {
                fire(w);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// One upload-reaper scan. Returns the number of reaped rows.
    pub async fn reaper_scan(&self) -> DomainResult<usize> {
        let started = std::time::Instant::now();
        let r = self.reaper_scan_inner().await;
        self.metrics
            .upload_reaper_scan_duration_seconds
            .record(started.elapsed().as_secs_f64(), &[]);
        r
    }

    async fn reaper_scan_inner(&self) -> DomainResult<usize> {
        let stale = time::Duration::seconds(
            i64::try_from(self.cfg.upload_reaper.stale_after_secs).unwrap_or(300),
        );
        let cutoff = clock::now() - stale;
        let conn = self.db.conn()?;
        let rows = attachment::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(
                Condition::all()
                    .add(attachment::Column::Status.is_in(["pending", "uploaded"]))
                    .add(attachment::Column::DeletedAt.is_null())
                    .add(attachment::Column::CleanupStatus.is_null())
                    .add(attachment::Column::UpdatedAt.lt(cutoff)),
            )
            .order_by(attachment::Column::UpdatedAt, sea_orm::Order::Asc)
            .limit(SCAN_LIMIT)
            .all(&conn)
            .await?;
        let mut reaped = 0;
        for row in rows {
            if row.secondary_file_id.is_some() {
                tracing::warn!(
                    attachment_id = %row.id,
                    "mini-chat: abandoned upload has a secondary file that is not deleted"
                );
            }
            let from_status = row.status.clone();
            let outbox = Arc::clone(&self.outbox);
            let res = self
                .tx(move |tx| {
                    let outbox = Arc::clone(&outbox);
                    let row = row.clone();
                    Box::pin(async move {
                        let now = clock::now();
                        let scope = AccessScope::for_tenant(row.tenant_id);
                        let mut q = attachment::Entity::update_many()
                            .col_expr(attachment::Column::Status, Expr::value("failed"))
                            .col_expr(
                                attachment::Column::ErrorCode,
                                Expr::value(Some("upload_abandoned")),
                            )
                            .col_expr(attachment::Column::UpdatedAt, Expr::value(now));
                        if row.provider_file_id.is_some() {
                            q = q
                                .col_expr(
                                    attachment::Column::CleanupStatus,
                                    Expr::value(Some("pending")),
                                )
                                .col_expr(
                                    attachment::Column::CleanupUpdatedAt,
                                    Expr::value(Some(now)),
                                );
                        }
                        let r = q
                            .filter(
                                Condition::all()
                                    .add(attachment::Column::Id.eq(row.id))
                                    .add(attachment::Column::Status.eq(row.status.clone()))
                                    .add(attachment::Column::DeletedAt.is_null())
                                    .add(attachment::Column::CleanupStatus.is_null())
                                    .add(attachment::Column::UpdatedAt.lt(cutoff)),
                            )
                            .secure()
                            .scope_with(&scope)
                            .exec(tx)
                            .await?;
                        if r.rows_affected == 0 {
                            return Ok(None);
                        }
                        let mut wakes = Vec::new();
                        if row.provider_file_id.is_some() {
                            let event = AttachmentCleanupEvent {
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
                            wakes.push(
                                outbox
                                    .enqueue_json(
                                        tx,
                                        &outbox.queues.cleanup_queue_name,
                                        row.tenant_id,
                                        PAYLOAD_ATTACHMENT_CLEANUP,
                                        &event,
                                    )
                                    .await?,
                            );
                        }
                        Ok(Some(wakes))
                    })
                })
                .await;
            match res {
                Ok(Some(w)) => {
                    fire(w);
                    reaped += 1;
                    self.metrics.attachment_upload_abandoned.add(
                        1,
                        &crate::infra::metrics::labels(&[("from_status", from_status.as_str())]),
                    );
                }
                Ok(None) => {}
                Err(e) => tracing::error!(error = %e, "mini-chat: upload reaper failed on a row"),
            }
        }
        let _ = chat::Column::Id;
        Ok(reaped)
    }
}
