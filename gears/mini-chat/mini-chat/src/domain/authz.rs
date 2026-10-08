//! Policy Enforcement Point helpers (DESIGN §3.8).

use authz_resolver_sdk::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use uuid::Uuid;

use crate::domain::error::DomainError;

pub const CHAT_RESOURCE: ResourceType = ResourceType::from_static(
    "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID, pep_properties::RESOURCE_ID],
);

pub const MODEL_RESOURCE: ResourceType =
    ResourceType::from_static("gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~", &[]);

pub const USER_QUOTA_RESOURCE: ResourceType = ResourceType::from_static(
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

/// Denied / compile failure → 403; PDP evaluation failure → 503 (fail closed).
pub fn map_enforcer_err(err: EnforcerError) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => {
            tracing::debug!(error = %err, "authorization denied");
            DomainError::PermissionDenied
        }
        EnforcerError::EvaluationFailed(cause) => {
            tracing::error!(error = %cause, "authorization evaluation failed");
            DomainError::PdpUnavailable
        }
    }
}

/// Scopes produced by the PEP for chat operations.
#[derive(Debug, Clone)]
pub struct ChatScopes {
    /// Owner-scoped (tenant + owner) — for `chats`, `message_reactions`, `quota_usage`.
    pub owner: AccessScope,
    /// Tenant-only scope for child tables (filtered by an owner-checked `chat_id`).
    pub tenant: AccessScope,
}

/// Evaluate a chat action and compile the scope; an owner predicate for the
/// subject is always added (defence in depth).
///
/// # Errors
/// Returns `DomainError` when the PDP denies access or the authorization call fails.
pub async fn chat_scope(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
    action: &str,
    chat_id: Option<Uuid>,
) -> Result<ChatScopes, DomainError> {
    let mut request = AccessRequest::new().require_constraints(true);
    if chat_id.is_none() {
        request = request
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
    }
    let scope = enforcer
        .access_scope_with(ctx, &CHAT_RESOURCE, action, chat_id, &request)
        .await
        .map_err(map_enforcer_err)?;
    let owner = scope.ensure_owner(ctx.subject_id());
    let tenant = owner.tenant_only();
    Ok(ChatScopes { owner, tenant })
}

/// Permission-only check for the Models API.
///
/// # Errors
/// Returns `DomainError` when the PDP denies access or the authorization call fails.
pub async fn model_permission(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
    action: &str,
) -> Result<(), DomainError> {
    let request = AccessRequest::new().require_constraints(false);
    enforcer
        .access_scope_with(ctx, &MODEL_RESOURCE, action, None, &request)
        .await
        .map(|_| ())
        .map_err(map_enforcer_err)
}

/// Owner scope for `GET /v1/quota/status`.
///
/// # Errors
/// Returns `DomainError` when the PDP denies access or the authorization call fails.
pub async fn quota_scope(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<AccessScope, DomainError> {
    let request = AccessRequest::new()
        .require_constraints(true)
        .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
        .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
    let scope = enforcer
        .access_scope_with(ctx, &USER_QUOTA_RESOURCE, actions::READ, None, &request)
        .await
        .map_err(map_enforcer_err)?;
    Ok(scope.ensure_owner(ctx.subject_id()))
}
