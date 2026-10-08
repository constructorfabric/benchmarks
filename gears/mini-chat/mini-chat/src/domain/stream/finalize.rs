//! Turn finalization (DESIGN §5.7–5.9): one transaction with the CAS guard
//! on `state = 'running'`, the quota settlement and the outbox enqueue of
//! the usage and audit events (plus the thread-summary work item).

use mini_chat_sdk::{
    AuditEvent, AuditLatency, ModelTier, PolicyDecisions, QuotaPolicyDecision, ToolCalls,
    TurnAuditEvent, UsageEvent, UsageTokens, UserLimits,
};
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::DBRunner;
use uuid::Uuid;

use super::QuotaWarningView;
use crate::domain::error::{DomainError, QuotaScope};
use crate::domain::quota::{
    CreditsError, Periods, Usage, buckets_for, credits_micro, limit_for, statuses,
};
use crate::domain::service::Services;
use crate::infra::db::entities::message;
use crate::infra::db::repo::messages::{self, OrderKey};
use crate::infra::db::repo::quota::{self, BucketKey, Delta};
use crate::infra::db::repo::summaries;
use crate::infra::db::repo::turns::{self, Terminal};
use crate::infra::db::{now_ts, tenant_scope};
use crate::infra::outbox::{OutboxBridge, Queue, ThreadSummaryMsg};

/// Book the reserve on the user's bucket rows and re-check the limits in the
/// same transaction (TOCTOU, ADR-0008).
#[allow(clippy::too_many_arguments)]
pub async fn apply_reserve(
    runner: &impl DBRunner,
    tenant: Uuid,
    user: Uuid,
    periods: Periods,
    tier: ModelTier,
    reserved_credits: i64,
    limits: &UserLimits,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let scope = quota::user_scope(tenant, user);
    for (pt, ps) in periods.list() {
        for bucket in buckets_for(tier) {
            quota::apply_delta(
                runner,
                &scope,
                tenant,
                user,
                &BucketKey {
                    period_type: pt,
                    period_start: ps,
                    bucket,
                },
                Delta {
                    reserved_credits_micro: reserved_credits,
                    ..Delta::default()
                },
                now,
            )
            .await?;
        }
    }
    let rows = quota::rows_for_periods(runner, &scope, tenant, user, &periods.list()).await?;
    let usage = Usage::from_rows(&rows, periods);
    for bucket in buckets_for(tier) {
        for (pt, _) in periods.list() {
            let (spent, reserved) = usage.used(pt, bucket);
            if spent.saturating_add(reserved) > limit_for(limits, bucket, pt) {
                return Err(DomainError::QuotaExceeded(QuotaScope::Tokens));
            }
        }
    }
    Ok(())
}

/// How a turn is settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleMethod {
    Actual(UsageTokens),
    Estimated,
    Released,
}

/// Persisted reserve fields of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReserveFields {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserved_credits_micro: i64,
    pub floor_applied: i64,
}

/// Settlement inputs.
#[derive(Debug, Clone, Copy)]
pub struct Settlement {
    pub tenant: Uuid,
    pub user: Uuid,
    pub periods: Periods,
    pub tier: ModelTier,
    pub in_mult: i64,
    pub out_mult: i64,
    pub reserve: ReserveFields,
    pub method: SettleMethod,
    pub web_search_calls: i32,
    pub code_interpreter_calls: i32,
    pub tolerance: f64,
}

/// Result of a settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settled {
    pub credits: i64,
    pub method: &'static str,
    pub overshoot: bool,
    pub capped: bool,
}

