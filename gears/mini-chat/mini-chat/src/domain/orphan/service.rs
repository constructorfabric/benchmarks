//! The watchdog scan: finds stale `running` turns and finalizes each one in its own transaction
//! (DESIGN "Orphan Turn Watchdog").

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mini_chat_sdk::{ModelTier, SettlementMethod, TurnLatency, UsageTerminalState};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit_macros::domain_model;
use toolkit_security::AccessScope;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::elector::LeaderElector;
use super::model::{OrphanCandidate, OrphanReserve};
use crate::domain::billing::{Settlement, SettlementBasis, derive_billing, settle};
use crate::domain::error::DomainError;
use crate::domain::metrics::Metrics;
use crate::domain::ports::ModelPolicyGateway;
use crate::domain::quota::periods_at;
use crate::domain::stream::{
    FinalizeCommand, FinalizeOutcome, TerminalFacts, ToolCounts, TurnFacts, TurnRepository,
    build_events,
};
use crate::domain::turn::TurnState;

/// The stored error code of an orphan-finalized turn.
const ORPHAN_ERROR_CODE: &str = "orphan_timeout";
/// The `reason` label of the orphan metrics.
const REASON: &str = "stale_progress";
/// The `trigger` label of `streams_aborted`.
const ABORT_TRIGGER: &str = "orphan_timeout";
/// Finalization attempts per scan (fixed, not configurable), also the size of one candidate query.
const SCAN_LIMIT: u64 = 100;
/// Candidates a scan attempts at most, transient failures included (bounds the exclusion list).
const MAX_ATTEMPTED_PER_SCAN: usize = 1_000;
/// How many timeouts past the orphan cutoff a turn is retried while the policy plugin is
/// transiently unavailable; after that the turn is finalized with a released reserve, so no turn
/// stays `running` indefinitely (stale for `timeout * (1 + RETRY_WINDOW_TIMEOUTS)`).
const RETRY_WINDOW_TIMEOUTS: u32 = 3;

/// Discovery of orphan candidates.
#[async_trait]
pub trait OrphanRepository: Send + Sync {
    /// At most `limit` `running`, non-deleted turns of any tenant whose `last_progress_at` (or
    /// `started_at` when NULL) is at or before `cutoff`, oldest progress first and without the
    /// turns in `attempted` (already tried by the running scan). Advisory: the finalization
    /// re-checks the predicate.
    async fn stale_turns(
        &self,
        cutoff: OffsetDateTime,
        limit: u64,
        attempted: &[Uuid],
    ) -> Result<Vec<OrphanCandidate>, DomainError>;
}

/// The result of one scan.
#[domain_model]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Candidates the scan found.
    pub detected: usize,
    /// Candidates this scan finalized.
    pub finalized: usize,
}

/// Collaborators and settings of the watchdog.
pub struct OrphanWatchdogParts {
    /// Candidate discovery.
    pub candidates: Arc<dyn OrphanRepository>,
    /// The finalization transaction (shared with the stream path).
    pub turns: Arc<dyn TurnRepository>,
    /// The policy snapshots the multipliers of a settlement come from.
    pub policy: Arc<dyn ModelPolicyGateway>,
    /// The metrics sink.
    pub metrics: Arc<dyn Metrics>,
    /// `orphan_watchdog.timeout_secs`.
    pub timeout: Duration,
    /// `orphan_watchdog.scan_interval_secs`.
    pub scan_interval: Duration,
}

/// The orphan watchdog.
#[domain_model]
pub struct OrphanWatchdog {
    candidates: Arc<dyn OrphanRepository>,
    turns: Arc<dyn TurnRepository>,
    policy: Arc<dyn ModelPolicyGateway>,
    metrics: Arc<dyn Metrics>,
    timeout: Duration,
    scan_interval: Duration,
}

