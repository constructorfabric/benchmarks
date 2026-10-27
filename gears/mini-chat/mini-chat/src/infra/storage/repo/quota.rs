//! Quota usage repository (bucket rows).

use sea_orm::ExprTrait;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::Date;
use toolkit_db::secure::{AccessScope, DBRunner, SecureEntityExt, SecureUpdateExt};
use uuid::Uuid;

use super::{Db, now};
use super::super::entity::quota_usage;
use crate::error::DomainError;

/// Bucket names.
pub mod bucket {
    /// Global ceiling (all tiers).
    pub const TOTAL: &str = "total";
    /// Premium sub-cap.
    pub const PREMIUM: &str = "tier:premium";
}

/// Period types.
pub mod period {
    /// Daily (UTC).
    pub const DAILY: &str = "daily";
    /// Monthly (UTC).
    pub const MONTHLY: &str = "monthly";
}

/// A bucket key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BucketKey {
    /// Period type (`daily` | `monthly`).
    pub period_type: &'static str,
    /// Period start date (UTC).
    pub period_start: Date,
    /// Bucket name.
    pub bucket: &'static str,
}

/// Quota usage repository.
#[derive(Clone)]
pub struct QuotaRepo {
    #[allow(dead_code)]
    db: Db,
}

/// Select helper with the standard scope (tenant + owner via entity).
fn select_scoped(
    scope: &AccessScope,
) -> toolkit_db::secure::SecureSelect<quota_usage::Entity, toolkit_db::secure::Scoped> {
    quota_usage::Entity::find().secure().scope_with(scope)
}

/// A bucket row state (defaults to zeros when no row exists).
#[derive(Debug, Clone, Copy, Default)]
pub struct BucketState {
    /// Committed spend.
    pub spent_credits_micro: i64,
    /// In-flight reserves.
    pub reserved_credits_micro: i64,
    /// Completed web search calls (bucket `total` only).
    pub web_search_calls: i32,
    /// Completed code interpreter calls (bucket `total` only).
    pub code_interpreter_calls: i32,
    /// Settlements against this bucket.
    pub calls: i32,
}

impl QuotaRepo {
    /// Create a repository.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Read the current state of the given buckets.
    pub async fn read_buckets(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        keys: &[BucketKey],
    ) -> Result<Vec<(BucketKey, BucketState)>, DomainError> {
        let rows = select_scoped(scope)
            .filter(Condition::all().add(quota_usage::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(quota_usage::Column::UserId.eq(user_id)))
            .all(runner)
            .await?;
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let row = rows.iter().find(|r| {
                r.period_type == key.period_type
                    && r.period_start == key.period_start
                    && r.bucket == key.bucket
            });
            let state = row.map_or_else(BucketState::default, |r| BucketState {
                spent_credits_micro: r.spent_credits_micro,
                reserved_credits_micro: r.reserved_credits_micro,
                web_search_calls: r.web_search_calls,
                code_interpreter_calls: r.code_interpreter_calls,
                calls: r.calls,
            });
            out.push((*key, state));
        }
        Ok(out)
    }

