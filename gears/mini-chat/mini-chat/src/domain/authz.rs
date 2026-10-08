//! PEP helpers: resource types, actions and scope construction (DESIGN §3.8).

use authz_resolver_sdk::{AccessRequest, PolicyEnforcer, ResourceType};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use uuid::Uuid;

use crate::domain::error::DomainResult;

/// Chat resource (sub-resources inherit the chat decision).
pub const CHAT: ResourceType = ResourceType::from_static(
    "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~",
    &[
        pep_properties::OWNER_TENANT_ID,
        pep_properties::OWNER_ID,
        pep_properties::RESOURCE_ID,
    ],
);

/// Model resource (permission-only).
pub const MODEL: ResourceType = ResourceType::from_static(
    "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~",
    &[pep_properties::OWNER_TENANT_ID],
);

/// User quota resource.
pub const USER_QUOTA: ResourceType = ResourceType::from_static(
    "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID],
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

/// Evaluates a chat action and returns the owner-narrowed scope.
///
/// # Errors
/// `AuthzDenied` (403) or `AuthzUnavailable` (503).
pub async fn chat_scope(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
    action: &str,
    chat_id: Option<Uuid>,
) -> DomainResult<AccessScope> {
    let req = if action == actions::CREATE {
        AccessRequest::new()
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .resource_property(pep_properties::OWNER_ID, ctx.subject_id())
    } else {
        AccessRequest::new()
    };
    let scope = enforcer
        .access_scope_with(ctx, &CHAT, action, chat_id, &req)
        .await?;
    Ok(scope.ensure_owner(ctx.subject_id()))
}

/// Evaluates a model action (decision only).
///
/// # Errors
/// `AuthzDenied` (403) or `AuthzUnavailable` (503).
pub async fn model_permission(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
    action: &str,
) -> DomainResult<()> {
    enforcer
        .access_scope_with(
            ctx,
            &MODEL,
            action,
            None,
            &AccessRequest::new().require_constraints(false),
        )
        .await?;
    Ok(())
}

/// Evaluates the quota read action and returns the owner-narrowed scope.
///
/// # Errors
/// `AuthzDenied` (403) or `AuthzUnavailable` (503).
pub async fn quota_scope(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
) -> DomainResult<AccessScope> {
    let scope = enforcer
        .access_scope_with(
            ctx,
            &USER_QUOTA,
            actions::READ,
            None,
            &AccessRequest::new()
                .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
                .resource_property(pep_properties::OWNER_ID, ctx.subject_id()),
        )
        .await?;
    Ok(scope.ensure_owner(ctx.subject_id()))
}

/// Scope for child rows of an already authorized chat.
#[must_use]
pub fn child_scope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}
