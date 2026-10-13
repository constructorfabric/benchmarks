//! Quota policy: period math, the downgrade cascade with per-candidate
//! reserve checks, tool quotas, warnings and the status projection
//! (DESIGN §3.2 quota service, §4 "Downgrade Decision Flow", §5.4).

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, TierLimits, UserLimits};
use time::{Date, Month, OffsetDateTime, Time};

use crate::domain::credits::{ReserveEstimate, SurchargeFlags, reserve_for};

pub const BUCKET_TOTAL: &str = "total";
pub const BUCKET_PREMIUM: &str = "tier:premium";
pub const PERIOD_DAILY: &str = "daily";
pub const PERIOD_MONTHLY: &str = "monthly";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Periods {
    pub daily: Date,
    pub monthly: Date,
}

impl Periods {
    #[must_use]
    pub fn at(now: OffsetDateTime) -> Self {
        let utc = now.to_offset(time::UtcOffset::UTC);
        let daily = utc.date();
        #[allow(clippy::unwrap_used)]
        let monthly = Date::from_calendar_date(daily.year(), daily.month(), 1).unwrap();
        Self { daily, monthly }
    }

    /// Daily/monthly period starts derived from a turn's `started_at`
    /// (orphan watchdog path).
    #[must_use]
    pub fn from_started_at(started_at: OffsetDateTime) -> Self {
        Self::at(started_at)
    }

    #[must_use]
    pub fn start(&self, period: &str) -> Date {
        if period == PERIOD_MONTHLY { self.monthly } else { self.daily }
    }
}

/// Next reset instant for a period, relative to `now` (UTC midnight).
#[must_use]
pub fn next_reset(period: &str, now: OffsetDateTime) -> OffsetDateTime {
    let today = now.to_offset(time::UtcOffset::UTC).date();
    let date = if period == PERIOD_MONTHLY {
        let (y, m) = if today.month() == Month::December {
            (today.year() + 1, Month::January)
        } else {
            (today.year(), today.month().next())
        };
        #[allow(clippy::unwrap_used)]
        Date::from_calendar_date(y, m, 1).unwrap()
    } else {
        today.next_day().unwrap_or(today)
    };
    date.with_time(Time::MIDNIGHT).assume_utc()
}

/// Usage counters of one bucket row (zero when the row does not exist).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketUsage {
    pub spent: i64,
    pub reserved: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

/// The four rows read at preflight.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageSnapshot {
    pub total_daily: BucketUsage,
    pub total_monthly: BucketUsage,
    pub premium_daily: BucketUsage,
    pub premium_monthly: BucketUsage,
}

impl UsageSnapshot {
    #[must_use]
    pub fn get(&self, bucket: &str, period: &str) -> BucketUsage {
        match (bucket == BUCKET_PREMIUM, period == PERIOD_MONTHLY) {
            (false, false) => self.total_daily,
            (false, true) => self.total_monthly,
            (true, false) => self.premium_daily,
            (true, true) => self.premium_monthly,
        }
    }
}

#[must_use]
pub fn limit_for(limits: &UserLimits, bucket: &str, period: &str) -> i64 {
    let t: &TierLimits = if bucket == BUCKET_PREMIUM { &limits.premium } else { &limits.standard };
    if period == PERIOD_MONTHLY { t.limit_monthly_credits_micro } else { t.limit_daily_credits_micro }
}

/// Buckets that a turn on `tier` books against.
#[must_use]
pub fn buckets_for(tier: ModelTier) -> &'static [&'static str] {
    match tier {
        ModelTier::Premium => &[BUCKET_TOTAL, BUCKET_PREMIUM],
        ModelTier::Standard => &[BUCKET_TOTAL],
    }
}

fn bucket_available(usage: BucketUsage, limit: i64, this_reserve: i64) -> bool {
    usage
        .spent
        .saturating_add(usage.reserved)
        .saturating_add(this_reserve)
        <= limit
}

/// `true` when `reserve` fits in every bucket and period required by `tier`.
#[must_use]
pub fn tier_available(tier: ModelTier, usage: &UsageSnapshot, limits: &UserLimits, reserve: i64) -> bool {
    for bucket in buckets_for(tier) {
        for period in [PERIOD_DAILY, PERIOD_MONTHLY] {
            if !bucket_available(usage.get(bucket, period), limit_for(limits, bucket, period), reserve) {
                return false;
            }
        }
    }
    true
}

/// Facts about the request that influence the reserve estimate.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct RequestFacts {
    pub message: String,
    pub prior_context_tokens: i64,
    pub image_count: usize,
    pub web_search_requested: bool,
    pub chat_has_ready_documents: bool,
    pub chat_has_ready_code_files: bool,
    pub cfg_max_output_tokens: u32,
}

/// Tools that would be sent to `model` under the kill switches.
#[must_use]
pub fn tool_flags(model: &ModelCatalogEntry, facts: &RequestFacts, ks: &KillSwitches) -> SurchargeFlags {
    let ts = &model.general_config.tool_support;
    SurchargeFlags {
        file_search: facts.chat_has_ready_documents && ts.file_search && !ks.disable_file_search,
        web_search: facts.web_search_requested && ts.web_search,
        code_interpreter: facts.chat_has_ready_code_files
            && ts.code_interpreter
            && !ks.disable_code_interpreter,
    }
}

