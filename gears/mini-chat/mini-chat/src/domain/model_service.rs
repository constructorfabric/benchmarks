//! Models API and quota status (DESIGN §3.3, §3.2).

use mini_chat_sdk::ModelCatalogEntry;
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::domain::quota::{self, PeriodStarts, PeriodStatus};
use crate::domain::service::Svc;
use crate::infra::db::now;

impl Svc {
    /// `GET /models`.
    ///
    /// # Errors
    /// PDP / policy errors.
    pub async fn list_models(&self, ctx: &SecurityContext) -> Result<Vec<ModelCatalogEntry>, DomainError> {
        self.authz.model_check(ctx, "list").await?;
        let s = self.policy.current_snapshot(ctx.subject_id()).await?;
        Ok(s.model_catalog.into_iter().filter(|m| m.enabled).collect())
    }

    /// `GET /models/{id}`.
    ///
    /// # Errors
    /// 404 when missing or disabled.
    pub async fn get_model(&self, ctx: &SecurityContext, id: &str) -> Result<ModelCatalogEntry, DomainError> {
        self.authz.model_check(ctx, "read").await?;
        let s = self.policy.current_snapshot(ctx.subject_id()).await?;
        s.enabled_model(id).cloned().ok_or(DomainError::ModelNotFound)
    }

    /// `GET /quota/status`.
    ///
    /// # Errors
    /// PDP / policy errors.
    pub async fn quota_status(&self, ctx: &SecurityContext) -> Result<Vec<PeriodStatus>, DomainError> {
        self.authz.quota_scope(ctx).await?;
        let s = self.policy.current_snapshot(ctx.subject_id()).await?;
        let limits = self.policy.user_limits(ctx.subject_id(), s.policy_version).await?;
        let starts = PeriodStarts::of(now());
        let conn = self.db.conn()?;
        let rows = quota::read_rows(&conn, ctx.subject_tenant_id(), ctx.subject_id(), &starts).await?;
        Ok(quota::status_entries(&rows, &limits, starts, self.cfg.quota.warning_threshold_pct))
    }
}
