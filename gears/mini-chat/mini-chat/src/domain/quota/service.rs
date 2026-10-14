//! The quota service: the preflight decision, the reserve write and the finalization
//! settlement contracts the streaming pipeline calls, and the quota status read.
//!
//! `preflight` writes nothing and reads the clock exactly once. `write_reserve` and
//! `finalize_turn` run inside the caller's open transaction `tx` (the pipeline composes them
//! with its own writes); the caller commits, then fires the returned `Wake` and calls the
//! `record_after_commit` of the receipt or the result. Neither contract has a dependency on
//! the provider or on OAGW.
use std::sync::Arc;

use mini_chat_sdk::{
    ModelTier, PolicySnapshot, QuotaDecision, UsageEvent, UsageTokens, UserLimits,
};
use time::OffsetDateTime;
use toolkit_db::DBProvider;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::DbTx;
use toolkit_macros::domain_model;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::authz::{Operation, PepService};
use crate::domain::error::{DomainError, QuotaScope};
use crate::domain::model_policy::ModelPolicyGateway;
use crate::domain::quota::availability::{BucketLimit, bucket_available, checked_limits};
use crate::domain::quota::billing::{
    BillingDerivation, BillingOutcome, SettlementMethod, derive_billing,
};
use crate::domain::quota::cascade::{CascadeInput, DowngradeReason, run_cascade};
use crate::domain::quota::estimation::RequestFacts;
use crate::domain::quota::periods::{PeriodStarts, PeriodType};
use crate::domain::quota::ports::{
    Clock, QuotaLedger, QuotaMetrics, ReserveIncrement, SettleDelta, TurnPreflightColumns,
    TurnRecord, TurnTerminalUpdate, UsageQueue,
};
use crate::domain::quota::rows::{Bucket, BucketKey, QuotaRows};
use crate::domain::quota::settings::QuotaSettings;
use crate::domain::quota::settlement::{SettlementComputation, TurnBilling, compute_settlement};
use crate::domain::quota::status::{
    QuotaStatus, QuotaWarning, TierStatus, compute_status, quota_warnings,
};
use crate::domain::quota::tool_quota::check_tool_quota;
use crate::domain::quota::usage_event::{
    NoReserveUsageEventInput, ToolCallCounts, TurnUsageEventInput, build_no_reserve_usage_event,
    build_turn_usage_event,
};

/// The collaborators of the [`QuotaService`].
#[domain_model]
pub struct QuotaDeps {
    pub pep: Arc<PepService>,
    pub gateway: Arc<dyn ModelPolicyGateway>,
    pub db: Arc<DBProvider<DomainError>>,
    pub ledger: Arc<dyn QuotaLedger>,
    pub queue: Arc<dyn UsageQueue>,
    pub metrics: Arc<dyn QuotaMetrics>,
    pub clock: Arc<dyn Clock>,
    pub settings: QuotaSettings,
}

/// The facts of one request the preflight needs.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightRequest {
    /// The chat of the turn (for `prior_context_tokens`).
    pub chat_id: Uuid,
    pub selected_model: String,
    pub facts: RequestFacts,
}

/// The preflight decision: the values the send pipeline persists and uses.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct PreflightDecision {
    pub effective_model: String,
    pub selected_model: String,
    pub tier: ModelTier,
    pub provider_id: String,
    /// Whether the effective model accepts image input (the pipeline's image guard).
    pub vision_input: bool,
    pub quota_decision: QuotaDecision,
    pub downgrade_from: Option<String>,
    pub downgrade_reason: Option<DowngradeReason>,
    /// The hard output cap sent to the provider.
    pub max_output_tokens_applied: i64,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i64,
    /// The version bound at preflight (`chat_turns.policy_version_applied`).
    pub policy_version_applied: u64,
    /// For the `INPUT_TOO_LONG` check ([`input_too_long`](Self::input_too_long)).
    pub estimated_text_tokens: i64,
    pub max_input_tokens: u32,
    /// The one preflight instant: also written as `chat_turns.started_at`.
    pub started_at: OffsetDateTime,
    /// The daily and monthly period starts computed from [`started_at`](Self::started_at).
    pub period_starts: PeriodStarts,
    /// The limits of the effective tier's required buckets in both periods (what the reserve
    /// write re-checks).
    pub limits: Vec<BucketLimit>,
}

impl PreflightDecision {
    /// The input limit check: `max_input_tokens` is not 0 and `estimated_text_tokens` exceeds it.
    #[must_use]
    pub fn input_too_long(&self) -> bool {
        // a `max_input_tokens` of 0 means no separate limit
        self.max_input_tokens != 0 && self.estimated_text_tokens > i64::from(self.max_input_tokens)
    }
}

/// The identity of the turn a reserve belongs to.
#[allow(
    clippy::struct_field_names,
    reason = "the four identifiers are the turn identity of the reserve write contract"
)]
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnIdentity {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
}

/// How the turn row meets the reserve.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveTurn {
    /// The send path: the pipeline inserts the `running` turn with the preflight columns of
    /// the decision (and `started_at` = the preflight instant) in the same transaction; the
    /// reserve write only increments the buckets and re-checks.
    New,
    /// Retry and edit: the turn was committed earlier with NULL preflight columns; the reserve
    /// write fills them and sets `started_at` to the preflight instant of the decision.
    Existing { turn_id: Uuid },
}

