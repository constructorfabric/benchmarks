//! Leader-elected background workers: orphan watchdog and upload reaper (DESIGN
//! "Orphan Turn Watchdog", B.9.5). Without the `k8s` feature a no-op elector is
//! used and every instance scans; the CAS guards prevent double processing.

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{
    LatencyMs, MiniChatAuditEvent, ModelTier, PolicyDecisions, QuotaPolicyDecision, ToolCalls,
    TurnAuditEvent, UsageEvent, UsageTokens,
};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::domain::app::AppServices;
use crate::domain::credits::{self, SettlementMethod, TurnReserve};
use crate::domain::error::{DomainResult, retry_contention};
use crate::domain::quota::{self, Periods, SettleInput};
use crate::domain::stream::finalize::dedupe_key;
use crate::domain::time::now;
use crate::infra::db::entities::{attachment, chat_turn};
use crate::infra::db::repo;
use crate::infra::outbox::{AttachmentCleanupPayload, Wakes};

/// Scan batch size (fixed).
const SCAN_LIMIT: u64 = 100;

/// Spawns the enabled workers.
#[must_use]
pub fn spawn(svc: &Arc<AppServices>, cancel: &CancellationToken) -> Vec<JoinHandle<()>> {
    let mut out = Vec::new();
    if svc.cfg.orphan_watchdog.enabled {
        let svc = Arc::clone(svc);
        let cancel = cancel.clone();
        let every = Duration::from_secs(svc.cfg.orphan_watchdog.scan_interval_secs.max(1));
        out.push(tokio::spawn(async move {
            periodic(cancel, every, || {
                let svc = Arc::clone(&svc);
                async move {
                    let t = std::time::Instant::now();
                    if let Err(e) = orphan_scan(&svc).await {
                        tracing::warn!(error = %e, "orphan watchdog scan failed");
                    }
                    svc.metrics.record(
                        "orphan_scan_duration_seconds",
                        t.elapsed().as_secs_f64(),
                        &[],
                    );
                }
            })
            .await;
        }));
    }
    if svc.cfg.upload_reaper.enabled {
        let svc = Arc::clone(svc);
        let cancel = cancel.clone();
        let every = Duration::from_secs(svc.cfg.upload_reaper.scan_interval_secs.max(1));
        out.push(tokio::spawn(async move {
            periodic(cancel, every, || {
                let svc = Arc::clone(&svc);
                async move {
                    let t = std::time::Instant::now();
                    if let Err(e) = upload_reaper_scan(&svc).await {
                        tracing::warn!(error = %e, "upload reaper scan failed");
                    }
                    svc.metrics.record(
                        "upload_reaper_scan_duration_seconds",
                        t.elapsed().as_secs_f64(),
                        &[],
                    );
                }
            })
            .await;
        }));
    }
    out
}

async fn periodic<F, Fut>(cancel: CancellationToken, every: Duration, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = ticker.tick() => {}
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = f() => {}
        }
    }
}

/// Orphan finalization guard: still running, not deleted, still stale.
fn orphan_guard(cutoff: time::OffsetDateTime) -> Condition {
    Condition::all()
        .add(chat_turn::Column::State.eq("running"))
        .add(chat_turn::Column::DeletedAt.is_null())
        .add(repo::stale_progress(cutoff))
}

/// One orphan watchdog scan.
///
/// # Errors
/// Database errors of the candidate scan.
pub async fn orphan_scan(svc: &Arc<AppServices>) -> DomainResult<usize> {
    let timeout =
        time::Duration::seconds(i64::try_from(svc.cfg.orphan_watchdog.timeout_secs).unwrap_or(300));
    let cutoff = now() - timeout;
    let candidates = {
        let conn = svc.db.conn()?;
        repo::orphan_candidates(&conn, cutoff, SCAN_LIMIT).await?
    };
    let mut finalized = 0;
    for turn in candidates {
        svc.metrics
            .inc("orphan_detected", &[("reason", "stale_progress")]);
        match finalize_orphan(svc, &turn, cutoff).await {
            Ok(true) => {
                finalized += 1;
                svc.metrics
                    .inc("orphan_finalized", &[("reason", "stale_progress")]);
                svc.metrics
                    .inc("streams_aborted", &[("trigger", "orphan_timeout")]);
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, turn_id = %turn.id, "orphan finalization failed"),
        }
    }
    Ok(finalized)
}

