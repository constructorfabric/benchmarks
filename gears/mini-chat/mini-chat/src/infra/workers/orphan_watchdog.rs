//! Orphan turn watchdog (DESIGN §4 "Orphan Turn Watchdog", Appendix B.9.1, ADR-0010).
//!
//! Finalizes `running` turns whose durable progress is older than `orphan_watchdog.timeout_secs`
//! through its own CAS (re-checking the stale-progress predicate), settles them on the estimated
//! path and enqueues one usage and one audit event in the same transaction.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mini_chat_sdk::{
    AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, MiniChatAuditEvent, TurnAuditEvent, UsageEvent,
};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::quota::{self, BillingOutcome, Settlement, SettlementInput, SettlementMethod};
use crate::domain::services::AppServices;
use crate::infra::db::entities::chat_turn::{self, Column};
use crate::infra::metrics;
use crate::infra::outbox::PAYLOAD_AUDIT;

/// Candidates fetched per scan (fixed).
pub const SCAN_LIMIT: u64 = 100;
pub const ERROR_CODE: &str = "orphan_timeout";
const REASON: &str = "stale_progress";

/// Runs until `cancel` fires.
pub async fn run(app: Arc<AppServices>, cancel: CancellationToken) {
    let cfg = app.cfg.orphan_watchdog.clone();
    if !cfg.enabled {
        cancel.cancelled().await;
        return;
    }
    let interval = Duration::from_secs(cfg.scan_interval_secs.max(1));
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(interval) => {}
        }
        if let Err(e) = scan_once(&app, crate::clock::now()).await {
            tracing::warn!(error = %e, "orphan watchdog scan failed");
        }
    }
}

/// `last_progress_at <= cutoff OR (last_progress_at IS NULL AND started_at <= cutoff)`.
fn stale_condition(cutoff: OffsetDateTime) -> Condition {
    Condition::any()
        .add(Column::LastProgressAt.lte(cutoff))
        .add(Condition::all().add(Column::LastProgressAt.is_null()).add(Column::StartedAt.lte(cutoff)))
}

fn running_stale(cutoff: OffsetDateTime) -> Condition {
    Condition::all()
        .add(Column::State.eq("running"))
        .add(Column::DeletedAt.is_null())
        .add(stale_condition(cutoff))
}

/// One scan at `now`: returns the number of turns finalized by this scan.
///
/// # Errors
/// DB errors of the candidate query (per-turn failures are logged and skipped).
pub async fn scan_once(app: &Arc<AppServices>, now: OffsetDateTime) -> Result<usize, DomainError> {
    let started = Instant::now();
    let now = crate::clock::normalize(now);
    let timeout = time::Duration::seconds(i64::try_from(app.cfg.orphan_watchdog.timeout_secs).unwrap_or(i64::MAX));
    let cutoff = crate::clock::normalize(now - timeout);
    let candidates = find_candidates(app, cutoff).await?;
    let mut finalized = 0;
    for turn in candidates {
        if process_candidate(app, turn, now, cutoff).await {
            finalized += 1;
        }
    }
    metrics::record("mini_chat_orphan_scan_duration_seconds", started.elapsed().as_secs_f64(), &[]);
    Ok(finalized)
}

/// Stale running turns (advisory: the CAS re-checks every predicate).
async fn find_candidates(app: &AppServices, cutoff: OffsetDateTime) -> Result<Vec<chat_turn::Model>, DomainError> {
    let conn = app.db.conn()?;
    let rows = chat_turn::Entity::find()
        .filter(running_stale(cutoff))
        .order_by(Column::StartedAt, Order::Asc)
        .limit(SCAN_LIMIT)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await?;
    Ok(rows)
}

/// Finalizes one candidate; `true` when this scan won the CAS.
async fn process_candidate(app: &Arc<AppServices>, turn: chat_turn::Model, now: OffsetDateTime, cutoff: OffsetDateTime) -> bool {
    metrics::incr("mini_chat_orphan_detected", 1, &[("reason", REASON.to_owned())]);
    let turn_id = turn.id;
    let res = finalize(app, turn, now, cutoff).await;
    if let Err(e) = &res {
        tracing::warn!(%turn_id, error = %e, "orphan finalization failed");
    }
    let won = matches!(res, Ok(true));
    if won {
        metrics::incr("mini_chat_orphan_finalized", 1, &[("reason", REASON.to_owned())]);
        tracing::info!(%turn_id, "orphan turn finalized");
    }
    won
}

