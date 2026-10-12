//! The quota service: preflight cascade, reserve plan and quota warnings (DESIGN sections 3.2
//! "quota service", 5.4).

mod cascade;
pub mod model;
mod status;

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{PolicySnapshot, UserLimits};
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use toolkit_security::AccessScope;
use uuid::Uuid;

pub use cascade::{
    CascadeInputs, DowngradeReason, PreflightDecision, periods_at, quota_breakdown, quota_warnings,
    resolve_effective_model,
};
pub use model::{
    Bucket, BucketLimit, BucketUsage, PeriodBreakdown, PeriodType, QuotaPeriod, QuotaSnapshot,
    QuotaWarning, ReservePlan,
};
pub use status::{QuotaStatus, QuotaStatusService, TierStatus};

use super::error::DomainError;
use super::estimation::{ReserveInputs, ToolFacts};
use crate::config::QuotaConfig;

/// Read access to the `quota_usage` rows of a user.
#[async_trait]
pub trait QuotaRepository: Send + Sync {
    /// The bucket rows of `user_id` for `periods`. `scope` carries the tenant and owner of the
    /// caller.
    async fn usage(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        periods: &[QuotaPeriod; 2],
    ) -> Result<QuotaSnapshot, DomainError>;
}

/// Preflight request of the quota service.
#[domain_model]
#[derive(Debug, Clone)]
pub struct PreflightRequest<'a> {
    /// The caller's tenant.
    pub tenant_id: Uuid,
    /// The caller.
    pub user_id: Uuid,
    /// The chat's model.
    pub selected_model: &'a str,
    /// Model-independent reserve inputs.
    pub reserve_inputs: ReserveInputs,
    /// What the request and the chat offer to a model.
    pub tool_facts: ToolFacts,
    /// The instant the preflight runs (selects the period rows).
    pub now: OffsetDateTime,
}

/// The quota service.
pub struct QuotaService {
    repo: Arc<dyn QuotaRepository>,
    config: QuotaConfig,
}

impl QuotaService {
    /// Creates the service.
    #[must_use]
    pub fn new(repo: Arc<dyn QuotaRepository>, config: QuotaConfig) -> Self {
        Self { repo, config }
    }

    /// The configured warning threshold.
    #[must_use]
    pub fn config(&self) -> &QuotaConfig {
        &self.config
    }

    /// The preflight: reads the user's bucket rows and runs the downgrade cascade with the daily
    /// tool-quota checks. Nothing is written.
    ///
    /// # Errors
    /// `QuotaExceeded` or an internal error from the repository.
    pub async fn preflight(
        &self,
        scope: &AccessScope,
        snapshot: &PolicySnapshot,
        limits: &UserLimits,
        request: &PreflightRequest<'_>,
    ) -> Result<PreflightDecision, DomainError> {
        let periods = periods_at(request.now);
        let usage = self
            .repo
            .usage(scope, request.tenant_id, request.user_id, &periods)
            .await?;
        resolve_effective_model(&CascadeInputs {
            snapshot,
            limits,
            usage: &usage,
            selected_model: request.selected_model,
            reserve_inputs: request.reserve_inputs,
            tool_facts: request.tool_facts,
            periods,
            tool_quotas: &self.config,
        })
    }

    /// The per-tier, per-period breakdown of the user's quota at `at`.
    ///
    /// # Errors
    /// An internal error from the repository.
    pub async fn breakdown(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        limits: &UserLimits,
        at: OffsetDateTime,
    ) -> Result<Vec<PeriodBreakdown>, DomainError> {
        let periods = periods_at(at);
        let usage = self.repo.usage(scope, tenant_id, user_id, &periods).await?;
        Ok(quota_breakdown(
            limits,
            &usage,
            &periods,
            self.config.warning_threshold_pct,
        ))
    }

    /// The warnings of the user's quota state at `at` (the turn's period rows).
    ///
    /// # Errors
    /// An internal error from the repository.
    pub async fn warnings(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        user_id: Uuid,
        limits: &UserLimits,
        at: OffsetDateTime,
    ) -> Result<Vec<QuotaWarning>, DomainError> {
        let periods = periods_at(at);
        let usage = self.repo.usage(scope, tenant_id, user_id, &periods).await?;
        Ok(quota_warnings(
            limits,
            &usage,
            &periods,
            self.config.warning_threshold_pct,
        ))
    }
}
