//! Quota service (DESIGN section 3.2 "quota service", section 4 "Downgrade
//! Decision Flow", sections 5.3-5.5 and 5.8).
//!
//! Two-phase counting over `quota_usage` bucket rows: [`QuotaService::preflight`]
//! resolves the effective model with the downgrade cascade (read-only),
//! [`QuotaService::reserve_in_tx`] books the reserve and re-checks the limits
//! inside the caller's transaction, and [`QuotaService::settle_in_tx`] converts
//! the reserve into the committed spend at finalization.
//!
//! Row scoping: the status endpoint reads with the PEP scope from
//! `AuthzPort::quota_scope`; every other path works on behalf of a stored
//! `(tenant_id, user_id)` (the request's caller, or the turn's requester for
//! the finalization and watchdog paths) and uses `quota_repo::user_scope`,
//! the same tenant + owner narrowing, plus explicit tenant/user filters.

use std::sync::Arc;

use mini_chat_sdk::{
    KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, UsageTokens, UserLimits,
};
use time::{Date, Month, OffsetDateTime, UtcOffset};
use toolkit_db::DBProvider;
use toolkit_db::secure::DbTx;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::billing::SettlementMethod;
use crate::domain::credits::credits_micro_checked;
use crate::domain::enums::{PeriodType, QuotaBucket};
use crate::domain::error::{DomainError, QuotaScope};
use crate::domain::estimation::{
    ToolContext, ToolGates, estimate_text_tokens, max_output_applied, tool_gates, tool_surcharges,
};
use crate::domain::ports::{AuthzPort, PolicyProvider};
use crate::domain::time::db_now;
use crate::infra::db::entities::quota_usage;
use crate::infra::db::repos::quota_repo::{self, BucketKey, SettleDelta};

/// Downgrade reasons (DESIGN section 4, "Downgrade Decision Flow").
pub const REASON_PREMIUM_EXHAUSTED: &str = "premium_quota_exhausted";
pub const REASON_FORCE_STANDARD: &str = "force_standard_tier";
pub const REASON_DISABLE_PREMIUM: &str = "disable_premium_tier";
pub const REASON_MODEL_DISABLED: &str = "model_disabled";

// ── Periods ──────────────────────────────────────────────────────────────────

/// UTC period starts of the bucket rows a turn reserves against. Computed
/// once at preflight and reused unchanged by settlement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeriodStarts {
    pub daily: Date,
    pub monthly: Date,
}

impl PeriodStarts {
    /// Daily = UTC date of `t`; monthly = the 1st of that UTC month.
    #[must_use]
    pub fn at(t: OffsetDateTime) -> Self {
        let utc = t.to_offset(UtcOffset::UTC).date();
        // Day 1 exists in every month; `utc` is the fallback that never fires.
        let monthly = utc.replace_day(1).unwrap_or(utc);
        Self {
            daily: utc,
            monthly,
        }
    }

    /// The orphan watchdog's derivation from `chat_turns.started_at`.
    #[must_use]
    pub fn from_started_at(started_at: OffsetDateTime) -> Self {
        Self::at(started_at)
    }

    fn start(self, period: PeriodType) -> Date {
        match period {
            PeriodType::Daily => self.daily,
            PeriodType::Monthly => self.monthly,
        }
    }

    /// Midnight UTC after the period: tomorrow, or the 1st of next month.
    fn next_reset(self, period: PeriodType) -> Result<OffsetDateTime, DomainError> {
        let next = match period {
            PeriodType::Daily => self.daily.next_day(),
            PeriodType::Monthly => {
                let (year, month) = match self.monthly.month() {
                    Month::December => (self.monthly.year() + 1, Month::January),
                    m => (self.monthly.year(), m.next()),
                };
                Date::from_calendar_date(year, month, 1).ok()
            }
        };
        next.map(|d| d.midnight().assume_utc())
            .ok_or_else(|| DomainError::Internal("quota period reset out of range".to_owned()))
    }
}

// ── Preflight types ──────────────────────────────────────────────────────────

