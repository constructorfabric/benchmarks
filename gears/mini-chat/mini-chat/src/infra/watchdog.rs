//! Background workers: the orphan turn watchdog and the upload reaper.
//!
//! Both run under the gear's leader elector (a no-op elector without the
//! `k8s` feature): only the leader scans. Double finalization is prevented
//! by the CAS guards in any case.

use std::sync::Arc;

use mini_chat_sdk::{PolicyDecisions, QuotaAuditDecision, ToolCalls, TurnAuditEvent, UsageEvent};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use crate::domain::finalize::{dedupe_key, now};
use crate::domain::quota_service::{SettlementKind, compute_settlement};
use crate::error::DomainError;
use crate::gear::GearState;
use crate::infra::election;
use crate::infra::metrics::label;
use crate::infra::outbox::OutboxEnqueuer;
use crate::infra::outbox::AttachmentCleanupPayload;

/// Prepare the leader electors and spawn the enabled background workers.
///
/// # Errors
/// Returns an error when the k8s leader elector cannot be prepared (missing
/// `POD_NAMESPACE`/`POD_NAME`, no Kubernetes configuration).
pub async fn spawn(state: &Arc<GearState>, cancel: CancellationToken) -> Result<(), String> {
    if state.ctx.config.orphan_watchdog.enabled {
        let elector = election::prepare("mini-chat-orphan-watchdog", &cancel).await?;
        let state = Arc::clone(state);
        let cancel = cancel.clone();
        tokio::spawn(async move { orphan_watchdog_loop(state, cancel, elector).await });
    }
    if state.ctx.config.upload_reaper.enabled {
        let elector = election::prepare("mini-chat-upload-reaper", &cancel).await?;
        let state = Arc::clone(state);
        tokio::spawn(async move { upload_reaper_loop(state, cancel, elector).await });
    }
    Ok(())
}

/// The orphan watchdog loop: only the leader scans.
async fn orphan_watchdog_loop(
    state: Arc<GearState>,
    cancel: CancellationToken,
    elector: Arc<dyn election::LeaderElector>,
) {
    let cfg = state.ctx.config.orphan_watchdog.clone();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(cfg.scan_interval_secs));
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            _ = interval.tick() => {
                if elector.is_leader() {
                    orphan_scan_once(&state).await;
                }
            }
        }
    }
}

/// One orphan scan: detect, finalize, settle, emit.
async fn orphan_scan_once(state: &Arc<GearState>) {
    let scan_start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(state.ctx.config.orphan_watchdog.timeout_secs);
    let cutoff = now() - time::Duration::try_from(timeout).unwrap_or(time::Duration::MAX);
    let Ok(conn) = state.ctx.db.conn() else {
        return;
    };
    let candidates = match state.ctx.repos.turns.orphan_scan(&conn, cutoff, 100).await {
        Ok(c) => c,
        Err(err) => {
            tracing::error!("mini-chat: orphan scan failed: {err}");
            return;
        }
    };
    for turn in candidates {
        state
            .ctx
            .metrics
            .orphan_detected
            .add(1, &[label("reason", "stale_progress")]);
        let finalized = finalize_orphan(&state.ctx, &turn, cutoff).await;
        match finalized {
            Ok(true) => {
                state
                    .ctx
                    .metrics
                    .orphan_finalized
                    .add(1, &[label("reason", "stale_progress")]);
            }
            Ok(false) => {}
            Err(err) => {
                tracing::error!("mini-chat: orphan finalization failed for {}: {err}", turn.id);
            }
        }
    }
    state
        .ctx
        .metrics
        .orphan_scan_duration_seconds
        .record(scan_start.elapsed().as_secs_f64(), &[]);
}

