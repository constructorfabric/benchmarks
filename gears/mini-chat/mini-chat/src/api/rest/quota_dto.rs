//! DTOs of the quota status endpoint and of `done.quota_warnings` (`OpenAPI`
//! names `QuotaStatusResponse`, `QuotaTierStatus`, `QuotaPeriodStatus`,
//! `QuotaWarning`, `QuotaTier`, `QuotaPeriod`).

use time::OffsetDateTime;

use crate::domain::quota::{
    Period, PeriodStatus, QuotaTier as DomainTier, QuotaWarning as DomainWarning, TierStatus,
};
use crate::domain::quota_status::QuotaStatusView;

/// Quota tier classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaTier {
    /// The premium sub-cap.
    Premium,
    /// The overall cap across every tier.
    Total,
}

/// Quota period classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaPeriod {
    /// UTC calendar day.
    Daily,
    /// UTC calendar month.
    Monthly,
}

/// One period of a tier.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
#[allow(clippy::struct_excessive_bools, reason = "mirrors the OpenAPI schema")]
pub struct QuotaPeriodStatus {
    /// The period.
    pub period: QuotaPeriod,
    /// Limit in micro-credits.
    pub limit_credits_micro: i64,
    /// `spent + reserved` in micro-credits.
    pub used_credits_micro: i64,
    /// `max(0, limit - used)` in micro-credits.
    pub remaining_credits_micro: i64,
    /// Floored remaining percentage (0-100).
    pub remaining_percentage: u32,
    /// When the period resets.
    #[serde(with = "time::serde::rfc3339")]
    pub next_reset: OffsetDateTime,
    /// `remaining_percentage <= 100 - warning_threshold_pct`.
    pub warning: bool,
    /// Less than 1 % left.
    pub exhausted: bool,
}

/// One tier of the status.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaTierStatus {
    /// The tier.
    pub tier: QuotaTier,
    /// Its periods, daily then monthly.
    pub periods: Vec<QuotaPeriodStatus>,
}

/// Response of `GET /v1/quota/status`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaStatusResponse {
    /// Per-tier status.
    pub tiers: Vec<QuotaTierStatus>,
    /// The configured warning threshold in percent.
    pub warning_threshold_pct: u32,
}

/// One entry of the `quota_warnings` array of the `done` event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaWarning {
    /// The tier.
    pub tier: QuotaTier,
    /// The period.
    pub period: QuotaPeriod,
    /// Floored remaining percentage (0-100).
    pub remaining_percentage: u32,
    /// Threshold crossed.
    pub warning: bool,
    /// Less than 1 % left.
    pub exhausted: bool,
    /// RFC 3339 reset time; present when `warning` or `exhausted`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub next_reset: Option<OffsetDateTime>,
}

impl From<DomainTier> for QuotaTier {
    fn from(tier: DomainTier) -> Self {
        match tier {
            DomainTier::Premium => Self::Premium,
            DomainTier::Total => Self::Total,
        }
    }
}

impl From<Period> for QuotaPeriod {
    fn from(period: Period) -> Self {
        match period {
            Period::Daily => Self::Daily,
            Period::Monthly => Self::Monthly,
        }
    }
}

impl From<PeriodStatus> for QuotaPeriodStatus {
    fn from(status: PeriodStatus) -> Self {
        Self {
            period: status.period.into(),
            limit_credits_micro: status.limit_credits_micro,
            used_credits_micro: status.used_credits_micro,
            remaining_credits_micro: status.remaining_credits_micro,
            remaining_percentage: u32::from(status.remaining_percentage),
            next_reset: status.next_reset,
            warning: status.warning,
            exhausted: status.exhausted,
        }
    }
}

impl From<TierStatus> for QuotaTierStatus {
    fn from(status: TierStatus) -> Self {
        Self {
            tier: status.tier.into(),
            periods: status.periods.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<QuotaStatusView> for QuotaStatusResponse {
    fn from(view: QuotaStatusView) -> Self {
        Self {
            tiers: view.tiers.into_iter().map(Into::into).collect(),
            warning_threshold_pct: u32::from(view.warning_threshold_pct),
        }
    }
}

impl From<DomainWarning> for QuotaWarning {
    fn from(warning: DomainWarning) -> Self {
        Self {
            tier: warning.tier.into(),
            period: warning.period.into(),
            remaining_percentage: u32::from(warning.remaining_percentage),
            warning: warning.warning,
            exhausted: warning.exhausted,
            next_reset: warning.next_reset,
        }
    }
}
