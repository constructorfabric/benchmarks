//! Quota service: preflight (downgrade cascade), reserve, settlement and quota status
//! (DESIGN sections 3.2, 3.7, 5.3 - 5.5, 5.8; ADR-0008).

pub mod billing;
pub mod estimate;
pub mod ledger;
pub mod periods;
pub mod preflight;
pub mod status;

use std::sync::Arc;

use mini_chat_sdk::UserLimits;
use toolkit_db::DBProvider;
use uuid::Uuid;

pub use billing::{BillingOutcome, SettlementMethod, derive_billing};
pub use ledger::{QuotaMetricsFacts, SettleInput, SettleResult};
pub use periods::{Bucket, Period, PeriodStarts};
pub use preflight::{PreflightDecision, PreflightInput, QuotaDecision, ReserveAmounts, ToolGates};
pub use status::{PeriodStatus, QuotaStatus, QuotaWarning, TierStatus};

use crate::config::QuotaConfig;
use crate::domain::authz::Authz;
use crate::domain::error::DomainError;
use crate::infra::db::entity::quota_usage;
use crate::infra::db::repo::quota_usage::BucketKey;
use crate::infra::gateways::policy::PolicyGateway;
use crate::metrics::Metrics;

/// Per-user credit quotas: admission (preflight), reserve, settlement and status.
pub struct QuotaService {
    db: Arc<DBProvider<DomainError>>,
    authz: Arc<Authz>,
    policy: Arc<dyn PolicyGateway>,
    cfg: QuotaConfig,
    metrics: Arc<Metrics>,
}

impl QuotaService {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        authz: Arc<Authz>,
        policy: Arc<dyn PolicyGateway>,
        cfg: QuotaConfig,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            db,
            authz,
            policy,
            cfg,
            metrics,
        }
    }
}

/// The user's rows of the current periods.
struct Usage(Vec<quota_usage::Model>);

impl Usage {
    fn row(&self, period: Period, bucket: Bucket) -> Option<&quota_usage::Model> {
        self.0
            .iter()
            .find(|r| r.period_type == period.as_str() && r.bucket == bucket.as_str())
    }

    /// `spent + reserved` of a bucket row (0 without a row).
    fn used(&self, period: Period, bucket: Bucket) -> i64 {
        self.row(period, bucket).map_or(0, |r| {
            r.spent_credits_micro
                .saturating_add(r.reserved_credits_micro)
        })
    }
}

/// Every `(period, bucket)` row a turn touches, in the canonical lock order: sorted by the
/// stored `(period_type, bucket)` strings, the order of the preflight's locking select
/// (`ORDER BY period_type, bucket`). Reserve, its re-check and settlement iterate this list, so
/// concurrent turns of one user lock rows in the same order and cannot deadlock on `PostgreSQL`.
fn lock_order(premium: bool) -> Vec<(Period, Bucket)> {
    let buckets: &[Bucket] = if premium {
        &[Bucket::Premium, Bucket::Total]
    } else {
        &[Bucket::Total]
    };
    Period::ALL
        .into_iter()
        .flat_map(|period| buckets.iter().map(move |&bucket| (period, bucket)))
        .collect()
}

/// Limit of a bucket and period: `total` -> `standard`, `tier:premium` -> `premium`.
fn limit_of(limits: &UserLimits, bucket: Bucket, period: Period) -> i64 {
    let tier = match bucket {
        Bucket::Total => &limits.standard,
        Bucket::Premium => &limits.premium,
    };
    match period {
        Period::Daily => tier.limit_daily_credits_micro,
        Period::Monthly => tier.limit_monthly_credits_micro,
    }
}

fn bucket_key(
    tenant_id: Uuid,
    user_id: Uuid,
    starts: PeriodStarts,
    period: Period,
    bucket: Bucket,
) -> BucketKey {
    BucketKey {
        tenant_id,
        user_id,
        period,
        period_start: starts.start(period),
        bucket,
    }
}

#[cfg(test)]
mod tests;