/// Finalize one orphan turn: CAS + estimated settlement + usage + audit in
/// one transaction.
pub(crate) async fn finalize_orphan(
    ctx: &Arc<crate::domain::services::ServiceContext>,
    turn: &crate::infra::storage::entity::chat_turns::Model,
    cutoff: OffsetDateTime,
) -> Result<bool, DomainError> {
    let now = now();
    let result = ctx
        .db
        .transaction(|tx| {
            let turn = turn.clone();
            let ctx = Arc::clone(ctx);
            Box::pin(async move {
                let won = ctx
                    .repos
                    .turns
                    .orphan_finalize(tx, turn.id, cutoff, now)
                    .await?;
                if !won {
                    return Ok((false, Vec::new()));
                }
                // Settlement: skipped (with a warning) when reserve fields
                // or the requester are NULL.
                let mut wakes = Vec::new();
                let has_reserve = turn.reserve_tokens.is_some()
                    && turn.max_output_tokens_applied.is_some()
                    && turn.reserved_credits_micro.is_some()
                    && turn.minimal_generation_floor_applied.is_some()
                    && turn.policy_version_applied.is_some()
                    && turn.requester_user_id.is_some();
                let mut committed_credits_micro = 0_i64;
                if has_reserve {
                    let requester = turn.requester_user_id.unwrap_or_default();
                    let snapshot = ctx
                        .policy
                        .policy_snapshot(requester, turn.policy_version_applied.unwrap_or(0))
                        .await
                        .map_err(|e| {
                            tracing::warn!("mini-chat: orphan settlement policy lookup failed: {e}");
                            DomainError::Internal
                        })?;
                    let entry = turn
                        .effective_model
                        .as_deref()
                        .and_then(|m| snapshot.model(m));
                    let (in_mult, out_mult) = entry.map_or((1, 1), |m| {
                        (
                            m.input_tokens_credit_multiplier_micro,
                            m.output_tokens_credit_multiplier_micro,
                        )
                    });
                    let settlement = compute_settlement(
                        SettlementKind::Aborted,
                        turn.reserve_tokens.unwrap_or(0),
                        i64::from(turn.max_output_tokens_applied.unwrap_or(0)),
                        i64::from(turn.minimal_generation_floor_applied.unwrap_or(0)),
                        turn.reserved_credits_micro.unwrap_or(0),
                        None,
                        in_mult,
                        out_mult,
                        ctx.config.quota.overshoot_tolerance_factor,
                    )
                    .map_err(|e| {
                        tracing::warn!("mini-chat: orphan settlement computation failed: {e}");
                        DomainError::Internal
                    })?;
                    committed_credits_micro = settlement.committed_credits_micro;
                    let is_premium = entry.is_some_and(|m| m.tier == mini_chat_sdk::ModelTier::Premium);
                    let daily = crate::domain::services::periods::daily_start(turn.started_at);
                    let monthly = crate::domain::services::periods::monthly_start(turn.started_at);
                    for (period_type, period_start) in [
                        (crate::infra::storage::repo::quota::period::DAILY, daily),
                        (crate::infra::storage::repo::quota::period::MONTHLY, monthly),
                    ] {
                        ctx
                            .repos
                            .quota
                            .settle(
                                tx,
                                turn.tenant_id,
                                requester,
                                crate::infra::storage::repo::quota::BucketKey {
                                    period_type,
                                    period_start,
                                    bucket: crate::infra::storage::repo::quota::bucket::TOTAL,
                                },
                                turn.reserved_credits_micro.unwrap_or(0),
                                committed_credits_micro,
                                0,
                                0,
                                turn.web_search_completed_count,
                                turn.code_interpreter_completed_count,
                            )
                            .await?;
                        if is_premium {
                            ctx
                                .repos
                                .quota
                                .settle(
                                    tx,
                                    turn.tenant_id,
                                    requester,
                                    crate::infra::storage::repo::quota::BucketKey {
                                        period_type,
                                        period_start,
                                        bucket: crate::infra::storage::repo::quota::bucket::PREMIUM,
                                    },
                                    turn.reserved_credits_micro.unwrap_or(0),
                                    committed_credits_micro,
                                    0,
                                    0,
                                    0,
                                    0,
                                )
                                .await?;
                        }
                    }
                } else {
                    tracing::warn!(
                        "mini-chat: orphan turn {} has no reserve fields; skipping settlement",
                        turn.id
                    );
                }
                // Usage event (always enqueued for a finalized orphan).
                let effective_model = turn.effective_model.clone().unwrap_or_default();
                let usage_event = UsageEvent {
                    tenant_id: turn.tenant_id,
                    user_id: turn.requester_user_id,
                    chat_id: turn.chat_id,
                    turn_id: Some(turn.id),
                    request_id: turn.request_id,
                    effective_model: effective_model.clone(),
                    selected_model: effective_model.clone(),
                    terminal_state: "failed".to_owned(),
                    billing_outcome: "aborted".to_owned(),
                    usage: None,
                    actual_credits_micro: committed_credits_micro,
                    settlement_method: "estimated".to_owned(),
                    policy_version_applied: turn.policy_version_applied.unwrap_or(0),
                    web_search_calls: u32::try_from(turn.web_search_completed_count).unwrap_or(0),
                    code_interpreter_calls: u32::try_from(turn.code_interpreter_completed_count)
                        .unwrap_or(0),
                    file_search_calls: u32::try_from(turn.file_search_completed_count).unwrap_or(0),
                    timestamp: crate::api::rest::dto::fmt_ts(&now),
                    requester_type: turn.requester_type.clone(),
                    dedupe_key: dedupe_key(turn.tenant_id, turn.id, turn.request_id),
                    system_task_type: None,
                };
                wakes.push(ctx.outbox.enqueue_usage(tx, &usage_event).await?);
                let audit = TurnAuditEvent {
                    event_type: "turn_failed".to_owned(),
                    tenant_id: turn.tenant_id,
                    user_id: turn.requester_user_id,
                    actor_user_id: None,
                    chat_id: turn.chat_id,
                    turn_id: Some(turn.id),
                    request_id: turn.request_id,
                    original_request_id: None,
                    new_request_id: None,
                    selected_model: effective_model.clone(),
                    effective_model,
                    usage: None,
                    latency_ms: None,
                    tool_calls: ToolCalls {
                        web_search_calls: u64::try_from(turn.web_search_completed_count)
                            .unwrap_or(0),
                        file_search_calls: u64::try_from(turn.file_search_completed_count)
                            .unwrap_or(0),
                    },
                    policy_decisions: PolicyDecisions {
                        quota: QuotaAuditDecision {
                            decision: "unknown".to_owned(),
                            downgrade_from: None,
                            downgrade_reason: None,
                        },
                        license: String::new(),
                    },
                    error_code: Some("orphan_timeout".to_owned()),
                    prompt: String::new(),
                    response: String::new(),
                    attachments: Vec::new(),
                    quota_scope: String::new(),
                    trace_id: None,
                    timestamp: crate::api::rest::dto::fmt_ts(&now),
                };
                wakes.push(ctx.outbox.enqueue_audit(tx, &audit).await?);
                Ok((true, wakes))
            })
        })
        .await?;
    let (won, wakes) = result;
    for wake in wakes {
        wake.fire();
    }
    Ok(won)
}

