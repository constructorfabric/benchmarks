//! Quota service (D§3.2 "quota service", D§3.7 `quota_usage`, D§5.4):
//! preflight downgrade cascade, reserve with re-check, settlement, quota
//! status and warnings.

use std::collections::HashMap;
use std::sync::Arc;

use mini_chat_sdk::{ModelCatalogEntry, ModelTier, PolicySnapshot, UserLimits};
use time::{Date, Month, OffsetDateTime, UtcOffset};
use toolkit_db::secure::{AccessScope, DBRunner, ScopeConstraint, ScopeFilter, pep_properties};
use toolkit_db::{DBProvider, DbTx};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::authz::Pep;
use crate::domain::billing::{Settlement, SettlementMethod, TurnReserve};
use crate::domain::clock::Clock;
use crate::domain::error::{DomainError, QuotaScope};
use crate::domain::estimation::{ReserveInputs, ReservePlan, candidate_reserve};
use crate::domain::ports::PolicyPort;
use crate::infra::db::entity::quota_usage;
use crate::infra::db::repos::{BucketKey, QuotaIncrement, QuotaUsageRepo};
use crate::infra::db::tx::with_retry;
use crate::infra::metrics::MiniChatMetrics;

/// Quota period (P1: daily and monthly, UTC calendar).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Period {
    Daily,
    Monthly,
}

impl Period {
    pub const ALL: [Self; 2] = [Self::Daily, Self::Monthly];

    /// `quota_usage.period_type` / wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }
}

/// Enforcement bucket. `Total` is the global ceiling (limits of
/// `user_limits.standard`), `Premium` the premium subcap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bucket {
    Premium,
    Total,
}

impl Bucket {
    /// `quota_usage.bucket` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Premium => "tier:premium",
            Self::Total => "total",
        }
    }

    /// Tier name in the quota status / warnings (`premium` / `total`).
    #[must_use]
    pub const fn tier_str(self) -> &'static str {
        match self {
            Self::Premium => "premium",
            Self::Total => "total",
        }
    }

    /// Buckets a turn of `tier` reserves and settles against.
    #[must_use]
    pub fn for_tier(tier: ModelTier) -> &'static [Self] {
        match tier {
            ModelTier::Premium => &[Self::Total, Self::Premium],
            ModelTier::Standard => &[Self::Total],
        }
    }

    /// Limit of this bucket for `period`.
    #[must_use]
    pub const fn limit(self, period: Period, limits: &UserLimits) -> i64 {
        let tier = match self {
            Self::Premium => &limits.premium,
            Self::Total => &limits.standard,
        };
        match period {
            Period::Daily => tier.limit_daily_credits_micro,
            Period::Monthly => tier.limit_monthly_credits_micro,
        }
    }
}

/// `period_start` values of a turn (UTC date; 1st of the UTC month).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodStarts {
    pub daily: Date,
    pub monthly: Date,
}

impl PeriodStarts {
    #[must_use]
    pub const fn get(self, period: Period) -> Date {
        match period {
            Period::Daily => self.daily,
            Period::Monthly => self.monthly,
        }
    }
}

/// Period starts of the current time (preflight).
#[must_use]
pub fn period_starts(now: OffsetDateTime) -> PeriodStarts {
    let daily = now.to_offset(UtcOffset::UTC).date();
    PeriodStarts {
        daily,
        monthly: daily.replace_day(1).unwrap_or(daily),
    }
}

/// Period starts of a turn's `started_at` (orphan watchdog).
#[must_use]
pub fn period_starts_from(started_at: OffsetDateTime) -> PeriodStarts {
    period_starts(started_at)
}

/// Next reset of `period` after `now` (midnight UTC tomorrow / the 1st of
/// next month).
#[must_use]
pub fn next_reset(period: Period, now: OffsetDateTime) -> OffsetDateTime {
    let today = period_starts(now).daily;
    let next = match period {
        Period::Daily => today.next_day(),
        Period::Monthly => {
            let (year, month) = match today.month() {
                Month::December => (today.year() + 1, Month::January),
                m => (today.year(), m.next()),
            };
            Date::from_calendar_date(year, month, 1).ok()
        }
    }
    .unwrap_or(Date::MAX);
    next.midnight().assume_utc()
}

/// Counters of one bucket row read by the quota checks (missing row = 0).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketUsage {
    pub spent_credits_micro: i64,
    pub reserved_credits_micro: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

