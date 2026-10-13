//! Policy enforcement point (DESIGN §3.8).

use authz_resolver_sdk::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Chat resource (`gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~`).
pub const CHAT: ResourceType = ResourceType::from_static(
    "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID, pep_properties::RESOURCE_ID],
);
/// Model resource (permission only).
pub const MODEL: ResourceType =
    ResourceType::from_static("gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~", &[]);
/// User quota resource.
pub const USER_QUOTA: ResourceType = ResourceType::from_static(
    "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~",
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

fn map_err(e: EnforcerError) -> DomainError {
    match e {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => DomainError::PermissionDenied,
        EnforcerError::EvaluationFailed(err) => {
            tracing::error!(error = %err, "PDP evaluation failed");
            DomainError::PdpUnavailable
        }
    }
}

/// PEP wrapper.
#[derive(Clone)]
pub struct Authz {
    enforcer: PolicyEnforcer,
}

impl Authz {
    /// New PEP.
    #[must_use]
    pub fn new(enforcer: PolicyEnforcer) -> Self {
        Self { enforcer }
    }

    /// Scope for a chat operation; adds the owner predicate (defence in depth).
    ///
    /// # Errors
    /// 403 on deny/compile failure, 503 on PDP failure.
    pub async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError> {
        let mut req = AccessRequest::new()
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .require_constraints(true);
        if chat_id.is_none() {
            req = req.resource_property(pep_properties::OWNER_ID, ctx.subject_id());
        }
        let scope = self
            .enforcer
            .access_scope_with(ctx, &CHAT, action, chat_id, &req)
            .await
            .map_err(map_err)?;
        Ok(scope.ensure_owner(ctx.subject_id()))
    }

    /// Permission check for the Models API.
    ///
    /// # Errors
    /// 403 / 503.
    pub async fn model_check(&self, ctx: &SecurityContext, action: &str) -> Result<(), DomainError> {
        let req = AccessRequest::new().require_constraints(false);
        self.enforcer
            .access_scope_with(ctx, &MODEL, action, None, &req)
            .await
            .map_err(map_err)
            .map(|_| ())
    }

    /// Scope for `GET /quota/status`.
    ///
    /// # Errors
    /// 403 / 503.
    pub async fn quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError> {
        let req = AccessRequest::new()
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .resource_property(pep_properties::OWNER_ID, ctx.subject_id())
            .require_constraints(true);
        let scope = self
            .enforcer
            .access_scope_with(ctx, &USER_QUOTA, "read", None, &req)
            .await
            .map_err(map_err)?;
        Ok(scope.ensure_owner(ctx.subject_id()))
    }
}

/// Scope used for children of an authorized chat (tenant only).
#[must_use]
pub fn child_scope(chat_scope: &AccessScope) -> AccessScope {
    chat_scope.tenant_only()
}

/// Scope for background work bound to one tenant.
#[must_use]
pub fn tenant_scope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}