#[must_use]
pub fn candidate_reserve(model: &ModelCatalogEntry, facts: &RequestFacts, ks: &KillSwitches) -> ReserveEstimate {
    reserve_for(
        &facts.message,
        facts.prior_context_tokens,
        facts.image_count,
        tool_flags(model, facts, ks),
        &model.estimation_budgets,
        model.max_output_tokens,
        facts.cfg_max_output_tokens,
        model.input_tokens_credit_multiplier_micro,
        model.output_tokens_credit_multiplier_micro,
    )
}

/// Outcome of the cascade.
#[derive(Debug, Clone)]
pub struct CascadeDecision {
    pub effective: ModelCatalogEntry,
    pub downgraded: bool,
    pub downgrade_reason: Option<String>,
    pub reserve: ReserveEstimate,
    pub tools: SurchargeFlags,
}

impl CascadeDecision {
    #[must_use]
    pub fn quota_decision(&self) -> &'static str {
        if self.downgraded { "downgrade" } else { "allow" }
    }
}

fn candidate_for<'a>(snap: &'a PolicySnapshot, tier: ModelTier, selected: &str) -> Option<&'a ModelCatalogEntry> {
    let enabled = || snap.model_catalog.iter().filter(move |m| m.enabled && m.tier == tier);
    enabled()
        .find(|m| m.id == selected)
        .or_else(|| enabled().find(|m| m.is_default()))
        .or_else(|| enabled().next())
}

/// Run the downgrade cascade. `None` means every tier is unavailable
/// (429 `quota_exceeded`, scope `tokens`).
#[must_use]
pub fn resolve_effective_model(
    selected: &str,
    snap: &PolicySnapshot,
    limits: &UserLimits,
    usage: &UsageSnapshot,
    facts: &RequestFacts,
) -> Option<CascadeDecision> {
    let ks = &snap.kill_switches;
    let mut reason: Option<String> = None;
    let start_tier = match snap.find_model(selected) {
        Some(m) if m.enabled => m.tier,
        Some(m) => {
            reason = Some("model_disabled".into());
            m.tier
        }
        None => {
            reason = Some("model_disabled".into());
            ModelTier::Premium
        }
    };
    let cascade: &[ModelTier] = match start_tier {
        ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
        ModelTier::Standard => &[ModelTier::Standard],
    };
    for &tier in cascade {
        if tier == ModelTier::Premium {
            if ks.force_standard_tier {
                reason.get_or_insert_with(|| "force_standard_tier".into());
                continue;
            }
            if ks.disable_premium_tier {
                reason.get_or_insert_with(|| "disable_premium_tier".into());
                continue;
            }
        }
        let Some(candidate) = candidate_for(snap, tier, selected) else {
            continue;
        };
        let reserve = candidate_reserve(candidate, facts, ks);
        if reserve.reserved_credits_micro == i64::MAX {
            tracing::warn!(model = %candidate.id, "cascade candidate reserve cannot be computed");
        }
        if reserve.reserved_credits_micro != i64::MAX
            && tier_available(tier, usage, limits, reserve.reserved_credits_micro)
        {
            let downgraded = candidate.id != selected || reason.is_some();
            return Some(CascadeDecision {
                effective: candidate.clone(),
                downgraded,
                downgrade_reason: if downgraded { reason } else { None },
                reserve,
                tools: tool_flags(candidate, facts, ks),
            });
        }
        if tier == ModelTier::Premium {
            reason.get_or_insert_with(|| "premium_quota_exhausted".into());
        }
    }
    None
}

/// One tier/period entry of the status API or `done.quota_warnings`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodStatus {
    pub tier: &'static str,
    pub period: &'static str,
    pub limit: i64,
    pub used: i64,
    pub remaining: i64,
    pub remaining_percentage: i64,
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

/// Per-tier, per-period quota status (periods with limit <= 0 are skipped).
#[must_use]
pub fn quota_status(
    usage: &UsageSnapshot,
    limits: &UserLimits,
    warning_threshold_pct: u8,
    now: OffsetDateTime,
) -> Vec<PeriodStatus> {
    let mut out = Vec::new();
    for (tier, bucket) in [("premium", BUCKET_PREMIUM), ("total", BUCKET_TOTAL)] {
        for period in [PERIOD_DAILY, PERIOD_MONTHLY] {
            let limit = limit_for(limits, bucket, period);
            if limit <= 0 {
                continue;
            }
            let u = usage.get(bucket, period);
            let used = u.spent.saturating_add(u.reserved).max(0);
            let remaining = limit.saturating_sub(used).max(0);
            let pct = i64::try_from(i128::from(remaining) * 100 / i128::from(limit)).unwrap_or(0);
            out.push(PeriodStatus {
                tier,
                period,
                limit,
                used,
                remaining,
                remaining_percentage: pct,
                next_reset: next_reset(period, now),
                warning: pct <= 100 - i64::from(warning_threshold_pct),
                exhausted: pct == 0,
            });
        }
    }
    out
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod quota_tests;
