//! Quota status (DESIGN §3.2 "Quota Warning Thresholds", "Quota Status Endpoint").
//!
//! [`compute_tier_status`] is the single computation behind both
//! `GET /v1/quota/status` and the SSE `done.quota_warnings`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use mini_chat_sdk::{TierLimits, UserLimits};
use toolkit_db::DBProvider;
use toolkit_security::SecurityContext;

use crate::config::MiniChatConfig;
use crate::domain::authz::ChatAuthz;
use crate::domain::clock::now_utc;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::estimation::{next_reset, period_starts};
use crate::domain::model::{Bucket, PeriodType};
use crate::infra::db::entities::quota_usage;
use crate::infra::db::repos::QuotaRepo;
use crate::infra::gateways::model_policy::ModelPolicyGateway;

/// Quota tier of the status response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaTier {
    Premium,
    Total,
}

impl QuotaTier {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Premium => "premium",
            Self::Total => "total",
        }
    }

    const fn bucket(self) -> Bucket {
        match self {
            Self::Premium => Bucket::TierPremium,
            Self::Total => Bucket::Total,
        }
    }

    const fn limits(self, limits: &UserLimits) -> TierLimits {
        match self {
            Self::Premium => limits.premium,
            Self::Total => limits.standard,
        }
    }
}

/// One period of one tier. Credit values are in micro-credits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodStatus {
    pub period: PeriodType,
    pub limit: i64,
    /// `spent + reserved` (conservative).
    pub used: i64,
    pub remaining: i64,
    /// Floor of `remaining * 100 / limit`, in `0..=100`.
    pub remaining_percentage: u8,
    pub next_reset: DateTime<Utc>,
    pub warning: bool,
    pub exhausted: bool,
}

/// The periods of one tier whose limit is positive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierStatus {
    pub tier: QuotaTier,
    pub periods: Vec<PeriodStatus>,
}

/// Response of the quota status endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaStatus {
    pub tiers: Vec<TierStatus>,
    pub warning_threshold_pct: u8,
}

/// Per-tier, per-period status at `now` (tiers `premium` then `total`, periods
/// `daily` then `monthly`). Only the rows of the current UTC periods count;
/// periods whose limit is `<= 0` are skipped, and a tier left without periods
/// is omitted. `warning` fires when
/// `remaining_percentage <= 100 - warning_threshold_pct`, `exhausted` when it is 0.
#[must_use]
pub fn compute_tier_status(
    limits: &UserLimits,
    rows: &[quota_usage::Model],
    now: DateTime<Utc>,
    warning_threshold_pct: u8,
) -> Vec<TierStatus> {
    let (daily_start, monthly_start) = period_starts(now);
    [QuotaTier::Premium, QuotaTier::Total]
        .into_iter()
        .filter_map(|tier| {
            let tier_limits = tier.limits(limits);
            let periods = [
                (
                    PeriodType::Daily,
                    daily_start,
                    tier_limits.limit_daily_credits_micro,
                ),
                (
                    PeriodType::Monthly,
                    monthly_start,
                    tier_limits.limit_monthly_credits_micro,
                ),
            ]
            .into_iter()
            .filter(|(_, _, limit)| *limit > 0)
            .map(|(period, start, limit)| {
                let used = rows
                    .iter()
                    .filter(|r| {
                        r.bucket == tier.bucket().as_str()
                            && r.period_type == period.as_str()
                            && r.period_start == start
                    })
                    .fold(0_i64, |acc, r| {
                        acc.saturating_add(r.spent_credits_micro)
                            .saturating_add(r.reserved_credits_micro)
                    });
                period_status(period, limit, used, now, warning_threshold_pct)
            })
            .collect::<Vec<_>>();
            // A tier without any positive limit is not reported at all.
            (!periods.is_empty()).then_some(TierStatus { tier, periods })
        })
        .collect()
}

fn period_status(
    period: PeriodType,
    limit: i64,
    used: i64,
    now: DateTime<Utc>,
    warning_threshold_pct: u8,
) -> PeriodStatus {
    let remaining = limit.saturating_sub(used).max(0);
    #[allow(
        clippy::integer_division,
        reason = "the percentage is floored by definition"
    )]
    let pct = (i128::from(remaining) * 100 / i128::from(limit)).clamp(0, 100);
    let remaining_percentage = u8::try_from(pct).unwrap_or(100);
    PeriodStatus {
        period,
        limit,
        used,
        remaining,
        remaining_percentage,
        next_reset: next_reset(period, now),
        warning: remaining_percentage <= 100_u8.saturating_sub(warning_threshold_pct),
        exhausted: remaining_percentage == 0,
    }
}

pub struct QuotaStatusService {
    config: Arc<MiniChatConfig>,
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<ChatAuthz>,
    policy: Arc<dyn ModelPolicyGateway>,
}

impl QuotaStatusService {
    #[must_use]
    pub fn new(
        config: Arc<MiniChatConfig>,
        db: Arc<DBProvider<DomainError>>,
        authz: Arc<ChatAuthz>,
        policy: Arc<dyn ModelPolicyGateway>,
    ) -> Self {
        Self {
            config,
            db,
            authz,
            policy,
        }
    }

    /// Quota status of the caller at the current UTC time.
    ///
    /// # Errors
    /// Authorization errors, policy plugin and database failures.
    pub async fn status(&self, ctx: &SecurityContext) -> DomainResult<QuotaStatus> {
        let scope = self.authz.quota_scope(ctx).await?;
        let user_id = ctx.subject_id();
        let snapshot = self.policy.current_snapshot(user_id).await?;
        let limits = self
            .policy
            .user_limits(user_id, snapshot.policy_version)
            .await?;
        let now = now_utc();
        let (daily, monthly) = period_starts(now);
        let rows =
            QuotaRepo::rows_in_scope(&self.db.conn()?, &scope, user_id, daily, monthly).await?;
        let warning_threshold_pct = self.config.quota.warning_threshold_pct;
        Ok(QuotaStatus {
            tiers: compute_tier_status(&limits, &rows, now, warning_threshold_pct),
            warning_threshold_pct,
        })
    }
}

#[cfg(test)]
#[path = "quota_status_tests.rs"]
mod quota_status_tests;