/// Bucket rows of one user for the current periods.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeriodUsage(HashMap<(Period, Bucket), BucketUsage>);

impl PeriodUsage {
    #[must_use]
    pub fn get(&self, period: Period, bucket: Bucket) -> BucketUsage {
        self.0.get(&(period, bucket)).copied().unwrap_or_default()
    }

    pub fn set(&mut self, period: Period, bucket: Bucket, usage: BucketUsage) {
        self.0.insert((period, bucket), usage);
    }

    /// Usage of the rows matching `periods` (other rows are ignored).
    #[must_use]
    pub fn from_rows(rows: &[quota_usage::Model], periods: PeriodStarts) -> Self {
        let mut usage = Self::default();
        for row in rows {
            let period = Period::ALL
                .into_iter()
                .find(|p| p.as_str() == row.period_type && periods.get(*p) == row.period_start);
            let bucket = [Bucket::Premium, Bucket::Total]
                .into_iter()
                .find(|b| b.as_str() == row.bucket);
            if let (Some(period), Some(bucket)) = (period, bucket) {
                usage.set(
                    period,
                    bucket,
                    BucketUsage {
                        spent_credits_micro: row.spent_credits_micro,
                        reserved_credits_micro: row.reserved_credits_micro,
                        web_search_calls: i64::from(row.web_search_calls),
                        code_interpreter_calls: i64::from(row.code_interpreter_calls),
                    },
                );
            }
        }
        usage
    }

    /// `spent + reserved + extra <= limit` for every period of every bucket
    /// in `buckets` (an overflowing sum never fits).
    fn fits(&self, buckets: &[Bucket], limits: &UserLimits, extra: i64) -> bool {
        buckets.iter().all(|&b| {
            Period::ALL.into_iter().all(|p| {
                let u = self.get(p, b);
                u.spent_credits_micro
                    .checked_add(u.reserved_credits_micro)
                    .and_then(|used| used.checked_add(extra))
                    .is_some_and(|total| total <= b.limit(p, limits))
            })
        })
    }
}

/// Gear configuration values the preflight uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaParams {
    /// `streaming.max_output_tokens`.
    pub max_output_tokens: u32,
    /// `estimation_budgets.minimal_generation_floor`.
    pub minimal_generation_floor: u32,
    pub web_search_daily_quota: u32,
    pub code_interpreter_daily_quota: u32,
}

impl QuotaParams {
    #[must_use]
    pub fn from_config(cfg: &MiniChatConfig) -> Self {
        Self {
            max_output_tokens: cfg.streaming.max_output_tokens,
            minimal_generation_floor: cfg.estimation_budgets.minimal_generation_floor,
            web_search_daily_quota: cfg.quota.web_search_daily_quota,
            code_interpreter_daily_quota: cfg.quota.code_interpreter_daily_quota,
        }
    }
}

/// Preflight decision kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaDecision {
    Allow,
    Downgrade,
}

impl QuotaDecision {
    /// Wire value (`allow` / `downgrade`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Downgrade => "downgrade",
        }
    }
}

/// Outcome of the cascade and the daily tool-quota checks.
#[derive(Debug, Clone, PartialEq)]
pub struct Evaluation {
    pub effective: ModelCatalogEntry,
    pub effective_tier: ModelTier,
    pub decision: QuotaDecision,
    pub downgrade_reason: Option<&'static str>,
    pub plan: ReservePlan,
}