/// What a committed reserve write leaves for the post-commit steps.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReserveReceipt {
    pub tier: ModelTier,
    pub reserved_credits_micro: i64,
    /// The periods whose rows were incremented: daily, monthly.
    pub periods: Vec<PeriodType>,
}

impl ReserveReceipt {
    /// Record `mini_chat_quota_reserve` once per period; call after the transaction committed.
    pub fn record_after_commit(&self, metrics: &dyn QuotaMetrics) {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-metric
        for period in &self.periods {
            metrics.reserve(*period);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-metric
    }
}

/// Who finalizes the turn.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finalizer {
    /// The stream finalization: carries the period starts of the preflight and the model the
    /// chat selected.
    Stream {
        selected_model: String,
        period_starts: PeriodStarts,
    },
    /// The orphan watchdog: adds the `deleted_at` and stale-progress predicates to the CAS,
    /// derives the period starts from `chat_turns.started_at` and reports the effective model
    /// as the selected model.
    Orphan { stale_cutoff: OffsetDateTime },
}

/// The input of the finalization settlement.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizeRequest {
    pub tenant_id: Uuid,
    pub turn_id: Uuid,
    /// The terminal-state-specific columns of the CAS update.
    pub update: TurnTerminalUpdate,
    pub finalizer: Finalizer,
    /// The provider usage when known.
    pub usage: Option<UsageTokens>,
    pub tool_counts: ToolCallCounts,
}

/// What a settlement reports to the finalizing path.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct SettlementResult {
    pub billing_outcome: BillingOutcome,
    pub settlement_method: SettlementMethod,
    pub committed_credits_micro: i64,
    pub charged_tokens: i64,
    /// The actual tokens exceeded the reserve.
    pub overshoot: bool,
    /// The committed credits were capped at the reserve (internal; not exported).
    pub overshoot_capped: bool,
    /// The tier of the persisted effective model in the snapshot of the applied version;
    /// `None` for a turn without a reserve.
    pub tier: Option<ModelTier>,
    pub effective_model: String,
    /// The provider tokens of an `actual` settlement (input plus output), else 0.
    pub actual_tokens: i64,
    pub code_interpreter_calls: u32,
    /// The period starts the buckets were settled against.
    pub period_starts: PeriodStarts,
    /// The payload enqueued in the same transaction.
    pub usage_event: UsageEvent,
}

impl SettlementResult {
    /// After the commit: `mini_chat_quota_commit` per period and `mini_chat_quota_actual_tokens`
    /// and `mini_chat_code_interpreter_calls` for `actual` settlements only, and
    /// `mini_chat_quota_overshoot` per period when the actual tokens exceeded the reserve.
    pub fn record_after_commit(&self, metrics: &dyn QuotaMetrics) {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-metrics
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-overshoot-metric
        // `actual` settlements only; estimated and released settlements record nothing here and
        // never exceed the reserve
        if self.settlement_method != SettlementMethod::Actual {
            return;
        }
        for period in PeriodType::ALL {
            metrics.commit(period);
            if self.overshoot {
                // recorded in both branches (within the tolerance and capped)
                metrics.overshoot(period);
            }
        }
        metrics.actual_tokens(self.actual_tokens);
        if self.code_interpreter_calls > 0 {
            metrics.code_interpreter_calls(
                &self.effective_model,
                u64::from(self.code_interpreter_calls),
            );
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-overshoot-metric
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-metrics
    }
}

/// A settled turn: the result and the wake of the enqueued usage message.
#[domain_model]
#[derive(Debug)]
pub struct Settlement {
    pub result: SettlementResult,
    /// Fire after the caller's transaction committed.
    pub wake: Wake,
}

/// The outcome of the finalization settlement call.
#[domain_model]
#[derive(Debug)]
pub enum FinalizeOutcome {
    /// The CAS was lost (another finalizer won, or the turn is not finalizable): nothing
    /// changed, nothing was enqueued.
    AlreadyFinalized,
    /// The CAS was won for a code without a billing outcome (`quota_exceeded`): the turn is
    /// terminal, no quota changed and no usage message was enqueued.
    NoSettlement,
    /// The CAS was won and the turn was settled.
    Settled(Box<Settlement>),
}

/// The quota service. Cloning shares the collaborators (every field is an `Arc` or `Copy`), so a
/// transaction closure of the persistence layer can own a handle.
#[domain_model]
#[derive(Clone)]
pub struct QuotaService {
    pub pep: Arc<PepService>,
    pub gateway: Arc<dyn ModelPolicyGateway>,
    pub db: Arc<DBProvider<DomainError>>,
    pub ledger: Arc<dyn QuotaLedger>,
    pub queue: Arc<dyn UsageQueue>,
    pub metrics: Arc<dyn QuotaMetrics>,
    pub clock: Arc<dyn Clock>,
    pub settings: QuotaSettings,
}

impl QuotaService {
    /// Create the service over its collaborators.
    #[must_use]
    pub fn new(deps: QuotaDeps) -> Self {
        Self {
            pep: deps.pep,
            gateway: deps.gateway,
            db: deps.db,
            ledger: deps.ledger,
            queue: deps.queue,
            metrics: deps.metrics,
            clock: deps.clock,
            settings: deps.settings,
        }
    }

