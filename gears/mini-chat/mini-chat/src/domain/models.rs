//! Models API and quota status.

use mini_chat_sdk::ModelCatalogEntry;
use toolkit_security::SecurityContext;

use super::authz::actions;
use super::error::DomainError;
use super::quota::PeriodStatus;
use super::service::MiniChat;

/// Quota status of the caller.
#[derive(Debug, Clone)]
pub struct QuotaStatus {
    pub periods: Vec<PeriodStatus>,
    pub warning_threshold_pct: u8,
}

impl MiniChat {
    /// `GET /v1/models`: globally enabled catalog entries.
    ///
    /// # Errors
    /// PDP / plugin errors.
    pub async fn list_models(&self, ctx: &SecurityContext) -> Result<Vec<ModelCatalogEntry>, DomainError> {
        self.authz.model_access(ctx, actions::LIST).await?;
        let policy = self.policy.current(ctx.subject_id()).await?;
        Ok(policy.snapshot.enabled_models().cloned().collect())
    }

    /// `GET /v1/models/{id}`.
    ///
    /// # Errors
    /// `ModelNotFound` when missing or disabled.
    pub async fn get_model(&self, ctx: &SecurityContext, id: &str) -> Result<ModelCatalogEntry, DomainError> {
        self.authz.model_access(ctx, actions::READ).await?;
        let policy = self.policy.current(ctx.subject_id()).await?;
        policy.find_enabled(id).cloned().ok_or_else(|| DomainError::ModelNotFound(id.to_owned()))
    }

    /// `GET /v1/quota/status`.
    ///
    /// # Errors
    /// PDP / plugin / DB errors.
    pub async fn quota_status(&self, ctx: &SecurityContext) -> Result<QuotaStatus, DomainError> {
        let scope = self.authz.quota_scope(ctx).await?;
        let policy = self.policy.current(ctx.subject_id()).await?;
        let limits = self.policy.user_limits(ctx.subject_id(), policy.version).await?;
        let conn = self.db.conn()?;
        let periods = self
            .quota()
            .status(&conn, &scope, ctx.subject_tenant_id(), ctx.subject_id(), &limits)
            .await?;
        Ok(QuotaStatus { periods, warning_threshold_pct: self.cfg.quota.warning_threshold_pct })
    }
}
