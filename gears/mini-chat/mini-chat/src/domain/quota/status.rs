//! The quota status of the caller (`GET /v1/quota/status`, DESIGN "Quota Status Endpoint (P1)").

use std::sync::Arc;

use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;

use super::{PeriodBreakdown, QuotaService};
use crate::domain::authz::Pep;
use crate::domain::clock;
use crate::domain::error::DomainError;
use crate::domain::ports::ModelPolicyGateway;

/// The remaining quota of one tier.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierStatus {
    /// `premium` or `total`.
    pub tier: &'static str,
    /// The periods with a positive limit, daily first.
    pub periods: Vec<PeriodBreakdown>,
}

/// The caller's quota state.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaStatus {
    /// Both tiers, `premium` first.
    pub tiers: Vec<TierStatus>,
    /// `quota.warning_threshold_pct`.
    pub warning_threshold_pct: u8,
}

/// Serves the quota status: the user's policy limits against their `quota_usage` rows, computed
/// exactly like the `quota_warnings` of the `done` event. Nothing is written.
#[domain_model]
pub struct QuotaStatusService {
    pep: Pep,
    policy: Arc<dyn ModelPolicyGateway>,
    quota: Arc<QuotaService>,
}

impl QuotaStatusService {
    /// Creates the service.
    #[must_use]
    pub fn new(pep: Pep, policy: Arc<dyn ModelPolicyGateway>, quota: Arc<QuotaService>) -> Self {
        Self { pep, policy, quota }
    }

    /// The caller's quota status.
    ///
    /// # Errors
    /// `AuthzDenied`, `AuthzUnavailable`, `PolicyUnavailable` or an internal error.
    pub async fn status(&self, ctx: &SecurityContext) -> Result<QuotaStatus, DomainError> {
        let scope = self.pep.user_quota_scope(ctx).await?;
        let user_id = ctx.subject_id();
        let snapshot = self.policy.current_snapshot(user_id).await?;
        let limits = self
            .policy
            .user_limits(user_id, snapshot.policy_version)
            .await?;
        let breakdown = self
            .quota
            .breakdown(
                &scope,
                ctx.subject_tenant_id(),
                user_id,
                &limits,
                clock::now(),
            )
            .await?;
        let tiers = ["premium", "total"]
            .into_iter()
            .map(|tier| TierStatus {
                tier,
                periods: breakdown
                    .iter()
                    .filter(|entry| entry.tier == tier)
                    .cloned()
                    .collect(),
            })
            .collect();
        Ok(QuotaStatus {
            tiers,
            warning_threshold_pct: self.quota.config().warning_threshold_pct,
        })
    }
}