    /// The preflight reserve (`cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve`): the
    /// decision phase of the two-phase counting. Resolves the policy through the gateway (no
    /// cache), rejects `web_search.enabled` under `disable_web_search` before any estimation,
    /// captures the preflight instant once, reads the rows in one read transaction, runs the
    /// cascade and the daily tool quota check, and records `mini_chat_quota_preflight` and
    /// `mini_chat_quota_estimated_tokens` before any reserve is written. Writes nothing.
    ///
    /// # Errors
    ///
    /// `PolicyUnavailable` (500) when the plugin fails, `FeatureDisabled { web_search }` (400),
    /// `QuotaExceeded { scope }` (429) for a cascade reject (`tokens`) or an exhausted tool
    /// quota (`web_search`, `code_interpreter`), or an internal database failure.
    pub async fn preflight(
        &self,
        ctx: &SecurityContext,
        request: &PreflightRequest,
    ) -> Result<PreflightDecision, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-policy
        // the policy is asked on every preflight (no cache); a plugin failure is a rejection of
        // kind internal error, mapped to 500
        let (snapshot, limits) = self.current_policy(ctx.subject_id()).await?;
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-policy
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-web-kill
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-return-kill
        // the parameter is never silently ignored: a rejection before any estimation and before
        // the quota cascade
        if request.facts.web_search_enabled && snapshot.kill_switches.disable_web_search {
            return Err(DomainError::FeatureDisabled {
                subject: WEB_SEARCH_SUBJECT,
            });
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-return-kill
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-web-kill
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-periods
        // the one instant of the preflight: the period starts and `chat_turns.started_at` of the
        // reserve write both come from it, and the clock is not read again
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries:p1:inst-period-single-instant
        let started_at = self.clock.now();
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries:p1:inst-period-single-instant
        let period_starts = PeriodStarts::of(started_at);
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-periods
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-prior-context
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-no-write
        // one read transaction for the rows and the prior context; nothing is written and the
        // transaction ends when the reads are done
        let (rows, prior_context_tokens) = self
            .read_preflight_state(ctx, request.chat_id, period_starts)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-no-write
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-prior-context
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-cascade
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-before-outbound
        // every product-level decision is made here, before any request reaches OAGW: the quota
        // service has no dependency on the provider or on OAGW
        let cascade = run_cascade(&CascadeInput {
            snapshot: &snapshot,
            selected_model: &request.selected_model,
            limits: &limits,
            rows: &rows,
            facts: &request.facts,
            prior_context_tokens,
            settings: &self.settings,
        });
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-before-outbound
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-cascade
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-cascade-rejected
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-return-quota
        let Some(cascade) = cascade else {
            self.metrics.preflight(
                DECISION_REJECT,
                &request.selected_model,
                tier_label(starting_tier(&snapshot, &request.selected_model)),
            );
            return Err(DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens,
            });
        };
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-return-quota
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-cascade-rejected
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-metrics
        // recorded before any reserve is written, so a request rejected later (by the tool
        // quota or by the reserve re-check) still records them
        self.metrics.preflight(
            match cascade.quota_decision {
                QuotaDecision::Allow => DECISION_ALLOW,
                QuotaDecision::Downgrade => DECISION_DOWNGRADE,
                // the cascade never produces it; only the orphan audit path reports `unknown`
                QuotaDecision::Unknown => DECISION_UNKNOWN,
            },
            &cascade.effective_model,
            tier_label(cascade.tier),
        );
        self.metrics
            .estimated_tokens(cascade.reserve.reserve_tokens);
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-metrics
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-tool-quota
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-tool-quota-request:p1:inst-toolreq-preflight
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-tool-quota-request:p1:inst-toolreq-rejected
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-tool-quota-request:p1:inst-toolreq-return-429
        // the daily tool quota of the effective model, after the cascade picked it
        let effective = snapshot
            .model_catalog
            .iter()
            .find(|entry| entry.id == cascade.effective_model)
            .ok_or_else(|| {
                DomainError::internal("the effective model is missing from the catalog")
            })?;
        check_tool_quota(
            effective,
            &snapshot.kill_switches,
            &request.facts,
            &rows.get(PeriodType::Daily, Bucket::Total),
            &self.settings,
        )
        .map_err(|scope| DomainError::QuotaExceeded { scope })?;
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-tool-quota-request:p1:inst-toolreq-return-429
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-tool-quota-request:p1:inst-toolreq-rejected
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-tool-quota-request:p1:inst-toolreq-preflight
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-tool-quota
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-return-decision
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-turn-admission:p1:inst-admit-return-decision
        Ok(PreflightDecision {
            effective_model: cascade.effective_model,
            selected_model: request.selected_model.clone(),
            tier: cascade.tier,
            provider_id: cascade.provider_id,
            vision_input: cascade.vision_input,
            quota_decision: cascade.quota_decision,
            downgrade_from: cascade.downgrade_from,
            downgrade_reason: cascade.downgrade_reason,
            max_output_tokens_applied: cascade.reserve.max_output_tokens_applied,
            reserve_tokens: cascade.reserve.reserve_tokens,
            reserved_credits_micro: cascade.reserve.reserved_credits_micro,
            minimal_generation_floor_applied: cascade.reserve.minimal_generation_floor_applied,
            policy_version_applied: snapshot.policy_version,
            estimated_text_tokens: cascade.estimate.estimated_text_tokens,
            max_input_tokens: cascade.max_input_tokens,
            started_at,
            period_starts,
            limits: checked_limits(cascade.tier, &limits),
        })
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-turn-admission:p1:inst-admit-return-decision
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-preflight-reserve:p1:inst-preflight-return-decision
    }

