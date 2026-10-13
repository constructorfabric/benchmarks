//! Policy enforcement point (DESIGN 3.8).
//!
//! Chat content is owner-only: every chat scoped operation asks the PDP for
//! `owner_tenant_id`, `owner_id` and `id` constraints and the compiled scope is
//! additionally narrowed to the subject (`owner_id = subject`), so a foreign
//! chat stays invisible even when the PDP answers with a tenant predicate only.
//! The Models API is a permission-only check (no constraints).
//!
//! Fail-closed: a denial or a constraint compile failure is
//! `AccessDenied` (403 `AUTHZ_DENIED`), a PDP that cannot evaluate is
//! `ServiceUnavailable` (503, `Retry-After: 5`). The cause is kept for logs and
//! never serialized.

use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_gts::gts_id;
use toolkit_security::{
    AccessScope, ScopeConstraint, ScopeFilter, SecurityContext, pep_properties,
};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// GTS id of the Chat resource type (sub-resources inherit its decision).
pub const CHAT_RESOURCE_TYPE: &str = gts_id!("cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~");

/// GTS id of the Model resource type (permission-only, no table).
pub const MODEL_RESOURCE_TYPE: &str =
    gts_id!("cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~");

/// GTS id of the `UserQuota` resource type (`quota_usage`).
pub const USER_QUOTA_RESOURCE_TYPE: &str =
    gts_id!("cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~");

/// `Retry-After` seconds reported when the PDP cannot evaluate a request.
pub const PDP_RETRY_AFTER_SECS: u64 = 5;

/// PEP action names (one per operation of the authorization matrix).
pub mod actions {
    /// `POST /v1/chats`.
    pub const CREATE: &str = "create";
    /// `GET /v1/chats`, `GET /v1/models`.
    pub const LIST: &str = "list";
    /// `GET /v1/chats/{id}`, `GET /v1/models/{id}`, `GET /v1/quota/status`.
    pub const READ: &str = "read";
    /// `PATCH /v1/chats/{id}`.
    pub const UPDATE: &str = "update";
    /// `DELETE /v1/chats/{id}`.
    pub const DELETE: &str = "delete";
    /// `GET /v1/chats/{id}/messages`.
    pub const LIST_MESSAGES: &str = "list_messages";
    /// `POST /v1/chats/{id}/messages:stream`.
    pub const SEND_MESSAGE: &str = "send_message";
    /// `POST /v1/chats/{id}/attachments`.
    pub const UPLOAD_ATTACHMENT: &str = "upload_attachment";
    /// `GET /v1/chats/{id}/attachments/{attachment_id}`.
    pub const READ_ATTACHMENT: &str = "read_attachment";
    /// `DELETE /v1/chats/{id}/attachments/{attachment_id}`.
    pub const DELETE_ATTACHMENT: &str = "delete_attachment";
    /// `GET /v1/chats/{id}/turns/{request_id}`.
    pub const READ_TURN: &str = "read_turn";
    /// `POST /v1/chats/{id}/turns/{request_id}/retry`.
    pub const RETRY_TURN: &str = "retry_turn";
    /// `PATCH /v1/chats/{id}/turns/{request_id}`.
    pub const EDIT_TURN: &str = "edit_turn";
    /// `DELETE /v1/chats/{id}/turns/{request_id}`.
    pub const DELETE_TURN: &str = "delete_turn";
    /// `PUT /v1/chats/{id}/messages/{msg_id}/reaction`.
    pub const SET_REACTION: &str = "set_reaction";
    /// `DELETE /v1/chats/{id}/messages/{msg_id}/reaction`.
    pub const DELETE_REACTION: &str = "delete_reaction";
}

/// Chat resource: supported properties `owner_tenant_id`, `owner_id`, `id`.
#[must_use]
pub fn chat_resource() -> ResourceType {
    ResourceType::new(
        CHAT_RESOURCE_TYPE.to_owned(),
        &[
            pep_properties::OWNER_TENANT_ID,
            pep_properties::OWNER_ID,
            pep_properties::RESOURCE_ID,
        ],
    )
}

/// Model resource: permission-only, declares no supported property so the PDP
/// answers with a plain permit instead of a tenant clamp the PEP could not
/// compile.
#[must_use]
pub fn model_resource() -> ResourceType {
    ResourceType::new(MODEL_RESOURCE_TYPE.to_owned(), &[])
}

/// `UserQuota` resource: supported properties `owner_tenant_id`, `owner_id`.
#[must_use]
pub fn user_quota_resource() -> ResourceType {
    ResourceType::new(
        USER_QUOTA_RESOURCE_TYPE.to_owned(),
        &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID],
    )
}

/// Fail-closed mapping of enforcer errors (DESIGN 3.8 "Fail-Closed Behavior").
#[must_use]
pub fn map_enforcer_err(err: EnforcerError) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => {
            DomainError::AccessDenied {
                cause: Some(Box::new(err)),
            }
        }
        EnforcerError::EvaluationFailed(_) => DomainError::ServiceUnavailable {
            detail: "authorization could not be evaluated".to_owned(),
            retry_after_secs: Some(PDP_RETRY_AFTER_SECS),
            cause: Some(Box::new(err)),
        },
    }
}

/// Permission-only check: no resource id, `require_constraints = false`.
///
/// # Errors
/// `AccessDenied` when the PDP denies, `ServiceUnavailable` when it cannot
/// evaluate the request.
pub async fn authorize_decision(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
    resource: &ResourceType,
    action: &str,
) -> Result<(), DomainError> {
    let request = AccessRequest::new().require_constraints(false);
    enforcer
        .access_scope_with(ctx, resource, action, None, &request)
        .await
        .map(|_| ())
        .map_err(map_enforcer_err)
}

/// Chat scoped authorization: returns the PDP scope narrowed to the subject.
///
/// `chat_id` is the `resource.id` of the request (absent for `create` and
/// `list`). For `create` the owner properties are taken from the
/// `SecurityContext`, never from caller input.
///
/// # Errors
/// `AccessDenied` on a denial, a missing constraint set or a compile failure;
/// `ServiceUnavailable` when the PDP cannot evaluate the request.
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
        .access_scope_with(ctx, &chat_resource(), action, chat_id, &request)
        .await
        .map_err(map_enforcer_err)?;
    Ok(narrow_to_owner(&scope, ctx.subject_id()))
}

/// `UserQuota` scoped authorization (`GET /v1/quota/status`): the PDP scope of
/// `read` on the caller's own quota rows, guaranteed to carry `owner_id =
/// subject` even when the PDP answers with a tenant predicate only.
///
/// # Errors
/// `AccessDenied` on a denial, a missing constraint set or a compile failure;
/// `ServiceUnavailable` when the PDP cannot evaluate the request.
pub async fn user_quota_scope(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<AccessScope, DomainError> {
    let request = AccessRequest::new().require_constraints(true);
    let scope = enforcer
        .access_scope_with(ctx, &user_quota_resource(), actions::READ, None, &request)
        .await
        .map_err(map_enforcer_err)?;
    Ok(scope.ensure_owner(ctx.subject_id()))
}

/// Adds `owner_id = subject` to every constraint of the scope.
fn narrow_to_owner(scope: &AccessScope, subject: Uuid) -> AccessScope {
    let owner = ScopeFilter::eq(pep_properties::OWNER_ID, subject);
    let constraints = scope
        .constraints()
        .iter()
        .map(|constraint| {
            let mut filters = constraint.filters().to_vec();
            filters.push(owner.clone());
            ScopeConstraint::new(filters)
        })
        .collect();
    AccessScope::from_constraints(constraints)
}
