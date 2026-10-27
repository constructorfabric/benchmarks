//! Shared per-tier/per-period quota status computation (`GET
//! /v1/quota/status` and the SSE `done.quota_warnings` field.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::api::rest::dto::QuotaWarning;
use crate::domain::services::{ServiceContext, periods};
use crate::error::DomainError;
use crate::infra::storage::repo::quota::{BucketKey, bucket, period};

/// A per-bucket, per-period quota reading.
#[derive(Debug, Clone, Copy)]
pub struct TierPeriodStatus {
    /// Tier label (`premium` or `total`).
    pub tier: crate::api::rest::dto::QuotaTier,
    /// Period.
    pub period: crate::api::rest::dto::QuotaPeriod,
    /// Limit.
    pub limit_credits_micro: i64,
    /// Used (spent + reserved).
    pub used_credits_micro: i64,
    /// Remaining.
    pub remaining_credits_micro: i64,
    /// Floored remaining percentage (0–100).
    pub remaining_percentage: u32,
    /// Next reset.
    pub next_reset: OffsetDateTime,
    /// Warning flag.
    pub warning: bool,
    /// Exhausted flag.
    pub exhausted: bool,
}

/// Compute the per-tier/per-period status of a user.
pub async fn compute(
    ctx: &ServiceContext,
    tenant_id: Uuid,
    user_id: Uuid,
    limits: &mini_chat_sdk::UserLimits,
    now: OffsetDateTime,
) -> Result<Vec<TierPeriodStatus>, DomainError> {
    let keys = [
        BucketKey {
            period_type: period::DAILY,
            period_start: periods::daily_start(now),
            bucket: bucket::TOTAL,
        },
        BucketKey {
            period_type: period::MONTHLY,
            period_start: periods::monthly_start(now),
            bucket: bucket::TOTAL,
        },
        BucketKey {
            period_type: period::DAILY,
            period_start: periods::daily_start(now),
            bucket: bucket::PREMIUM,
        },
        BucketKey {
            period_type: period::MONTHLY,
            period_start: periods::monthly_start(now),
            bucket: bucket::PREMIUM,
        },
    ];
    let states = ctx
        .repos
        .quota
        .read_buckets(
            &ctx.db.conn()?,
            &toolkit_db::secure::AccessScope::allow_all(),
            tenant_id,
            user_id,
            &keys,
        )
        .await?;
    let threshold = u32::from(ctx.config.quota.warning_threshold_pct);
    let mut out = Vec::new();
    for (tier, bucket_name, tier_limits) in [
        (
            crate::api::rest::dto::QuotaTier::Premium,
            bucket::PREMIUM,
            limits.premium,
        ),
        (
            crate::api::rest::dto::QuotaTier::Total,
            bucket::TOTAL,
            limits.standard,
        ),
    ] {
        for (period_kind, ptype, pstart, limit, next_reset) in [
            (
                crate::api::rest::dto::QuotaPeriod::Daily,
                period::DAILY,
                periods::daily_start(now),
                tier_limits.limit_daily_credits_micro,
                periods::daily_next_reset(now),
            ),
            (
                crate::api::rest::dto::QuotaPeriod::Monthly,
                period::MONTHLY,
                periods::monthly_start(now),
                tier_limits.limit_monthly_credits_micro,
                periods::monthly_next_reset(now),
            ),
        ] {
            if limit <= 0 {
                continue;
            }
            let state = states
                .iter()
                .find(|(k, _)| k.period_type == ptype && k.period_start == pstart && k.bucket == bucket_name)
                .map(|(_, s)| *s)
                .unwrap_or_default();
            let used = state.spent_credits_micro + state.reserved_credits_micro;
            let remaining = (limit - used).max(0);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let remaining_percentage = ((remaining * 100) / limit) as u32;
            let warning = remaining_percentage <= 100 - threshold;
            let exhausted = remaining_percentage == 0;
            out.push(TierPeriodStatus {
                tier,
                period: period_kind,
                limit_credits_micro: limit,
                used_credits_micro: used,
                remaining_credits_micro: remaining,
                remaining_percentage,
                next_reset,
                warning,
                exhausted,
            });
        }
    }
    Ok(out)
}

/// Project the status list into SSE `quota_warnings` entries (`next_reset`
/// only when warning or exhausted).
#[must_use]
pub fn to_warnings(statuses: &[TierPeriodStatus]) -> Vec<QuotaWarning> {
    statuses
        .iter()
        .map(|s| QuotaWarning {
            tier: s.tier,
            period: s.period,
            remaining_percentage: s.remaining_percentage,
            warning: s.warning,
            exhausted: s.exhausted,
            next_reset: (s.warning || s.exhausted).then_some(s.next_reset),
        })
        .collect()
}
