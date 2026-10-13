//! Authorization (PEP, DESIGN §3.8).
//!
//! Chat content is owner-only: every chat-scoped operation is evaluated
//! against the Chat resource and the compiled scope is additionally narrowed
//! to the subject's tenant and the subject as owner (defence in depth — the
//! PDP may return only a tenant predicate).

use authz_resolver_sdk::{AccessRequest, PolicyEnforcer, ResourceType, TenantMode};
use toolkit_gts::gts_id;
use toolkit_security::{
    AccessScope, ScopeConstraint, ScopeFilter, SecurityContext, pep_properties,
};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Chat resource (properties `owner_tenant_id`, `owner_id`, `id`).
pub const CHAT: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~"),
    &[
        pep_properties::OWNER_TENANT_ID,
        pep_properties::OWNER_ID,
        pep_properties::RESOURCE_ID,
    ],
);

/// Model resource (catalog-sourced; permission-only, no properties).
pub const MODEL: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~"),
    &[],
);

/// User quota resource (properties `owner_tenant_id`, `owner_id`).
pub const USER_QUOTA: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~"),
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID],
);

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

/// Policy enforcement point of the gear.
#[derive(Clone)]
pub struct Pep {
    enforcer: PolicyEnforcer,
}

impl Pep {
    #[must_use]
    pub fn new(enforcer: PolicyEnforcer) -> Self {
        Self { enforcer }
    }

    /// Root-only tenant context of the subject (no hierarchy for chat content).
    fn base_request(ctx: &SecurityContext) -> AccessRequest {
        AccessRequest::new()
            .context_tenant_id(ctx.subject_tenant_id())
            .tenant_mode(TenantMode::RootOnly)
    }

    /// Scope for a Chat action (`resource.id` = `chat_id`), narrowed to the
    /// subject's tenant and the subject as owner. `create` passes the owner
    /// properties of the new chat.
    ///
    /// # Errors
    /// `AuthzDenied` (denied / compile failure), `AuthzUnavailable` (PDP failure).
    pub async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError> {
        let tenant = ctx.subject_tenant_id();
        let user = ctx.subject_id();
        let mut req = Self::base_request(ctx);
        if action == CREATE {
            req = req
                .resource_property(pep_properties::OWNER_TENANT_ID, tenant)
                .resource_property(pep_properties::OWNER_ID, user);
        }
        let scope = self
            .enforcer
            .access_scope_with(ctx, &CHAT, action, chat_id, &req)
            .await?;
        Ok(restrict_to_subject(&scope, tenant, user))
    }

    /// Permission-only check on the Model resource (`require_constraints = false`).
    ///
    /// # Errors
    /// `AuthzDenied` / `AuthzUnavailable`.
    pub async fn model_access(
        &self,
        ctx: &SecurityContext,
        action: &str,
    ) -> Result<(), DomainError> {
        let req = Self::base_request(ctx).require_constraints(false);
        self.enforcer
            .access_scope_with(ctx, &MODEL, action, None, &req)
            .await?;
        Ok(())
    }

    /// Scope for reading the subject's quota (`UserQuota` `read`), narrowed to
    /// the subject's tenant and the subject as owner.
    ///
    /// # Errors
    /// `AuthzDenied` / `AuthzUnavailable`.
    pub async fn quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError> {
        let req = Self::base_request(ctx);
        let scope = self
            .enforcer
            .access_scope_with(ctx, &USER_QUOTA, READ, None, &req)
            .await?;
        Ok(restrict_to_subject(
            &scope,
            ctx.subject_tenant_id(),
            ctx.subject_id(),
        ))
    }
}

/// Narrow `scope` so every constraint requires `owner_tenant_id = tenant` and
/// `owner_id = user` (intersection semantics: a constraint whose tenant or
/// owner filter excludes the subject is dropped; deny-all stays deny-all).
pub(crate) fn restrict_to_subject(scope: &AccessScope, tenant: Uuid, user: Uuid) -> AccessScope {
    if scope.is_deny_all() {
        return AccessScope::deny_all();
    }
    let tenant_filter = ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, tenant);
    let tenant_scoped = if scope.is_unconstrained() {
        AccessScope::single(ScopeConstraint::new(vec![tenant_filter]))
    } else {
        let constraints: Vec<ScopeConstraint> = scope
            .constraints()
            .iter()
            .filter_map(|c| {
                let is_tenant = |f: &&ScopeFilter| f.property() == pep_properties::OWNER_TENANT_ID;
                let admits_subject = c
                    .filters()
                    .iter()
                    .filter(is_tenant)
                    .all(|f| f.values().iter().any(|v| v.as_uuid() == Some(tenant)));
                if !admits_subject {
                    return None;
                }
                let mut filters: Vec<ScopeFilter> = c
                    .filters()
                    .iter()
                    .filter(|f| !is_tenant(f))
                    .cloned()
                    .collect();
                filters.push(tenant_filter.clone());
                Some(ScopeConstraint::new(filters))
            })
            .collect();
        if constraints.is_empty() {
            return AccessScope::deny_all();
        }
        AccessScope::from_constraints(constraints)
    };
    tenant_scoped.ensure_owner(user)
}

#[cfg(test)]
#[path = "authz_tests.rs"]
mod tests;
