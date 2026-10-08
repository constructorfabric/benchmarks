//! Policy Enforcement Point helpers.
//!
//! Chat content is owner-only: every chat-scoped operation evaluates the PDP
//! for the Chat resource (`resource.id` = chat id), compiles the constraints
//! to an `AccessScope` and additionally ANDs an owner predicate for the
//! subject into every constraint (defence in depth), so a foreign chat is
//! invisible even if the PDP returned only a tenant predicate.

use std::sync::Arc;

use authz_resolver_sdk::models::TenantMode;
use authz_resolver_sdk::pep::{AccessRequest, PolicyEnforcer, ResourceType};
use toolkit_gts::gts_id;
use toolkit_security::{
    AccessScope, ScopeConstraint, ScopeFilter, SecurityContext, pep_properties,
};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Chat resource (sub-resources inherit the chat decision).
pub const CHAT: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~"),
    &[
        pep_properties::OWNER_TENANT_ID,
        pep_properties::OWNER_ID,
        pep_properties::RESOURCE_ID,
    ],
);

/// Model catalog resource (permission-only).
pub const MODEL: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~"),
    &[],
);

/// User quota resource.
pub const USER_QUOTA: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~"),
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

/// Thin wrapper over the platform `PolicyEnforcer`.
#[derive(Clone)]
pub struct Authorizer {
    enforcer: Arc<PolicyEnforcer>,
}

impl Authorizer {
    #[must_use]
    pub fn new(enforcer: PolicyEnforcer) -> Self {
        Self {
            enforcer: Arc::new(enforcer),
        }
    }

    fn request(ctx: &SecurityContext) -> AccessRequest {
        AccessRequest::new()
            .tenant_mode(TenantMode::RootOnly)
            .context_tenant_id(ctx.subject_tenant_id())
    }

    /// Scope for a chat-level action. `chat_id` is `None` for `list` and
    /// `create`. The returned scope carries the owner predicate.
    ///
    /// # Errors
    /// `AccessDenied` (403) on deny / compile failure, `AuthzUnavailable`
    /// (503) on PDP evaluation failure.
    pub async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError> {
        let mut req = Self::request(ctx);
        if action == actions::CREATE {
            req = req
                .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
                .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
        }
        let scope = self
            .enforcer
            .access_scope_with(ctx, &CHAT, action, chat_id, &req)
            .await?;
        Ok(with_owner(&scope, ctx))
    }

    /// Permission-only check for the Models API.
    ///
    /// # Errors
    /// As [`Self::chat_scope`].
    pub async fn model_permission(
        &self,
        ctx: &SecurityContext,
        action: &str,
    ) -> Result<(), DomainError> {
        let req = Self::request(ctx).require_constraints(false);
        self.enforcer
            .access_scope_with(ctx, &MODEL, action, None, &req)
            .await?;
        Ok(())
    }

    /// Scope for reading the caller's quota rows.
    ///
    /// # Errors
    /// As [`Self::chat_scope`].
    pub async fn quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError> {
        let req = Self::request(ctx)
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
        let scope = self
            .enforcer
            .access_scope_with(ctx, &USER_QUOTA, actions::READ, None, &req)
            .await?;
        Ok(with_owner(&scope, ctx))
    }
}

/// AND an `owner_id = subject` predicate into every constraint of the scope.
/// An unconstrained scope becomes `tenant = subject tenant AND owner =
/// subject`; a deny-all scope stays deny-all.
#[must_use]
pub fn with_owner(scope: &AccessScope, ctx: &SecurityContext) -> AccessScope {
    let owner = ScopeFilter::eq(pep_properties::OWNER_ID, ctx.subject_id());
    if scope.is_deny_all() {
        return scope.clone();
    }
    if scope.is_unconstrained() {
        return AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()),
            owner,
        ]));
    }
    let constraints = scope
        .constraints()
        .iter()
        .map(|c| {
            let mut filters = c.filters().to_vec();
            if !filters
                .iter()
                .any(|f| f.property() == pep_properties::OWNER_ID)
            {
                filters.push(owner.clone());
            }
            ScopeConstraint::new(filters)
        })
        .collect();
    AccessScope::from_constraints(constraints)
}

/// Tenant-only scope used for child tables of an already authorized chat.
#[must_use]
pub fn tenant_scope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .unwrap_or_else(|_| SecurityContext::anonymous())
    }

    #[test]
    fn owner_predicate_is_added_to_every_constraint() {
        let c = ctx();
        let scope = AccessScope::for_tenant(c.subject_tenant_id());
        let owned = with_owner(&scope, &c);
        assert!(owned.contains_uuid(pep_properties::OWNER_ID, c.subject_id()));
        assert!(owned.contains_uuid(pep_properties::OWNER_TENANT_ID, c.subject_tenant_id()));
    }

    #[test]
    fn deny_all_stays_deny_all() {
        let c = ctx();
        assert!(with_owner(&AccessScope::deny_all(), &c).is_deny_all());
    }

    #[test]
    fn unconstrained_scope_is_narrowed_to_subject() {
        let c = ctx();
        let owned = with_owner(&AccessScope::allow_all(), &c);
        assert!(!owned.is_unconstrained());
        assert!(owned.contains_uuid(pep_properties::OWNER_ID, c.subject_id()));
    }
}
