//! Model catalog read API and quota status API.

use std::sync::Arc;

use toolkit_security::SecurityContext;

use crate::api::rest::dto::{
    ModelDto, ModelListDto, QuotaPeriod, QuotaPeriodStatus, QuotaStatusResponse, QuotaTier,
    QuotaTierStatus,
};
use crate::domain::chat_service::{policy_err, tier_dto};
use crate::domain::services::{ServiceContext, periods};
use crate::error::{DomainError, ResourceKind};
use crate::infra::storage::repo::quota::{BucketKey, bucket, period};

/// The model service.
pub struct ModelService {
    ctx: Arc<ServiceContext>,
}

impl ModelService {
    /// Create the service.
    #[must_use]
    pub fn new(ctx: Arc<ServiceContext>) -> Self {
        Self { ctx }
    }

    /// List enabled models of the active policy snapshot.
    pub async fn list_models(&self, ctx: &SecurityContext) -> Result<ModelListDto, DomainError> {
        self.ctx.pep.model_permission(ctx, "list").await?;
        let snapshot = self.snapshot(ctx).await?;
        let items = snapshot
            .model_catalog
            .iter()
            .filter(|m| m.enabled)
            .map(model_dto)
            .collect();
        Ok(ModelListDto { items })
    }

    /// Get one enabled model.
    pub async fn get_model(
        &self,
        ctx: &SecurityContext,
        model_id: &str,
    ) -> Result<ModelDto, DomainError> {
        self.ctx.pep.model_permission(ctx, "read").await?;
        let snapshot = self.snapshot(ctx).await?;
        snapshot
            .enabled_model(model_id)
            .map(model_dto)
            .ok_or(DomainError::NotFound(ResourceKind::Model))
    }

    /// Per-tier, per-period quota status of the calling user.
    pub async fn quota_status(
        &self,
        ctx: &SecurityContext,
    ) -> Result<QuotaStatusResponse, DomainError> {
        let scope = self.ctx.pep.user_quota_scope(ctx).await?;
        let user_id = ctx.subject_id();
        let tenant_id = ctx.subject_tenant_id();
        let version = self
            .ctx
            .policy
            .current_policy_version(user_id)
            .await
            .map_err(policy_err)?;
        let limits = self
            .ctx
            .policy
            .user_limits(user_id, version)
            .await
            .map_err(policy_err)?;
        let now = time::OffsetDateTime::now_utc();
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
        let states = self
            .ctx
            .repos
            .quota
            .read_buckets(&self.ctx.db.conn()?, &scope, tenant_id, user_id, &keys)
            .await?;
        let threshold = u32::from(self.ctx.config.quota.warning_threshold_pct);
        let mut tiers = Vec::new();
        for (tier, bucket_name, tier_limits) in [
            (QuotaTier::Premium, bucket::PREMIUM, limits.premium),
            (QuotaTier::Total, bucket::TOTAL, limits.standard),
        ] {
            let mut period_statuses = Vec::new();
            for (period_kind, _period_start, limit, next_reset) in [
                (
                    QuotaPeriod::Daily,
                    periods::daily_start(now),
                    tier_limits.limit_daily_credits_micro,
                    periods::daily_next_reset(now),
                ),
                (
                    QuotaPeriod::Monthly,
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
                    .find(|(k, _)| k.period_type == period_kind_str(period_kind) && k.bucket == bucket_name)
                    .map(|(_, s)| *s)
                    .unwrap_or_default();
                let used = state.spent_credits_micro + state.reserved_credits_micro;
                let remaining = (limit - used).max(0);
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let remaining_percentage = ((remaining * 100) / limit) as u32;
                let warning = remaining_percentage <= 100 - threshold;
                let exhausted = remaining_percentage == 0;
                period_statuses.push(QuotaPeriodStatus {
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
            tiers.push(QuotaTierStatus {
                tier,
                periods: period_statuses,
            });
        }
        Ok(QuotaStatusResponse {
            tiers,
            warning_threshold_pct: threshold,
        })
    }

    async fn snapshot(
        &self,
        ctx: &SecurityContext,
    ) -> Result<mini_chat_sdk::PolicySnapshot, DomainError> {
        let user_id = ctx.subject_id();
        let version = self
            .ctx
            .policy
            .current_policy_version(user_id)
            .await
            .map_err(policy_err)?;
        self.ctx
            .policy
            .policy_snapshot(user_id, version)
            .await
            .map_err(policy_err)
    }
}

fn period_kind_str(kind: QuotaPeriod) -> &'static str {
    match kind {
        QuotaPeriod::Daily => period::DAILY,
        QuotaPeriod::Monthly => period::MONTHLY,
    }
}

/// Model catalog entry -> DTO (public projection; no internal fields).
fn model_dto(entry: &mini_chat_sdk::ModelCatalogEntry) -> ModelDto {
    ModelDto {
        model_id: entry.id.clone(),
        display_name: entry.display_name.clone(),
        tier: tier_dto(entry.tier),
        multiplier_display: entry.multiplier_display.clone(),
        description: if entry.description.is_empty() {
            None
        } else {
            Some(entry.description.clone())
        },
        multimodal_capabilities: entry.multimodal_capabilities.clone(),
        context_window: entry.context_window,
    }
}
