//! Quota service: periods, status, preflight cascade, reserve and settlement
//! (DESIGN §3.2 quota service, §5.4).

use std::collections::HashMap;

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, UserLimits};
use time::{Date, Month, OffsetDateTime, Time};
use toolkit_db::secure::DBRunner;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::app::AppServices;
use crate::domain::authz;
use crate::domain::credits::{self, ReserveInputs, Settlement, SettlementMethod};
use crate::domain::error::{DomainError, DomainResult, QuotaScope};
use crate::domain::policy;
use crate::infra::db::entities::quota_usage;
use crate::infra::db::repo::{self, QuotaDeltas};

pub const BUCKET_TOTAL: &str = "total";
pub const BUCKET_PREMIUM: &str = "tier:premium";
pub const DAILY: &str = "daily";
pub const MONTHLY: &str = "monthly";

/// Period starts of a turn (persisted in memory; derived from `started_at` by the watchdog).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Periods {
    pub daily: Date,
    pub monthly: Date,
}

impl Periods {
    #[must_use]
    pub fn at(t: OffsetDateTime) -> Self {
        let t = t.to_offset(time::UtcOffset::UTC);
        let daily = t.date();
        let monthly = Date::from_calendar_date(daily.year(), daily.month(), 1).unwrap_or(daily);
        Self { daily, monthly }
    }

