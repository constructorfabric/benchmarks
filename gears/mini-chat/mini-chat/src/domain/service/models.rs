//! Models API and quota status (DESIGN §3.3 Models API, Quota Status Endpoint).

use mini_chat_sdk::ModelCatalogEntry;
use time::OffsetDateTime;
use toolkit_security::SecurityContext;

use super::MiniChatService;
use crate::domain::authz::{actions, model_permission, quota_scope};
use crate::domain::error::{DomainError, DomainResult, Res};
use crate::domain::quota::{PeriodStatus, load_usage, quota_status};

/// Quota status of the caller.
#[derive(Debug, Clone)]
pub struct QuotaStatusView {
    pub tiers: Vec<(&'static str, Vec<PeriodStatus>)>,
    pub warning_threshold_pct: u8,
}

impl MiniChatService {
    /// `GET /v1/models` — enabled catalog entries.
    ///
    /// # Errors
    /// Authorization or policy failure.
    pub async fn list_models(&self, ctx: &SecurityContext) -> DomainResult<Vec<ModelCatalogEntry>> {
        model_permission(&self.enforcer, ctx, actions::LIST).await?;
        let snap = self.snapshot(ctx).await?;
        Ok(snap.model_catalog.iter().filter(|m| m.enabled).cloned().collect())
    }

    /// `GET /v1/models/{id}` — 404 when disabled or unknown.
    ///
    /// # Errors
    /// Authorization, policy failure or not found.
    pub async fn get_model(&self, ctx: &SecurityContext, model_id: &str) -> DomainResult<ModelCatalogEntry> {
        model_permission(&self.enforcer, ctx, actions::READ).await?;
        let snap = self.snapshot(ctx).await?;
        snap.find_enabled(model_id).cloned().ok_or(DomainError::NotFound(Res::Model))
    }

    /// `GET /v1/quota/status`.
    ///
    /// # Errors
    /// Authorization, policy or database failure.
    pub async fn quota_status(&self, ctx: &SecurityContext) -> DomainResult<QuotaStatusView> {
        let _scope = quota_scope(&self.enforcer, ctx).await?;
        let version = self.snapshot(ctx).await?.policy_version;
        let limits = self.policy.user_limits(ctx.subject_id(), version).await?;
        let conn = self.db.conn()?;
        let now = OffsetDateTime::now_utc();
        let usage = load_usage(&conn, ctx.subject_tenant_id(), ctx.subject_id(), now).await?;
        Ok(QuotaStatusView {
            tiers: quota_status(&usage, &limits, self.cfg.quota.warning_threshold_pct, now),
            warning_threshold_pct: self.cfg.quota.warning_threshold_pct,
        })
    }
}