/// Committed credits of a settlement (DESIGN §5.4.4–5.4.5, §5.8).
pub fn compute_settlement(s: &Settlement) -> Result<Settled, CreditsError> {
    match s.method {
        SettleMethod::Actual(u) => {
            let actual = credits_micro(u.input_tokens, u.output_tokens, s.in_mult, s.out_mult)?;
            let actual_tokens = u.input_tokens.saturating_add(u.output_tokens);
            let overshoot = actual_tokens > s.reserve.reserve_tokens;
            #[allow(clippy::cast_precision_loss)]
            let capped = overshoot
                && s.reserve.reserve_tokens > 0
                && (actual_tokens as f64) / (s.reserve.reserve_tokens as f64) > s.tolerance;
            Ok(Settled {
                credits: if capped {
                    s.reserve.reserved_credits_micro
                } else {
                    actual
                },
                method: "actual",
                overshoot,
                capped,
            })
        }
        SettleMethod::Estimated => {
            let est_in = (s.reserve.reserve_tokens - s.reserve.max_output_tokens_applied).max(0);
            let credits = credits_micro(
                est_in,
                s.reserve.floor_applied.max(0),
                s.in_mult,
                s.out_mult,
            )?;
            Ok(Settled {
                credits,
                method: "estimated",
                overshoot: false,
                capped: false,
            })
        }
        SettleMethod::Released => Ok(Settled {
            credits: 0,
            method: "released",
            overshoot: false,
            capped: false,
        }),
    }
}

/// Apply a settlement to the bucket rows (bucket `total`, plus
/// `tier:premium` for premium turns).
pub async fn apply_settlement(
    runner: &impl DBRunner,
    s: &Settlement,
    settled: &Settled,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    let scope = quota::user_scope(s.tenant, s.user);
    let (in_tok, out_tok) = match s.method {
        SettleMethod::Actual(u) => (u.input_tokens, u.output_tokens),
        _ => (0, 0),
    };
    let (ws, ci) = match s.method {
        SettleMethod::Released => (0, 0),
        _ => (s.web_search_calls, s.code_interpreter_calls),
    };
    for (pt, ps) in s.periods.list() {
        for bucket in buckets_for(s.tier) {
            let total = *bucket == quota::BUCKET_TOTAL;
            quota::apply_delta(
                runner,
                &scope,
                s.tenant,
                s.user,
                &BucketKey {
                    period_type: pt,
                    period_start: ps,
                    bucket,
                },
                Delta {
                    spent_credits_micro: settled.credits,
                    reserved_credits_micro: -s.reserve.reserved_credits_micro,
                    calls: 1,
                    input_tokens: if total { in_tok } else { 0 },
                    output_tokens: if total { out_tok } else { 0 },
                    web_search_calls: if total { ws } else { 0 },
                    code_interpreter_calls: if total { ci } else { 0 },
                },
                now,
            )
            .await?;
        }
    }
    Ok(())
}

/// `{tenant}/{turn}/{request}` in the simple UUID form.
#[must_use]
pub fn dedupe_key(tenant: Uuid, turn: Uuid, request: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant.as_simple(),
        turn.as_simple(),
        request.as_simple()
    )
}

/// Billing outcome of a terminal condition (DESIGN §5.8).
#[must_use]
pub fn billing_outcome(state: &str, error_code: Option<&str>) -> &'static str {
    match (state, error_code) {
        ("completed", _) => "completed",
        ("cancelled", _) | ("failed", Some("orphan_timeout")) => "aborted",
        _ => "failed",
    }
}

/// Settlement method of a terminal condition.
#[must_use]
pub fn settle_method(
    state: &str,
    error_code: Option<&str>,
    usage: Option<UsageTokens>,
) -> SettleMethod {
    match state {
        "completed" => SettleMethod::Actual(usage.unwrap_or_default()),
        "cancelled" => SettleMethod::Estimated,
        _ => match error_code {
            Some("orphan_timeout") => SettleMethod::Estimated,
            Some(
                "context_length_exceeded"
                | "validation_error"
                | "input_too_long"
                | "turn_setup_failed",
            ) => SettleMethod::Released,
            _ => match usage {
                Some(u) if u.input_tokens > 0 || u.output_tokens > 0 => SettleMethod::Actual(u),
                _ => SettleMethod::Estimated,
            },
        },
    }
}

/// Everything the finalization of a streaming turn needs.
#[derive(Debug, Clone)]
pub struct FinalizeCtx {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub tier: ModelTier,
    pub in_mult: i64,
    pub out_mult: i64,
    pub policy_version: u64,
    pub reserve: ReserveFields,
    pub periods: Periods,
    pub limits: UserLimits,
    pub downgrade: bool,
    pub downgrade_reason: Option<String>,
    pub started: std::time::Instant,
    pub summary_trigger: bool,
}