impl OrphanWatchdog {
    /// Creates the watchdog.
    #[must_use]
    pub fn new(parts: OrphanWatchdogParts) -> Self {
        Self {
            candidates: parts.candidates,
            turns: parts.turns,
            policy: parts.policy,
            metrics: parts.metrics,
            timeout: parts.timeout,
            scan_interval: parts.scan_interval,
        }
    }

    /// Scans every `scan_interval` while this process is the leader, until `cancel` fires.
    pub async fn run(&self, elector: Arc<dyn LeaderElector>, cancel: CancellationToken) {
        info!(
            timeout_secs = self.timeout.as_secs(),
            scan_interval_secs = self.scan_interval.as_secs(),
            "the orphan watchdog started"
        );
        while self.wait_for_next_scan(&cancel).await {
            self.scan_as_leader(elector.as_ref()).await;
        }
        info!("the orphan watchdog stopped");
    }

    /// Sleeps one scan interval; `false` when cancelled meanwhile.
    async fn wait_for_next_scan(&self, cancel: &CancellationToken) -> bool {
        tokio::select! {
            () = cancel.cancelled() => false,
            () = tokio::time::sleep(self.scan_interval) => true,
        }
    }

    async fn scan_as_leader(&self, elector: &dyn LeaderElector) {
        if !elector.is_leader() {
            debug!("not the orphan watchdog leader; skipping the scan");
            return;
        }
        let report = self.scan_once(crate::domain::clock::now()).await;
        if report.detected > 0 {
            info!(
                detected = report.detected,
                finalized = report.finalized,
                "orphan watchdog scan finished"
            );
        }
    }

    /// One scan with `now` as the application clock.
    ///
    /// A candidate that could not be finalized this time (a transient failure) never blocks the
    /// ones behind it: it is excluded from the following queries of the scan, which refill the
    /// budget of [`SCAN_LIMIT`] finalization attempts from the next candidates.
    pub async fn scan_once(&self, now: OffsetDateTime) -> ScanReport {
        let started = Instant::now();
        let mut report = ScanReport::default();
        let cutoff = now - self.timeout;
        let mut budget = SCAN_LIMIT;
        let mut attempted: Vec<Uuid> = Vec::new();
        while budget > 0 && attempted.len() < MAX_ATTEMPTED_PER_SCAN {
            let candidates = match self
                .candidates
                .stale_turns(cutoff, budget, &attempted)
                .await
            {
                Ok(candidates) if !candidates.is_empty() => candidates,
                Ok(_) => break,
                Err(err) => {
                    warn!(error = %err, "the orphan scan could not list candidates");
                    break;
                }
            };
            for candidate in candidates {
                report.detected += 1;
                attempted.push(candidate.turn_id);
                match self.finalize_candidate(&candidate, cutoff, now).await {
                    Attempt::Finalized => {
                        report.finalized += 1;
                        budget = budget.saturating_sub(1);
                    }
                    Attempt::Lost => budget = budget.saturating_sub(1),
                    // Transient failures do not consume the budget of the scan.
                    Attempt::Failed => {}
                }
            }
        }
        self.metrics
            .orphan_scan_duration_seconds(started.elapsed().as_secs_f64());
        report
    }