    /// The reserve write (`cpt-cf-mini-chat-algo-quota-and-billing-reserve-write`) inside the
    /// caller's reserve transaction: for [`ReserveTurn::Existing`] fill the NULL preflight
    /// columns and `started_at` once; add the reserved credits to bucket `total` (and
    /// `tier:premium` for a premium turn) of the daily and monthly rows; re-read the rows in
    /// the same transaction and verify every bucket of the decision in widened arithmetic.
    /// Does not record metrics (see [`ReserveReceipt::record_after_commit`]).
    ///
    /// # Errors
    ///
    /// `QuotaExceeded { scope: Tokens }` when any bucket is over its limit or an increment
    /// overflows (the caller's transaction must roll back; no cascade runs again, no
    /// downgrade); `Internal` for a turn that cannot be filled or a database failure.
    pub async fn write_reserve(
        &self,
        tx: &DbTx<'_>,
        identity: &TurnIdentity,
        decision: &PreflightDecision,
        turn: &ReserveTurn,
    ) -> Result<ReserveReceipt, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-transaction
        // everything below runs in the reserve transaction of the caller (`tx`): when this
        // returns an error the caller rolls the whole transaction back, increments, turn columns
        // and the caller's own rows included, and no cascade runs again
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-transaction
        // the sentinel of a failed credit computation is never booked
        if decision.reserved_credits_micro == i64::MAX || decision.reserved_credits_micro < 0 {
            return Err(over_limit());
        }
        self.write_turn_columns(tx, identity, decision, turn)
            .await?;
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-for-each-period
        for period in PeriodType::ALL {
            let start = decision.period_starts.start_of(period);
            let key = |bucket| BucketKey {
                tenant_id: identity.tenant_id,
                user_id: identity.user_id,
                period,
                period_start: start,
                bucket,
            };
            // a standard turn updates bucket `total` only; a premium turn adds the same amount
            // to bucket `tier:premium`
            let buckets: &[Bucket] = match decision.tier {
                ModelTier::Standard => &[Bucket::Total],
                ModelTier::Premium => &[Bucket::Total, Bucket::TierPremium],
            };
            for &bucket in buckets {
                let outcome = self
                    .ledger
                    .add_reserved(tx, &key(bucket), decision.reserved_credits_micro)
                    .await?;
                // a database arithmetic overflow of an increment is an over-limit bucket
                // (rollback and 429), never a 500
                if outcome == ReserveIncrement::Overflow {
                    return Err(over_limit());
                }
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-for-each-period
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-recheck
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-concurrency
        // the plain re-read in the same transaction sees every reserve committed before it (the
        // increments hold the row locks or the write lock), so of two concurrent turns that both
        // passed preflight the later one finds a bucket over its limit
        let scope = AccessScope::for_tenant(identity.tenant_id).ensure_owner(identity.user_id);
        let rows = self
            .ledger
            .read_rows(tx, &scope, &decision.period_starts)
            .await?;
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-over-limit
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-rollback
        // `spent + reserved <= limit` of the limits carried by the decision, in widened
        // arithmetic; an over-limit bucket is the same response as a preflight reject
        let within = decision.limits.iter().all(|limit| {
            bucket_available(
                &rows.get(limit.period, limit.bucket),
                0,
                limit.limit_credits_micro,
            )
        });
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-turn-admission:p1:inst-admit-recheck-failed
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-turn-admission:p1:inst-admit-return-recheck
        if !within {
            return Err(over_limit());
        }
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-turn-admission:p1:inst-admit-return-recheck
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-turn-admission:p1:inst-admit-recheck-failed
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-rollback
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-over-limit
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-concurrency
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-recheck
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-before-provider
        // the provider call may start only after the caller committed this transaction, so a
        // `running` turn with its reserve exists before the first outbound byte; a crash in
        // between is settled by the orphan watchdog on the estimated path
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-before-provider
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-return
        // @cpt-begin:cpt-cf-mini-chat-state-quota-and-billing-turn-reserve:p1:inst-reserve-state-reserved
        Ok(ReserveReceipt {
            tier: decision.tier,
            reserved_credits_micro: decision.reserved_credits_micro,
            periods: PeriodType::ALL.to_vec(),
        })
        // @cpt-end:cpt-cf-mini-chat-state-quota-and-billing-turn-reserve:p1:inst-reserve-state-reserved
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-return
    }

    /// The finalization settlement (`cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement`)
    /// inside the caller's finalization transaction: the CAS, the billing derivation, the
    /// settlement computation on the snapshot of the turn's `policy_version_applied`, the
    /// bucket updates and the usage message enqueue, together or not at all.
    ///
    /// # Errors
    ///
    /// `PolicyUnavailable` when the snapshot fails, `Internal` when the credit computation or
    /// the enqueue fails: the caller's transaction must roll back and the turn stays `running`.
    pub async fn finalize_turn(
        &self,
        tx: &DbTx<'_>,
        request: &FinalizeRequest,
    ) -> Result<FinalizeOutcome, DomainError> {
        // the first terminal transition is authoritative: one conditional UPDATE guarded by
        // `state = 'running'` (the orphan watchdog adds its stale-progress and `deleted_at`
        // predicates), no in-memory lock
        let stale_cutoff = match &request.finalizer {
            Finalizer::Stream { .. } => None,
            Finalizer::Orphan { stale_cutoff } => Some(*stale_cutoff),
        };
        // @cpt-begin:cpt-cf-mini-chat-state-quota-and-billing-turn-billing:p1:inst-bill-completed
        // @cpt-begin:cpt-cf-mini-chat-state-quota-and-billing-turn-billing:p1:inst-bill-failed
        // @cpt-begin:cpt-cf-mini-chat-state-quota-and-billing-turn-billing:p1:inst-bill-aborted
        // IN_PROGRESS moves to exactly one terminal state (`completed`, `failed`, or `cancelled`
        // and `failed` with `orphan_timeout` for ABORTED) when the CAS affected one row
        let won = self
            .ledger
            .cas_terminal(
                tx,
                request.tenant_id,
                request.turn_id,
                &request.update,
                stale_cutoff,
            )
            .await?;
        // @cpt-end:cpt-cf-mini-chat-state-quota-and-billing-turn-billing:p1:inst-bill-aborted
        // @cpt-end:cpt-cf-mini-chat-state-quota-and-billing-turn-billing:p1:inst-bill-failed
        // @cpt-end:cpt-cf-mini-chat-state-quota-and-billing-turn-billing:p1:inst-bill-completed
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-cas-lost
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-return-noop
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-turn-settlement:p1:inst-settle-cas-lost
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-turn-settlement:p1:inst-settle-return-noop
        // no quota change, no usage message, no state change and no metric: every competing
        // terminal signal that arrives later loses in the same way
        if !won {
            return Ok(FinalizeOutcome::AlreadyFinalized);
        }
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-turn-settlement:p1:inst-settle-return-noop
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-turn-settlement:p1:inst-settle-cas-lost
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-return-noop
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-cas-lost
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-derive
        // the one shared derivation; `quota_exceeded` has no billing outcome because no reserve
        // exists, so the turn is terminal without a settlement or a usage message
        let Some(derivation) = derive_billing(
            request.update.state,
            request.update.error_code.as_deref(),
            request.usage.as_ref(),
        ) else {
            return Ok(FinalizeOutcome::NoSettlement);
        };
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-derive
        let turn = self
            .ledger
            .load_turn(tx, request.tenant_id, request.turn_id)
            .await?
            .ok_or_else(|| DomainError::internal("the finalized turn cannot be loaded"))?;
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries:p1:inst-period-boundary-invariant
        // the rows of the reserve: the stream path carries the starts of the preflight, the
        // orphan watchdog derives them from `started_at` (the preflight instant), so a turn that
        // started before a UTC midnight is settled on the day-1 rows, never on the current clock
        let period_starts = match &request.finalizer {
            Finalizer::Stream { period_starts, .. } => *period_starts,
            Finalizer::Orphan { .. } => PeriodStarts::of(turn.started_at),
        };
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-period-boundaries:p1:inst-period-boundary-invariant
        let settlement = match reserve_of(&turn) {
            Some(reserved) => {
                self.settle_reserved(tx, request, &turn, &reserved, derivation, period_starts)
                    .await?
            }
            None => {
                self.settle_without_reserve(tx, request, &turn, period_starts)
                    .await?
            }
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-replay-excluded
        // a replay of a completed turn never reaches this call (the send pipeline answers it from
        // the stored turn), so it takes no reserve, updates no row and enqueues nothing
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-replay-excluded
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-return
        Ok(FinalizeOutcome::Settled(Box::new(settlement)))
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-return
    }

    /// `GET /v1/quota/status`: the PEP (`read` on `UserQuota`, constraints required), the policy
    /// binding, the periods of the current instant, the rows through the owner-scoped access
    /// scope in one consistent read, and the status computation.
    ///
    /// # Errors
    ///
    /// An admission refusal (`AuthzDenied`, `AuthzUnavailable`), `PolicyUnavailable`, or an
    /// internal database failure.
    pub async fn quota_status(&self, ctx: &SecurityContext) -> Result<QuotaStatus, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-return-refusal
        // `read` on `UserQuota` with no `resource.id` and constraints required; a refusal is
        // returned unchanged (401, 403 or 503) and nothing is read
        let authorization = self
            .pep
            .authorize(ctx, Operation::GetQuotaStatus, None)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-refused
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-admission
        let scope = authorization.scope().cloned().ok_or_else(|| {
            DomainError::internal("the quota status admission returned no access scope")
        })?;
        let tiers = self.status_tiers(ctx, scope).await?;
        // billing outcomes, settlement details and the tool counters are never part of it
        Ok(QuotaStatus {
            tiers,
            warning_threshold_pct: self.settings.warning_threshold_pct,
        })
    }

    /// The `quota_warnings` entries for the `done` event, read after the settlement of the
    /// turn: the status computation of the current instant projected by
    /// [`quota_warnings`](crate::domain::quota::quota_warnings). Runtime-owned (no PEP).
    ///
    /// # Errors
    ///
    /// `PolicyUnavailable` or an internal database failure.
    pub async fn quota_warnings(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<QuotaWarning>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-quota-warnings:p1:inst-qw-post-settlement
        // runtime-owned (no PEP): the rows of the calling user, read after the settlement of the
        // turn so the exhaustion this turn caused is visible, with the rules of the status
        let scope = AccessScope::for_tenant(ctx.subject_tenant_id()).ensure_owner(ctx.subject_id());
        let tiers = self.status_tiers(ctx, scope).await?;
        Ok(quota_warnings(&tiers))
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-quota-warnings:p1:inst-qw-post-settlement
    }

    /// The current policy version, snapshot and the user limits (kept apart from the shared
    /// snapshot) through the gateway; there is no cache and no license check.
    async fn current_policy(
        &self,
        user_id: Uuid,
    ) -> Result<(PolicySnapshot, UserLimits), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-policy-binding:p1:inst-policy-current
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-policy-binding:p1:inst-policy-no-cache
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-policy-binding:p1:inst-policy-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-policy-binding:p1:inst-policy-return-error
        // a plugin that cannot be resolved or fails is a `PolicyUnavailable` error, which the
        // caller maps to 500 at preflight and for the status endpoint
        let snapshot = self.gateway.current_snapshot(user_id).await?;
        let limits = self
            .gateway
            .user_limits(user_id, snapshot.policy_version)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-policy-binding:p1:inst-policy-return-error
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-policy-binding:p1:inst-policy-failed
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-policy-binding:p1:inst-policy-no-cache
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-policy-binding:p1:inst-policy-current
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-policy-binding:p1:inst-policy-return
        Ok((snapshot, limits))
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-policy-binding:p1:inst-policy-return
    }

