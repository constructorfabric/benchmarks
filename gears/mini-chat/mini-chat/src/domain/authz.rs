//! Policy Enforcement Point helpers (DESIGN §3.8).

use authz_resolver_sdk::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Chat resource (owner-only content).
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
    &[],
);

/// User quota resource.
pub const USER_QUOTA: ResourceType = ResourceType::from_static(
    "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID],
);

/// PEP action names (DESIGN §3.8 matrix).
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

/// Maps enforcer failures fail-closed: deny/compile → 403, evaluation failure → 503.
#[must_use]
pub fn map_enforcer_err(err: EnforcerError) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => {
            tracing::debug!(error = %err, "authorization denied");
            DomainError::authz_denied()
        }
        EnforcerError::EvaluationFailed(cause) => {
            tracing::error!(error = %cause, "authorization evaluation failed");
            DomainError::ServiceUnavailable {
                retry_after_secs: 5,
                detail: "Authorization service temporarily unavailable".to_owned(),
            }
        }
    }
}

/// Owner-scoped chat scope for `action` (owner predicate added as defence in depth).
///
/// # Errors
/// 403 on denial / compile failure, 503 on PDP evaluation failure.
pub async fn chat_scope(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
    action: &str,
    chat_id: Option<Uuid>,
) -> Result<AccessScope, DomainError> {
    let mut request = AccessRequest::new().require_constraints(true);
    if action == actions::CREATE {
        request = request
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
    }
    let scope = enforcer
        .access_scope_with(ctx, &CHAT, action, chat_id, &request)
        .await
        .map_err(map_enforcer_err)?;
    let scope = scope.ensure_owner(ctx.subject_id());
    if scope.is_deny_all() {
        return Err(DomainError::authz_denied());
    }
    Ok(scope)
}

/// Permission-only check for the Models API.
///
/// # Errors
/// 403 on denial, 503 on PDP evaluation failure.
pub async fn model_permission(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
    action: &str,
) -> Result<(), DomainError> {
    let request = AccessRequest::new().require_constraints(false);
    enforcer
        .access_scope_with(ctx, &MODEL, action, None, &request)
        .await
        .map_err(map_enforcer_err)?;
    Ok(())
}

/// Scope for `GET /v1/quota/status` (UserQuota, action `read`).
///
/// # Errors
/// 403 on denial, 503 on PDP evaluation failure.
pub async fn user_quota_scope(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<AccessScope, DomainError> {
    let request = AccessRequest::new()
        .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
        .resource_property(pep_properties::OWNER_ID, ctx.subject_id())
        .require_constraints(true);
    let scope = enforcer
        .access_scope_with(ctx, &USER_QUOTA, actions::READ, None, &request)
        .await
        .map_err(map_enforcer_err)?;
    Ok(scope.ensure_owner(ctx.subject_id()))
}