/// Quota decision of a turn (DTO `QuotaDecisionKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaDecisionKind {
    Allow,
    Downgrade,
}

impl QuotaDecisionKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Downgrade => "downgrade",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreflightInput {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub selected_model: String,
    /// UTF-8 byte length of the current user message.
    pub message_bytes: usize,
    /// `input_tokens + output_tokens` of the latest assistant message with usage.
    pub prior_context_tokens: i64,
    pub num_images: u32,
    pub tool_ctx: ToolContext,
    pub now: OffsetDateTime,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreflightDecision {
    pub snapshot: Arc<PolicySnapshot>,
    pub limits: UserLimits,
    pub effective_model: ModelCatalogEntry,
    pub selected_model: String,
    pub decision: QuotaDecisionKind,
    pub downgrade_reason: Option<&'static str>,
    /// Tools the effective model is sent with.
    pub tools: ToolGates,
    pub max_output_tokens_applied: i64,
    pub estimated_input_tokens: i64,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i64,
    pub periods: PeriodStarts,
}

impl PreflightDecision {
    /// Whether the effective model is premium (selects the premium bucket).
    #[must_use]
    pub fn premium(&self) -> bool {
        self.effective_model.tier == ModelTier::Premium
    }
}

/// Reserve booked in the turn-creating transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReserveRequest {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub premium: bool,
    pub reserved_credits_micro: i64,
    pub periods: PeriodStarts,
    pub limits: UserLimits,
}

/// Settlement of one turn (values persisted on `chat_turns` at preflight).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettlementInput {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    /// The effective model's tier in the snapshot of `policy_version_applied`.
    pub premium: bool,
    pub periods: PeriodStarts,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
    pub max_output_tokens_applied: i64,
    pub minimal_generation_floor_applied: i64,
    /// Multipliers from the snapshot of `policy_version_applied`.
    pub in_mult: i64,
    pub out_mult: i64,
    pub method: SettlementMethod,
    pub usage: Option<UsageTokens>,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettlementResult {
    /// Committed credits (outbox `actual_credits_micro`).
    pub charged_credits_micro: i64,
    /// Overshoot beyond `quota.overshoot_tolerance_factor` was capped at the
    /// reserve (internal only, not exported).
    pub overshoot_capped: bool,
}

// ── Status / warning types ───────────────────────────────────────────────────

/// Quota tier as reported to clients (DTO `QuotaTier`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaTierKind {
    Premium,
    Total,
}

impl QuotaTierKind {
    /// Reporting order: `premium` then `total`.
    const ALL: [Self; 2] = [Self::Premium, Self::Total];

    const fn bucket(self) -> QuotaBucket {
        match self {
            Self::Premium => QuotaBucket::TierPremium,
            Self::Total => QuotaBucket::Total,
        }
    }
}

/// Quota period as reported to clients (DTO `QuotaPeriod`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaPeriodKind {
    Daily,
    Monthly,
}

impl QuotaPeriodKind {
    const ALL: [Self; 2] = [Self::Daily, Self::Monthly];

    const fn period_type(self) -> PeriodType {
        match self {
            Self::Daily => PeriodType::Daily,
            Self::Monthly => PeriodType::Monthly,
        }
    }
}

