//! The domain service: shared dependencies of every use case. Use cases are
//! implemented as `impl Services` blocks in the sibling modules.

use std::sync::Arc;

use authz_resolver_sdk::TenantMode;
use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use uuid::Uuid;

use super::error::DomainError;
use crate::config::MiniChatConfig;
use crate::infra::db::Db;
use crate::infra::llm::client::LlmClient;
use crate::infra::llm::registry::ProviderRegistry;
use crate::infra::metrics::Metrics;
use crate::infra::outbox::OutboxBridge;
use crate::infra::plugins_gateway::{AuditGateway, PolicyGateway};

/// GTS type of the Chat resource (PEP).
pub const CHAT_RESOURCE_TYPE: &str = "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~";
/// GTS type of the Model resource (PEP).
pub const MODEL_RESOURCE_TYPE: &str = "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~";
/// GTS type of the `UserQuota` resource (PEP).
pub const USER_QUOTA_RESOURCE_TYPE: &str =
    "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~";

pub const CHAT: ResourceType = ResourceType::from_static(
    CHAT_RESOURCE_TYPE,
    &[
        pep_properties::OWNER_TENANT_ID,
        pep_properties::OWNER_ID,
        pep_properties::RESOURCE_ID,
    ],
);
pub const MODEL: ResourceType = ResourceType::from_static(MODEL_RESOURCE_TYPE, &[]);
pub const USER_QUOTA: ResourceType = ResourceType::from_static(
    USER_QUOTA_RESOURCE_TYPE,
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID],
);

/// Shared dependencies of the domain use cases.
pub struct Services {
    pub cfg: Arc<MiniChatConfig>,
    pub db: Arc<Db>,
    pub enforcer: PolicyEnforcer,
    pub policy: Arc<PolicyGateway>,
    pub audit: Arc<AuditGateway>,
    pub llm: Arc<LlmClient>,
    pub providers: Arc<ProviderRegistry>,
    pub outbox: Arc<OutboxBridge>,
    pub metrics: Arc<Metrics>,
    pub upload_slots: Arc<Semaphore>,
    pub shutdown: CancellationToken,
}

pub fn map_enforcer_err(err: EnforcerError) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => DomainError::AuthzDenied,
        EnforcerError::EvaluationFailed(e) => DomainError::AuthzUnavailable(e.to_string()),
    }
}

impl Services {
    /// Owner-scoped access scope for a Chat action.
    pub async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError> {
        let tenant = ctx.subject_tenant_id();
        let mut req = AccessRequest::new()
            .context_tenant_id(tenant)
            .tenant_mode(TenantMode::RootOnly)
            .require_constraints(true);
        if action == "create" {
            req = req
                .resource_property(pep_properties::OWNER_TENANT_ID, tenant)
                .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
        }
        let scope = self
            .enforcer
            .access_scope_with(ctx, &CHAT, action, chat_id, &req)
            .await
            .map_err(map_enforcer_err)?;
        Ok(scope.ensure_owner(ctx.subject_id()))
    }

    /// Permission-only check for the Models API.
    pub async fn model_permission(
        &self,
        ctx: &SecurityContext,
        action: &str,
    ) -> Result<(), DomainError> {
        let req = AccessRequest::new()
            .context_tenant_id(ctx.subject_tenant_id())
            .tenant_mode(TenantMode::RootOnly)
            .require_constraints(false);
        self.enforcer
            .access_scope_with(ctx, &MODEL, action, None, &req)
            .await
            .map_err(map_enforcer_err)?;
        Ok(())
    }

    /// Owner-scoped access scope for the quota status.
    pub async fn quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError> {
        let tenant = ctx.subject_tenant_id();
        let req = AccessRequest::new()
            .context_tenant_id(tenant)
            .tenant_mode(TenantMode::RootOnly)
            .require_constraints(true);
        let scope = self
            .enforcer
            .access_scope_with(ctx, &USER_QUOTA, "read", None, &req)
            .await
            .map_err(map_enforcer_err)?;
        Ok(scope.ensure_owner(ctx.subject_id()))
    }
}