/// Terminal outcome of a streaming turn.
#[derive(Debug, Clone)]
pub enum Outcome {
    Completed {
        text: String,
        usage: Option<UsageTokens>,
        response_id: Option<String>,
    },
    Failed {
        code: String,
        message: String,
        usage: Option<UsageTokens>,
        response_id: Option<String>,
    },
    Cancelled {
        text: String,
    },
}

impl Outcome {
    #[must_use]
    pub const fn state(&self) -> &'static str {
        match self {
            Self::Completed { .. } => turns::STATE_COMPLETED,
            Self::Failed { .. } => turns::STATE_FAILED,
            Self::Cancelled { .. } => turns::STATE_CANCELLED,
        }
    }
}

/// Tool-call counts of a turn.
#[derive(Debug, Clone, Copy, Default)]
pub struct Counts {
    pub web_search: i32,
    pub code_interpreter: i32,
    pub file_search: i32,
    pub ttft_ms: Option<u64>,
}

/// Result of a finalization attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finalized {
    /// This finalizer won the CAS and committed `state`.
    Committed { state: &'static str },
    /// The assistant message could not be persisted; the turn is `failed`.
    PersistFailed,
    /// Another finalizer already finalized the turn.
    Lost,
    /// The finalization transaction failed; the turn stays `running`.
    TxFailed(String),
}

fn usage_event(
    fc: &FinalizeCtx,
    state: &str,
    error_code: Option<&str>,
    settled: &Settled,
    usage: Option<UsageTokens>,
    counts: Counts,
) -> UsageEvent {
    UsageEvent {
        tenant_id: fc.tenant_id,
        user_id: Some(fc.user_id),
        chat_id: Some(fc.chat_id),
        turn_id: Some(fc.turn_id),
        request_id: fc.request_id,
        effective_model: fc.effective_model.clone(),
        selected_model: fc.selected_model.clone(),
        terminal_state: state.to_owned(),
        billing_outcome: billing_outcome(state, error_code).to_owned(),
        usage: if settled.method == "actual" {
            usage.or(Some(UsageTokens::default()))
        } else {
            None
        },
        actual_credits_micro: settled.credits,
        settlement_method: settled.method.to_owned(),
        policy_version_applied: fc.policy_version,
        web_search_calls: u32::try_from(counts.web_search).unwrap_or(0),
        code_interpreter_calls: u32::try_from(counts.code_interpreter).unwrap_or(0),
        file_search_calls: u32::try_from(counts.file_search).unwrap_or(0),
        timestamp: OffsetDateTime::now_utc(),
        requester_type: "user".to_owned(),
        dedupe_key: dedupe_key(fc.tenant_id, fc.turn_id, fc.request_id),
        system_task_type: None,
    }
}

fn turn_audit(
    fc: &FinalizeCtx,
    state: &str,
    error_code: Option<&str>,
    usage: Option<UsageTokens>,
    counts: Counts,
) -> AuditEvent {
    let total_ms = u64::try_from(fc.started.elapsed().as_millis()).unwrap_or(u64::MAX);
    AuditEvent::Turn(Box::new(TurnAuditEvent {
        event_type: if state == turns::STATE_COMPLETED {
            "turn_completed"
        } else {
            "turn_failed"
        }
        .to_owned(),
        timestamp: OffsetDateTime::now_utc(),
        tenant_id: fc.tenant_id,
        requester_type: "user".to_owned(),
        user_id: Some(fc.user_id),
        chat_id: fc.chat_id,
        turn_id: fc.turn_id,
        request_id: fc.request_id,
        selected_model: fc.selected_model.clone(),
        effective_model: fc.effective_model.clone(),
        terminal_state: state.to_owned(),
        error_code: error_code.map(str::to_owned),
        usage,
        latency: AuditLatency {
            ttft_ms: counts.ttft_ms,
            total_ms,
        },
        tool_calls: ToolCalls {
            web_search_calls: u64::try_from(counts.web_search).unwrap_or(0),
            file_search_calls: u64::try_from(counts.file_search).unwrap_or(0),
        },
        policy_decisions: PolicyDecisions {
            quota: QuotaPolicyDecision {
                decision: if fc.downgrade { "downgrade" } else { "allow" }.to_owned(),
                downgrade_from: fc.downgrade.then(|| fc.selected_model.clone()),
                downgrade_reason: fc.downgrade_reason.clone(),
            },
            license: None,
        },
        prompt: None,
        response: None,
        attachments: vec![],
        quota_scope: None,
        trace_id: None,
    }))
}