/// Downgrade cascade (D "Downgrade Decision Flow") followed by the daily
/// web-search / code-interpreter quota checks for the effective model.
///
/// # Errors
/// `QuotaExceeded(Tokens)` when no tier is available,
/// `QuotaExceeded(WebSearch | CodeInterpreter)` when a daily tool quota is
/// used up and the tool would be sent.
pub fn evaluate(
    selected_model: &str,
    snap: &PolicySnapshot,
    limits: &UserLimits,
    inputs: &ReserveInputs,
    usage: &PeriodUsage,
    params: &QuotaParams,
) -> Result<Evaluation, DomainError> {
    let catalog = &snap.model_catalog;
    let ks = &snap.kill_switches;
    let selected = catalog.iter().find(|m| m.id == selected_model);
    let (start_tier, mut reason) = match selected {
        Some(m) if m.enabled => (m.tier, None),
        Some(m) => (m.tier, Some("model_disabled")),
        None => (ModelTier::Premium, Some("model_disabled")),
    };
    let cascade: &[ModelTier] = match start_tier {
        ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
        ModelTier::Standard => &[ModelTier::Standard],
    };

    for &tier in cascade {
        if tier == ModelTier::Premium {
            if ks.force_standard_tier {
                reason.get_or_insert("force_standard_tier");
                continue;
            }
            if ks.disable_premium_tier {
                reason.get_or_insert("disable_premium_tier");
                continue;
            }
        }
        let Some(candidate) = candidate_of_tier(catalog, tier, selected_model) else {
            continue;
        };
        let plan = candidate_reserve(
            candidate,
            inputs,
            ks,
            params.max_output_tokens,
            params.minimal_generation_floor,
        );
        if !usage.fits(Bucket::for_tier(tier), limits, plan.reserved_credits_micro) {
            if tier == ModelTier::Premium {
                reason.get_or_insert("premium_quota_exhausted");
            }
            continue;
        }
        check_tool_quotas(&plan, usage, params)?;
        let decision = if candidate.id == selected_model && reason.is_none() {
            QuotaDecision::Allow
        } else {
            QuotaDecision::Downgrade
        };
        return Ok(Evaluation {
            effective: candidate.clone(),
            effective_tier: tier,
            decision,
            downgrade_reason: reason,
            plan,
        });
    }
    Err(DomainError::QuotaExceeded(QuotaScope::Tokens))
}

/// Candidate of `tier`: the selected model when it is an enabled model of
/// the tier, else the enabled `is_default` model, else the first enabled one.
fn candidate_of_tier<'a>(
    catalog: &'a [ModelCatalogEntry],
    tier: ModelTier,
    selected_model: &str,
) -> Option<&'a ModelCatalogEntry> {
    let mut enabled = catalog.iter().filter(|m| m.enabled && m.tier == tier);
    enabled
        .clone()
        .find(|m| m.id == selected_model)
        .or_else(|| {
            enabled
                .clone()
                .find(|m| m.preference.is_some_and(|p| p.is_default))
        })
        .or_else(|| enabled.next())
}

/// Daily web-search / code-interpreter quotas, checked only for a tool the
/// request will carry.
fn check_tool_quotas(
    plan: &ReservePlan,
    usage: &PeriodUsage,
    params: &QuotaParams,
) -> Result<(), DomainError> {
    let daily = usage.get(Period::Daily, Bucket::Total);
    if plan.tools.web_search && daily.web_search_calls >= i64::from(params.web_search_daily_quota) {
        return Err(DomainError::QuotaExceeded(QuotaScope::WebSearch));
    }
    if plan.tools.code_interpreter
        && daily.code_interpreter_calls >= i64::from(params.code_interpreter_daily_quota)
    {
        return Err(DomainError::QuotaExceeded(QuotaScope::CodeInterpreter));
    }
    Ok(())
}

/// Quota status of one period of one tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodStatus {
    pub period: Period,
    pub limit_credits_micro: i64,
    /// `spent + reserved`.
    pub used_credits_micro: i64,
    pub remaining_credits_micro: i64,
    pub remaining_percentage: u32,
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

/// Status of a period with `limit` (`None` when `limit <= 0`).
#[must_use]
pub fn period_status(
    period: Period,
    limit: i64,
    usage: &BucketUsage,
    now: OffsetDateTime,
    warning_threshold_pct: u8,
) -> Option<PeriodStatus> {
    if limit <= 0 {
        return None;
    }
    let used = usage
        .spent_credits_micro
        .saturating_add(usage.reserved_credits_micro);
    let remaining = limit.saturating_sub(used).max(0);
    #[allow(clippy::integer_division, reason = "floored percentage is normative")]
    let pct = i128::from(remaining) * 100 / i128::from(limit);
    let remaining_percentage = u32::try_from(pct).unwrap_or(0);
    Some(PeriodStatus {
        period,
        limit_credits_micro: limit,
        used_credits_micro: used,
        remaining_credits_micro: remaining,
        remaining_percentage,
        next_reset: next_reset(period, now),
        warning: remaining_percentage <= 100 - u32::from(warning_threshold_pct),
        exhausted: remaining_percentage == 0,
    })
}

/// Quota status of one tier (`premium` / `total`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierStatus {
    pub tier: Bucket,
    pub periods: Vec<PeriodStatus>,
}

/// `GET /v1/quota/status` data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaStatus {
    /// `premium` then `total`.
    pub tiers: Vec<TierStatus>,
    pub warning_threshold_pct: u8,
}

