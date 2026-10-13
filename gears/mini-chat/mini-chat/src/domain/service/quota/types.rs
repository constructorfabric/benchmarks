//! Shared vocabulary of the quota service: periods, buckets, tiers and the
//! bucket row as the domain sees it.

use mini_chat_sdk::UserLimits;
use toolkit_macros::domain_model;

/// A quota period (`quota_usage.period_type`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum QuotaPeriod {
    /// UTC calendar day.
    Daily,
    /// UTC calendar month.
    Monthly,
}

impl QuotaPeriod {
    /// Both periods in the canonical order (daily before monthly).
    pub const ALL: [Self; 2] = [Self::Daily, Self::Monthly];

    /// `quota_usage.period_type` and metric label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }
}

/// A quota bucket (`quota_usage.bucket`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum QuotaBucket {
    /// Overall cap across all tiers (`user_limits.standard`).
    Total,
    /// Premium sub-cap (`user_limits.premium`).
    TierPremium,
}

impl QuotaBucket {
    /// Both buckets in the canonical order (`total` before `tier:premium`).
    pub const ALL: [Self; 2] = [Self::Total, Self::TierPremium];

    /// `quota_usage.bucket` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::TierPremium => "tier:premium",
        }
    }
}

/// Tier as shown by the quota status (`QuotaTier` of the `OpenAPI`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaTier {
    /// Premium sub-cap (bucket `tier:premium`).
    Premium,
    /// Overall cap (bucket `total`).
    Total,
}

impl QuotaTier {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Premium => "premium",
            Self::Total => "total",
        }
    }

    /// The bucket the tier is enforced on.
    #[must_use]
    pub const fn bucket(self) -> QuotaBucket {
        match self {
            Self::Premium => QuotaBucket::TierPremium,
            Self::Total => QuotaBucket::Total,
        }
    }
}

/// One `quota_usage` row of the current user, reduced to the columns the
/// domain decides on. A bucket without a row counts as all zeros.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketRow {
    /// Period of the row.
    pub period: QuotaPeriod,
    /// Bucket of the row.
    pub bucket: QuotaBucket,
    /// `spent_credits_micro`.
    pub spent_credits_micro: i64,
    /// `reserved_credits_micro`.
    pub reserved_credits_micro: i64,
    /// `web_search_calls`.
    pub web_search_calls: i64,
    /// `code_interpreter_calls`.
    pub code_interpreter_calls: i64,
}

impl BucketRow {
    /// A row with the given credit counters and zero tool counters.
    #[must_use]
    pub const fn new(
        period: QuotaPeriod,
        bucket: QuotaBucket,
        spent_credits_micro: i64,
        reserved_credits_micro: i64,
    ) -> Self {
        Self {
            period,
            bucket,
            spent_credits_micro,
            reserved_credits_micro,
            web_search_calls: 0,
            code_interpreter_calls: 0,
        }
    }

    /// The same row with tool counters.
    #[must_use]
    pub const fn with_tool_calls(mut self, web_search: i64, code_interpreter: i64) -> Self {
        self.web_search_calls = web_search;
        self.code_interpreter_calls = code_interpreter;
        self
    }
}

/// The limit of `bucket` and `period`: bucket `total` uses
/// `user_limits.standard`, bucket `tier:premium` uses `user_limits.premium`.
#[must_use]
pub const fn limit_of(limits: &UserLimits, bucket: QuotaBucket, period: QuotaPeriod) -> i64 {
    let tier = match bucket {
        QuotaBucket::Total => &limits.standard,
        QuotaBucket::TierPremium => &limits.premium,
    };
    match period {
        QuotaPeriod::Daily => tier.limit_daily_credits_micro,
        QuotaPeriod::Monthly => tier.limit_monthly_credits_micro,
    }
}

/// The row of `(period, bucket)`, if any.
#[must_use]
pub fn find_row(
    rows: &[BucketRow],
    period: QuotaPeriod,
    bucket: QuotaBucket,
) -> Option<&BucketRow> {
    rows.iter()
        .find(|row| row.period == period && row.bucket == bucket)
}
