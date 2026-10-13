//! Quota status and `quota_warnings` (DESIGN section 3.2 "Quota Warning Thresholds",
//! "Quota Status Endpoint").

use mini_chat_sdk::UserLimits;
use serde::Serialize;
use time::OffsetDateTime;
use toolkit_db::secure::DBRunner;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::periods::{Bucket, Period, next_reset, period_starts};
use super::{QuotaService, Usage, limit_of};
use crate::domain::error::DomainError;
use crate::infra::db::repo::quota_usage as repo;

/// One tier × period entry of the SSE `done` event's `quota_warnings`; serializes as the
/// `QuotaWarning` schema (`tier` `premium|total`, `next_reset` RFC 3339, omitted when `None`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuotaWarning {
    /// The tier is named after the bucket: `tier:premium` -> `premium`, `total` -> `total`.
    #[serde(serialize_with = "serialize_tier")]
    pub tier: Bucket,
    pub period: Period,
    pub remaining_percentage: u8,
    pub warning: bool,
    pub exhausted: bool,
    /// Set only when `warning || exhausted`.
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub next_reset: Option<OffsetDateTime>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // signature required by `serialize_with`
fn serialize_tier<S: serde::Serializer>(tier: &Bucket, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(tier.tier_name())
}

/// Quota status of one period of a tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodStatus {
    pub period: Period,
    pub limit_credits_micro: i64,
    /// `spent + reserved`.
    pub used_credits_micro: i64,
    /// `max(limit - used, 0)`.
    pub remaining_credits_micro: i64,
    pub remaining_percentage: u8,
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

/// Quota status of one tier (`premium`, then `total`); periods with a limit `<= 0` are omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierStatus {
    /// `Premium` (tier `premium`) or `Total` (tier `total`).
    pub tier: Bucket,
    pub periods: Vec<PeriodStatus>,
}

/// Result of [`super::QuotaService::status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaStatus {
    pub tiers: Vec<TierStatus>,
    pub warning_threshold_pct: u8,
}

impl QuotaService {
    /// Quota status of the caller (`GET /v1/quota/status`): rows read with the PDP's `UserQuota`
    /// scope, limits of the user's current policy version.
    ///
    /// # Errors
    /// `AccessDenied` / `AuthzUnavailable` from the PDP, `Internal` on a policy or database error.
    pub async fn status(&self, ctx: &SecurityContext) -> Result<QuotaStatus, DomainError> {
        let scope = self.authz.quota_scope(ctx).await?;
        let user_id = ctx.subject_id();
        let snapshot = self.policy.current_snapshot(user_id).await?;
        let limits = self
            .policy
            .user_limits(user_id, snapshot.policy_version)
            .await?;
        let now = OffsetDateTime::now_utc();
        let conn = self.db.conn()?;
        let rows = repo::load_current(&conn, &scope, &period_starts(now), false).await?;
        Ok(QuotaStatus {
            tiers: tier_statuses(&Usage(rows), &limits, now, self.cfg.warning_threshold_pct),
            warning_threshold_pct: self.cfg.warning_threshold_pct,
        })
    }

    /// `quota_warnings` of the user: every tier × period with a limit `> 0`, premium first;
    /// `next_reset` only on entries with `warning` or `exhausted`.
    ///
    /// # Errors
    /// `Internal` on a database error.
    pub async fn warnings(
        &self,
        conn: &impl DBRunner,
        tenant_id: Uuid,
        user_id: Uuid,
        limits: &UserLimits,
        now: OffsetDateTime,
    ) -> Result<Vec<QuotaWarning>, DomainError> {
        let scope = repo::owner_scope(tenant_id, user_id);
        let rows = repo::load_current(conn, &scope, &period_starts(now), false).await?;
        let tiers = tier_statuses(&Usage(rows), limits, now, self.cfg.warning_threshold_pct);
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
}

/// Tiers `premium` (bucket `tier:premium`) and `total`, each with its periods whose limit is
/// `> 0`.
fn tier_statuses(
    usage: &Usage,
    limits: &UserLimits,
    now: OffsetDateTime,
    warning_threshold_pct: u8,
) -> Vec<TierStatus> {
    [Bucket::Premium, Bucket::Total]
        .into_iter()
        .map(|bucket| TierStatus {
            tier: bucket,
            periods: Period::ALL
                .into_iter()
                .filter_map(|period| {
                    let limit = limit_of(limits, bucket, period);
                    (limit > 0).then(|| {
                        period_status(
                            period,
                            limit,
                            usage.used(period, bucket),
                            now,
                            warning_threshold_pct,
                        )
                    })
                })
                .collect(),
        })
        .collect()
}

/// `remaining_percentage = floor(max(limit - used, 0) * 100 / limit)`; `warning` at or below
/// `100 - warning_threshold_pct`; `exhausted` at 0. `limit` must be positive.
fn period_status(
    period: Period,
    limit: i64,
    used: i64,
    now: OffsetDateTime,
    warning_threshold_pct: u8,
) -> PeriodStatus {
    let remaining = limit.saturating_sub(used).max(0);
    #[allow(clippy::integer_division)] // floored by definition
    let pct = i128::from(remaining) * 100 / i128::from(limit);
    let remaining_percentage = u8::try_from(pct.clamp(0, 100)).unwrap_or(100);
    PeriodStatus {
        period,
        limit_credits_micro: limit,
        used_credits_micro: used,
        remaining_credits_micro: remaining,
        remaining_percentage,
        next_reset: next_reset(period, now),
        warning: remaining_percentage <= 100u8.saturating_sub(warning_threshold_pct),
        exhausted: remaining_percentage == 0,
    }
}
