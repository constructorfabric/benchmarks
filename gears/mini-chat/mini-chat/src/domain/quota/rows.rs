//! `quota_usage` vocabulary: the two buckets, the row key and the counters the quota rules read.
//! Plain containers; the arithmetic over them lives in the rules and the ledger.

use std::collections::BTreeMap;

use time::Date;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::quota::periods::PeriodType;

/// The quota enforcement scope of a row (`quota_usage.bucket`). `tier:standard` rows are not
/// required for enforcement and are never written.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Bucket {
    /// The overall cap across all tiers, limited by `UserLimits::standard`.
    Total,
    /// The premium-only sub-cap, limited by `UserLimits::premium`.
    TierPremium,
}

impl Bucket {
    /// The `quota_usage.bucket` value: `total` or `tier:premium`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::TierPremium => "tier:premium",
        }
    }
}

/// The unique key of a `quota_usage` row.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketKey {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub period: PeriodType,
    pub period_start: Date,
    pub bucket: Bucket,
}

/// The counters of one bucket row; a missing row is all zeros.
#[domain_model]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketRow {
    pub spent_credits_micro: i64,
    pub reserved_credits_micro: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

/// The rows of one user for the current periods, keyed by period and bucket.
#[domain_model]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuotaRows {
    rows: BTreeMap<(PeriodType, Bucket), BucketRow>,
}

impl QuotaRows {
    /// No rows.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace the row of `(period, bucket)`.
    #[must_use]
    pub fn with(mut self, period: PeriodType, bucket: Bucket, row: BucketRow) -> Self {
        self.rows.insert((period, bucket), row);
        self
    }

    /// Add or replace the row of `(period, bucket)` in place.
    pub fn insert(&mut self, period: PeriodType, bucket: Bucket, row: BucketRow) {
        self.rows.insert((period, bucket), row);
    }

    /// The row of `(period, bucket)`; all zeros when absent.
    #[must_use]
    pub fn get(&self, period: PeriodType, bucket: Bucket) -> BucketRow {
        self.rows
            .get(&(period, bucket))
            .copied()
            .unwrap_or_default()
    }
}