/// Thread summary scheduling inside the finalization transaction. Returns the
/// wake when a work item was enqueued.
async fn schedule_summary(
    runner: &(impl DBRunner + Sync),
    ob: &OutboxBridge,
    fc: &FinalizeCtx,
) -> Result<Option<Wake>, DomainError> {
    let scope = tenant_scope(fc.tenant_id);
    let Some(target) =
        messages::latest_key_excluding_request(runner, &scope, fc.chat_id, fc.request_id).await?
    else {
        return Ok(None);
    };
    let base: Option<OrderKey> = summaries::find(runner, &scope, fc.chat_id)
        .await?
        .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
    if let Some(b) = base
        && b >= target
    {
        return Ok(None);
    }
    let msg = ThreadSummaryMsg {
        tenant_id: fc.tenant_id,
        chat_id: fc.chat_id,
        system_request_id: Uuid::new_v4(),
        base_frontier_created_at: base.map(|b| b.0),
        base_frontier_message_id: base.map(|b| b.1),
        frozen_target_created_at: target.0,
        frozen_target_message_id: target.1,
        system_task_type: "thread_summary_update".to_owned(),
    };
    Ok(Some(
        ob.enqueue(runner, Queue::ThreadSummary, fc.chat_id, &msg)
            .await?,
    ))
}

impl Services {
    /// Quota warnings of a user after settlement.
    pub async fn quota_warnings(
        &self,
        tenant: Uuid,
        user: Uuid,
        limits: &UserLimits,
    ) -> Vec<QuotaWarningView> {
        let periods = Periods::at(OffsetDateTime::now_utc());
        let Ok(conn) = self.db.conn() else {
            return vec![];
        };
        let rows = quota::rows_for_periods(
            &conn,
            &quota::user_scope(tenant, user),
            tenant,
            user,
            &periods.list(),
        )
        .await
        .unwrap_or_default();
        let usage = Usage::from_rows(&rows, periods);
        statuses(
            &usage,
            limits,
            periods,
            self.cfg.quota.warning_threshold_pct,
        )
        .into_iter()
        .map(|s| QuotaWarningView {
            tier: s.tier,
            period: s.period,
            remaining_percentage: s.remaining_percentage,
            warning: s.warning,
            exhausted: s.exhausted,
            next_reset: (s.warning || s.exhausted).then_some(s.next_reset),
        })
        .collect()
    }

    /// Finalize a streaming turn: CAS, settlement, outbox (one transaction).
    pub async fn finalize_turn(
        &self,
        fc: &FinalizeCtx,
        outcome: &Outcome,
        counts: Counts,
    ) -> Finalized {
        let started = std::time::Instant::now();
        let res = self.finalize_tx(fc, outcome, counts, true).await;
        let out = match res {
            Ok(Some(state)) => Finalized::Committed { state },
            Ok(None) => Finalized::Lost,
            Err(FinalizeErr::Message(e)) => {
                tracing::warn!(error = %e, turn_id = %fc.turn_id, "assistant message persistence failed");
                match outcome {
                    Outcome::Completed {
                        usage, response_id, ..
                    } => {
                        let failed = Outcome::Failed {
                            code: "message_persistence_failed".to_owned(),
                            message: "The answer could not be persisted".to_owned(),
                            usage: *usage,
                            response_id: response_id.clone(),
                        };
                        match self.finalize_tx(fc, &failed, counts, false).await {
                            Ok(Some(_)) => Finalized::PersistFailed,
                            Ok(None) => Finalized::Lost,
                            Err(e) => Finalized::TxFailed(e.to_string()),
                        }
                    }
                    Outcome::Cancelled { .. } => {
                        match self.finalize_tx(fc, outcome, counts, false).await {
                            Ok(Some(state)) => Finalized::Committed { state },
                            Ok(None) => Finalized::Lost,
                            Err(e) => Finalized::TxFailed(e.to_string()),
                        }
                    }
                    Outcome::Failed { .. } => Finalized::TxFailed(e),
                }
            }
            Err(e) => Finalized::TxFailed(e.to_string()),
        };
        #[allow(clippy::cast_precision_loss)]
        self.metrics.record(
            "finalization_latency_ms",
            started.elapsed().as_millis() as f64,
            &[],
        );
        out
    }

