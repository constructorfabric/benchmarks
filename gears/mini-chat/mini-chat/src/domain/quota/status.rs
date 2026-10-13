//! Quota status and warnings (DESIGN §3.2 "Quota Warning Thresholds", "Quota Status Endpoint").

use mini_chat_sdk::UserLimits;
use time::OffsetDateTime;
use toolkit_security::SecurityContext;

use super::arith::{next_daily_reset, next_monthly_reset};
use super::buckets::{self, Rows, limit_of};
use super::{BUCKET_PREMIUM, BUCKET_TOTAL, PERIOD_DAILY, PERIOD_MONTHLY, QuotaWarning};
use crate::domain::error::DomainError;
use crate::domain::services::AppServices;

/// Tier names reported to clients with their buckets.
const TIERS: [(&str, &str); 2] = [("premium", BUCKET_PREMIUM), ("total", BUCKET_TOTAL)];

/// One period of a tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodStatus {
    /// `daily` | `monthly`
    pub period: &'static str,
    pub limit_credits_micro: i64,
    pub used_credits_micro: i64,
    pub remaining_credits_micro: i64,
    pub remaining_percentage: u32,
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

/// One tier (`premium` | `total`) with its non-skipped periods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierStatus {
    pub tier: &'static str,
    pub periods: Vec<PeriodStatus>,
}

/// Quota status of a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaStatus {
    pub tiers: Vec<TierStatus>,
    pub warning_threshold_pct: u8,
}

fn clamp_i64(v: i128) -> i64 {
    i64::try_from(v).unwrap_or(if v < 0 { i64::MIN } else { i64::MAX })
}

/// Per-period figures; `None` when the limit is `<= 0` (period skipped).
fn period_status(
    rows: &Rows,
    limits: &UserLimits,
    bucket: &str,
    period: &'static str,
    now: OffsetDateTime,
    threshold_pct: u8,
) -> Option<PeriodStatus> {
    let limit = limit_of(limits, bucket, period);
    if limit <= 0 {
        return None;
    }
    let used = rows.used(period, bucket);
    let remaining = (i128::from(limit) - used).max(0);
    let pct = (remaining * 100).div_euclid(i128::from(limit)).clamp(0, 100);
    let remaining_percentage = u32::try_from(pct).unwrap_or(0);
    let warning = remaining_percentage <= 100 - u32::from(threshold_pct.min(100));
    let next_reset = if period == PERIOD_DAILY { next_daily_reset(now) } else { next_monthly_reset(now) };
    Some(PeriodStatus {
        period,
        limit_credits_micro: limit,
        used_credits_micro: clamp_i64(used),
        remaining_credits_micro: clamp_i64(remaining),
        remaining_percentage,
        next_reset,
        warning,
        exhausted: remaining_percentage == 0,
    })
}

/// Status of every tier / period from loaded rows.
pub(super) fn compute(rows: &Rows, limits: &UserLimits, now: OffsetDateTime, threshold_pct: u8) -> Vec<TierStatus> {
    TIERS
        .iter()
        .filter_map(|(tier, bucket)| {
            let periods: Vec<PeriodStatus> = [PERIOD_DAILY, PERIOD_MONTHLY]
                .into_iter()
                .filter_map(|p| period_status(rows, limits, bucket, p, now, threshold_pct))
                .collect();
            (!periods.is_empty()).then_some(TierStatus { tier, periods })
        })
        .collect()
}

pub(super) fn warnings_of(tiers: &[TierStatus]) -> Vec<QuotaWarning> {
    tiers
        .iter()
        .flat_map(|t| {
            t.periods.iter().map(|p| QuotaWarning {
                tier: t.tier.to_owned(),
                period: p.period.to_owned(),
                remaining_percentage: p.remaining_percentage,
                warning: p.warning,
                exhausted: p.exhausted,
                next_reset: (p.warning || p.exhausted).then_some(p.next_reset),
            })
        })
        .collect()
}

/// `GET /quota/status` for the caller (PEP `USER_QUOTA` / `read`).
///
/// # Errors
/// Authorization, policy plugin or DB errors.
pub async fn quota_status(app: &AppServices, ctx: &SecurityContext) -> Result<QuotaStatus, DomainError> {
    let scope = app.authz.quota_scope(ctx).await?;
    let (tenant_id, user_id) = (ctx.subject_tenant_id(), ctx.subject_id());
    let snapshot = app.policy.current_snapshot(user_id).await?;
    let limits = app.policy.user_limits(user_id, snapshot.policy_version).await?;
    let now = crate::clock::now();
    let periods = super::period_starts(now);
    let conn = app.db.conn()?;
    let rows = buckets::load_rows_scoped(&conn, &scope, tenant_id, user_id, periods).await?;
    let threshold = app.cfg.quota.warning_threshold_pct;
    Ok(QuotaStatus { tiers: compute(&rows, &limits, now, threshold), warning_threshold_pct: threshold })
}