fn has_reserve(turn: &chat_turn::Model) -> bool {
    turn.reserve_tokens.is_some()
        && turn.max_output_tokens_applied.is_some()
        && turn.reserved_credits_micro.is_some()
        && turn.policy_version_applied.is_some()
        && turn.effective_model.is_some()
        && turn.requester_user_id.is_some()
}

fn count(v: i32) -> u32 {
    u32::try_from(v).unwrap_or(0)
}

/// Builds the audit event of an orphan-finalized turn.
fn audit_event(turn: &chat_turn::Model, model: &str, now: OffsetDateTime) -> MiniChatAuditEvent {
    let latency = (now - turn.started_at).whole_milliseconds().max(0);
    MiniChatAuditEvent::Turn(TurnAuditEvent {
        event_type: "turn_failed".to_owned(),
        tenant_id: turn.tenant_id,
        user_id: turn.requester_user_id,
        chat_id: turn.chat_id,
        turn_id: turn.id,
        request_id: turn.request_id,
        selected_model: model.to_owned(),
        effective_model: model.to_owned(),
        terminal_state: "failed".to_owned(),
        error_code: Some(ERROR_CODE.to_owned()),
        usage: None,
        latency_ms: u64::try_from(latency).unwrap_or(u64::MAX),
        tool_calls: AuditToolCalls {
            web_search_calls: count(turn.web_search_completed_count),
            file_search_calls: count(turn.file_search_completed_count),
        },
        policy_decisions: AuditPolicyDecisions {
            quota: AuditQuotaDecision { decision: "unknown".to_owned(), downgrade_from: None, downgrade_reason: None },
            license: None,
            quota_scope: None,
        },
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        trace_id: None,
        timestamp: now,
    })
}

/// CAS-finalizes one candidate; `Ok(false)` when the CAS lost.
async fn finalize(
    app: &Arc<AppServices>,
    turn: chat_turn::Model,
    now: OffsetDateTime,
    cutoff: OffsetDateTime,
) -> Result<bool, DomainError> {
    let app2 = Arc::clone(app);
    let wake: Option<Wake> = app
        .db
        .transaction(move |tx| {
            Box::pin(async move {
                let app = &*app2;
                let res = chat_turn::Entity::update_many()
                    .secure()
                    .col_expr(Column::State, Expr::value("failed"))
                    .col_expr(Column::ErrorCode, Expr::value(ERROR_CODE))
                    .col_expr(Column::CompletedAt, Expr::value(now))
                    .col_expr(Column::UpdatedAt, Expr::value(now))
                    .filter(Condition::all().add(Column::Id.eq(turn.id)).add(running_stale(cutoff)))
                    .scope_with(&AccessScope::allow_all())
                    .exec(tx)
                    .await?;
                if res.rows_affected == 0 {
                    return Ok(None);
                }
                let settled = has_reserve(&turn);
                let input = SettlementInput {
                    tenant_id: turn.tenant_id,
                    user_id: turn.requester_user_id.unwrap_or(Uuid::nil()),
                    turn: turn.clone(),
                    billing_outcome: BillingOutcome::Aborted,
                    method: SettlementMethod::Estimated,
                    usage: None,
                    web_search_calls: count(turn.web_search_completed_count),
                    code_interpreter_calls: count(turn.code_interpreter_completed_count),
                    periods: quota::period_starts(turn.started_at),
                };
                let (billing, method) = quota::derive_billing("failed", Some(ERROR_CODE), None);
                let settlement = if settled {
                    quota::settle(app, tx, &input).await?
                } else {
                    tracing::warn!(turn_id = %turn.id, "orphan turn without reserve or requester; settlement skipped");
                    Settlement { method, billing_outcome: billing, actual_credits_micro: 0, overshoot_capped: false }
                };
                let model = if settled { turn.effective_model.clone().unwrap_or_default() } else { String::new() };
                let file_search = count(turn.file_search_completed_count);
                let mut event: UsageEvent = quota::usage_event(&input, &settlement, &model, file_search, "failed", now);
                if !settled {
                    event.effective_model = String::new();
                    event.selected_model = String::new();
                    event.policy_version_applied = 0;
                    event.usage = None;
                    event.actual_credits_micro = 0;
                }
                let mut wake = quota::enqueue_usage(app, tx, &event).await?;
                let audit = audit_event(&turn, &model, now);
                wake += app.outbox.enqueue_json(tx, app.outbox.audit_queue(), turn.tenant_id, PAYLOAD_AUDIT, &audit).await?;
                Ok(Some(wake))
            })
        })
        .await?;
    Ok(wake.map(Wake::fire).is_some())
}

#[cfg(test)]
#[path = "orphan_watchdog_tests.rs"]
mod tests;