    /// Counts and finalizes one candidate.
    async fn finalize_candidate(
        &self,
        candidate: &OrphanCandidate,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Attempt {
        self.metrics.orphan_detected(REASON);
        match self.finalize(candidate, cutoff, now).await {
            Ok(true) => Attempt::Finalized,
            Ok(false) => {
                debug!(turn_id = %candidate.turn_id, "the turn is no longer orphan-finalizable");
                Attempt::Lost
            }
            Err(err) => {
                error!(turn_id = %candidate.turn_id, error = %err, "the orphan turn could not be finalized; the next scan retries");
                Attempt::Failed
            }
        }
    }

    /// Finalizes one candidate. `Ok(false)` when the CAS found the turn no longer finalizable.
    ///
    /// # Errors
    /// Only transient failures (policy plugin unavailable, database): the turn stays `running`
    /// and the next scan retries. A deterministic failure of the settlement basis finalizes the
    /// turn with a released reserve instead.
    async fn finalize(
        &self,
        candidate: &OrphanCandidate,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let (billing_outcome, method) =
            derive_billing(UsageTerminalState::Failed, Some(ORPHAN_ERROR_CODE), None);
        let overdue = self.retry_window_elapsed(candidate, now);
        let plan = self.plan_of(candidate, method, overdue).await?;
        // The typed ids of the facts and the command are not optional; a NULL requester is the
        // nil id there, never booked (`book_quota` is off) and never sent in the usage event.
        let user_id = candidate.user_id.unwrap_or_else(Uuid::nil);
        let tool_counts = ToolCounts {
            web_search: candidate.web_search_calls,
            code_interpreter: candidate.code_interpreter_calls,
            file_search: candidate.file_search_calls,
        };
        let premium = plan.tier == Tier::Premium;
        // The selected model is not stored on the turn: the events carry the effective model.
        let facts = TurnFacts {
            turn_id: candidate.turn_id,
            tenant_id: candidate.tenant_id,
            user_id,
            chat_id: candidate.chat_id,
            request_id: candidate.request_id,
            assistant_message_id: Uuid::nil(),
            selected_model: plan.effective_model.clone(),
            effective_model: plan.effective_model,
            quota_decision: "unknown",
            downgrade_from: None,
            downgrade_reason: None,
            policy_version: plan.policy_version,
            started_at: candidate.started_at,
            basis: plan.basis,
            premium,
            thread_summary_tokens: None,
            trace_id: None,
            summary_trigger: None,
        };
        let terminal = TerminalFacts {
            state: UsageTerminalState::Failed,
            error_code: Some(ORPHAN_ERROR_CODE),
            usage: None,
            billing_outcome,
            tool_counts,
            latency: TurnLatency {
                time_to_first_token_ms: None,
                total_ms: u64::try_from((now - candidate.started_at).whole_milliseconds())
                    .unwrap_or(0),
            },
            now,
        };
        let (mut usage_event, audit_event) = build_events(&facts, &terminal, &plan.settlement);
        if candidate.user_id.is_none() {
            usage_event.user_id = None;
        }
        let command = FinalizeCommand {
            turn_id: candidate.turn_id,
            tenant_id: candidate.tenant_id,
            user_id,
            chat_id: candidate.chat_id,
            request_id: candidate.request_id,
            state: TurnState::Failed,
            error_code: Some(ORPHAN_ERROR_CODE.to_owned()),
            error_detail: None,
            provider_response_id: None,
            assistant_message: None,
            tool_counts,
            periods: periods_at(candidate.started_at),
            reserved_credits_micro: plan.basis.reserved_credits_micro,
            premium: match plan.tier {
                Tier::Premium => Some(true),
                Tier::Standard => Some(false),
                Tier::Unknown => None,
            },
            book_quota: plan.book_quota,
            orphan_cutoff: Some(cutoff),
            require_unreserved: candidate.reserve.is_none(),
            settlement: plan.settlement,
            now,
            usage_event,
            audit_event,
            summary: None,
        };
        // Background work carries the tenant of the row it finalizes.
        let scope = AccessScope::for_tenant(candidate.tenant_id);
        let outcome = self.turns.finalize(&scope, command).await?;
        if outcome == FinalizeOutcome::Committed {
            self.metrics.orphan_finalized(REASON);
            self.metrics.streams_aborted(ABORT_TRIGGER);
        }
        Ok(outcome == FinalizeOutcome::Committed)
    }

    /// Whether the turn has been stale for longer than the transient retry window: the cutoff
    /// plus [`RETRY_WINDOW_TIMEOUTS`] timeouts.
    fn retry_window_elapsed(&self, candidate: &OrphanCandidate, now: OffsetDateTime) -> bool {
        let progress = candidate.last_progress_at.unwrap_or(candidate.started_at);
        let window = self.timeout.saturating_mul(1 + RETRY_WINDOW_TIMEOUTS);
        now - progress >= window
    }

    /// How a candidate is settled (DESIGN "`FinalizeTurn` Invariant", orphan watchdog).
    ///
    /// A turn without reserve fields or requester books nothing. A reserved turn is settled on the
    /// estimated path with the multipliers of its recorded policy version; when that basis can
    /// never be resolved (version or model unknown, credit computation refused) the turn is still
    /// finalized and its reserve released without a charge, so it is never left `running`.
    ///
    /// # Errors
    /// The policy snapshot is temporarily unavailable and the retry window has not elapsed (`overdue`
    /// is false): the next scan retries.
    async fn plan_of(
        &self,
        candidate: &OrphanCandidate,
        method: SettlementMethod,
        overdue: bool,
    ) -> Result<SettlementPlan, DomainError> {
        let Some(user_id) = candidate.user_id else {
            warn!(
                turn_id = %candidate.turn_id,
                reserved = candidate.reserve.is_some(),
                "requester_user_id is NULL; the orphan turn is finalized without settlement"
            );
            return Ok(SettlementPlan::unreserved(method));
        };
        let Some(reserve) = &candidate.reserve else {
            warn!(
                turn_id = %candidate.turn_id,
                "the reserve fields are NULL; the orphan turn is finalized without settlement"
            );
            return Ok(SettlementPlan::unreserved(method));
        };
        self.settlement_of(candidate.turn_id, user_id, reserve, method, overdue)
            .await
    }

    /// The tier of the effective model of a turn whose recorded policy version cannot give it: the
    /// model in the current policy snapshot, `Unknown` when it is not there either. The quota rows
    /// are never consulted: the premium bucket is shared by every premium turn of the user.
    ///
    /// # Errors
    /// The current snapshot is temporarily unavailable and the retry window has not elapsed.
    async fn tier_of_current(
        &self,
        user_id: Uuid,
        reserve: &OrphanReserve,
        overdue: bool,
    ) -> Result<Tier, DomainError> {
        match self.policy.current_snapshot(user_id).await {
            Ok(snapshot) => Ok(snapshot
                .model_catalog
                .iter()
                .find(|entry| entry.id == reserve.effective_model)
                .map_or(Tier::Unknown, |entry| tier_of(entry.tier))),
            Err(err @ DomainError::PolicyUnavailable { .. }) if !overdue => Err(err),
            Err(_) => Ok(Tier::Unknown),
        }
    }

    /// The settlement of a reserved turn: the multipliers of the effective model in the immutable
    /// snapshot of the policy version the turn was admitted under.
    ///
    /// A transient policy failure is retried by the next scan until `overdue`; then, like any
    /// deterministic failure, it releases the reserve without a charge.
    async fn settlement_of(
        &self,
        turn_id: Uuid,
        user_id: Uuid,
        reserve: &OrphanReserve,
        method: SettlementMethod,
        overdue: bool,
    ) -> Result<SettlementPlan, DomainError> {
        let snapshot = match self
            .policy
            .snapshot_at(user_id, reserve.policy_version)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(err @ DomainError::PolicyUnavailable { .. }) if !overdue => return Err(err),
            Err(err) => {
                let tier = self.tier_of_current(user_id, reserve, overdue).await?;
                return Ok(SettlementPlan::released(
                    turn_id,
                    reserve,
                    tier,
                    &err.to_string(),
                ));
            }
        };
        let Some(entry) = snapshot
            .model_catalog
            .iter()
            .find(|entry| entry.id == reserve.effective_model)
        else {
            let tier = self.tier_of_current(user_id, reserve, overdue).await?;
            return Ok(SettlementPlan::released(
                turn_id,
                reserve,
                tier,
                &format!(
                    "model `{}` is not in policy version {}",
                    reserve.effective_model, reserve.policy_version
                ),
            ));
        };
        let tier = tier_of(entry.tier);
        let basis = SettlementBasis {
            reserve_tokens: reserve.reserve_tokens,
            max_output_tokens_applied: reserve.max_output_tokens_applied,
            reserved_credits_micro: reserve.reserved_credits_micro,
            minimal_generation_floor_applied: reserve.minimal_generation_floor_applied,
            in_mult: i64::try_from(entry.input_tokens_credit_multiplier_micro).unwrap_or(i64::MAX),
            out_mult: i64::try_from(entry.output_tokens_credit_multiplier_micro)
                .unwrap_or(i64::MAX),
        };
        match settle(&basis, method, None, 1.0) {
            Ok(settlement) => Ok(SettlementPlan {
                settlement,
                basis,
                book_quota: true,
                effective_model: reserve.effective_model.clone(),
                policy_version: reserve.policy_version,
                tier,
            }),
            Err(err) => Ok(SettlementPlan::released(
                turn_id,
                reserve,
                tier,
                &format!("orphan settlement refused: {err}"),
            )),
        }
    }
}

