//! PEP implementation over the platform `PolicyEnforcer` (DESIGN §3.8).

use async_trait::async_trait;
use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::AuthzPort;

pub const CHAT_RESOURCE_TYPE: &str = "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~";
pub const MODEL_RESOURCE_TYPE: &str = "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~";
pub const USER_QUOTA_RESOURCE_TYPE: &str = "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~";

const CHAT: ResourceType = ResourceType::from_static(
    CHAT_RESOURCE_TYPE,
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID, pep_properties::RESOURCE_ID],
);
const MODEL: ResourceType = ResourceType::from_static(MODEL_RESOURCE_TYPE, &[]);
const USER_QUOTA: ResourceType = ResourceType::from_static(
    USER_QUOTA_RESOURCE_TYPE,
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID],
);

/// Maps enforcer failures: denial / compile failure → 403, evaluation failure → 503 (fail closed).
#[must_use]
pub fn map_enforcer_err(err: &EnforcerError) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => {
            tracing::info!(error = %err, "authorization denied");
            DomainError::permission_denied()
        }
        EnforcerError::EvaluationFailed(_) => {
            tracing::error!(error = %err, "authorization evaluation failed");
            DomainError::unavailable(5, "authorization evaluation failed")
        }
    }
}

/// Adds the subject's tenant and owner predicates to a PDP scope.
#[must_use]
pub fn harden_scope(scope: &AccessScope, ctx: &SecurityContext) -> AccessScope {
    let owned = scope.ensure_owner(ctx.subject_id());
    if owned.has_property(pep_properties::OWNER_TENANT_ID) {
        owned
    } else if owned.is_deny_all() {
        owned
    } else {
        AccessScope::for_tenant(ctx.subject_tenant_id()).ensure_owner(ctx.subject_id())
    }
}

/// Production PEP.
pub struct EnforcerAuthz {
    enforcer: PolicyEnforcer,
}

impl EnforcerAuthz {
    #[must_use]
    pub fn new(enforcer: PolicyEnforcer) -> Self {
        Self { enforcer }
    }
}

#[async_trait]
impl AuthzPort for EnforcerAuthz {
    async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError> {
        let mut req = AccessRequest::new().context_tenant_id(ctx.subject_tenant_id()).require_constraints(true);
        if action == "create" {
            req = req
                .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
                .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
        }
        let scope = self
            .enforcer
            .access_scope_with(ctx, &CHAT, action, chat_id, &req)
            .await
            .map_err(|e| map_enforcer_err(&e))?;
        Ok(harden_scope(&scope, ctx))
    }

    async fn model_access(&self, ctx: &SecurityContext, action: &str) -> Result<(), DomainError> {
        let req = AccessRequest::new().context_tenant_id(ctx.subject_tenant_id()).require_constraints(false);
        self.enforcer
            .access_scope_with(ctx, &MODEL, action, None, &req)
            .await
            .map_err(|e| map_enforcer_err(&e))?;
        Ok(())
    }

    async fn quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError> {
        let req = AccessRequest::new()
            .context_tenant_id(ctx.subject_tenant_id())
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .resource_property(pep_properties::OWNER_ID, ctx.subject_id())
            .require_constraints(true);
        let scope = self
            .enforcer
            .access_scope_with(ctx, &USER_QUOTA, "read", None, &req)
            .await
            .map_err(|e| map_enforcer_err(&e))?;
        Ok(harden_scope(&scope, ctx))
    }
}