/// The upload reaper loop: only the leader scans.
async fn upload_reaper_loop(
    state: Arc<GearState>,
    cancel: CancellationToken,
    elector: Arc<dyn election::LeaderElector>,
) {
    let cfg = state.ctx.config.upload_reaper.clone();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(cfg.scan_interval_secs));
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            _ = interval.tick() => {
                if elector.is_leader() {
                    reap_once(&state).await;
                }
            }
        }
    }
}

/// One reaper scan.
async fn reap_once(state: &Arc<GearState>) {
    let scan_start = std::time::Instant::now();
    let cutoff = now()
        - time::Duration::seconds(i64::try_from(state.ctx.config.upload_reaper.stale_after_secs).unwrap_or(300));
    let Ok(conn) = state.ctx.db.conn() else {
        return;
    };
    let stale_rows = match state.ctx.repos.attachments.reaper_scan(&conn, cutoff, 100).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!("mini-chat: upload reaper scan failed: {err}");
            return;
        }
    };
    for row in stale_rows {
        let from_status = row.status.clone();
        let result = reap_row(&state.ctx, &row, cutoff).await;
        match result {
            Ok(true) => {
                state
                    .ctx
                    .metrics
                    .attachment_upload_abandoned
                    .add(1, &[label("from_status", from_status)]);
            }
            Ok(false) => {}
            Err(err) => {
                tracing::error!("mini-chat: upload reaper failed for {}: {err}", row.id);
            }
        }
    }
    state
        .ctx
        .metrics
        .upload_reaper_scan_duration_seconds
        .record(scan_start.elapsed().as_secs_f64(), &[]);
}

/// Reap one row: CAS to failed + enqueue the provider file delete.
pub(crate) async fn reap_row(
    ctx: &Arc<crate::domain::services::ServiceContext>,
    row: &crate::infra::storage::entity::attachments::Model,
    cutoff: OffsetDateTime,
) -> Result<bool, DomainError> {
    let has_provider_file = row.provider_file_id.is_some();
    if has_provider_file && row.secondary_file_id.is_some() {
        // The reaper's cleanup event carries no secondary_ref; log it.
        tracing::warn!(
            "mini-chat: reaped row {} has an Anthropic secondary copy that is not deleted",
            row.id
        );
    }
    let result = ctx
        .db
        .transaction(|tx| {
            let row = row.clone();
            let ctx = Arc::clone(ctx);
            Box::pin(async move {
                let won = ctx
                    .repos
                    .attachments
                    .reaper_fail(tx, row.id, cutoff, has_provider_file)
                    .await?;
                if !won {
                    return Ok((false, None));
                }
                if let Some(file_id) = row.provider_file_id.clone() {
                    let payload = AttachmentCleanupPayload {
                        event_type: "attachment_upload_abandoned".to_owned(),
                        tenant_id: row.tenant_id,
                        chat_id: row.chat_id,
                        attachment_id: row.id,
                        provider_file_id: Some(file_id),
                        vector_store_id: None,
                        storage_backend: row.storage_backend.clone(),
                        attachment_kind: row.attachment_kind.clone(),
                        deleted_at: crate::api::rest::dto::fmt_ts(&now()),
                        secondary_ref: None,
                    };
                    let wake = ctx.outbox.enqueue_attachment_cleanup(tx, &payload).await?;
                    return Ok((true, Some(wake)));
                }
                Ok((true, None))
            })
        })
        .await;
    let (won, wake) = result?;
    if let Some(wake) = wake {
        wake.fire();
    }
    Ok(won)
}