/// The tier of a catalog entry.
fn tier_of(tier: ModelTier) -> Tier {
    if tier == ModelTier::Premium {
        Tier::Premium
    } else {
        Tier::Standard
    }
}

/// The result of one finalization attempt.
enum Attempt {
    /// This scan finalized the turn.
    Finalized,
    /// The CAS found the turn no longer finalizable.
    Lost,
    /// A transient failure: the turn stays `running` for the next scan.
    Failed,
}

/// The tier a settlement books its reserve in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tier {
    Premium,
    Standard,
    /// The model could not be resolved.
    Unknown,
}

/// What the settlement of a candidate needs.
struct SettlementPlan {
    settlement: Settlement,
    basis: SettlementBasis,
    book_quota: bool,
    effective_model: String,
    policy_version: i64,
    tier: Tier,
}

impl SettlementPlan {
    /// A turn without reserve fields or requester: no quota change, empty models, policy
    /// version 0.
    fn unreserved(method: SettlementMethod) -> Self {
        Self {
            settlement: Settlement {
                method,
                committed_credits_micro: 0,
                telemetry_input_tokens: 0,
                telemetry_output_tokens: 0,
                overshoot: false,
            },
            basis: SettlementBasis {
                reserve_tokens: 0,
                max_output_tokens_applied: 0,
                reserved_credits_micro: 0,
                minimal_generation_floor_applied: 0,
                in_mult: 0,
                out_mult: 0,
            },
            book_quota: false,
            effective_model: String::new(),
            policy_version: 0,
            tier: Tier::Standard,
        }
    }

    /// The persisted reserve is released and nothing is charged, because the multipliers of the
    /// turn cannot be resolved.
    fn released(turn_id: Uuid, reserve: &OrphanReserve, tier: Tier, reason: &str) -> Self {
        error!(
            turn_id = %turn_id,
            reason,
            "the settlement basis of the orphan turn cannot be resolved; its reserve is released without a charge"
        );
        Self {
            settlement: Settlement {
                method: SettlementMethod::Released,
                committed_credits_micro: 0,
                telemetry_input_tokens: 0,
                telemetry_output_tokens: 0,
                overshoot: false,
            },
            basis: SettlementBasis {
                reserve_tokens: reserve.reserve_tokens,
                max_output_tokens_applied: reserve.max_output_tokens_applied,
                reserved_credits_micro: reserve.reserved_credits_micro,
                minimal_generation_floor_applied: reserve.minimal_generation_floor_applied,
                in_mult: 0,
                out_mult: 0,
            },
            book_quota: true,
            effective_model: reserve.effective_model.clone(),
            policy_version: reserve.policy_version,
            tier,
        }
    }
}
