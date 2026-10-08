//! Periodic background workers: orphan turn watchdog and upload reaper
//! (DESIGN B.9.1, B.9.5). Without a cluster leader elector every instance is
//! the leader (no-op elector); the CAS guards prevent double processing.

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{
    MiniChatAuditEvent, ModelTier, PolicyDecisions, QuotaPolicyDecision, ToolCalls, TurnAuditEvent,
    UsageEvent,
};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureUpdateExt};

use super::AppState;
use super::finalize::dedupe_key;
use super::outbox::AttachmentCleanupEvent;
use super::quota::{ToolCallCounts, settle_in_tx};
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::quota::{PeriodStarts, ReserveFields, Settlement, Terminal, settle};
use crate::infra::db::entity::{attachment, chat_turn};
use crate::infra::repo::now_utc;

/// Candidates fetched per scan.
const SCAN_BATCH: u64 = 100;

fn stale_condition(cutoff: OffsetDateTime) -> Condition {
    Condition::any()
        .add(chat_turn::Column::LastProgressAt.lte(cutoff))
        .add(
            Condition::all()
                .add(chat_turn::Column::LastProgressAt.is_null())
                .add(chat_turn::Column::StartedAt.lte(cutoff)),
        )
}

/// Spawn the enabled workers; they stop when `state.cancel` fires.
pub fn spawn_workers(state: &Arc<AppState>) {
    let cfg = &state.cfg;
    if cfg.orphan_watchdog.enabled {
        let st = Arc::clone(state);
        let every = Duration::from_secs(cfg.orphan_watchdog.scan_interval_secs.max(1));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = st.cancel.cancelled() => return,
                    () = tokio::time::sleep(every) => {}
                }
                if let Err(e) = st.orphan_scan().await {
                    tracing::warn!(error = %e, "orphan watchdog scan failed");
                }
            }
        });
    }
    if cfg.upload_reaper.enabled {
        let st = Arc::clone(state);
        let every = Duration::from_secs(cfg.upload_reaper.scan_interval_secs.max(1));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = st.cancel.cancelled() => return,
                    () = tokio::time::sleep(every) => {}
                }
                if let Err(e) = st.reaper_scan().await {
                    tracing::warn!(error = %e, "upload reaper scan failed");
                }
            }
        });
    }
}

