//! Orphan turn watchdog (DESIGN §3.x "Orphan Turn Watchdog", B.9.1, ADR-0010).
//!
//! Every `orphan_watchdog.scan_interval_secs` the leader scans at most 100 running,
//! non-deleted turns whose `COALESCE(last_progress_at, started_at)` is older than
//! `orphan_watchdog.timeout_secs`, and finalizes each in its own transaction: an orphan CAS
//! (`failed` / `orphan_timeout`, re-checking the stale-progress predicate), the estimated quota
//! settlement on the periods of `started_at`, and the usage + `turn_failed` audit outbox events.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::{DomainError, stream_codes};
use crate::domain::service::Deps;
use crate::domain::service::billing::{
    TerminalState, TurnAuditInput, UsageEventInput, build_turn_audit_event, build_usage_event,
    derive_billing,
};
use crate::domain::service::quota::{QuotaPeriods, QuotaService, SettlementInput};
use crate::infra::db::entity::chat_turn;

/// Fixed maximum number of candidates per scan.
pub const SCAN_LIMIT: u64 = 100;

/// Leader elector abstraction: only the leader runs scans.
#[async_trait]
pub trait LeaderElector: Send + Sync {
    /// Whether this process is currently the leader of the role.
    async fn is_leader(&self) -> bool;
}

/// No-op elector (single-process mode): always the leader.
pub struct AlwaysLeader;

#[async_trait]
impl LeaderElector for AlwaysLeader {
    async fn is_leader(&self) -> bool {
        true
    }
}

/// Elector of a role (`orphan-watchdog`, `upload-reaper`). The Kubernetes Lease elector is not
/// implemented in this build: every role uses the no-op elector (with the `k8s` feature a
/// warning is logged). Double finalization is still prevented by the orphan CAS.
#[must_use]
pub fn elector_for(role: &str) -> Arc<dyn LeaderElector> {
    #[cfg(feature = "k8s")]
    tracing::warn!(role, "Kubernetes Lease elector not implemented; using the no-op leader elector");
    #[cfg(not(feature = "k8s"))]
    tracing::debug!(role, "using the no-op leader elector");
    Arc::new(AlwaysLeader)
}

/// Runs the watchdog loop until `cancel` fires.
pub async fn run(deps: Arc<Deps>, cancel: CancellationToken) {
    let elector = elector_for("orphan-watchdog");
    run_with_elector(deps, elector, cancel).await;
}

/// Watchdog loop with an explicit elector.
pub async fn run_with_elector(deps: Arc<Deps>, elector: Arc<dyn LeaderElector>, cancel: CancellationToken) {
    let interval = Duration::from_secs(deps.cfg.orphan_watchdog.scan_interval_secs.max(1));
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = ticker.tick() => {}
        }
        if !elector.is_leader().await {
            continue;
        }
        let started = std::time::Instant::now();
        match scan_once(&deps, OffsetDateTime::now_utc()).await {
            Ok(n) if n > 0 => tracing::info!(finalized = n, "orphan watchdog finalized turns"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "orphan watchdog scan failed"),
        }
        tracing::debug!(elapsed_ms = started.elapsed().as_millis(), "orphan watchdog scan finished");
    }
}

/// Stale-progress predicate: `last_progress_at <= cutoff OR (last_progress_at IS NULL AND
/// started_at <= cutoff)`.
fn stale(cutoff: OffsetDateTime) -> Condition {
    Condition::any()
        .add(chat_turn::Column::LastProgressAt.lte(cutoff))
        .add(
            Condition::all()
                .add(chat_turn::Column::LastProgressAt.is_null())
                .add(chat_turn::Column::StartedAt.lte(cutoff)),
        )
}

fn running_not_deleted() -> Condition {
    Condition::all()
        .add(chat_turn::Column::State.eq("running"))
        .add(chat_turn::Column::DeletedAt.is_null())
}

/// One scan at `now`. Returns the number of turns finalized by this scan.
///
/// # Errors
/// Candidate query failure (per-turn failures are logged and skipped).
pub async fn scan_once(deps: &Arc<Deps>, now: OffsetDateTime) -> Result<usize, DomainError> {
    let timeout = i64::try_from(deps.cfg.orphan_watchdog.timeout_secs).unwrap_or(i64::MAX);
    let cutoff = now - time::Duration::seconds(timeout);
    let candidates = {
        let conn = deps.db.conn()?;
        chat_turn::Entity::find()
            .filter(running_not_deleted().add(stale(cutoff)))
            .order_by_asc(chat_turn::Column::StartedAt)
            .limit(SCAN_LIMIT)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await?
    };
    let quota = Arc::new(QuotaService::new(Arc::clone(deps)));
    let mut finalized = 0;
    for turn in candidates {
        tracing::info!(turn_id = %turn.id, reason = "stale_progress", "orphan turn candidate detected");
        match finalize_orphan(deps, &quota, turn.id, cutoff, now).await {
            Ok(true) => finalized += 1,
            Ok(false) => tracing::debug!(turn_id = %turn.id, "orphan candidate no longer finalizable"),
            Err(e) => tracing::warn!(turn_id = %turn.id, error = %e, "orphan finalization failed"),
        }
    }
    Ok(finalized)
}

