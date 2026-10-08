//! Policy enforcement point (DESIGN §3.8).
//!
//! Chat content is owner-only: the compiled scope is always narrowed to the
//! calling subject (`ensure_owner`), whatever the PDP returned. A PDP denial
//! or constraint compile failure is 403; a PDP evaluation failure is 503 with
//! `Retry-After` (both fail closed).

use authz_resolver_sdk::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use uuid::Uuid;

use super::error::DomainError;

/// Chat resource type (sub-resources inherit its decision).
pub const CHAT: ResourceType = ResourceType::from_static(
    mini_chat_sdk::CHAT_AUTHZ_RESOURCE_TYPE,
    &[
        pep_properties::OWNER_TENANT_ID,
        pep_properties::OWNER_ID,
        pep_properties::RESOURCE_ID,
    ],
);

/// Model catalog resource type (permission-only).
pub const MODEL: ResourceType = ResourceType::from_static(
    mini_chat_sdk::MODEL_AUTHZ_RESOURCE_TYPE,
    &[pep_properties::OWNER_TENANT_ID],
);

/// Per-user quota resource type.
pub const USER_QUOTA: ResourceType = ResourceType::from_static(
    mini_chat_sdk::USER_QUOTA_AUTHZ_RESOURCE_TYPE,
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID],
);

/// PEP actions.
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

fn map_enforcer_err(err: EnforcerError) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => {
            tracing::debug!(error = %err, "authorization denied");
            DomainError::AccessDenied
        }
        EnforcerError::EvaluationFailed(cause) => {
            tracing::error!(error = %cause, "authorization evaluation failed");
            DomainError::AuthzUnavailable
        }
    }
}

/// Mini-chat PEP.
#[derive(Clone)]
pub struct Authz {
    enforcer: PolicyEnforcer,
}

impl Authz {
    #[must_use]
    pub fn new(enforcer: PolicyEnforcer) -> Self {
        Self { enforcer }
    }

    /// Scope for a chat operation, always narrowed to the subject as owner.
    ///
    /// # Errors
    /// `AccessDenied` (403) or `AuthzUnavailable` (503).
    pub async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError> {
        let request = AccessRequest::new()
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .resource_property(pep_properties::OWNER_ID, ctx.subject_id())
            .require_constraints(true);
        let scope = self
            .enforcer
            .access_scope_with(ctx, &CHAT, action, chat_id, &request)
            .await
            .map_err(map_enforcer_err)?;
        if scope.is_deny_all() {
            return Err(DomainError::AccessDenied);
        }
        Ok(scope.ensure_owner(ctx.subject_id()))
    }

    /// Permission-only check for the model catalog.
    ///
    /// # Errors
    /// `AccessDenied` (403) or `AuthzUnavailable` (503).
    pub async fn model_access(&self, ctx: &SecurityContext, action: &str) -> Result<(), DomainError> {
        let request = AccessRequest::new()
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .require_constraints(false);
        self.enforcer
            .access_scope_with(ctx, &MODEL, action, None, &request)
            .await
            .map_err(map_enforcer_err)?;
        Ok(())
    }

    /// Scope for the quota status read.
    ///
    /// # Errors
    /// `AccessDenied` (403) or `AuthzUnavailable` (503).
    pub async fn quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError> {
        let request = AccessRequest::new()
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .resource_property(pep_properties::OWNER_ID, ctx.subject_id())
            .require_constraints(true);
        let scope = self
            .enforcer
            .access_scope_with(ctx, &USER_QUOTA, actions::READ, None, &request)
            .await
            .map_err(map_enforcer_err)?;
        if scope.is_deny_all() {
            return Err(DomainError::AccessDenied);
        }
        Ok(scope.ensure_owner(ctx.subject_id()))
    }
}

/// Tenant-only scope for child tables of an already authorized chat.
#[must_use]
pub fn tenant_scope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}