/// One `quota_warnings` entry of the SSE `done` event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuotaWarningView {
    pub tier: QuotaTierKind,
    pub period: QuotaPeriodKind,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    /// Set only when `warning` or `exhausted`.
    pub next_reset: Option<OffsetDateTime>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuotaPeriodStatusView {
    pub period: QuotaPeriodKind,
    pub limit_credits_micro: i64,
    /// `spent + reserved` (conservative).
    pub used_credits_micro: i64,
    pub remaining_credits_micro: i64,
    pub remaining_percentage: u32,
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaTierStatusView {
    pub tier: QuotaTierKind,
    pub periods: Vec<QuotaPeriodStatusView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaStatusView {
    pub tiers: Vec<QuotaTierStatusView>,
    pub warning_threshold_pct: u8,
}

// ── Row lookup helpers ───────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default)]
struct Counters {
    spent: i64,
    reserved: i64,
    web_search_calls: i32,
    code_interpreter_calls: i32,
}

/// The user's rows of the checked periods; a missing row counts as zeros.
struct Usage(Vec<quota_usage::Model>);

impl Usage {
    fn get(&self, period: PeriodType, bucket: QuotaBucket) -> Counters {
        self.0
            .iter()
            .find(|r| r.period_type == period.as_str() && r.bucket == bucket.as_str())
            .map(|r| Counters {
                spent: r.spent_credits_micro,
                reserved: r.reserved_credits_micro,
                web_search_calls: r.web_search_calls,
                code_interpreter_calls: r.code_interpreter_calls,
            })
            .unwrap_or_default()
    }

    /// `spent + reserved + reserve <= limit` (DESIGN section 5.4.2).
    fn bucket_available(
        &self,
        limits: &UserLimits,
        period: PeriodType,
        bucket: QuotaBucket,
        reserve: i64,
    ) -> bool {
        let c = self.get(period, bucket);
        c.spent.saturating_add(c.reserved).saturating_add(reserve)
            <= limit_for(limits, bucket, period)
    }

    /// Bucket `total` for every tier, plus `tier:premium` for premium; every
    /// period. Checked in order total/premium, daily/monthly.
    fn tier_available(&self, limits: &UserLimits, tier: ModelTier, reserve: i64) -> bool {
        required_buckets(tier == ModelTier::Premium)
            .iter()
            .all(|b| {
                [PeriodType::Daily, PeriodType::Monthly]
                    .iter()
                    .all(|p| self.bucket_available(limits, *p, *b, reserve))
            })
    }
}

fn required_buckets(premium: bool) -> &'static [QuotaBucket] {
    if premium {
        &[QuotaBucket::Total, QuotaBucket::TierPremium]
    } else {
        &[QuotaBucket::Total]
    }
}

/// `total` -> `user_limits.standard` (global cap); `tier:premium` ->
/// `user_limits.premium` (subcap). `tier:standard` is analytics only.
fn limit_for(limits: &UserLimits, bucket: QuotaBucket, period: PeriodType) -> i64 {
    let tier = match bucket {
        QuotaBucket::TierPremium => &limits.premium,
        QuotaBucket::Total | QuotaBucket::TierStandard => &limits.standard,
    };
    match period {
        PeriodType::Daily => tier.limit_daily_credits_micro,
        PeriodType::Monthly => tier.limit_monthly_credits_micro,
    }
}

/// Bucket rows a turn touches, in a fixed order (row-lock order on Postgres).
fn bucket_keys(
    tenant_id: Uuid,
    user_id: Uuid,
    premium: bool,
    periods: PeriodStarts,
) -> Vec<BucketKey> {
    let mut keys = Vec::with_capacity(4);
    for bucket in required_buckets(premium) {
        for period in [PeriodType::Daily, PeriodType::Monthly] {
            keys.push(BucketKey {
                tenant_id,
                user_id,
                period_type: period,
                period_start: periods.start(period),
                bucket: *bucket,
            });
        }
    }
    keys
}

/// The reserve a cascade candidate would book (DESIGN section 5.4.1).
struct CandidateReserve {
    tools: ToolGates,
    max_output_tokens_applied: i64,
    estimated_input_tokens: i64,
    reserved_credits_micro: i64,
}

/// One candidate per tier among enabled models: the selected model if it is
/// of this tier, else the tier's default, else its first model.
fn candidate<'a>(
    snap: &'a PolicySnapshot,
    tier: ModelTier,
    selected: &str,
) -> Option<&'a ModelCatalogEntry> {
    let in_tier = || snap.enabled().filter(move |m| m.tier == tier);
    in_tier()
        .find(|m| m.id == selected)
        .or_else(|| in_tier().find(|m| m.is_default()))
        .or_else(|| in_tier().next())
}

