//! Policy Enforcement Point helpers (DESIGN §3.8).

use authz_resolver_sdk::pep::{AccessRequest, ResourceType};
use authz_resolver_sdk::PolicyEnforcer;
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use uuid::Uuid;

use super::error::{DomainError, map_enforcer_err};

/// Chat resource (sub-resources inherit its decision).
pub const CHAT: ResourceType = ResourceType::from_static(
    "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID, pep_properties::RESOURCE_ID],
);

/// Model resource (permission-only).
pub const MODEL: ResourceType =
    ResourceType::from_static("gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~", &[]);

/// User quota resource.
pub const USER_QUOTA: ResourceType = ResourceType::from_static(
    "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID],
);

/// PDP action names.
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

/// Authorization helper wrapping the platform `PolicyEnforcer`.
#[derive(Clone)]
pub struct Authz {
    enforcer: PolicyEnforcer,
}

impl Authz {
    #[must_use]
    pub fn new(enforcer: PolicyEnforcer) -> Self {
        Self { enforcer }
    }

    /// Chat-level scope for `action`, narrowed with an owner predicate for
    /// the subject (defence in depth: a foreign chat is invisible even if the
    /// PDP returned only a tenant predicate).
    ///
    /// # Errors
    /// `Forbidden` on a PDP denial / compile failure, `AuthzUnavailable` on a
    /// PDP evaluation failure.
    pub async fn chat_scope(
        &self,
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
        let scope = self
            .enforcer
            .access_scope_with(ctx, &CHAT, action, chat_id, &req)
            .await
            .map_err(map_enforcer_err)?;
        if scope.is_deny_all() {
            return Err(DomainError::Forbidden);
        }
        Ok(scope.ensure_owner(ctx.subject_id()))
    }

    /// Permission-only check for the Models API.
    ///
    /// # Errors
    /// `Forbidden` / `AuthzUnavailable` as for [`Self::chat_scope`].
    pub async fn model_access(&self, ctx: &SecurityContext, action: &str) -> Result<(), DomainError> {
        let req = AccessRequest::new().require_constraints(false);
        let scope = self
            .enforcer
            .access_scope_with(ctx, &MODEL, action, None, &req)
            .await
            .map_err(map_enforcer_err)?;
        if scope.is_deny_all() {
            return Err(DomainError::Forbidden);
        }
        Ok(())
    }

    /// User-quota scope (`read`), narrowed to the subject.
    ///
    /// # Errors
    /// `Forbidden` / `AuthzUnavailable` as for [`Self::chat_scope`].
    pub async fn quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError> {
        let req = AccessRequest::new()
            .require_constraints(true)
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
        let scope = self
            .enforcer
            .access_scope_with(ctx, &USER_QUOTA, actions::READ, None, &req)
            .await
            .map_err(map_enforcer_err)?;
        if scope.is_deny_all() {
            return Err(DomainError::Forbidden);
        }
        Ok(scope.ensure_owner(ctx.subject_id()))
    }
}

/// Scope for child tables of an already-authorized chat (tenant predicate only;
/// queries are additionally filtered by `chat_id`).
#[must_use]
pub fn child_scope(chat_scope: &AccessScope, tenant_id: Uuid) -> AccessScope {
    let t = chat_scope.tenant_only();
    if t.is_unconstrained() || t.is_deny_all() {
        AccessScope::for_tenant(tenant_id)
    } else {
        t
    }
}
