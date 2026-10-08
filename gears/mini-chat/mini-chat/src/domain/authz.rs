//! Policy Enforcement Point helpers (DESIGN §3.8).

use authz_resolver_sdk::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use uuid::Uuid;

use crate::domain::error::{DomainError, Resource};

pub const CHAT: ResourceType = ResourceType::from_static(
    "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~",
    &[
        pep_properties::OWNER_TENANT_ID,
        pep_properties::OWNER_ID,
        pep_properties::RESOURCE_ID,
    ],
);
pub const MODEL: ResourceType = ResourceType::from_static(
    "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~",
    &[],
);
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

/// Maps a PEP error: deny / compile failure → 403 `AUTHZ_DENIED`; evaluation failure → 503 Retry-After 5.
#[must_use]
pub fn map_enforcer_err(err: EnforcerError, resource: Resource) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => {
            DomainError::authz_denied(resource)
        }
        EnforcerError::EvaluationFailed(cause) => {
            tracing::error!(error = %cause, "mini-chat: PDP evaluation failed");
            DomainError::pdp_unavailable()
        }
    }
}

/// Evaluates a chat action. `chat_id` is `None` for collection actions.
///
/// # Errors
/// 403 / 503 per the fail-closed rules.
pub async fn chat_scope(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
    action: &str,
    chat_id: Option<Uuid>,
) -> Result<AccessScope, DomainError> {
    let mut req = AccessRequest::new().require_constraints(true);
    if action == actions::CREATE {
        req = req
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
    }
    enforcer
        .access_scope_with(ctx, &CHAT, action, chat_id, &req)
        .await
        .map_err(|e| map_enforcer_err(e, Resource::Chat))
}

/// Permission-only check for the Models API.
///
/// # Errors
/// 403 / 503 per the fail-closed rules.
pub async fn model_permission(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
    action: &str,
) -> Result<(), DomainError> {
    enforcer
        .access_scope_with(
            ctx,
            &MODEL,
            action,
            None,
            &AccessRequest::new().require_constraints(false),
        )
        .await
        .map(|_| ())
        .map_err(|e| map_enforcer_err(e, Resource::Model))
}

/// Scope for `GET /v1/quota/status`.
///
/// # Errors
/// 403 / 503 per the fail-closed rules.
pub async fn quota_scope(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<AccessScope, DomainError> {
    enforcer
        .access_scope_with(
            ctx,
            &USER_QUOTA,
            actions::READ,
            None,
            &AccessRequest::new()
                .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
                .resource_property(pep_properties::OWNER_ID, ctx.subject_id())
                .require_constraints(true),
        )
        .await
        .map_err(|e| map_enforcer_err(e, Resource::Chat))
}