fn to_i32(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

// ── Service ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct QuotaService {
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<dyn AuthzPort>,
    policy: Arc<dyn PolicyProvider>,
    cfg: Arc<MiniChatConfig>,
}

impl QuotaService {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        authz: Arc<dyn AuthzPort>,
        policy: Arc<dyn PolicyProvider>,
        cfg: Arc<MiniChatConfig>,
    ) -> Self {
        Self {
            db,
            authz,
            policy,
            cfg,
        }
    }

    fn candidate_reserve(
        &self,
        m: &ModelCatalogEntry,
        ks: KillSwitches,
        input: &PreflightInput,
    ) -> CandidateReserve {
        let b = &m.estimation_budgets;
        let tools = tool_gates(m, &ks, &input.tool_ctx);
        let images = i64::from(input.num_images).saturating_mul(i64::from(b.image_token_budget));
        let estimated = estimate_text_tokens(input.message_bytes, b)
            .saturating_add(input.prior_context_tokens.max(0))
            .saturating_add(images)
            .saturating_add(tool_surcharges(tools, b));
        let max_out = max_output_applied(m, self.cfg.streaming.max_output_tokens);
        let credits = credits_micro_checked(
            estimated,
            max_out,
            m.input_tokens_credit_multiplier_micro,
            m.output_tokens_credit_multiplier_micro,
        )
        .unwrap_or_else(|e| {
            tracing::warn!(model = %m.id, error = %e, "reserve not computable; candidate unavailable");
            i64::MAX
        });
        CandidateReserve {
            tools,
            max_output_tokens_applied: max_out,
            estimated_input_tokens: estimated,
            reserved_credits_micro: credits,
        }
    }

    /// Reads the user's rows of both periods in one transaction (`FOR UPDATE`
    /// on Postgres; nothing is written).
    async fn read_usage(
        &self,
        tenant_id: Uuid,
        user_id: Uuid,
        periods: PeriodStarts,
    ) -> Result<Usage, DomainError> {
        let rows = self
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    quota_repo::find_rows(
                        tx,
                        &quota_repo::user_scope(tenant_id, user_id),
                        tenant_id,
                        user_id,
                        periods.daily,
                        periods.monthly,
                        true,
                    )
                    .await
                })
            })
            .await?;
        Ok(Usage(rows))
    }

    /// Preflight decision (DESIGN section 4, "Downgrade Decision Flow").
    ///
    /// # Errors
    /// `FeatureDisabled{web_search}` (kill switch, before the cascade),
    /// `QuotaExceeded{tokens}` (no tier available),
    /// `QuotaExceeded{web_search|code_interpreter}` (daily tool quota of a tool
    /// sent with the effective model), `Internal` (policy plugin, database).
    pub async fn preflight(
        &self,
        input: &PreflightInput,
    ) -> Result<PreflightDecision, DomainError> {
        let snapshot = self.policy.current(input.user_id).await?;
        let ks = snapshot.kill_switches;
        if ks.disable_web_search && input.tool_ctx.web_search_requested {
            return Err(DomainError::FeatureDisabled {
                subject: "web_search",
            });
        }
        let limits = self
            .policy
            .user_limits(input.user_id, snapshot.policy_version)
            .await?;
        let periods = PeriodStarts::at(input.now);
        let usage = self
            .read_usage(input.tenant_id, input.user_id, periods)
            .await?;

        let (start_tier, mut reason) = match snapshot.find(&input.selected_model) {
            Some(m) if m.enabled => (m.tier, None),
            Some(m) => (m.tier, Some(REASON_MODEL_DISABLED)),
            None => (ModelTier::Premium, Some(REASON_MODEL_DISABLED)),
        };
        let cascade: &[ModelTier] = match start_tier {
            ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
            ModelTier::Standard => &[ModelTier::Standard],
        };

        let mut chosen = None;
        for &tier in cascade {
            if tier == ModelTier::Premium {
                if ks.force_standard_tier {
                    reason.get_or_insert(REASON_FORCE_STANDARD);
                    continue;
                }
                if ks.disable_premium_tier {
                    reason.get_or_insert(REASON_DISABLE_PREMIUM);
                    continue;
                }
            }
            let Some(model) = candidate(&snapshot, tier, &input.selected_model) else {
                continue;
            };
            let reserve = self.candidate_reserve(model, ks, input);
            if usage.tier_available(&limits, tier, reserve.reserved_credits_micro) {
                chosen = Some((model.clone(), reserve));
                break;
            }
            if tier == ModelTier::Premium {
                reason.get_or_insert(REASON_PREMIUM_EXHAUSTED);
            }
        }
        let Some((effective_model, reserve)) = chosen else {
            return Err(DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens,
            });
        };

        self.check_tool_quotas(&usage, reserve.tools)?;

        let decision = if effective_model.id == input.selected_model && reason.is_none() {
            QuotaDecisionKind::Allow
        } else {
            QuotaDecisionKind::Downgrade
        };
        let floor = i64::from(self.cfg.estimation_budgets.minimal_generation_floor)
            .min(reserve.max_output_tokens_applied);
        Ok(PreflightDecision {
            snapshot,
            limits,
            effective_model,
            selected_model: input.selected_model.clone(),
            decision,
            downgrade_reason: reason,
            tools: reserve.tools,
            max_output_tokens_applied: reserve.max_output_tokens_applied,
            estimated_input_tokens: reserve.estimated_input_tokens,
            reserve_tokens: reserve
                .estimated_input_tokens
                .saturating_add(reserve.max_output_tokens_applied),
            reserved_credits_micro: reserve.reserved_credits_micro,
            minimal_generation_floor_applied: floor,
            periods,
        })
    }

    /// Daily web search / code interpreter call quotas, read from the daily
    /// `total` row; checked only for tools sent with the effective model.
    fn check_tool_quotas(&self, usage: &Usage, tools: ToolGates) -> Result<(), DomainError> {
        let daily = usage.get(PeriodType::Daily, QuotaBucket::Total);
        let q = &self.cfg.quota;
        if tools.web_search
            && i64::from(daily.web_search_calls) >= i64::from(q.web_search_daily_quota)
        {
            return Err(DomainError::QuotaExceeded {
                scope: QuotaScope::WebSearch,
            });
        }
        if tools.code_interpreter
            && i64::from(daily.code_interpreter_calls) >= i64::from(q.code_interpreter_daily_quota)
        {
            return Err(DomainError::QuotaExceeded {
                scope: QuotaScope::CodeInterpreter,
            });
        }
        Ok(())
    }

    /// Books the reserve inside the caller's transaction: increments
    /// `reserved_credits_micro` of bucket `total` (and `tier:premium` for a
    /// premium turn) for both periods, then re-reads the rows and rejects if
    /// any is over its limit (DESIGN section 5.4.2, "TOCTOU"). Writes come
    /// before the read (Ruling R5).
    ///
    /// # Errors
    /// `QuotaExceeded{tokens}` (the caller rolls the transaction back),
    /// database failure.
    pub async fn reserve_in_tx(
        &self,
        tx: &DbTx<'_>,
        r: &ReserveRequest,
    ) -> Result<(), DomainError> {
        let scope = quota_repo::user_scope(r.tenant_id, r.user_id);
        let now = db_now();
        let keys = bucket_keys(r.tenant_id, r.user_id, r.premium, r.periods);
        for key in &keys {
            quota_repo::ensure_row(tx, &scope, key, now).await?;
            quota_repo::add_reserved(tx, &scope, key, r.reserved_credits_micro, now).await?;
        }
        let usage = Usage(
            quota_repo::find_rows(
                tx,
                &scope,
                r.tenant_id,
                r.user_id,
                r.periods.daily,
                r.periods.monthly,
                false,
            )
            .await?,
        );
        let over = keys
            .iter()
            .any(|k| !usage.bucket_available(&r.limits, k.period_type, k.bucket, 0));
        if over {
            return Err(DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens,
            });
        }
        Ok(())
    }

    /// Settles a turn inside the caller's (finalization) transaction (DESIGN
    /// section 3.7 commit semantics, 5.4.4-5.4.5, 5.8).
    ///
    /// # Errors
    /// `Internal` when the credit computation fails (out-of-range usage or
    /// multiplier) or on database failure.
    pub async fn settle_in_tx(
        &self,
        tx: &DbTx<'_>,
        s: &SettlementInput,
    ) -> Result<SettlementResult, DomainError> {
        let charge = settlement_charge(s, self.cfg.quota.overshoot_tolerance_factor)
            .inspect_err(|e| tracing::warn!(error = %e, "quota settlement not computable"))?;
        let scope = quota_repo::user_scope(s.tenant_id, s.user_id);
        let now = db_now();
        let tools_counted = s.method != SettlementMethod::Released;
        let total = SettleDelta {
            release_reserved: s.reserved_credits_micro,
            spent: charge.result.charged_credits_micro,
            input_tokens: charge.input_tokens,
            output_tokens: charge.output_tokens,
            web_search_calls: if tools_counted {
                to_i32(s.web_search_calls)
            } else {
                0
            },
            code_interpreter_calls: if tools_counted {
                to_i32(s.code_interpreter_calls)
            } else {
                0
            },
        };
        let premium = SettleDelta {
            release_reserved: s.reserved_credits_micro,
            spent: charge.result.charged_credits_micro,
            ..SettleDelta::default()
        };
        for key in bucket_keys(s.tenant_id, s.user_id, s.premium, s.periods) {
            let delta = if key.bucket == QuotaBucket::Total {
                &total
            } else {
                &premium
            };
            quota_repo::ensure_row(tx, &scope, &key, now).await?;
            quota_repo::apply_settlement(tx, &scope, &key, delta, now).await?;
        }
        Ok(charge.result)
    }

    /// `quota_warnings` for the SSE `done` event (DESIGN section 3.2).
    ///
    /// # Errors
    /// Database failure.
    pub async fn warnings(
        &self,
        tenant_id: Uuid,
        user_id: Uuid,
        limits: &UserLimits,
        now: OffsetDateTime,
    ) -> Result<Vec<QuotaWarningView>, DomainError> {
        let periods = PeriodStarts::at(now);
        let conn = self.db.conn()?;
        let usage = Usage(
            quota_repo::find_rows(
                &conn,
                &quota_repo::user_scope(tenant_id, user_id),
                tenant_id,
                user_id,
                periods.daily,
                periods.monthly,
                false,
            )
            .await?,
        );
        Ok(self
            .figures(&usage, limits, periods)?
            .into_iter()
            .map(|(tier, p)| QuotaWarningView {
                tier,
                period: p.period,
                remaining_percentage: p.remaining_percentage,
                warning: p.warning,
                exhausted: p.exhausted,
                next_reset: (p.warning || p.exhausted).then_some(p.next_reset),
            })
            .collect())
    }

    /// `GET /v1/quota/status` for the caller (PEP `USER_QUOTA` / `read`).
    ///
    /// # Errors
    /// Authorization failure, `Internal` (policy plugin, database).
    pub async fn status(&self, ctx: &SecurityContext) -> Result<QuotaStatusView, DomainError> {
        let scope = self.authz.quota_scope(ctx).await?;
        let (tenant_id, user_id) = (ctx.subject_tenant_id(), ctx.subject_id());
        let snapshot = self.policy.current(user_id).await?;
        let limits = self
            .policy
            .user_limits(user_id, snapshot.policy_version)
            .await?;
        let periods = PeriodStarts::at(OffsetDateTime::now_utc());
        let conn = self.db.conn()?;
        let usage = Usage(
            quota_repo::find_rows(
                &conn,
                &scope,
                tenant_id,
                user_id,
                periods.daily,
                periods.monthly,
                false,
            )
            .await?,
        );
        let figures = self.figures(&usage, &limits, periods)?;
        let tiers = QuotaTierKind::ALL
            .iter()
            .map(|&tier| QuotaTierStatusView {
                tier,
                periods: figures
                    .iter()
                    .filter(|(t, _)| *t == tier)
                    .map(|(_, p)| *p)
                    .collect(),
            })
            .collect();
        Ok(QuotaStatusView {
            tiers,
            warning_threshold_pct: self.cfg.quota.warning_threshold_pct,
        })
    }

    /// Per tier and period, in reporting order; limits `<= 0` are skipped.
    fn figures(
        &self,
        usage: &Usage,
        limits: &UserLimits,
        periods: PeriodStarts,
    ) -> Result<Vec<(QuotaTierKind, QuotaPeriodStatusView)>, DomainError> {
        let warn_at = 100_u32.saturating_sub(u32::from(self.cfg.quota.warning_threshold_pct));
        let mut out = Vec::with_capacity(4);
        for tier in QuotaTierKind::ALL {
            for period in QuotaPeriodKind::ALL {
                let pt = period.period_type();
                let limit = limit_for(limits, tier.bucket(), pt);
                if limit <= 0 {
                    continue;
                }
                let c = usage.get(pt, tier.bucket());
                let used = c.spent.saturating_add(c.reserved);
                let remaining = limit.saturating_sub(used).max(0);
                let pct = (i128::from(remaining) * 100)
                    .div_euclid(i128::from(limit))
                    .clamp(0, 100);
                let pct = u32::try_from(pct).unwrap_or(0);
                out.push((
                    tier,
                    QuotaPeriodStatusView {
                        period,
                        limit_credits_micro: limit,
                        used_credits_micro: used,
                        remaining_credits_micro: remaining,
                        remaining_percentage: pct,
                        next_reset: periods.next_reset(pt)?,
                        warning: pct <= warn_at,
                        exhausted: pct == 0,
                    },
                ));
            }
        }
        Ok(out)
    }
}