impl AppState {
    /// One orphan watchdog scan. Returns the number of finalized turns.
    ///
    /// # Errors
    /// Database failure of the candidate scan.
    pub async fn orphan_scan(&self) -> DomainResult<usize> {
        let timeout = time::Duration::seconds(
            i64::try_from(self.cfg.orphan_watchdog.timeout_secs).unwrap_or(300),
        );
        let cutoff = now_utc() - timeout;
        let conn = self.conn()?;
        let candidates = chat_turn::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(
                Condition::all()
                    .add(chat_turn::Column::State.eq("running"))
                    .add(chat_turn::Column::DeletedAt.is_null())
                    .add(stale_condition(cutoff)),
            )
            .order_by(chat_turn::Column::StartedAt, Order::Asc)
            .limit(SCAN_BATCH)
            .all(&conn)
            .await?;
        let mut done = 0;
        for turn in candidates {
            tracing::info!(turn_id = %turn.id, "orphan turn detected");
            match self.finalize_orphan(&turn, cutoff).await {
                Ok(true) => done += 1,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, turn_id = %turn.id, "orphan finalization failed");
                }
            }
        }
        Ok(done)
    }

    #[allow(clippy::too_many_lines)]
    #[allow(
        clippy::cognitive_complexity,
        reason = "sequential orchestration steps; splitting would obscure the flow"
    )]
    async fn finalize_orphan(
        &self,
        turn: &chat_turn::Model,
        cutoff: OffsetDateTime,
    ) -> DomainResult<bool> {
        let scope = AccessScope::for_tenant(turn.tenant_id);
        // Resolve the settlement inputs before the transaction.
        let reserve = match (
            turn.reserve_tokens,
            turn.max_output_tokens_applied,
            turn.reserved_credits_micro,
            turn.minimal_generation_floor_applied,
            turn.requester_user_id,
        ) {
            (Some(rt), Some(mo), Some(rc), Some(fl), Some(_)) => Some(ReserveFields {
                reserve_tokens: rt,
                max_output_tokens_applied: i64::from(mo),
                reserved_credits_micro: rc,
                minimal_generation_floor_applied: i64::from(fl),
            }),
            _ => None,
        };
        let effective = turn.effective_model.clone().unwrap_or_default();
        let version = u64::try_from(turn.policy_version_applied.unwrap_or(0)).unwrap_or(0);
        let settle_inputs = if let (Some(r), Some(user)) = (reserve, turn.requester_user_id) {
            let snap = self.policy.snapshot(user, version).await?;
            if let Some(m) = snap.find(&effective) {
                let s = settle(
                    &Terminal::Orphan,
                    None,
                    &r,
                    m.input_tokens_credit_multiplier_micro,
                    m.output_tokens_credit_multiplier_micro,
                    self.cfg.quota.overshoot_tolerance_factor,
                )
                .map_err(|e| DomainError::internal(format!("orphan settlement: {e}")))?;
                Some((user, r, s, m.tier == ModelTier::Premium))
            } else {
                // The model left the catalog: the turn must still be
                // finalized; release the reserve without a charge.
                tracing::warn!(
                    turn_id = %turn.id,
                    model = %effective,
                    "orphan turn model not in the catalog; releasing the reserve"
                );
                Some((
                    user,
                    r,
                    Settlement {
                        billing_outcome: "aborted",
                        settlement_method: "estimated",
                        committed_credits_micro: 0,
                        telemetry_input_tokens: 0,
                        telemetry_output_tokens: 0,
                        count_tool_calls: true,
                        overshoot: false,
                        overshoot_capped: false,
                    },
                    false,
                ))
            }
        } else {
            tracing::warn!(turn_id = %turn.id, "orphan turn without reserve fields; settlement skipped");
            None
        };
        let outbox = self.outbox.get().await?;
        let turn_c = turn.clone();
        let usage_queue = self.cfg.outbox.queue_name.clone();
        let audit_queue = self.cfg.outbox.audit_queue_name.clone();
        let partitions = self.cfg.outbox.num_partitions;
        let wake: Option<Wake> = self
            .write_tx(move |tx| {
                let turn = turn_c.clone();
                let scope = scope.clone();
                let outbox = Arc::clone(&outbox);
                let effective = effective.clone();
                let settle_inputs = settle_inputs.clone();
                let usage_queue = usage_queue.clone();
                let audit_queue = audit_queue.clone();
                Box::pin(async move {
                    let now = now_utc();
                    let res = chat_turn::Entity::update_many()
                        .secure()
                        .scope_with(&scope)
                        .col_expr(chat_turn::Column::State, Expr::value("failed"))
                        .col_expr(
                            chat_turn::Column::ErrorCode,
                            Expr::value(Some("orphan_timeout".to_owned())),
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
                        .exec(tx)
                        .await?;
                    if res.rows_affected == 0 {
                        return Ok(None);
                    }
                    let mut credits = 0;
                    let (sel, eff) = match &settle_inputs {
                        Some((user, r, s, premium)) => {
                            settle_in_tx(
                                tx,
                                &scope,
                                turn.tenant_id,
                                *user,
                                &PeriodStarts::at(turn.started_at),
                                *premium,
                                r.reserved_credits_micro,
                                s,
                                ToolCallCounts {
                                    web_search: i64::from(turn.web_search_completed_count),
                                    code_interpreter: i64::from(
                                        turn.code_interpreter_completed_count,
                                    ),
                                },
                            )
                            .await?;
                            credits = s.committed_credits_micro;
                            (effective.clone(), effective.clone())
                        }
                        None => (String::new(), String::new()),
                    };
                    let counts = |v: i32| u32::try_from(v).unwrap_or(0);
                    let ev = UsageEvent {
                        tenant_id: turn.tenant_id,
                        user_id: turn.requester_user_id,
                        chat_id: turn.chat_id,
                        turn_id: Some(turn.id),
                        request_id: turn.request_id,
                        effective_model: eff.clone(),
                        selected_model: sel.clone(),
                        terminal_state: "failed".into(),
                        billing_outcome: "aborted".into(),
                        usage: None,
                        actual_credits_micro: credits,
                        settlement_method: "estimated".into(),
                        policy_version_applied: if settle_inputs.is_some() { version } else { 0 },
                        web_search_calls: counts(turn.web_search_completed_count),
                        code_interpreter_calls: counts(turn.code_interpreter_completed_count),
                        file_search_calls: counts(turn.file_search_completed_count),
                        timestamp: OffsetDateTime::now_utc(),
                        requester_type: "user".into(),
                        dedupe_key: dedupe_key(turn.tenant_id, turn.id, turn.request_id),
                        system_task_type: None,
                    };
                    let mut wake = super::outbox::enqueue_json(
                        &outbox,
                        tx,
                        &usage_queue,
                        super::outbox::partition_for(turn.tenant_id, partitions),
                        super::outbox::PT_USAGE,
                        &ev,
                    )
                    .await?;
                    let audit = MiniChatAuditEvent::Turn(TurnAuditEvent {
                        event_type: "turn_failed".into(),
                        tenant_id: turn.tenant_id,
                        user_id: turn.requester_user_id.unwrap_or_default(),
                        chat_id: turn.chat_id,
                        turn_id: turn.id,
                        request_id: turn.request_id,
                        selected_model: sel,
                        effective_model: eff,
                        terminal_state: "failed".into(),
                        error_code: Some("orphan_timeout".into()),
                        usage: None,
                        latency_ms: None,
                        tool_calls: ToolCalls {
                            web_search_calls: counts(turn.web_search_completed_count),
                            file_search_calls: counts(turn.file_search_completed_count),
                        },
                        policy_decisions: PolicyDecisions {
                            quota: QuotaPolicyDecision {
                                decision: "unknown".into(),
                                downgrade_from: None,
                                downgrade_reason: None,
                            },
                            license: String::new(),
                        },
                        prompt: String::new(),
                        response: String::new(),
                        attachments: vec![],
                        quota_scope: String::new(),
                        trace_id: None,
                        timestamp: OffsetDateTime::now_utc(),
                    });
                    wake += super::outbox::enqueue_json(
                        &outbox,
                        tx,
                        &audit_queue,
                        super::outbox::partition_for(turn.tenant_id, partitions),
                        super::outbox::PT_AUDIT,
                        &audit,
                    )
                    .await?;
                    Ok(Some(wake))
                })
            })
            .await?;
        match wake {
            Some(w) => {
                w.fire();
                tracing::info!(turn_id = %turn.id, "orphan turn finalized");
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// One upload reaper scan. Returns the number of reaped rows.
    ///
    /// # Errors
    /// Database failure of the candidate scan.
    pub async fn reaper_scan(&self) -> DomainResult<usize> {
        let stale = time::Duration::seconds(
            i64::try_from(self.cfg.upload_reaper.stale_after_secs).unwrap_or(300),
        );
        let cutoff = now_utc() - stale;
        let conn = self.conn()?;
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
            .order_by(attachment::Column::UpdatedAt, Order::Asc)
            .limit(SCAN_BATCH)
            .all(&conn)
            .await?;
        let mut n = 0;
        for row in rows {
            match self.reap(&row, cutoff).await {
                Ok(true) => n += 1,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, attachment_id = %row.id, "upload reaper failed for row");
                }
            }
        }
        Ok(n)
    }

    async fn reap(&self, row: &attachment::Model, cutoff: OffsetDateTime) -> DomainResult<bool> {
        let outbox = self.outbox.get().await?;
        let q = self.cleanup_queue();
        let row_c = row.clone();
        if row.secondary_file_id.is_some() {
            tracing::warn!(
                attachment_id = %row.id,
                secondary_file_id = ?row.secondary_file_id,
                "abandoned upload has a secondary copy that is not cleaned up"
            );
        }
        let res: Option<Option<Wake>> = self
            .write_tx(move |tx| {
                let row = row_c.clone();
                let outbox = Arc::clone(&outbox);
                let q = q.clone();
                Box::pin(async move {
                    let scope = AccessScope::for_tenant(row.tenant_id);
                    let now = now_utc();
                    let mut upd = attachment::Entity::update_many()
                        .secure()
                        .scope_with(&scope)
                        .col_expr(attachment::Column::Status, Expr::value("failed"))
                        .col_expr(
                            attachment::Column::ErrorCode,
                            Expr::value(Some("upload_abandoned".to_owned())),
                        )
                        .col_expr(attachment::Column::UpdatedAt, Expr::value(now));
                    if row.provider_file_id.is_some() {
                        upd = upd
                            .col_expr(
                                attachment::Column::CleanupStatus,
                                Expr::value(Some("pending".to_owned())),
                            )
                            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)));
                    }
                    let r = upd
                        .filter(
                            Condition::all()
                                .add(attachment::Column::Id.eq(row.id))
                                .add(attachment::Column::Status.eq(row.status.clone()))
                                .add(attachment::Column::DeletedAt.is_null())
                                .add(attachment::Column::CleanupStatus.is_null())
                                .add(attachment::Column::UpdatedAt.lt(cutoff)),
                        )
                        .exec(tx)
                        .await?;
                    if r.rows_affected == 0 {
                        return Ok(None);
                    }
                    let Some(fid) = row.provider_file_id.clone() else {
                        return Ok(Some(None));
                    };
                    let ev = AttachmentCleanupEvent {
                        event_type: "attachment_upload_abandoned".into(),
                        tenant_id: row.tenant_id,
                        chat_id: row.chat_id,
                        attachment_id: row.id,
                        provider_file_id: Some(fid),
                        vector_store_id: None,
                        storage_backend: row.storage_backend.clone(),
                        attachment_kind: row.attachment_kind.clone(),
                        deleted_at: OffsetDateTime::now_utc(),
                        secondary_ref: None,
                    };
                    Ok(Some(Some(
                        super::attachments::st_enqueue_cleanup(&outbox, tx, &q, &ev).await?,
                    )))
                })
            })
            .await?;
        match res {
            None => Ok(false),
            Some(w) => {
                if let Some(w) = w {
                    w.fire();
                }
                tracing::info!(attachment_id = %row.id, from_status = %row.status, "abandoned upload reaped");
                Ok(true)
            }
        }
    }
}
