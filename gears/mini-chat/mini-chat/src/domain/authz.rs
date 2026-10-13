//! Policy Enforcement Point helpers (DESIGN §3.8).

use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_gts::gts_id;
use toolkit_security::pep_properties::{OWNER_ID, OWNER_TENANT_ID, RESOURCE_ID};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::error::DomainError;

pub const CHAT: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~"),
    &[OWNER_TENANT_ID, OWNER_ID, RESOURCE_ID],
);

pub const MODEL: ResourceType =
    ResourceType::from_static(gts_id!("cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~"), &[]);

pub const USER_QUOTA: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~"),
    &[OWNER_TENANT_ID, OWNER_ID],
);

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
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => DomainError::AuthzDenied,
        EnforcerError::EvaluationFailed(e) => {
            tracing::error!(error = %e, "PDP evaluation failed");
            DomainError::AuthzUnavailable
        }
    }
}

/// Authorizes a chat-scoped action and returns the compiled scope clamped to
/// the subject as owner (defence in depth).
///
/// # Errors
/// `AuthzDenied` (403) or `AuthzUnavailable` (503).
pub async fn chat_scope(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
    action: &str,
    chat_id: Option<Uuid>,
) -> Result<AccessScope, DomainError> {
    let req = AccessRequest::new()
        .resource_property(OWNER_TENANT_ID, ctx.subject_tenant_id())
        .resource_property(OWNER_ID, ctx.subject_id());
    let scope = enforcer
        .access_scope_with(ctx, &CHAT, action, chat_id, &req)
        .await
        .map_err(map_enforcer_err)?;
    Ok(scope.ensure_owner(ctx.subject_id()))
}

/// Permission-only check for the Models API.
///
/// # Errors
/// `AuthzDenied` (403) or `AuthzUnavailable` (503).
pub async fn model_permission(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
    action: &str,
) -> Result<(), DomainError> {
    let req = AccessRequest::new().require_constraints(false);
    enforcer
        .access_scope_with(ctx, &MODEL, action, None, &req)
        .await
        .map_err(map_enforcer_err)?;
    Ok(())
}

/// Scope for the quota status endpoint.
///
/// # Errors
/// `AuthzDenied` (403) or `AuthzUnavailable` (503).
pub async fn quota_scope(enforcer: &PolicyEnforcer, ctx: &SecurityContext) -> Result<AccessScope, DomainError> {
    let req = AccessRequest::new()
        .resource_property(OWNER_TENANT_ID, ctx.subject_tenant_id())
        .resource_property(OWNER_ID, ctx.subject_id());
    let scope = enforcer
        .access_scope_with(ctx, &USER_QUOTA, "read", None, &req)
        .await
        .map_err(map_enforcer_err)?;
    Ok(scope.ensure_owner(ctx.subject_id()))
}
