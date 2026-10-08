//! Models API and quota status API.

use mini_chat_sdk::ModelCatalogEntry;
use toolkit_security::SecurityContext;

use crate::domain::errors::{DomainError, DomainResult, Res};
use crate::domain::quota::{PeriodStatus, Periods, quota_status};
use crate::domain::state::AppState;
use crate::infra::db::repo;

impl AppState {
    pub async fn list_models(&self, ctx: &SecurityContext) -> DomainResult<Vec<ModelCatalogEntry>> {
        self.model_check(ctx, "list").await?;
        let snap = self.policy.current_snapshot(ctx.subject_id()).await?;
        Ok(snap.model_catalog.into_iter().filter(|m| m.enabled).collect())
    }

    pub async fn get_model(&self, ctx: &SecurityContext, id: &str) -> DomainResult<ModelCatalogEntry> {
        self.model_check(ctx, "read").await?;
        let snap = self.policy.current_snapshot(ctx.subject_id()).await?;
        snap.find_enabled_model(id)
            .cloned()
            .ok_or_else(|| DomainError::not_found(Res::Model, id))
    }

    pub async fn quota_status(&self, ctx: &SecurityContext) -> DomainResult<(Vec<PeriodStatus>, u8)> {
        self.quota_check(ctx).await?;
        let policy = self.policy.client().await?;
        let v = policy
            .get_current_policy_version(ctx.subject_id())
            .await
            .map_err(|e| DomainError::internal(format!("policy version: {e}")))?;
        let limits = self.policy.user_limits(ctx.subject_id(), v.policy_version).await?;
        let now = repo::now();
        let conn = self.db.conn()?;
        let usage = repo::read_usage(&conn, ctx.subject_tenant_id(), ctx.subject_id(), Periods::at(now)).await?;
        let threshold = self.cfg.quota.warning_threshold_pct;
        Ok((quota_status(&usage, &limits, threshold, now), threshold))
    }
}