struct Charge {
    result: SettlementResult,
    /// Token telemetry (actual settlements only).
    input_tokens: i64,
    output_tokens: i64,
}

/// Committed credits of a settlement: actual (with the overshoot cap),
/// estimated `credits(reserve_tokens - max_output, floor)`, or 0 for released.
fn settlement_charge(s: &SettlementInput, tolerance: f64) -> Result<Charge, DomainError> {
    match s.method {
        SettlementMethod::Released => Ok(Charge {
            result: SettlementResult {
                charged_credits_micro: 0,
                overshoot_capped: false,
            },
            input_tokens: 0,
            output_tokens: 0,
        }),
        SettlementMethod::Estimated => {
            let estimated_input = s
                .reserve_tokens
                .saturating_sub(s.max_output_tokens_applied)
                .max(0);
            let charged = credits_micro_checked(
                estimated_input,
                s.minimal_generation_floor_applied,
                s.in_mult,
                s.out_mult,
            )?;
            Ok(Charge {
                result: SettlementResult {
                    charged_credits_micro: charged,
                    overshoot_capped: false,
                },
                input_tokens: 0,
                output_tokens: 0,
            })
        }
        SettlementMethod::Actual => {
            let u = s.usage.unwrap_or_default();
            let (input, output) = (u.input_tokens.max(0), u.output_tokens.max(0));
            let actual_tokens = input.saturating_add(output);
            let capped = actual_tokens > s.reserve_tokens && {
                // Floating-point ratio (DESIGN section 5.4.5); a zero or
                // negative reserve always caps.
                #[allow(clippy::cast_precision_loss)]
                let factor = actual_tokens as f64 / s.reserve_tokens as f64;
                s.reserve_tokens <= 0 || factor > tolerance
            };
            let charged = if capped {
                s.reserved_credits_micro
            } else {
                credits_micro_checked(input, output, s.in_mult, s.out_mult)?
            };
            Ok(Charge {
                result: SettlementResult {
                    charged_credits_micro: charged,
                    overshoot_capped: capped,
                },
                input_tokens: input,
                output_tokens: output,
            })
        }
    }
}

#[cfg(test)]
#[path = "quota_service_tests.rs"]
mod quota_service_tests;
