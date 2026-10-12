//! Quota status (`GET /v1/quota/status`, DESIGN 3.2 "Quota Status Endpoint",
//! 3.8 `UserQuota`): a read-only view of the caller's current-period buckets
//! against the limits of the current policy version.

use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use toolkit_db::DBProvider;
use toolkit_security::SecurityContext;

use crate::config::MiniChatConfig;
use crate::domain::authz::user_quota_scope;
use crate::domain::clock::now_utc;
use crate::domain::error::DomainError;
use crate::domain::ports::{ModelPolicyPort, QuotaOwner, QuotaRepository};
use crate::domain::quota::{PeriodStarts, QuotaUsage, TierStatus, quota_status};

/// The status of every tier and the configured warning threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaStatusView {
    /// Per-tier, per-period status.
    pub tiers: Vec<TierStatus>,
    /// `quota.warning_threshold_pct`.
    pub warning_threshold_pct: u8,
}

/// Reads the quota status of the caller.
pub struct QuotaStatusService {
    db: Arc<DBProvider<DomainError>>,
    policy: Arc<dyn ModelPolicyPort>,
    config: Arc<MiniChatConfig>,
    enforcer: PolicyEnforcer,
}

impl QuotaStatusService {
    /// Creates the service.
    #[must_use]
    pub fn new<P: ModelPolicyPort + 'static>(
        db: Arc<DBProvider<DomainError>>,
        policy: Arc<P>,
        config: Arc<MiniChatConfig>,
        enforcer: PolicyEnforcer,
    ) -> Self {
        Self {
            db,
            policy,
            config,
            enforcer,
        }
    }

    /// The quota status of the caller: the rows of `(tenant, caller)` for the
    /// periods running now, the limits of the current policy version.
    ///
    /// # Errors
    /// `AccessDenied` / `ServiceUnavailable` from the PDP (fail closed),
    /// `Internal` when the policy plugin or the database fails.
    pub async fn status(&self, ctx: &SecurityContext) -> Result<QuotaStatusView, DomainError> {
        let scope = user_quota_scope(&self.enforcer, ctx).await?;
        let user_id = ctx.subject_id();
        let version = self.policy.current_policy_version(user_id).await?;
        let limits = self.policy.user_limits(user_id, version).await?;

        let now = now_utc();
        let conn = self.db.conn()?;
        let rows = conn
            .quota_rows(
                &scope,
                QuotaOwner {
                    tenant_id: ctx.subject_tenant_id(),
                    user_id,
                },
                PeriodStarts::at(now),
            )
            .await?;
        let threshold = self.config.quota.warning_threshold_pct;
        Ok(QuotaStatusView {
            tiers: quota_status(&limits, &QuotaUsage::from_rows(&rows), threshold, now),
            warning_threshold_pct: threshold,
        })
    }
}
