//! Policy Enforcement Point (DESIGN §3.8).
//!
//! Every chat-scoped request asks the PDP for constraints and narrows the
//! compiled scope to the caller (`ensure_owner`), so a foreign chat is invisible
//! even when the PDP returns only a tenant predicate. Failures are fail-closed:
//! a denial or an uncompilable answer is 403, an unreachable PDP is 503.

use authz_resolver_sdk::models::TenantMode;
use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use tracing::{debug, error};
use uuid::Uuid;

use crate::domain::error::{DomainError, DomainResult};

/// PDP actions (DESIGN §3.8 "Per-Operation Authorization Matrix").
pub mod actions {
    pub const CREATE: &str = "create";
    pub const LIST: &str = "list";
    pub const READ: &str = "read";
    pub const UPDATE: &str = "update";
    pub const DELETE: &str = "delete";
    pub const LIST_MESSAGES: &str = "list_messages";
    pub const SEND_MESSAGE: &str = "send_message";
    pub const UPLOAD_ATTACHMENT: &str = "upload_attachment";
    pub const READ_ATTACHMENT: &str = "read_attachment";
    pub const DELETE_ATTACHMENT: &str = "delete_attachment";
    pub const READ_TURN: &str = "read_turn";
    pub const RETRY_TURN: &str = "retry_turn";
    pub const EDIT_TURN: &str = "edit_turn";
    pub const DELETE_TURN: &str = "delete_turn";
    pub const SET_REACTION: &str = "set_reaction";
    pub const DELETE_REACTION: &str = "delete_reaction";
}

const CHAT_PROPERTIES: &[&str] = &[
    pep_properties::OWNER_TENANT_ID,
    pep_properties::OWNER_ID,
    pep_properties::RESOURCE_ID,
];

const QUOTA_PROPERTIES: &[&str] = &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID];

/// Chat resource (sub-resources inherit its decision).
pub const CHAT_RESOURCE: ResourceType = ResourceType::from_static(
    "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~",
    CHAT_PROPERTIES,
);

/// Model resource (catalog-sourced, permission-only).
pub const MODEL_RESOURCE: ResourceType = ResourceType::from_static(
    "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~",
    CHAT_PROPERTIES,
);

/// User quota resource (`GET /v1/quota/status`).
pub const USER_QUOTA_RESOURCE: ResourceType = ResourceType::from_static(
    "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~",
    QUOTA_PROPERTIES,
);

/// PEP wrapper used by every domain service.
pub struct ChatAuthz {
    enforcer: PolicyEnforcer,
}

impl ChatAuthz {
    #[must_use]
    pub fn new(enforcer: PolicyEnforcer) -> Self {
        Self { enforcer }
    }

    /// Scope for `action` on the chat resource (`chat_id` absent for `create`/`list`),
    /// narrowed to the caller as owner.
    ///
    /// # Errors
    /// `PermissionDenied` (403) on denial or compile failure, `AuthzUnavailable`
    /// (503) when the PDP cannot be evaluated.
    pub async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: &'static str,
        chat_id: Option<Uuid>,
    ) -> DomainResult<AccessScope> {
        let mut request = base_request(ctx).require_constraints(true);
        if action == actions::CREATE {
            request = request.resource_property(pep_properties::OWNER_ID, ctx.subject_id());
        }
        let scope = self
            .evaluate(ctx, &CHAT_RESOURCE, action, chat_id, &request)
            .await?;
        Ok(scope.ensure_owner(ctx.subject_id()))
    }

    /// Permission-only check on the model resource (`require_constraints = false`).
    ///
    /// # Errors
    /// Same mapping as [`Self::chat_scope`].
    pub async fn model_permission(
        &self,
        ctx: &SecurityContext,
        action: &'static str,
    ) -> DomainResult<()> {
        let request = base_request(ctx).require_constraints(false);
        self.evaluate(ctx, &MODEL_RESOURCE, action, None, &request)
            .await
            .map(drop)
    }

    /// Scope for reading the caller's quota rows (action `read` on `UserQuota`).
    ///
    /// # Errors
    /// Same mapping as [`Self::chat_scope`].
    pub async fn quota_scope(&self, ctx: &SecurityContext) -> DomainResult<AccessScope> {
        let request = base_request(ctx).require_constraints(true);
        let scope = self
            .evaluate(ctx, &USER_QUOTA_RESOURCE, actions::READ, None, &request)
            .await?;
        Ok(scope.ensure_owner(ctx.subject_id()))
    }

    async fn evaluate(
        &self,
        ctx: &SecurityContext,
        resource: &ResourceType,
        action: &'static str,
        resource_id: Option<Uuid>,
        request: &AccessRequest,
    ) -> DomainResult<AccessScope> {
        self.enforcer
            .access_scope_with(ctx, resource, action, resource_id, request)
            .await
            .map_err(|e| map_enforcer_err(resource, action, e))
    }
}

/// Owner tenant property plus a root-only tenant context on the caller's tenant.
fn base_request(ctx: &SecurityContext) -> AccessRequest {
    let tenant = ctx.subject_tenant_id();
    AccessRequest::new()
        .context_tenant_id(tenant)
        .tenant_mode(TenantMode::RootOnly)
        .resource_property(pep_properties::OWNER_TENANT_ID, tenant)
}

fn map_enforcer_err(resource: &ResourceType, action: &str, err: EnforcerError) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => {
            debug!(resource = resource.name(), action, error = %err, "authorization denied");
            DomainError::PermissionDenied
        }
        EnforcerError::EvaluationFailed(cause) => {
            error!(resource = resource.name(), action, error = %cause, "authorization evaluation failed");
            DomainError::AuthzUnavailable
        }
    }
}

#[cfg(test)]
#[path = "authz_tests.rs"]
mod authz_tests;