#[allow(clippy::too_many_lines)]
async fn finalize_orphan(
    svc: &Arc<AppServices>,
    turn: &chat_turn::Model,
    cutoff: time::OffsetDateTime,
) -> DomainResult<bool> {
    let user_id = turn.requester_user_id.unwrap_or_default();
    let effective = turn.effective_model.clone().unwrap_or_default();
    // The selected model is not persisted on the turn; the watchdog reports the effective one.
    let selected = effective.clone();
    let version = u64::try_from(turn.policy_version_applied.unwrap_or(0)).unwrap_or(0);
    let entry = match svc.policy.snapshot(user_id, version).await {
        Ok(s) => s.find(&effective).cloned(),
        Err(_) => match svc.policy.current_snapshot(user_id).await {
            Ok(s) => s.find(&effective).cloned(),
            Err(e) => {
                tracing::warn!(error = %e, "policy unavailable for orphan settlement");
                None
            }
        },
    };
    let (in_mult, out_mult, premium) = entry.as_ref().map_or((1_000_000, 1_000_000, false), |m| {
        (
            m.input_tokens_credit_multiplier_micro,
            m.output_tokens_credit_multiplier_micro,
            m.tier == ModelTier::Premium,
        )
    });
    let (billing, method) = credits::derive_billing("failed", Some("orphan_timeout"), false);
    let reserve = turn.reserve_tokens.map(|r| TurnReserve {
        reserve_tokens: r,
        max_output_tokens_applied: i64::from(turn.max_output_tokens_applied.unwrap_or(0)),
        reserved_credits_micro: turn.reserved_credits_micro.unwrap_or(0),
        minimal_generation_floor_applied: i64::from(
            turn.minimal_generation_floor_applied.unwrap_or(0),
        ),
    });
    let settlement = match &reserve {
        Some(r) => Some(
            credits::settle(
                method,
                r,
                None,
                in_mult,
                out_mult,
                svc.cfg.quota.overshoot_tolerance_factor,
            )
            .map_err(|e| {
                crate::domain::error::DomainError::internal(format!("orphan settlement: {e}"))
            })?,
        ),
        None => None,
    };
    let periods = Periods::at(turn.started_at);
    let total_ms = u64::try_from((now() - turn.started_at).whole_milliseconds()).unwrap_or(0);
    let t = turn.clone();
    let res = retry_contention(|| {
        let svc = Arc::clone(svc);
        let t = t.clone();
        let effective = effective.clone();
        let selected = selected.clone();
        async move {
            let outbox = Arc::clone(&svc.outbox);
            svc.db
                .transaction(move |tx| {
                    Box::pin(async move {
                        let ts = now();
                        let won = repo::update_turn_where(
                            tx,
                            t.tenant_id,
                            t.id,
                            orphan_guard(cutoff),
                            vec![
                                (chat_turn::Column::State, Expr::value("failed")),
                                (chat_turn::Column::ErrorCode, Expr::value("orphan_timeout")),
                                (chat_turn::Column::CompletedAt, Expr::value(ts)),
                                (chat_turn::Column::UpdatedAt, Expr::value(ts)),
                            ],
                        )
                        .await?;
                        if won == 0 {
                            return Ok(None);
                        }
                        let mut wakes = Wakes::default();
                        if let (Some(s), Some(r)) = (settlement, reserve) {
                            quota::apply_settlement(
                                tx,
                                &SettleInput {
                                    tenant_id: t.tenant_id,
                                    user_id,
                                    periods,
                                    premium,
                                    reserved_credits_micro: r.reserved_credits_micro,
                                    settlement: s,
                                    web_search_calls: i64::from(t.web_search_completed_count),
                                    code_interpreter_calls: i64::from(
                                        t.code_interpreter_completed_count,
                                    ),
                                },
                                ts,
                            )
                            .await?;
                        }
                        let ev = UsageEvent {
                            tenant_id: t.tenant_id,
                            user_id: t.requester_user_id,
                            chat_id: t.chat_id,
                            turn_id: Some(t.id),
                            request_id: t.request_id,
                            effective_model: effective.clone(),
                            selected_model: selected.clone(),
                            terminal_state: "failed".to_owned(),
                            billing_outcome: billing.as_str().to_owned(),
                            usage: None,
                            actual_credits_micro: settlement
                                .map_or(0, |s| s.committed_credits_micro),
                            settlement_method: SettlementMethod::Estimated.as_str().to_owned(),
                            policy_version_applied: version,
                            web_search_calls: u32::try_from(t.web_search_completed_count)
                                .unwrap_or(0),
                            code_interpreter_calls: u32::try_from(
                                t.code_interpreter_completed_count,
                            )
                            .unwrap_or(0),
                            file_search_calls: u32::try_from(t.file_search_completed_count)
                                .unwrap_or(0),
                            timestamp: ts,
                            requester_type: t.requester_type.clone(),
                            dedupe_key: dedupe_key(t.tenant_id, t.id, t.request_id),
                            system_task_type: None,
                        };
                        wakes.push(
                            outbox
                                .usage(tx, &ev)
                                .await
                                .map_err(crate::domain::turns::internal_payload)?,
                        );
                        let audit = MiniChatAuditEvent::Turn(TurnAuditEvent {
                            event_type: "turn_failed".to_owned(),
                            timestamp: ts,
                            tenant_id: t.tenant_id,
                            requester_type: t.requester_type.clone(),
                            requester_user_id: t.requester_user_id,
                            chat_id: t.chat_id,
                            turn_id: t.id,
                            request_id: t.request_id,
                            selected_model: selected,
                            effective_model: effective,
                            terminal_state: "failed".to_owned(),
                            error_code: Some("orphan_timeout".to_owned()),
                            prompt: String::new(),
                            response: String::new(),
                            attachments: Vec::new(),
                            usage: UsageTokens::default(),
                            latency_ms: LatencyMs {
                                ttft_ms: None,
                                total_ms,
                            },
                            tool_calls: ToolCalls {
                                web_search_calls: u32::try_from(t.web_search_completed_count)
                                    .unwrap_or(0),
                                file_search_calls: u32::try_from(t.file_search_completed_count)
                                    .unwrap_or(0),
                            },
                            policy_decisions: PolicyDecisions {
                                license: None,
                                quota: QuotaPolicyDecision {
                                    decision: "unknown".to_owned(),
                                    quota_scope: None,
                                    downgrade_from: None,
                                    downgrade_reason: None,
                                },
                            },
                            trace_id: None,
                        });
                        wakes.push(
                            outbox
                                .audit(tx, &audit)
                                .await
                                .map_err(crate::domain::turns::internal_payload)?,
                        );
                        Ok(Some(wakes))
                    })
                })
                .await
        }
    })
    .await?;
    match res {
        Some(w) => {
            w.fire();
            Ok(true)
        }
        None => Ok(false),
    }
}