/// Entry of the SSE `done` event's `quota_warnings`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaWarning {
    pub tier: Bucket,
    pub period: Period,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    /// Set only when `warning` or `exhausted`.
    pub next_reset: Option<OffsetDateTime>,
}

/// Result of [`QuotaService::preflight`].
#[derive(Debug, Clone, PartialEq)]
pub struct PreflightDecision {
    pub effective: ModelCatalogEntry,
    pub effective_tier: ModelTier,
    pub decision: QuotaDecision,
    pub downgrade_reason: Option<&'static str>,
    /// Reserve of the effective model (persisted on the turn).
    pub plan: ReservePlan,
    pub periods: PeriodStarts,
    pub policy_version: u64,
}

/// Input of [`QuotaService::settle`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SettleInput {
    pub tenant: Uuid,
    pub user: Uuid,
    /// Tier of the turn's effective model (in its policy version).
    pub effective_tier: ModelTier,
    /// The turn's preflight period starts (never recomputed).
    pub periods: PeriodStarts,
    pub turn: TurnReserve,
    pub method: SettlementMethod,
    pub settlement: Settlement,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
}

/// Input of [`QuotaService::release_reserve`]: the reserve of a turn whose
/// effective model (so its tier and price) is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseInput {
    pub tenant: Uuid,
    pub user: Uuid,
    pub periods: PeriodStarts,
    /// The turn's persisted `reserved_credits_micro`.
    pub reserved_credits_micro: i64,
    /// Whether the `tier:premium` rows may hold this reserve (the turn is the
    /// user's only running turn, so no other live reserve can be there).
    pub premium_candidate: bool,
}

/// Result of [`QuotaService::settle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettleResult {
    /// Bucket rows updated (2 for standard, 4 for premium turns).
    pub rows_updated: u64,
}

/// Quota enforcement over `quota_usage`.
pub struct QuotaService {
    db: Arc<DBProvider<DomainError>>,
    clock: Arc<dyn Clock>,
    pep: Arc<Pep>,
    policy: Arc<dyn PolicyPort>,
    config: Arc<MiniChatConfig>,
    metrics: Arc<MiniChatMetrics>,
}