    /// Increment the reserve of one bucket row (inserting it when missing).
    pub async fn add_reserve(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        user_id: Uuid,
        key: BucketKey,
        credits_micro: i64,
    ) -> Result<(), DomainError> {
        let now_ts = now();
        let res = quota_usage::Entity::update_many()
            .col_expr(
                quota_usage::Column::ReservedCreditsMicro,
                Expr::col(quota_usage::Column::ReservedCreditsMicro).add(credits_micro),
            )
            .col_expr(quota_usage::Column::UpdatedAt, Expr::value(now_ts))
            .filter(Condition::all().add(quota_usage::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(quota_usage::Column::UserId.eq(user_id)))
            .filter(Condition::all().add(quota_usage::Column::PeriodType.eq(key.period_type)))
            .filter(Condition::all().add(quota_usage::Column::PeriodStart.eq(key.period_start)))
            .filter(Condition::all().add(quota_usage::Column::Bucket.eq(key.bucket)))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        if res.rows_affected == 1 {
            return Ok(());
        }
        let am = quota_usage::ActiveModel {
            id: ActiveValue::Set(Uuid::new_v4()),
            tenant_id: ActiveValue::Set(tenant_id),
            user_id: ActiveValue::Set(user_id),
            period_type: ActiveValue::Set(key.period_type.to_owned()),
            period_start: ActiveValue::Set(key.period_start),
            bucket: ActiveValue::Set(key.bucket.to_owned()),
            spent_credits_micro: ActiveValue::Set(0),
            reserved_credits_micro: ActiveValue::Set(credits_micro),
            calls: ActiveValue::Set(0),
            input_tokens: ActiveValue::Set(0),
            output_tokens: ActiveValue::Set(0),
            file_search_calls: ActiveValue::Set(0),
            web_search_calls: ActiveValue::Set(0),
            code_interpreter_calls: ActiveValue::Set(0),
            rag_retrieval_calls: ActiveValue::Set(0),
            image_inputs: ActiveValue::Set(0),
            image_upload_bytes: ActiveValue::Set(0),
            updated_at: ActiveValue::Set(now_ts),
        };
        let insert_res = toolkit_db::secure::secure_insert::<quota_usage::Entity>(am, &AccessScope::allow_all(), runner).await;
        match insert_res {
            Ok(_) => Ok(()),
            Err(e) if e.is_unique_violation() => {
                // Lost the insert race; the winner's row now exists.
                let res = quota_usage::Entity::update_many()
                    .col_expr(
                        quota_usage::Column::ReservedCreditsMicro,
                        Expr::col(quota_usage::Column::ReservedCreditsMicro).add(credits_micro),
                    )
                    .col_expr(quota_usage::Column::UpdatedAt, Expr::value(now()))
                    .filter(Condition::all().add(quota_usage::Column::TenantId.eq(tenant_id)))
                    .filter(Condition::all().add(quota_usage::Column::UserId.eq(user_id)))
                    .filter(
                        Condition::all().add(quota_usage::Column::PeriodType.eq(key.period_type)),
                    )
                    .filter(
                        Condition::all().add(quota_usage::Column::PeriodStart.eq(key.period_start)),
                    )
                    .filter(Condition::all().add(quota_usage::Column::Bucket.eq(key.bucket)))
                    .secure()
                    .scope_with(&AccessScope::allow_all())
                    .exec(runner)
                    .await?;
                if res.rows_affected == 1 {
                    Ok(())
                } else {
                    tracing::error!("mini-chat: quota reserve row vanished after insert race");
                    Err(DomainError::Internal)
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    /// `Settlement` of one bucket row: release the reserve, commit the charge,
    /// bump `calls` and (bucket `total` only) the telemetry counters.
    #[allow(clippy::too_many_arguments)]
    pub async fn settle(
        &self,
        runner: &impl DBRunner,
        tenant_id: Uuid,
        user_id: Uuid,
        key: BucketKey,
        turn_reserved_credits_micro: i64,
        committed_credits_micro: i64,
        input_tokens: i64,
        output_tokens: i64,
        web_search_calls: i32,
        code_interpreter_calls: i32,
    ) -> Result<(), DomainError> {
        let total_bucket = key.bucket == bucket::TOTAL;
        let mut update = quota_usage::Entity::update_many()
            .col_expr(
                quota_usage::Column::ReservedCreditsMicro,
                Expr::col(quota_usage::Column::ReservedCreditsMicro)
                    .sub(turn_reserved_credits_micro),
            )
            .col_expr(
                quota_usage::Column::SpentCreditsMicro,
                Expr::col(quota_usage::Column::SpentCreditsMicro).add(committed_credits_micro),
            )
            .col_expr(quota_usage::Column::Calls, Expr::col(quota_usage::Column::Calls).add(1))
            .col_expr(quota_usage::Column::UpdatedAt, Expr::value(now()))
            .filter(Condition::all().add(quota_usage::Column::TenantId.eq(tenant_id)))
            .filter(Condition::all().add(quota_usage::Column::UserId.eq(user_id)))
            .filter(Condition::all().add(quota_usage::Column::PeriodType.eq(key.period_type)))
            .filter(Condition::all().add(quota_usage::Column::PeriodStart.eq(key.period_start)))
            .filter(Condition::all().add(quota_usage::Column::Bucket.eq(key.bucket)));
        if total_bucket {
            update = update
                .col_expr(
                    quota_usage::Column::InputTokens,
                    Expr::col(quota_usage::Column::InputTokens).add(input_tokens),
                )
                .col_expr(
                    quota_usage::Column::OutputTokens,
                    Expr::col(quota_usage::Column::OutputTokens).add(output_tokens),
                )
                .col_expr(
                    quota_usage::Column::WebSearchCalls,
                    Expr::col(quota_usage::Column::WebSearchCalls).add(web_search_calls),
                )
                .col_expr(
                    quota_usage::Column::CodeInterpreterCalls,
                    Expr::col(quota_usage::Column::CodeInterpreterCalls).add(code_interpreter_calls),
                );
        }
        let res = update
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(runner)
            .await?;
        if res.rows_affected == 0 {
            // The reserve transaction should have created the row; a missing
            // row at settlement is an internal inconsistency.
            tracing::error!(
                "mini-chat: quota settlement found no bucket row ({tenant_id}/{user_id}/{}/{}/{})",
                key.period_type,
                key.period_start,
                key.bucket
            );
            return Err(DomainError::Internal);
        }
        Ok(())
    }
}