    /// The rows and the prior context of the preflight in one read transaction.
    async fn read_preflight_state(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        period_starts: PeriodStarts,
    ) -> Result<(QuotaRows, i64), DomainError> {
        let ledger = Arc::clone(&self.ledger);
        let tenant_id = ctx.subject_tenant_id();
        let scope = AccessScope::for_tenant(tenant_id).ensure_owner(ctx.subject_id());
        self.db
            .transaction(move |tx| {
                Box::pin(async move {
                    let rows = ledger.read_rows(tx, &scope, &period_starts).await?;
                    let prior = ledger.prior_context_tokens(tx, tenant_id, chat_id).await?;
                    Ok((rows, prior))
                })
            })
            .await
    }

    /// The tiers of the status computation: the policy limits, the periods of the current
    /// instant and the rows of the user read in one transaction.
    async fn status_tiers(
        &self,
        ctx: &SecurityContext,
        scope: AccessScope,
    ) -> Result<Vec<TierStatus>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-policy
        let (_, limits) = self.current_policy(ctx.subject_id()).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-policy
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-periods
        let now = self.clock.now();
        let period_starts = PeriodStarts::of(now);
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-periods
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-read-failed
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-return-500-read
        // a failed read is a database error: a generic 500, the cause stays in the log
        let ledger = Arc::clone(&self.ledger);
        let rows = self
            .db
            .transaction(move |tx| {
                Box::pin(async move { ledger.read_rows(tx, &scope, &period_starts).await })
            })
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-return-500-read
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-read-failed
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-compute
        Ok(compute_status(
            &limits,
            &rows,
            self.settings.warning_threshold_pct,
            now,
        ))
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-compute
    }

    /// Write the preflight columns and `started_at` of an existing (retry or edit) turn; the
    /// turn of the send path is inserted by the pipeline with the columns of the decision.
    async fn write_turn_columns(
        &self,
        tx: &DbTx<'_>,
        identity: &TurnIdentity,
        decision: &PreflightDecision,
        turn: &ReserveTurn,
    ) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-turn-columns
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-started-at
        // the NULL columns are filled once and never changed, and `started_at` becomes the
        // preflight instant of this turn (not the instant of the mutation commit)
        let ReserveTurn::Existing { turn_id } = turn else {
            return Ok(());
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-policy-binding:p1:inst-policy-persist-version
        // the version bound at preflight is written once and carried in the usage event
        let policy_version_applied = i64::try_from(decision.policy_version_applied)
            .map_err(|_| DomainError::internal("the policy version does not fit its column"))?;
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-policy-binding:p1:inst-policy-persist-version
        let filled = self
            .ledger
            .fill_preflight_columns(
                tx,
                identity.tenant_id,
                *turn_id,
                &TurnPreflightColumns {
                    policy_version_applied,
                    effective_model: decision.effective_model.clone(),
                    reserve_tokens: decision.reserve_tokens,
                    max_output_tokens_applied: decision.max_output_tokens_applied,
                    reserved_credits_micro: decision.reserved_credits_micro,
                    minimal_generation_floor_applied: decision.minimal_generation_floor_applied,
                    started_at: decision.started_at,
                },
            )
            .await?;
        if filled {
            Ok(())
        } else {
            Err(DomainError::internal(
                "the turn cannot take its preflight columns: it does not exist, is not running or already holds a reserve",
            ))
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-started-at
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-write:p1:inst-rw-turn-columns
    }

    /// The settlement of a turn that holds a reserve.
    async fn settle_reserved(
        &self,
        tx: &DbTx<'_>,
        request: &FinalizeRequest,
        turn: &TurnRecord,
        reserved: &ReservedTurn,
        derivation: BillingDerivation,
        period_starts: PeriodStarts,
    ) -> Result<Settlement, DomainError> {
        let billing = &reserved.billing;
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-compute
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-policy-binding:p1:inst-policy-settlement-snapshot
        // only the snapshot of the turn's `policy_version_applied` is read: never the current
        // version, no cascade and no CCM call; a failure fails the finalization transaction
        let snapshot = self
            .gateway
            .snapshot_of_version(reserved.user_id, billing.policy_version_applied)
            .await?;
        let entry = snapshot
            .model_catalog
            .iter()
            .find(|entry| entry.id == billing.effective_model)
            .ok_or_else(|| {
                DomainError::internal(format!(
                    "the effective model `{}` is missing from the snapshot of policy version {}",
                    billing.effective_model, billing.policy_version_applied
                ))
            })?;
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-policy-binding:p1:inst-policy-settlement-snapshot
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-compute-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-fail-transaction
        // at settlement a failed credit computation fails the transaction (the CAS, the bucket
        // updates and the enqueue all roll back, the turn stays `running`) and names the
        // offending token count or multiplier in a warning
        let computation = compute_settlement(
            billing,
            derivation.method,
            request.usage.as_ref(),
            entry,
            self.settings.overshoot_tolerance_factor,
        )
        .map_err(|error| {
            tracing::warn!(
                turn_id = %turn.id,
                %error,
                "the settlement credits cannot be computed; the finalization fails"
            );
            DomainError::internal(format!(
                "the settlement credits cannot be computed: {error}"
            ))
        })?;
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-fail-transaction
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-compute-failed
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-compute
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-tier
        // the tier of the persisted effective model in the applied snapshot, not the current
        // catalog
        let tier = entry.tier;
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-tier
        let counts = settled_counts(derivation.method, request.tool_counts);
        self.settle_buckets(
            tx,
            turn,
            reserved,
            tier,
            &computation,
            counts,
            period_starts,
        )
        .await?;
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-orphan-values
        // an orphan-finalized turn reports the effective model as the selected model
        let selected_model = match &request.finalizer {
            Finalizer::Stream { selected_model, .. } => selected_model.clone(),
            Finalizer::Orphan { .. } => billing.effective_model.clone(),
        };
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-orphan-values
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-enqueue
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-turn-settlement:p1:inst-settle-commit
        // the usage event is built from the persisted turn and enqueued in this same transaction:
        // the message and the quota change commit together or not at all, and one logical enqueue
        // per turn follows from the CAS winner
        let usage_event = build_turn_usage_event(&TurnUsageEventInput {
            tenant_id: turn.tenant_id,
            user_id: reserved.user_id,
            chat_id: turn.chat_id,
            turn_id: turn.id,
            request_id: turn.request_id,
            effective_model: &billing.effective_model,
            selected_model: &selected_model,
            terminal_state: request.update.state,
            billing_outcome: derivation.outcome,
            settlement_method: derivation.method,
            usage: request.usage,
            committed_credits_micro: computation.committed_credits_micro,
            policy_version_applied: billing.policy_version_applied,
            tool_counts: counts,
            timestamp: request.update.completed_at,
        });
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-one-per-turn
        let wake = self.queue.enqueue_usage(tx, &usage_event).await?;
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-one-per-turn
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-turn-settlement:p1:inst-settle-commit
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-enqueue
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-reserve-released
        // the whole reserve of the turn was released by the bucket updates of this transaction,
        // so a terminal turn never keeps an unsettled reserve
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-reserve-released
        let actual = derivation.method == SettlementMethod::Actual;
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-turn-settlement:p1:inst-settle-return
        Ok(Settlement {
            result: SettlementResult {
                billing_outcome: derivation.outcome,
                settlement_method: derivation.method,
                committed_credits_micro: computation.committed_credits_micro,
                charged_tokens: computation.charged_tokens,
                overshoot: computation.overshoot,
                overshoot_capped: computation.overshoot_capped,
                tier: Some(tier),
                effective_model: billing.effective_model.clone(),
                actual_tokens: if actual {
                    computation
                        .telemetry_input_tokens
                        .saturating_add(computation.telemetry_output_tokens)
                } else {
                    0
                },
                code_interpreter_calls: counts.code_interpreter,
                period_starts,
                // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-return
                usage_event,
                // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-return
            },
            wake,
        })
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-turn-settlement:p1:inst-settle-return
    }

    /// The bucket updates of a settlement: the `total` row of each period, and the
    /// `tier:premium` row for a premium turn, on the rows of the reserve.
    #[allow(
        clippy::too_many_arguments,
        reason = "the settlement context: transaction, turn, reserve, tier, computation, counts and period starts"
    )]
    async fn settle_buckets(
        &self,
        tx: &DbTx<'_>,
        turn: &TurnRecord,
        reserved: &ReservedTurn,
        tier: ModelTier,
        computation: &SettlementComputation,
        counts: ToolCallCounts,
        period_starts: PeriodStarts,
    ) -> Result<(), DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-for-each-period
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-tool-counts
        // the committed credits are added to the spent counter and the reserve is released; the
        // actual provider tokens and the completed tool counts go to the `total` row only
        // (`file_search_calls`, `image_inputs`, `image_upload_bytes` and `rag_retrieval_calls`
        // are not populated); the premium row takes the credits and the call only
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-tool-quota-request:p1:inst-toolreq-continue
        // the call counts of the turn are added to the daily `total` row here, at settlement,
        // never at preflight
        let released = reserved.billing.reserved_credits_micro;
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-tool-quota-request:p1:inst-toolreq-continue
        let total_delta = SettleDelta {
            release_reserved_credits_micro: released,
            spent_credits_micro: computation.committed_credits_micro,
            input_tokens: computation.telemetry_input_tokens,
            output_tokens: computation.telemetry_output_tokens,
            web_search_calls: i64::from(counts.web_search),
            code_interpreter_calls: i64::from(counts.code_interpreter),
        };
        let premium_delta = SettleDelta {
            release_reserved_credits_micro: released,
            spent_credits_micro: computation.committed_credits_micro,
            input_tokens: 0,
            output_tokens: 0,
            web_search_calls: 0,
            code_interpreter_calls: 0,
        };
        for period in PeriodType::ALL {
            let key = |bucket| BucketKey {
                tenant_id: turn.tenant_id,
                user_id: reserved.user_id,
                period,
                period_start: period_starts.start_of(period),
                bucket,
            };
            self.ledger
                .settle_bucket(tx, &key(Bucket::Total), &total_delta)
                .await?;
            if tier == ModelTier::Premium {
                self.ledger
                    .settle_bucket(tx, &key(Bucket::TierPremium), &premium_delta)
                    .await?;
            }
        }
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation:p1:inst-settle-tool-counts
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-for-each-period
    }

    /// The settlement of a turn without a reserve (a retry or edit turn that stayed `running`
    /// before its preflight columns were written): no bucket changes and a placeholder event.
    async fn settle_without_reserve(
        &self,
        tx: &DbTx<'_>,
        request: &FinalizeRequest,
        turn: &TurnRecord,
        period_starts: PeriodStarts,
    ) -> Result<Settlement, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-no-reserve
        tracing::warn!(
            turn_id = %turn.id,
            "the turn holds no reserve; the bucket updates are skipped"
        );
        let usage_event = build_no_reserve_usage_event(&NoReserveUsageEventInput {
            tenant_id: turn.tenant_id,
            user_id: turn.requester_user_id,
            chat_id: turn.chat_id,
            turn_id: turn.id,
            request_id: turn.request_id,
            terminal_state: request.update.state,
            tool_counts: request.tool_counts,
            timestamp: request.update.completed_at,
        });
        let wake = self.queue.enqueue_usage(tx, &usage_event).await?;
        Ok(Settlement {
            result: SettlementResult {
                billing_outcome: BillingOutcome::Aborted,
                settlement_method: SettlementMethod::Estimated,
                committed_credits_micro: 0,
                charged_tokens: 0,
                overshoot: false,
                overshoot_capped: false,
                tier: None,
                effective_model: String::new(),
                actual_tokens: 0,
                code_interpreter_calls: request.tool_counts.code_interpreter,
                period_starts,
                usage_event,
            },
            wake,
        })
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-no-reserve
    }
}