    #[must_use]
    pub fn list(self) -> [(&'static str, Date); 2] {
        [(DAILY, self.daily), (MONTHLY, self.monthly)]
    }
}

/// Next reset of a period after `now` (midnight UTC tomorrow / 1st of next month).
#[must_use]
pub fn next_reset(period: &str, now: OffsetDateTime) -> OffsetDateTime {
    let d = now.to_offset(time::UtcOffset::UTC).date();
    let date = if period == DAILY {
        d.next_day().unwrap_or(d)
    } else {
        let (y, m) = if d.month() == Month::December {
            (d.year() + 1, Month::January)
        } else {
            (d.year(), d.month().next())
        };
        Date::from_calendar_date(y, m, 1).unwrap_or(d)
    };
    date.with_time(Time::MIDNIGHT).assume_utc()
}

/// Usage of one bucket row.
#[derive(Debug, Clone, Copy, Default)]
pub struct BucketUsage {
    pub spent: i64,
    pub reserved: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

/// Usage keyed by `(period_type, bucket)`.
pub type UsageMap = HashMap<(String, String), BucketUsage>;

#[must_use]
pub fn usage_map(rows: &[quota_usage::Model], periods: Periods) -> UsageMap {
    let mut m = UsageMap::new();
    for r in rows {
        let matches = (r.period_type == DAILY && r.period_start == periods.daily)
            || (r.period_type == MONTHLY && r.period_start == periods.monthly);
        if matches {
            m.insert(
                (r.period_type.clone(), r.bucket.clone()),
                BucketUsage {
                    spent: r.spent_credits_micro,
                    reserved: r.reserved_credits_micro,
                    web_search_calls: i64::from(r.web_search_calls),
                    code_interpreter_calls: i64::from(r.code_interpreter_calls),
                },
            );
        }
    }
    m
}

/// Loads the user's bucket rows for the periods.
///
/// # Errors
/// Database errors.
pub async fn load_usage(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    periods: &Periods,
) -> DomainResult<UsageMap> {
    let scope = repo::user_scope(tenant_id, user_id);
    let rows = repo::quota_rows(runner, &scope, tenant_id, user_id, &periods.list()).await?;
    Ok(usage_map(&rows, *periods))
}

fn limit_of(limits: &UserLimits, bucket: &str, period: &str) -> i64 {
    let t = if bucket == BUCKET_PREMIUM {
        &limits.premium
    } else {
        &limits.standard
    };
    if period == DAILY {
        t.limit_daily_credits_micro
    } else {
        t.limit_monthly_credits_micro
    }
}

/// Status of one tier period (DESIGN "Quota Status Endpoint").
#[derive(Debug, Clone)]
pub struct PeriodStatus {
    pub period: &'static str,
    pub limit: i64,
    pub used: i64,
    pub remaining: i64,
    pub remaining_pct: u32,
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

/// Status of one tier (`premium` or `total`).
#[derive(Debug, Clone)]
pub struct TierStatus {
    pub tier: &'static str,
    pub periods: Vec<PeriodStatus>,
}

/// Computes the status; periods with a limit `<= 0` are skipped.
#[must_use]
pub fn compute_status(
    limits: &UserLimits,
    usage: &UsageMap,
    now: OffsetDateTime,
    threshold_pct: u8,
) -> Vec<TierStatus> {
    let mut tiers = Vec::new();
    for (tier, bucket) in [("premium", BUCKET_PREMIUM), ("total", BUCKET_TOTAL)] {
        let mut periods = Vec::new();
        for period in [DAILY, MONTHLY] {
            let limit = limit_of(limits, bucket, period);
            if limit <= 0 {
                continue;
            }
            let u = usage
                .get(&(period.to_owned(), bucket.to_owned()))
                .copied()
                .unwrap_or_default();
            let used = u.spent.saturating_add(u.reserved).max(0);
            let remaining = limit.saturating_sub(used).max(0);
            #[allow(clippy::integer_division)] // whole-percent floor is the intended rounding
            let pct = i128::from(remaining) * 100 / i128::from(limit);
            let remaining_pct = u32::try_from(pct.clamp(0, 100)).unwrap_or(0);
            let warning = remaining_pct <= 100 - u32::from(threshold_pct);
            let exhausted = remaining_pct == 0;
            periods.push(PeriodStatus {
                period,
                limit,
                used,
                remaining,
                remaining_pct,
                next_reset: next_reset(period, now),
                warning,
                exhausted,
            });
        }
        tiers.push(TierStatus { tier, periods });
    }
    tiers
}

/// Quota decision of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaDecision {
    Allow,
    Downgrade,
}

/// Tools sent with a candidate model.
#[allow(clippy::struct_excessive_bools)] // one independent on/off flag per provider tool
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolSet {
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

/// Inputs of the preflight.
#[allow(clippy::struct_excessive_bools)] // independent request/chat facts consumed by the preflight
pub struct PreflightInput<'a> {
    pub snapshot: &'a PolicySnapshot,
    pub limits: &'a UserLimits,
    pub selected_model: &'a str,
    pub message_text: &'a str,
    pub prior_context_tokens: i64,
    pub image_count: u32,
    pub chat_has_ready_docs: bool,
    pub chat_has_ready_xlsx: bool,
    pub web_search_requested: bool,
    pub usage: &'a UsageMap,
    pub max_output_cap: u32,
    pub minimal_generation_floor: u32,
    pub web_search_daily_quota: u32,
    pub code_interpreter_daily_quota: u32,
    pub periods: Periods,
}

/// Preflight decision (DESIGN §4 "Downgrade Decision Flow").
#[derive(Debug, Clone)]
pub struct PreflightDecision {
    pub effective: ModelCatalogEntry,
    pub decision: QuotaDecision,
    pub downgrade_reason: Option<String>,
    pub tools: ToolSet,
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i64,
    pub policy_version: u64,
    pub premium: bool,
    pub periods: Periods,
    pub limits: UserLimits,
}

/// Tools a candidate model would get.
#[must_use]
pub fn tools_for(m: &ModelCatalogEntry, ks: KillSwitches, input: &PreflightInput<'_>) -> ToolSet {
    let ts = &m.general_config.tool_support;
    ToolSet {
        file_search: input.chat_has_ready_docs && ts.file_search && !ks.disable_file_search,
        web_search: input.web_search_requested && ts.web_search,
        code_interpreter: input.chat_has_ready_xlsx
            && ts.code_interpreter
            && !ks.disable_code_interpreter,
    }
}

struct CandidateReserve {
    estimated_input_tokens: i64,
    max_output_tokens_applied: i64,
    credits: i64,
}

fn candidate_reserve(
    m: &ModelCatalogEntry,
    tools: ToolSet,
    input: &PreflightInput<'_>,
) -> CandidateReserve {
    let b = &m.estimation_budgets;
    let est = credits::estimated_input_tokens(
        &ReserveInputs {
            message_tokens: credits::estimate_text_tokens(input.message_text, b),
            prior_context_tokens: input.prior_context_tokens,
            image_count: input.image_count,
            file_search: tools.file_search,
            web_search: tools.web_search,
            code_interpreter: tools.code_interpreter,
        },
        b,
    );
    let mota = i64::from(m.max_output_tokens.min(input.max_output_cap));
    let credits = credits::credits_micro(
        est,
        mota,
        m.input_tokens_credit_multiplier_micro,
        m.output_tokens_credit_multiplier_micro,
    )
    .unwrap_or_else(|e| {
        tracing::warn!(model = %m.id, error = %e, "cascade candidate reserve cannot be computed");
        i64::MAX
    });
    CandidateReserve {
        estimated_input_tokens: est,
        max_output_tokens_applied: mota,
        credits,
    }
}

fn bucket_available(
    usage: &UsageMap,
    limits: &UserLimits,
    bucket: &str,
    period: &str,
    reserve: i64,
) -> bool {
    let u = usage
        .get(&(period.to_owned(), bucket.to_owned()))
        .copied()
        .unwrap_or_default();
    let limit = limit_of(limits, bucket, period);
    u.spent
        .checked_add(u.reserved)
        .and_then(|v| v.checked_add(reserve))
        .is_some_and(|total| total <= limit)
}

fn tier_available(usage: &UsageMap, limits: &UserLimits, tier: ModelTier, reserve: i64) -> bool {
    [DAILY, MONTHLY].iter().all(|p| {
        bucket_available(usage, limits, BUCKET_TOTAL, p, reserve)
            && (tier != ModelTier::Premium
                || bucket_available(usage, limits, BUCKET_PREMIUM, p, reserve))
    })
}

/// Runs the downgrade cascade and the daily tool quotas.
///
/// # Errors
/// `QuotaExceeded(tokens|web_search|code_interpreter)`.
pub fn preflight(input: &PreflightInput<'_>) -> DomainResult<PreflightDecision> {
    let snapshot = input.snapshot;
    let ks = snapshot.kill_switches;
    let (start_tier, mut reason) = match snapshot.find(input.selected_model) {
        Some(m) if m.enabled => (m.tier, None),
        Some(m) => (m.tier, Some("model_disabled".to_owned())),
        None => (ModelTier::Premium, Some("model_disabled".to_owned())),
    };
    let cascade: &[ModelTier] = if start_tier == ModelTier::Premium {
        &[ModelTier::Premium, ModelTier::Standard]
    } else {
        &[ModelTier::Standard]
    };
    for &tier in cascade {
        if tier == ModelTier::Premium {
            if ks.force_standard_tier {
                reason.get_or_insert_with(|| "force_standard_tier".to_owned());
                continue;
            }
            if ks.disable_premium_tier {
                reason.get_or_insert_with(|| "disable_premium_tier".to_owned());
                continue;
            }
        }
        let Some(candidate) = policy::tier_candidate(snapshot, tier, input.selected_model) else {
            continue;
        };
        let tools = tools_for(candidate, ks, input);
        let r = candidate_reserve(candidate, tools, input);
        if !tier_available(input.usage, input.limits, tier, r.credits) {
            if tier == ModelTier::Premium {
                reason.get_or_insert_with(|| "premium_quota_exhausted".to_owned());
            }
            continue;
        }
        let decision = if candidate.id == input.selected_model && reason.is_none() {
            QuotaDecision::Allow
        } else {
            QuotaDecision::Downgrade
        };
        // Daily tool quotas, only for tools that are sent.
        let daily_total = input
            .usage
            .get(&(DAILY.to_owned(), BUCKET_TOTAL.to_owned()))
            .copied()
            .unwrap_or_default();
        if tools.web_search
            && daily_total.web_search_calls >= i64::from(input.web_search_daily_quota)
        {
            return Err(DomainError::QuotaExceeded(QuotaScope::WebSearch));
        }
        if tools.code_interpreter
            && daily_total.code_interpreter_calls >= i64::from(input.code_interpreter_daily_quota)
        {
            return Err(DomainError::QuotaExceeded(QuotaScope::CodeInterpreter));
        }
        let floor = i64::from(input.minimal_generation_floor).min(r.max_output_tokens_applied);
        return Ok(PreflightDecision {
            effective: candidate.clone(),
            decision,
            downgrade_reason: if decision == QuotaDecision::Downgrade {
                reason
            } else {
                None
            },
            tools,
            estimated_input_tokens: r.estimated_input_tokens,
            max_output_tokens_applied: r.max_output_tokens_applied,
            reserve_tokens: r.estimated_input_tokens + r.max_output_tokens_applied,
            reserved_credits_micro: r.credits,
            minimal_generation_floor_applied: floor,
            policy_version: snapshot.policy_version,
            premium: tier == ModelTier::Premium,
            periods: input.periods,
            limits: input.limits.clone(),
        });
    }
    Err(DomainError::QuotaExceeded(QuotaScope::Tokens))
}

/// Writes the reserve (bucket `total`, plus `tier:premium` for premium turns) and
/// re-checks the limits in the same transaction.
///
/// # Errors
/// `QuotaExceeded(Tokens)` when a bucket is over its limit after the increment.
pub async fn write_reserve(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    d: &PreflightDecision,
    now: OffsetDateTime,
) -> DomainResult<()> {
    let mut buckets = vec![BUCKET_TOTAL];
    if d.premium {
        buckets.push(BUCKET_PREMIUM);
    }
    for (pt, ps) in d.periods.list() {
        for b in &buckets {
            repo::ensure_quota_row(runner, tenant_id, user_id, pt, ps, b, now).await?;
            repo::bump_quota_row(
                runner,
                tenant_id,
                user_id,
                pt,
                ps,
                b,
                &QuotaDeltas {
                    reserved: d.reserved_credits_micro,
                    ..QuotaDeltas::default()
                },
                now,
            )
            .await?;
        }
    }
    let usage = load_usage(runner, tenant_id, user_id, &d.periods).await?;
    for (pt, _) in d.periods.list() {
        for b in &buckets {
            if !bucket_available(&usage, &d.limits, b, pt, 0) {
                return Err(DomainError::QuotaExceeded(QuotaScope::Tokens));
            }
        }
    }
    Ok(())
}

/// Settlement of one turn applied to its bucket rows.
pub struct SettleInput {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub periods: Periods,
    pub premium: bool,
    pub reserved_credits_micro: i64,
    pub settlement: Settlement,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

/// Applies a settlement to the bucket rows (inside the finalization transaction).
///
/// # Errors
/// Database errors.
pub async fn apply_settlement(
    runner: &impl DBRunner,
    s: &SettleInput,
    now: OffsetDateTime,
) -> DomainResult<()> {
    let tool_calls = s.settlement.method != SettlementMethod::Released;
    for (pt, ps) in s.periods.list() {
        repo::ensure_quota_row(runner, s.tenant_id, s.user_id, pt, ps, BUCKET_TOTAL, now).await?;
        repo::bump_quota_row(
            runner,
            s.tenant_id,
            s.user_id,
            pt,
            ps,
            BUCKET_TOTAL,
            &QuotaDeltas {
                reserved: -s.reserved_credits_micro,
                spent: s.settlement.committed_credits_micro,
                calls: 1,
                input_tokens: s.settlement.telemetry_input_tokens,
                output_tokens: s.settlement.telemetry_output_tokens,
                web_search_calls: if tool_calls { s.web_search_calls } else { 0 },
                code_interpreter_calls: if tool_calls {
                    s.code_interpreter_calls
                } else {
                    0
                },
            },
            now,
        )
        .await?;
        if s.premium {
            repo::ensure_quota_row(runner, s.tenant_id, s.user_id, pt, ps, BUCKET_PREMIUM, now)
                .await?;
            repo::bump_quota_row(
                runner,
                s.tenant_id,
                s.user_id,
                pt,
                ps,
                BUCKET_PREMIUM,
                &QuotaDeltas {
                    reserved: -s.reserved_credits_micro,
                    spent: s.settlement.committed_credits_micro,
                    calls: 1,
                    ..QuotaDeltas::default()
                },
                now,
            )
            .await?;
        }
    }
    Ok(())
}

impl AppServices {
    /// `GET /quota/status`.
    ///
    /// # Errors
    /// Authorization, policy and database errors.
    pub async fn quota_status(&self, ctx: &SecurityContext) -> DomainResult<Vec<TierStatus>> {
        let scope = authz::quota_scope(&self.enforcer, ctx).await?;
        let plugin = self.policy.plugin().await?;
        let version = plugin
            .get_current_policy_version(ctx.subject_id())
            .await
            .map_err(|e| DomainError::internal(format!("policy version: {e}")))?;
        let limits = self
            .policy
            .user_limits(ctx.subject_id(), version.policy_version)
            .await?;
        let now = crate::domain::time::now();
        let periods = Periods::at(now);
        let conn = self.db.conn()?;
        let rows = repo::quota_rows(
            &conn,
            &scope,
            ctx.subject_tenant_id(),
            ctx.subject_id(),
            &periods.list(),
        )
        .await?;
        let usage = usage_map(&rows, periods);
        Ok(compute_status(
            &limits,
            &usage,
            now,
            self.cfg.quota.warning_threshold_pct,
        ))
    }
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod quota_tests;
