//! `quota_usage` bucket-row access: reads of the current period rows and race-safe UPSERT
//! increments (DESIGN §3.7 `quota_usage`, §5.4.2-§5.4.4).

use sea_orm::sea_query::{Expr, SimpleExpr};
use sea_orm::{ColumnTrait, Condition, EntityTrait, ExprTrait, QueryFilter, Set};
use time::{Date, OffsetDateTime};
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureInsertExt, SecureOnConflict};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::QuotaPeriods;
use crate::domain::error::DomainError;
use crate::infra::db::entity::quota_usage;

/// Enforcement period of a bucket row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeriodKind {
    Daily,
    Monthly,
}

impl PeriodKind {
    pub const ALL: [Self; 2] = [Self::Daily, Self::Monthly];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }

    #[must_use]
    pub const fn start(self, periods: &QuotaPeriods) -> Date {
        match self {
            Self::Daily => periods.daily_start,
            Self::Monthly => periods.monthly_start,
        }
    }
}

/// Quota bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bucket {
    /// Overall cap (limits `user_limits.standard`).
    Total,
    /// Premium subcap (limits `user_limits.premium`).
    Premium,
}

impl Bucket {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::Premium => "tier:premium",
        }
    }
}

/// Bucket rows of one user for one pair of (daily, monthly) periods.
#[derive(Debug, Clone, Default)]
pub struct UsageRows {
    rows: Vec<quota_usage::Model>,
}

impl UsageRows {
    #[must_use]
    pub fn get(&self, periods: &QuotaPeriods, period: PeriodKind, bucket: Bucket) -> Option<&quota_usage::Model> {
        let start = period.start(periods);
        self.rows.iter().find(|r| {
            r.period_type == period.as_str() && r.period_start == start && r.bucket == bucket.as_str()
        })
    }

    /// `spent + reserved` of a row (0 when missing), saturating.
    #[must_use]
    pub fn used(&self, periods: &QuotaPeriods, period: PeriodKind, bucket: Bucket) -> i64 {
        self.get(periods, period, bucket)
            .map_or(0, |r| r.spent_credits_micro.saturating_add(r.reserved_credits_micro))
    }
}

/// Scope of a user's own quota rows (system access by tenant + owner).
#[must_use]
pub fn user_scope(tenant_id: Uuid, user_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id).ensure_owner(user_id)
}

/// Reads the user's bucket rows of the given daily and monthly periods.
///
/// # Errors
/// DB failure.
pub async fn load_rows(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    user_id: Uuid,
    periods: &QuotaPeriods,
) -> Result<UsageRows, DomainError> {
    use quota_usage::Column as C;
    let rows = quota_usage::Entity::find()
        .filter(
            Condition::all()
                .add(C::TenantId.eq(tenant_id))
                .add(C::UserId.eq(user_id))
                .add(
                    Condition::any()
                        .add(
                            Condition::all()
                                .add(C::PeriodType.eq(PeriodKind::Daily.as_str()))
                                .add(C::PeriodStart.eq(periods.daily_start)),
                        )
                        .add(
                            Condition::all()
                                .add(C::PeriodType.eq(PeriodKind::Monthly.as_str()))
                                .add(C::PeriodStart.eq(periods.monthly_start)),
                        ),
                ),
        )
        .secure()
        .scope_with(scope)
        .all(runner)
        .await?;
    Ok(UsageRows { rows })
}

/// Increments applied to one bucket row; the insert values of a missing row are the same
/// increments (a missing row counts as zeros), except `reserved` which never starts negative.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RowDelta {
    pub reserved: i64,
    pub spent: i64,
    pub calls: i32,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub web_search_calls: i32,
    pub code_interpreter_calls: i32,
}

fn add_expr(col: quota_usage::Column, v: impl Into<sea_orm::Value>) -> SimpleExpr {
    Expr::col((quota_usage::Entity, col)).add(Expr::val(v))
}

/// Race-safe `INSERT ... ON CONFLICT (tenant, user, period_type, period_start, bucket) DO UPDATE
/// SET col = col + delta` of one bucket row.
///
/// # Errors
/// DB failure.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_delta(
    runner: &impl DBRunner,
    tenant_id: Uuid,
    user_id: Uuid,
    period: PeriodKind,
    start: Date,
    bucket: Bucket,
    d: RowDelta,
    now: OffsetDateTime,
) -> Result<(), DomainError> {
    use quota_usage::Column as C;
    let am = quota_usage::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(tenant_id),
        user_id: Set(user_id),
        period_type: Set(period.as_str().to_owned()),
        period_start: Set(start),
        bucket: Set(bucket.as_str().to_owned()),
        spent_credits_micro: Set(d.spent),
        reserved_credits_micro: Set(std::cmp::max(d.reserved, 0)),
        calls: Set(d.calls),
        input_tokens: Set(d.input_tokens),
        output_tokens: Set(d.output_tokens),
        file_search_calls: Set(0),
        web_search_calls: Set(d.web_search_calls),
        code_interpreter_calls: Set(d.code_interpreter_calls),
        rag_retrieval_calls: Set(0),
        image_inputs: Set(0),
        image_upload_bytes: Set(0),
        updated_at: Set(now),
    };
    let on_conflict = SecureOnConflict::<quota_usage::Entity>::columns([
        C::TenantId,
        C::UserId,
        C::PeriodType,
        C::PeriodStart,
        C::Bucket,
    ])
    .value(C::ReservedCreditsMicro, add_expr(C::ReservedCreditsMicro, d.reserved))?
    .value(C::SpentCreditsMicro, add_expr(C::SpentCreditsMicro, d.spent))?
    .value(C::Calls, add_expr(C::Calls, d.calls))?
    .value(C::InputTokens, add_expr(C::InputTokens, d.input_tokens))?
    .value(C::OutputTokens, add_expr(C::OutputTokens, d.output_tokens))?
    .value(C::WebSearchCalls, add_expr(C::WebSearchCalls, d.web_search_calls))?
    .value(C::CodeInterpreterCalls, add_expr(C::CodeInterpreterCalls, d.code_interpreter_calls))?
    .value(C::UpdatedAt, Expr::val(now))?;
    let scope = user_scope(tenant_id, user_id);
    quota_usage::Entity::insert(am)
        .secure()
        .scope_unchecked(&scope)?
        .on_conflict(on_conflict)
        .exec(runner)
        .await?;
    Ok(())
}