impl QuotaService {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        clock: Arc<dyn Clock>,
        pep: Arc<Pep>,
        policy: Arc<dyn PolicyPort>,
        config: Arc<MiniChatConfig>,
        metrics: Arc<MiniChatMetrics>,
    ) -> Self {
        Self {
            db,
            clock,
            pep,
            policy,
            config,
            metrics,
        }
    }

    /// Preflight: reads the user's bucket rows of the current periods in one
    /// transaction, runs the downgrade cascade and the daily tool-quota
    /// checks. Writes nothing.
    ///
    /// # Errors
    /// `QuotaExceeded(scope)`; database errors.
    #[allow(clippy::too_many_arguments)]
    pub async fn preflight(
        &self,
        tenant: Uuid,
        user: Uuid,
        selected_model: &str,
        snap: &PolicySnapshot,
        limits: &UserLimits,
        inputs: &ReserveInputs,
        now: OffsetDateTime,
    ) -> Result<PreflightDecision, DomainError> {
        let periods = period_starts(now);
        let scope = subject_scope(tenant, user);
        // One transaction for every bucket row read (D§5.4.2). SecureORM has
        // no `FOR UPDATE`; the reserve re-check closes the TOCTOU gap.
        let rows = with_retry(&self.db, move |tx| {
            let scope = scope.clone();
            Box::pin(async move {
                Ok(QuotaUsageRepo
                    .list_period_rows(tx, &scope, &period_keys(periods))
                    .await?)
            })
        })
        .await?;
        let usage = PeriodUsage::from_rows(&rows, periods);
        let e = evaluate(
            selected_model,
            snap,
            limits,
            inputs,
            &usage,
            &QuotaParams::from_config(&self.config),
        )?;
        let tier = match e.effective_tier {
            ModelTier::Premium => "premium",
            ModelTier::Standard => "standard",
        };
        self.metrics
            .quota_preflight(e.decision.as_str(), &e.effective.id, tier);
        self.metrics.quota_estimated_tokens(e.plan.reserve_tokens);
        Ok(PreflightDecision {
            effective: e.effective,
            effective_tier: e.effective_tier,
            decision: e.decision,
            downgrade_reason: e.downgrade_reason,
            plan: e.plan,
            periods,
            policy_version: snap.policy_version,
        })
    }

    /// Books the decision's reserve on every bucket row of its tier and
    /// period, then re-checks `spent + reserved <= limit` on those rows.
    /// Runs inside the caller's transaction; on `Err` the caller rolls back.
    ///
    /// # Errors
    /// `QuotaExceeded(Tokens)` when a bucket is over its limit after the
    /// increments; database errors.
    pub async fn reserve(
        &self,
        tx: &DbTx<'_>,
        tenant: Uuid,
        user: Uuid,
        d: &PreflightDecision,
        limits: &UserLimits,
    ) -> Result<(), DomainError> {
        let scope = subject_scope(tenant, user);
        let now = self.clock.now();
        let buckets = Bucket::for_tier(d.effective_tier);
        let inc = QuotaIncrement {
            reserved_credits_micro: d.plan.reserved_credits_micro,
            ..QuotaIncrement::default()
        };
        for period in Period::ALL {
            for &bucket in buckets {
                let key = bucket_key(tenant, user, period, d.periods, bucket);
                QuotaUsageRepo
                    .increment(tx, &scope, &key, &inc, now)
                    .await?;
            }
        }
        // Re-check after the increments (ADR-0008): the increments hold the
        // row locks (PG) / the write lock (SQLite).
        let rows = QuotaUsageRepo
            .list_period_rows(tx, &scope, &period_keys(d.periods))
            .await?;
        if PeriodUsage::from_rows(&rows, d.periods).fits(buckets, limits, 0) {
            Ok(())
        } else {
            Err(DomainError::QuotaExceeded(QuotaScope::Tokens))
        }
    }

    /// Settles a turn's reserve on the bucket rows of its preflight periods
    /// (D§3.7 commit semantics). Runs inside the caller's transaction.
    ///
    /// # Errors
    /// Database errors.
    pub async fn settle(
        &self,
        tx: &DbTx<'_>,
        s: &SettleInput,
    ) -> Result<SettleResult, DomainError> {
        let scope = subject_scope(s.tenant, s.user);
        let now = self.clock.now();
        let base = QuotaIncrement {
            spent_credits_micro: s.settlement.committed_credits_micro,
            reserved_credits_micro: -s.turn.reserved_credits_micro,
            calls: 1,
            ..QuotaIncrement::default()
        };
        // Telemetry of bucket `total`: tokens on actual settlements only,
        // tool calls on actual and estimated ones.
        let mut total = base;
        if s.method == SettlementMethod::Actual
            && let Some((input, output)) = s.settlement.actual_tokens_for_telemetry
        {
            total.input_tokens = input;
            total.output_tokens = output;
        }
        if matches!(
            s.method,
            SettlementMethod::Actual | SettlementMethod::Estimated
        ) {
            total.web_search_calls = i32::try_from(s.web_search_calls).unwrap_or(i32::MAX);
            total.code_interpreter_calls =
                i32::try_from(s.code_interpreter_calls).unwrap_or(i32::MAX);
        }
        let mut rows_updated = 0;
        for period in Period::ALL {
            for &bucket in Bucket::for_tier(s.effective_tier) {
                let key = bucket_key(s.tenant, s.user, period, s.periods, bucket);
                let inc = if bucket == Bucket::Total {
                    &total
                } else {
                    &base
                };
                rows_updated += QuotaUsageRepo.increment(tx, &scope, &key, inc, now).await?;
            }
        }
        Ok(SettleResult { rows_updated })
    }

    /// Releases a turn's reserve without a debit (no multipliers needed):
    /// `spent += 0`, `reserved -= reserve`, `calls += 1`, like a `released`
    /// settlement. The `total` rows of the turn's periods always hold the
    /// reserve. The `tier:premium` rows hold it only for a premium turn, and
    /// the tier is unknown here: a premium row is released only when
    /// `premium_candidate` and the row still holds at least the reserve;
    /// otherwise a premium reserve stays booked until its period ends.
    /// Runs inside the caller's transaction.
    ///
    /// # Errors
    /// Database errors.
    pub async fn release_reserve(
        &self,
        tx: &DbTx<'_>,
        r: &ReleaseInput,
    ) -> Result<SettleResult, DomainError> {
        let scope = subject_scope(r.tenant, r.user);
        let now = self.clock.now();
        let inc = QuotaIncrement {
            reserved_credits_micro: -r.reserved_credits_micro,
            calls: 1,
            ..QuotaIncrement::default()
        };
        let premium = if r.premium_candidate {
            let rows = QuotaUsageRepo
                .list_period_rows(tx, &scope, &period_keys(r.periods))
                .await?;
            PeriodUsage::from_rows(&rows, r.periods)
        } else {
            PeriodUsage::default()
        };
        let mut rows_updated = 0;
        for period in Period::ALL {
            let mut buckets = vec![Bucket::Total];
            if r.premium_candidate
                && premium.get(period, Bucket::Premium).reserved_credits_micro
                    >= r.reserved_credits_micro
            {
                buckets.push(Bucket::Premium);
            }
            for bucket in buckets {
                let key = bucket_key(r.tenant, r.user, period, r.periods, bucket);
                rows_updated += QuotaUsageRepo
                    .increment(tx, &scope, &key, &inc, now)
                    .await?;
            }
        }
        Ok(SettleResult { rows_updated })
    }

    /// Quota status of the caller (PEP `UserQuota` `read`).
    ///
    /// # Errors
    /// Authorization, policy plugin and database errors.
    pub async fn status(&self, ctx: &SecurityContext) -> Result<QuotaStatus, DomainError> {
        let scope = self.pep.quota_scope(ctx).await?;
        let user = ctx.subject_id();
        let snap = self.policy.current_snapshot(user).await?;
        let limits = self.policy.user_limits(user, snap.policy_version).await?;
        let conn = self.db.conn()?;
        let tiers = self
            .tier_statuses(&conn, &scope, &limits, self.clock.now())
            .await?;
        Ok(QuotaStatus {
            tiers,
            warning_threshold_pct: self.config.quota.warning_threshold_pct,
        })
    }

    /// `quota_warnings` of the user at `now` (periods with `limit <= 0`
    /// skipped).
    ///
    /// # Errors
    /// Database errors.
    pub async fn warnings(
        &self,
        runner: &impl DBRunner,
        tenant: Uuid,
        user: Uuid,
        limits: &UserLimits,
        now: OffsetDateTime,
    ) -> Result<Vec<QuotaWarning>, DomainError> {
        let tiers = self
            .tier_statuses(runner, &subject_scope(tenant, user), limits, now)
            .await?;
        Ok(tiers
            .into_iter()
            .flat_map(|t| {
                t.periods.into_iter().map(move |p| QuotaWarning {
                    tier: t.tier,
                    period: p.period,
                    remaining_percentage: p.remaining_percentage,
                    warning: p.warning,
                    exhausted: p.exhausted,
                    next_reset: (p.warning || p.exhausted).then_some(p.next_reset),
                })
            })
            .collect())
    }

    /// Status of `premium` then `total` from the rows of the periods of `now`.
    async fn tier_statuses(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        limits: &UserLimits,
        now: OffsetDateTime,
    ) -> Result<Vec<TierStatus>, DomainError> {
        let periods = period_starts(now);
        let rows = QuotaUsageRepo
            .list_period_rows(runner, scope, &period_keys(periods))
            .await?;
        let usage = PeriodUsage::from_rows(&rows, periods);
        let threshold = self.config.quota.warning_threshold_pct;
        Ok([Bucket::Premium, Bucket::Total]
            .into_iter()
            .map(|tier| TierStatus {
                tier,
                periods: Period::ALL
                    .into_iter()
                    .filter_map(|p| {
                        period_status(
                            p,
                            tier.limit(p, limits),
                            &usage.get(p, tier),
                            now,
                            threshold,
                        )
                    })
                    .collect(),
            })
            .collect())
    }
}

/// Tenant + owner scope of `user` (the quota rows of one user).
fn subject_scope(tenant: Uuid, user: Uuid) -> AccessScope {
    AccessScope::single(ScopeConstraint::new(vec![
        ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, tenant),
        ScopeFilter::eq(pep_properties::OWNER_ID, user),
    ]))
}

fn period_keys(p: PeriodStarts) -> [(&'static str, Date); 2] {
    Period::ALL.map(|period| (period.as_str(), p.get(period)))
}

fn bucket_key(
    tenant: Uuid,
    user: Uuid,
    period: Period,
    starts: PeriodStarts,
    bucket: Bucket,
) -> BucketKey {
    BucketKey {
        tenant_id: tenant,
        user_id: user,
        period_type: period.as_str(),
        period_start: starts.get(period),
        bucket: bucket.as_str(),
    }
}

#[cfg(test)]
#[path = "quota_service_tests.rs"]
mod tests;
