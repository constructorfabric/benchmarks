//! Policy enforcement point: resource types, actions and scope helpers.

use authz_resolver_sdk::{AccessRequest, PolicyEnforcer, ResourceType};
use toolkit_security::{
    AccessScope, ScopeConstraint, ScopeFilter, SecurityContext, pep_properties,
};
use uuid::Uuid;

use super::error::DomainError;

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

/// Scopes produced for a chat-scoped operation.
#[derive(Debug, Clone)]
pub struct ChatScopes {
    /// PDP scope plus an owner predicate (for `chats` / owner tables).
    pub owner: AccessScope,
    /// PDP scope as returned (for child tables filtered by `chat_id`).
    pub tenant: AccessScope,
}

/// Add `owner_id = subject` and `owner_tenant_id = subject tenant` to every
/// constraint of the scope (defence in depth: a foreign chat is invisible
/// even if the PDP returned only a tenant predicate).
#[must_use]
pub fn with_owner(scope: &AccessScope, ctx: &SecurityContext) -> AccessScope {
    if scope.is_deny_all() {
        return scope.clone();
    }
    let owner = ScopeFilter::eq(pep_properties::OWNER_ID, ctx.subject_id());
    let tenant = ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id());
    if scope.is_unconstrained() {
        return AccessScope::single(ScopeConstraint::new(vec![tenant, owner]));
    }
    let constraints = scope
        .constraints()
        .iter()
        .map(|c| {
            let mut filters = c.filters().to_vec();
            filters.push(owner.clone());
            ScopeConstraint::new(filters)
        })
        .collect();
    AccessScope::from_constraints(constraints)
}

/// Tenant scope for child tables (constraints without the owner property,
/// which child tables do not have).
#[must_use]
pub fn tenant_only(scope: &AccessScope, ctx: &SecurityContext) -> AccessScope {
    if scope.is_deny_all() {
        return scope.clone();
    }
    if scope.is_unconstrained() {
        return AccessScope::for_tenant(ctx.subject_tenant_id());
    }
    let constraints: Vec<ScopeConstraint> = scope
        .constraints()
        .iter()
        .filter_map(|c| {
            let filters: Vec<ScopeFilter> = c
                .filters()
                .iter()
                .filter(|f| f.property() == pep_properties::OWNER_TENANT_ID)
                .cloned()
                .collect();
            if filters.is_empty() {
                None
            } else {
                Some(ScopeConstraint::new(filters))
            }
        })
        .collect();
    if constraints.is_empty() {
        AccessScope::for_tenant(ctx.subject_tenant_id())
    } else {
        AccessScope::from_constraints(constraints)
    }
}

/// Evaluate a chat-level action.
///
/// # Errors
/// 403 on denial / compile failure, 503 on PDP evaluation failure.
pub async fn chat_scopes(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
    action: &str,
    chat_id: Option<Uuid>,
) -> Result<ChatScopes, DomainError> {
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
    if scope.is_deny_all() {
        return Err(DomainError::forbidden());
    }
    Ok(ChatScopes {
        owner: with_owner(&scope, ctx),
        tenant: tenant_only(&scope, ctx),
    })
}

/// Permission-only check for the Models API.
///
/// # Errors
/// 403 / 503 as for chat actions.
pub async fn model_permission(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
    action: &str,
) -> Result<(), DomainError> {
    let scope = enforcer
        .access_scope_with(
            ctx,
            &MODEL,
            action,
            None,
            &AccessRequest::new().require_constraints(false),
        )
        .await?;
    if scope.is_deny_all() {
        return Err(DomainError::forbidden());
    }
    Ok(())
}

/// Scope for the quota status endpoint (`USER_QUOTA`, `read`).
///
/// # Errors
/// 403 / 503 as for chat actions.
pub async fn quota_scope(
    enforcer: &PolicyEnforcer,
    ctx: &SecurityContext,
) -> Result<AccessScope, DomainError> {
    let scope = enforcer
        .access_scope_with(ctx, &USER_QUOTA, actions::READ, None, &AccessRequest::new())
        .await?;
    if scope.is_deny_all() {
        return Err(DomainError::forbidden());
    }
    Ok(with_owner(&scope, ctx))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::from_u128(1))
            .subject_tenant_id(Uuid::from_u128(2))
            .build()
            .unwrap()
    }

    #[test]
    fn owner_predicate_is_added_to_every_constraint() {
        let c = ctx();
        let scope = AccessScope::for_tenant(Uuid::from_u128(2));
        let owned = with_owner(&scope, &c);
        assert!(owned.contains_uuid(pep_properties::OWNER_ID, Uuid::from_u128(1)));
        assert!(owned.contains_uuid(pep_properties::OWNER_TENANT_ID, Uuid::from_u128(2)));
        let unconstrained = with_owner(&AccessScope::allow_all(), &c);
        assert!(unconstrained.contains_uuid(pep_properties::OWNER_ID, Uuid::from_u128(1)));
        assert!(with_owner(&AccessScope::deny_all(), &c).is_deny_all());
    }

    #[test]
    fn tenant_only_keeps_tenant_filters() {
        let c = ctx();
        let scope = with_owner(&AccessScope::for_tenant(Uuid::from_u128(2)), &c);
        let t = tenant_only(&scope, &c);
        assert!(t.contains_uuid(pep_properties::OWNER_TENANT_ID, Uuid::from_u128(2)));
        assert!(!t.contains_uuid(pep_properties::OWNER_ID, Uuid::from_u128(1)));
    }
}