/// One upload reaper scan.
///
/// # Errors
/// Database errors of the candidate scan.
// One scan pass with per-row best-effort handling.
#[allow(clippy::cognitive_complexity)]
pub async fn upload_reaper_scan(svc: &Arc<AppServices>) -> DomainResult<usize> {
    let stale = time::Duration::seconds(
        i64::try_from(svc.cfg.upload_reaper.stale_after_secs).unwrap_or(300),
    );
    let cutoff = now() - stale;
    let rows = {
        let conn = svc.db.conn()?;
        repo::stale_uploads(&conn, cutoff, SCAN_LIMIT).await?
    };
    let mut reaped = 0;
    for row in rows {
        let from_status = if row.status == "pending" {
            "pending"
        } else {
            "uploaded"
        };
        if row.secondary_file_id.is_some() {
            tracing::warn!(attachment_id = %row.id, secondary_file_id = ?row.secondary_file_id, "secondary copy of an abandoned upload is not deleted");
        }
        let outbox = Arc::clone(&svc.outbox);
        let r = row.clone();
        let res = svc
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let ts = now();
                    let mut cols = vec![
                        (attachment::Column::Status, Expr::value("failed")),
                        (
                            attachment::Column::ErrorCode,
                            Expr::value("upload_abandoned"),
                        ),
                        (attachment::Column::UpdatedAt, Expr::value(ts)),
                    ];
                    if r.provider_file_id.is_some() {
                        cols.push((attachment::Column::CleanupStatus, Expr::value("pending")));
                        cols.push((attachment::Column::CleanupUpdatedAt, Expr::value(ts)));
                    }
                    let n = repo::update_attachment_where(
                        tx,
                        r.tenant_id,
                        r.id,
                        Condition::all()
                            .add(attachment::Column::Status.eq(r.status.clone()))
                            .add(attachment::Column::DeletedAt.is_null())
                            .add(attachment::Column::CleanupStatus.is_null())
                            .add(attachment::Column::UpdatedAt.lt(cutoff)),
                        cols,
                    )
                    .await?;
                    if n == 0 {
                        return Ok(None);
                    }
                    let mut wakes = Wakes::default();
                    if r.provider_file_id.is_some() {
                        let payload = AttachmentCleanupPayload {
                            event_type: "attachment_upload_abandoned".to_owned(),
                            tenant_id: r.tenant_id,
                            chat_id: r.chat_id,
                            attachment_id: r.id,
                            provider_file_id: r.provider_file_id.clone(),
                            vector_store_id: None,
                            storage_backend: r.storage_backend.clone(),
                            attachment_kind: r.attachment_kind.clone(),
                            deleted_at: ts,
                            secondary_ref: None,
                        };
                        wakes.push(
                            outbox
                                .attachment_cleanup(tx, &payload)
                                .await
                                .map_err(crate::domain::turns::internal_payload)?,
                        );
                    }
                    Ok(Some(wakes))
                })
            })
            .await;
        match res {
            Ok(Some(w)) => {
                w.fire();
                reaped += 1;
                svc.metrics.inc(
                    "attachment_upload_abandoned",
                    &[("from_status", from_status)],
                );
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, attachment_id = %row.id, "upload reaper update failed");
            }
        }
    }
    Ok(reaped)
}
