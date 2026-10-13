//! [`AuthzPort`] backed by the platform `PolicyEnforcer` (DESIGN section 3.8).

use async_trait::async_trait;
use authz_resolver_sdk::TenantMode;
use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_gts::gts_id;
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::{AuthzPort, ChatAction};

/// Chat resource: `owner_tenant_id`, `owner_id` and `id` are constrainable.
pub const CHAT: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~"),
    &[
        pep_properties::OWNER_TENANT_ID,
        pep_properties::OWNER_ID,
        pep_properties::RESOURCE_ID,
    ],
);

/// Model catalog resource: a pure permission check, no constrainable property.
pub const MODEL: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~"),
    &[],
);

/// Per-user quota usage (`quota_usage`).
pub const USER_QUOTA: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~"),
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID],
);

/// Map a PEP failure (fail-closed): a denial or a constraint compile failure is
/// `AuthzDenied`; a PDP that could not evaluate is `AuthzUnavailable`. The
/// cause is logged, never returned.
fn map_enforcer_err(err: &EnforcerError) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => {
            tracing::warn!(error = %err, "authorization denied");
            DomainError::AuthzDenied
        }
        EnforcerError::EvaluationFailed(_) => {
            // DESIGN section 3.8: a PDP outage is logged at `error` (503, not 403).
            tracing::error!(error = %err, "authorization evaluation failed");
            DomainError::AuthzUnavailable
        }
    }
}

/// Tenant-only (owner-only content, no hierarchy) request for `ctx`.
fn owner_request(ctx: &SecurityContext) -> AccessRequest {
    AccessRequest::new()
        .context_tenant_id(ctx.subject_tenant_id())
        .tenant_mode(TenantMode::RootOnly)
}

pub struct PolicyEnforcerAuthz {
    enforcer: PolicyEnforcer,
}

impl PolicyEnforcerAuthz {
    #[must_use]
    pub fn new(enforcer: PolicyEnforcer) -> Self {
        Self { enforcer }
    }
}

#[async_trait]
impl AuthzPort for PolicyEnforcerAuthz {
    async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: ChatAction,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError> {
        let mut request = owner_request(ctx);
        if action == ChatAction::Create {
            request = request
                .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
                .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
        }
        let scope = self
            .enforcer
            .access_scope_with(ctx, &CHAT, action.as_str(), chat_id, &request)
            .await
            .map_err(|e| map_enforcer_err(&e))?;
        // Defence in depth: the static PDP only emits a tenant predicate.
        Ok(scope.ensure_owner(ctx.subject_id()))
    }

    async fn model_access(&self, ctx: &SecurityContext, action: &str) -> Result<(), DomainError> {
        let request = AccessRequest::new().require_constraints(false);
        self.enforcer
            .access_scope_with(ctx, &MODEL, action, None, &request)
            .await
            .map(|_| ())
            .map_err(|e| map_enforcer_err(&e))
    }

    async fn quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError> {
        let scope = self
            .enforcer
            .access_scope_with(ctx, &USER_QUOTA, "read", None, &owner_request(ctx))
            .await
            .map_err(|e| map_enforcer_err(&e))?;
        Ok(scope.ensure_owner(ctx.subject_id()))
    }
}

#[cfg(test)]
#[path = "authz_tests.rs"]
mod authz_tests;