/// The subject of the `FEATURE_DISABLED` violation of the web search kill switch.
const WEB_SEARCH_SUBJECT: &str = "web_search";

/// The `decision` label values of `mini_chat_quota_preflight`.
const DECISION_ALLOW: &str = "allow";
const DECISION_DOWNGRADE: &str = "downgrade";
const DECISION_REJECT: &str = "reject";
const DECISION_UNKNOWN: &str = "unknown";

/// The `tier` label of the preflight metric.
const fn tier_label(tier: ModelTier) -> &'static str {
    match tier {
        ModelTier::Standard => "standard",
        ModelTier::Premium => "premium",
    }
}

/// The tier the cascade starts at: the tier of the selected model, premium when it is not in the
/// catalog.
fn starting_tier(snapshot: &PolicySnapshot, selected_model: &str) -> ModelTier {
    snapshot
        .model_catalog
        .iter()
        .find(|entry| entry.id == selected_model)
        .map_or(ModelTier::Premium, |entry| entry.tier)
}

/// The rejection of a reserve that does not fit: the same response as a preflight reject.
const fn over_limit() -> DomainError {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping:p1:inst-reject-no-side-effects
    // @cpt-begin:cpt-cf-mini-chat-state-quota-and-billing-turn-reserve:p1:inst-reserve-state-rejected
    // the caller rolls its transaction back on this error: no `chat_turns` row with a reserve,
    // no reserve, no settlement and no usage event
    DomainError::QuotaExceeded {
        scope: QuotaScope::Tokens,
    }
    // @cpt-end:cpt-cf-mini-chat-state-quota-and-billing-turn-reserve:p1:inst-reserve-state-rejected
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping:p1:inst-reject-no-side-effects
}