    async fn finalize_tx(
        &self,
        fc: &FinalizeCtx,
        outcome: &Outcome,
        counts: Counts,
        with_message: bool,
    ) -> Result<Option<&'static str>, FinalizeErr> {
        let mut attempt = 0u32;
        loop {
            match self
                .finalize_tx_once(fc, outcome, counts, with_message)
                .await
            {
                Err(FinalizeErr::Db(DomainError::Contention(e))) if attempt < 6 => {
                    attempt += 1;
                    tracing::debug!(attempt, error = %e, "finalization contention; retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(25 * u64::from(attempt)))
                        .await;
                }
                other => return other,
            }
        }
    }

    async fn finalize_tx_once(
        &self,
        fc: &FinalizeCtx,
        outcome: &Outcome,
        counts: Counts,
        with_message: bool,
    ) -> Result<Option<&'static str>, FinalizeErr> {
        let state = outcome.state();
        let (error_code, error_detail, usage, response_id, text) = match outcome {
            Outcome::Completed {
                usage,
                response_id,
                text,
                ..
            } => (None, None, *usage, response_id.clone(), Some(text.clone())),
            Outcome::Failed {
                code,
                message,
                usage,
                response_id,
            } => (
                Some(code.clone()),
                Some(message.clone()),
                *usage,
                response_id.clone(),
                None,
            ),
            Outcome::Cancelled { text } => (
                None,
                None,
                None,
                None,
                (!text.is_empty()).then(|| text.clone()),
            ),
        };
        let persist_message = with_message && text.is_some();
        let method = settle_method(state, error_code.as_deref(), usage);
        let settlement = Settlement {
            tenant: fc.tenant_id,
            user: fc.user_id,
            periods: fc.periods,
            tier: fc.tier,
            in_mult: fc.in_mult,
            out_mult: fc.out_mult,
            reserve: fc.reserve,
            method,
            web_search_calls: counts.web_search,
            code_interpreter_calls: counts.code_interpreter,
            tolerance: self.cfg.quota.overshoot_tolerance_factor,
        };
        let settled = compute_settlement(&settlement)
            .map_err(|e| FinalizeErr::Other(format!("credit computation: {e}")))?;
        let usage_ev = usage_event(fc, state, error_code.as_deref(), &settled, usage, counts);
        let audit_ev = turn_audit(fc, state, error_code.as_deref(), usage, counts);
        let summary = state == turns::STATE_COMPLETED
            && fc.summary_trigger
            && self.cfg.thread_summary_worker.enabled;
        let ob = self.outbox.clone();
        let fcc = fc.clone();
        let msg_model = text.map(|t| {
            let u = usage.unwrap_or_default();
            let mut m = messages::new_model(
                fc.message_id,
                fc.tenant_id,
                fc.chat_id,
                fc.request_id,
                "assistant",
                t,
                now_ts(),
            );
            m.model = Some(fc.effective_model.clone());
            m.input_tokens = u.input_tokens.max(0);
            m.output_tokens = u.output_tokens.max(0);
            m.cache_read_input_tokens = u.cache_read_input_tokens.max(0);
            m.cache_write_input_tokens = u.cache_write_input_tokens.max(0);
            m.reasoning_tokens = u.reasoning_tokens.max(0);
            m.provider_response_id.clone_from(&response_id);
            m
        });
        let res = self
            .db
            .db()
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    let now = now_ts();
                    let ts = tenant_scope(fcc.tenant_id);
                    let assistant_id = if persist_message && let Some(m) = &msg_model {
                        insert_message(tx, &ts, m).await.map_err(|e| match e {
                            DomainError::Contention(_) => TxErr(FinalizeErr::Db(e)),
                            other => TxErr(FinalizeErr::Message(other.to_string())),
                        })?;
                        Some(m.id)
                    } else {
                        None
                    };
                    let won = turns::cas_finalize(
                        tx,
                        &ts,
                        fcc.turn_id,
                        &Terminal {
                            state,
                            error_code: error_code.clone(),
                            error_detail: error_detail.clone(),
                            assistant_message_id: assistant_id,
                            provider_response_id: response_id.clone(),
                            web_search_completed_count: Some(counts.web_search),
                            code_interpreter_completed_count: Some(counts.code_interpreter),
                            file_search_completed_count: Some(counts.file_search),
                        },
                        now,
                    )
                    .await?;
                    if !won {
                        return Err(TxErr(FinalizeErr::Lost));
                    }
                    apply_settlement(tx, &settlement, &settled, now).await?;
                    let mut wakes = Vec::new();
                    wakes.push(
                        ob.enqueue(tx, Queue::Usage, fcc.tenant_id, &usage_ev)
                            .await?,
                    );
                    wakes.push(
                        ob.enqueue(tx, Queue::Audit, fcc.tenant_id, &audit_ev)
                            .await?,
                    );
                    let mut scheduled = None;
                    if summary {
                        let w = schedule_summary(tx, &ob, &fcc).await?;
                        scheduled = Some(w.is_some());
                        if let Some(w) = w {
                            wakes.push(w);
                        }
                    }
                    Ok((wakes, scheduled))
                })
            })
            .await;
        match res {
            Ok((wakes, scheduled)) => {
                crate::infra::outbox::fire(wakes);
                if let Some(s) = scheduled {
                    self.metrics.inc(
                        "thread_summary_trigger",
                        &[("result", if s { "scheduled" } else { "not_needed" })],
                    );
                }
                for (pt, _) in fc.periods.list() {
                    if settled.method == "actual" {
                        self.metrics.inc("quota_commit", &[("period", pt)]);
                    }
                    if settled.overshoot {
                        self.metrics.inc("quota_overshoot", &[("period", pt)]);
                    }
                }
                Ok(Some(state))
            }
            Err(TxErr(FinalizeErr::Lost)) => Ok(None),
            Err(TxErr(e)) => Err(e),
        }
    }
}

