//! Quota status DTOs (`QuotaStatusResponse`, `QuotaTierStatus`, `QuotaPeriodStatus`,
//! `QuotaTier`, `QuotaPeriod`).

use time::OffsetDateTime;

use crate::domain::quota::{Bucket, Period, PeriodStatus, QuotaStatus, TierStatus};

// Doc comments of these DTOs become the published schema descriptions (`docs/api/api.json`);
// only the two enums carry one.

/// Quota tier classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaTier {
    Premium,
    Total,
}

/// Quota period classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaPeriod {
    Daily,
    Monthly,
}

#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaPeriodStatus {
    pub period: QuotaPeriod,
    pub limit_credits_micro: i64,
    pub used_credits_micro: i64,
    pub remaining_credits_micro: i64,
    pub remaining_percentage: u32,
    #[serde(with = "crate::api::dto::timestamp")]
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaTierStatus {
    pub tier: QuotaTier,
    pub periods: Vec<QuotaPeriodStatus>,
}

#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaStatusResponse {
    pub tiers: Vec<QuotaTierStatus>,
    pub warning_threshold_pct: u32,
}

impl From<Period> for QuotaPeriod {
    fn from(p: Period) -> Self {
        match p {
            Period::Daily => Self::Daily,
            Period::Monthly => Self::Monthly,
        }
    }
}

impl From<Bucket> for QuotaTier {
    fn from(b: Bucket) -> Self {
        match b {
            Bucket::Premium => Self::Premium,
            Bucket::Total => Self::Total,
        }
    }
}

impl From<PeriodStatus> for QuotaPeriodStatus {
    fn from(p: PeriodStatus) -> Self {
        Self {
            period: p.period.into(),
            limit_credits_micro: p.limit_credits_micro,
            used_credits_micro: p.used_credits_micro,
            remaining_credits_micro: p.remaining_credits_micro,
            remaining_percentage: u32::from(p.remaining_percentage),
            next_reset: p.next_reset,
            warning: p.warning,
            exhausted: p.exhausted,
        }
    }
}

impl From<TierStatus> for QuotaTierStatus {
    fn from(t: TierStatus) -> Self {
        Self {
            tier: t.tier.into(),
            periods: t.periods.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<QuotaStatus> for QuotaStatusResponse {
    fn from(s: QuotaStatus) -> Self {
        Self {
            tiers: s.tiers.into_iter().map(Into::into).collect(),
            warning_threshold_pct: u32::from(s.warning_threshold_pct),
        }
    }
}