/// The reserve a turn holds: its requester and the persisted billing columns.
struct ReservedTurn {
    user_id: Uuid,
    billing: TurnBilling,
}

/// The reserve of a turn, or `None` when its reserve columns or `requester_user_id` are NULL.
fn reserve_of(turn: &TurnRecord) -> Option<ReservedTurn> {
    Some(ReservedTurn {
        user_id: turn.requester_user_id?,
        billing: TurnBilling {
            reserve_tokens: turn.reserve_tokens?,
            max_output_tokens_applied: turn.max_output_tokens_applied?,
            reserved_credits_micro: turn.reserved_credits_micro?,
            minimal_generation_floor_applied: turn.minimal_generation_floor_applied?,
            policy_version_applied: u64::try_from(turn.policy_version_applied?).ok()?,
            effective_model: turn.effective_model.clone()?,
        },
    })
}

/// The tool call counts a settlement adds: nothing on a `released` settlement.
fn settled_counts(method: SettlementMethod, counts: ToolCallCounts) -> ToolCallCounts {
    if method == SettlementMethod::Released {
        ToolCallCounts::default()
    } else {
        counts
    }
}

#[cfg(test)]
#[path = "preflight_tests.rs"]
mod preflight_tests;

#[cfg(test)]
#[path = "reserve_write_tests.rs"]
mod reserve_write_tests;

#[cfg(test)]
#[path = "finalization_tests.rs"]
mod finalization_tests;