async fn insert_message(
    tx: &impl DBRunner,
    scope: &toolkit_security::AccessScope,
    m: &message::Model,
) -> Result<(), DomainError> {
    messages::insert(tx, scope, m).await
}

/// Internal finalization error.
#[derive(Debug)]
pub enum FinalizeErr {
    Lost,
    Message(String),
    Other(String),
    Db(DomainError),
}

impl std::fmt::Display for FinalizeErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lost => write!(f, "CAS lost"),
            Self::Message(m) | Self::Other(m) => write!(f, "{m}"),
            Self::Db(e) => write!(f, "{e}"),
        }
    }
}

/// Transaction error wrapper (`From<DbError>` for the DB provider).
#[derive(Debug)]
pub struct TxErr(pub FinalizeErr);

impl From<toolkit_db::DbError> for TxErr {
    fn from(e: toolkit_db::DbError) -> Self {
        Self(FinalizeErr::Db(DomainError::from(e)))
    }
}

impl From<DomainError> for TxErr {
    fn from(e: DomainError) -> Self {
        Self(FinalizeErr::Db(e))
    }
}

#[cfg(test)]
mod tests {
    use mini_chat_sdk::{ModelTier, UsageTokens};
    use uuid::Uuid;

    use super::*;

    fn settlement(method: SettleMethod) -> Settlement {
        Settlement {
            tenant: Uuid::nil(),
            user: Uuid::nil(),
            periods: Periods::at(time::OffsetDateTime::now_utc()),
            tier: ModelTier::Standard,
            in_mult: 1_000_000,
            out_mult: 3_000_000,
            reserve: ReserveFields {
                reserve_tokens: 1_200,
                max_output_tokens_applied: 1_000,
                reserved_credits_micro: 200 + 3_000,
                floor_applied: 50,
            },
            method,
            web_search_calls: 1,
            code_interpreter_calls: 0,
            tolerance: 1.10,
        }
    }