/// Finalizes one candidate in a single transaction. `Ok(false)` when the CAS lost.
async fn finalize_orphan(
    deps: &Arc<Deps>,
    quota: &Arc<QuotaService>,
    turn_id: Uuid,
    cutoff: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<bool, DomainError> {
    let quota = Arc::clone(quota);
    let outbox = Arc::clone(&deps.outbox);
    let wakes = deps
        .db
        .transaction(move |tx| {
            Box::pin(async move {
                let scope = AccessScope::allow_all();
                let won = chat_turn::Entity::update_many()
                    .col_expr(chat_turn::Column::State, Expr::value("failed"))
                    .col_expr(chat_turn::Column::ErrorCode, Expr::value(stream_codes::ORPHAN_TIMEOUT))
                    .col_expr(chat_turn::Column::CompletedAt, Expr::value(now))
                    .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
                    .filter(
                        running_not_deleted()
                            .add(chat_turn::Column::Id.eq(turn_id))
                            .add(stale(cutoff)),
                    )
                    .secure()
                    .scope_with(&scope)
                    .exec(tx)
                    .await?
                    .rows_affected;
                if won != 1 {
                    return Ok(None);
                }
                let turn = chat_turn::Entity::find()
                    .secure()
                    .scope_with(&scope)
                    .and_id(turn_id)?
                    .one(tx)
                    .await?
                    .ok_or_else(|| DomainError::internal("finalized orphan turn vanished"))?;
                let (outcome, method) =
                    derive_billing(TerminalState::Failed, Some(stream_codes::ORPHAN_TIMEOUT), None);
                let reserve = match (
                    turn.requester_user_id,
                    turn.reserve_tokens,
                    turn.max_output_tokens_applied,
                    turn.reserved_credits_micro,
                    turn.policy_version_applied,
                    turn.effective_model.clone(),
                    turn.minimal_generation_floor_applied,
                ) {
                    (Some(user), Some(rt), Some(mo), Some(rc), Some(pv), Some(model), Some(floor)) => {
                        Some((user, rt, mo, rc, pv, model, floor))
                    }
                    _ => None,
                };
                let web = u32::try_from(turn.web_search_completed_count).unwrap_or(0);
                let ci = u32::try_from(turn.code_interpreter_completed_count).unwrap_or(0);
                let fs = u32::try_from(turn.file_search_completed_count).unwrap_or(0);
                let (credits, model, policy_version) = if let Some((user, rt, mo, rc, pv, model, floor)) = reserve {
                    let policy_version = u64::try_from(pv).unwrap_or(0);
                    let res = quota
                        .settle_in_tx(
                            tx,
                            &SettlementInput {
                                tenant_id: turn.tenant_id,
                                user_id: user,
                                effective_model: model.clone(),
                                policy_version,
                                reserve_tokens: rt,
                                max_output_tokens_applied: i64::from(mo),
                                reserved_credits_micro: rc,
                                minimal_generation_floor_applied: i64::from(floor),
                                periods: QuotaPeriods::of(turn.started_at),
                                method,
                                usage: None,
                                web_search_calls: web,
                                code_interpreter_calls: ci,
                            },
                        )
                        .await?;
                    (res.committed_credits_micro, model, policy_version)
                } else {
                    tracing::warn!(
                        turn_id = %turn.id,
                        "orphan turn without reserve fields or requester; quota settlement skipped"
                    );
                    (0, String::new(), 0)
                };
                let usage_ev = build_usage_event(&UsageEventInput {
                    tenant_id: turn.tenant_id,
                    user_id: turn.requester_user_id,
                    chat_id: turn.chat_id,
                    turn_id: turn.id,
                    request_id: turn.request_id,
                    effective_model: model.clone(),
                    selected_model: model.clone(),
                    terminal_state: TerminalState::Failed,
                    outcome,
                    method,
                    usage: None,
                    actual_credits_micro: credits,
                    policy_version_applied: policy_version,
                    web_search_calls: web,
                    code_interpreter_calls: ci,
                    file_search_calls: fs,
                });
                let latency_ms = u64::try_from((now - turn.started_at).whole_milliseconds()).unwrap_or(0);
                let audit_ev = build_turn_audit_event(&TurnAuditInput {
                    tenant_id: turn.tenant_id,
                    requester_user_id: turn.requester_user_id,
                    chat_id: turn.chat_id,
                    turn_id: turn.id,
                    request_id: turn.request_id,
                    selected_model: model.clone(),
                    effective_model: model,
                    terminal_state: TerminalState::Failed,
                    error_code: Some(stream_codes::ORPHAN_TIMEOUT.to_owned()),
                    usage: None,
                    latency_ms,
                    web_search_calls: web,
                    file_search_calls: fs,
                    quota_decision: "unknown".to_owned(),
                    downgrade_from: None,
                    downgrade_reason: None,
                });
                let w1 = outbox.enqueue_usage(tx, &usage_ev).await?;
                let w2 = outbox.enqueue_audit(tx, &audit_ev).await?;
                Ok(Some((w1, w2)))
            })
        })
        .await?;
    match wakes {
        Some((w1, w2)) => {
            w1.fire();
            w2.fire();
            tracing::info!(turn_id = %turn_id, reason = "stale_progress", "orphan turn finalized");
            Ok(true)
        }
        None => Ok(false),
    }
}

#[cfg(test)]
#[path = "orphan_watchdog_tests.rs"]
mod orphan_watchdog_tests;