    fn usage(i: i64, o: i64) -> UsageTokens {
        UsageTokens {
            input_tokens: i,
            output_tokens: o,
            ..UsageTokens::default()
        }
    }

    #[test]
    fn actual_settlement_charges_usage() {
        let s = compute_settlement(&settlement(SettleMethod::Actual(usage(100, 50)))).unwrap();
        assert_eq!(
            (s.credits, s.method, s.overshoot, s.capped),
            (250, "actual", false, false)
        );
    }

    #[test]
    fn overshoot_within_tolerance_is_charged_beyond_it_capped() {
        // 1300 tokens / 1200 reserved = 1.083 <= 1.10
        let s = compute_settlement(&settlement(SettleMethod::Actual(usage(300, 1000)))).unwrap();
        assert_eq!(
            (s.credits, s.overshoot, s.capped),
            (300 + 3000, true, false)
        );
        // 2000 / 1200 > 1.10 -> capped at the reserved credits
        let s = compute_settlement(&settlement(SettleMethod::Actual(usage(1000, 1000)))).unwrap();
        assert_eq!((s.credits, s.overshoot, s.capped), (3_200, true, true));
    }

    #[test]
    fn estimated_settlement_uses_input_estimate_plus_floor() {
        let s = compute_settlement(&settlement(SettleMethod::Estimated)).unwrap();
        // estimated input = 1200 - 1000 = 200, floor 50
        assert_eq!((s.credits, s.method), (200 + 150, "estimated"));
        assert!(
            s.credits
                < settlement(SettleMethod::Estimated)
                    .reserve
                    .reserved_credits_micro
        );
    }

    #[test]
    fn released_settlement_charges_nothing() {
        let s = compute_settlement(&settlement(SettleMethod::Released)).unwrap();
        assert_eq!((s.credits, s.method), (0, "released"));
    }

    #[test]
    fn billing_outcome_mapping() {
        assert_eq!(billing_outcome("completed", None), "completed");
        assert_eq!(billing_outcome("cancelled", None), "aborted");
        assert_eq!(billing_outcome("failed", Some("orphan_timeout")), "aborted");
        assert_eq!(billing_outcome("failed", Some("provider_error")), "failed");
        assert_eq!(
            billing_outcome("failed", Some("turn_setup_failed")),
            "failed"
        );
    }

    #[test]
    fn settle_method_mapping() {
        assert_eq!(
            settle_method("completed", None, None),
            SettleMethod::Actual(UsageTokens::default())
        );
        assert_eq!(
            settle_method("cancelled", None, Some(usage(5, 5))),
            SettleMethod::Estimated
        );
        assert_eq!(
            settle_method("failed", Some("orphan_timeout"), None),
            SettleMethod::Estimated
        );
        assert_eq!(
            settle_method("failed", Some("context_length_exceeded"), None),
            SettleMethod::Released
        );
        assert_eq!(
            settle_method("failed", Some("turn_setup_failed"), None),
            SettleMethod::Released
        );
        assert_eq!(
            settle_method("failed", Some("provider_error"), Some(usage(3, 0))),
            SettleMethod::Actual(usage(3, 0))
        );
        // zero usage counts as unknown on failure
        assert_eq!(
            settle_method("failed", Some("provider_error"), Some(usage(0, 0))),
            SettleMethod::Estimated
        );
        assert_eq!(
            settle_method("failed", Some("web_search_calls_exceeded"), None),
            SettleMethod::Estimated
        );
        assert_eq!(
            settle_method("failed", Some("something_new"), None),
            SettleMethod::Estimated
        );
    }

    #[test]
    fn dedupe_key_format() {
        let t = Uuid::from_u128(1);
        let k = dedupe_key(t, Uuid::from_u128(2), Uuid::from_u128(3));
        assert_eq!(k.split('/').count(), 3);
        assert!(!k.contains('-'));
    }
}
