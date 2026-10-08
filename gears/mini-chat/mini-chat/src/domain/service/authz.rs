//! Policy Enforcement Point (DESIGN §3.8).

use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use mini_chat_sdk::gts::{CHAT_PEP_RESOURCE_TYPE, MODEL_PEP_RESOURCE_TYPE, USER_QUOTA_PEP_RESOURCE_TYPE};
use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, SecurityContext, pep_properties};
use uuid::Uuid;

use crate::domain::error::DomainError;

pub const CHAT_RESOURCE: ResourceType = ResourceType::from_static(
    CHAT_PEP_RESOURCE_TYPE,
    &[
        pep_properties::OWNER_TENANT_ID,
        pep_properties::OWNER_ID,
        pep_properties::RESOURCE_ID,
    ],
);
pub const MODEL_RESOURCE: ResourceType = ResourceType::from_static(MODEL_PEP_RESOURCE_TYPE, &[]);
pub const USER_QUOTA_RESOURCE: ResourceType = ResourceType::from_static(
    USER_QUOTA_PEP_RESOURCE_TYPE,
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID],
);

/// PEP wrapper.
#[derive(Clone)]
pub struct Authz {
    enforcer: PolicyEnforcer,
}

fn map_err(err: EnforcerError) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => DomainError::AccessDenied,
        EnforcerError::EvaluationFailed(e) => {
            tracing::error!(error = %e, "authorization evaluation failed");
            DomainError::AuthzUnavailable(e.to_string())
        }
    }
}

/// Adds an owner predicate for the subject to every constraint (defence in depth).
#[must_use]
pub fn with_owner(scope: &AccessScope, owner: Uuid) -> AccessScope {
    if scope.is_deny_all() {
        return scope.clone();
    }
    if scope.is_unconstrained() {
        return AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_ID,
            owner,
        )]));
    }
    let constraints = scope
        .constraints()
        .iter()
        .map(|c| {
            let mut filters = c.filters().to_vec();
            filters.push(ScopeFilter::eq(pep_properties::OWNER_ID, owner));
            ScopeConstraint::new(filters)
        })
        .collect();
    AccessScope::from_constraints(constraints)
}

impl Authz {
    #[must_use]
    pub fn new(enforcer: PolicyEnforcer) -> Self {
        Self { enforcer }
    }

    /// Owner-scoped access scope for a chat action.
    ///
    /// # Errors
    /// 403 on denial / compile failure, 503 on PDP failure.
    pub async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError> {
        let mut req = AccessRequest::new().require_constraints(true);
        if action == "create" {
            req = req
                .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
                .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
        }
        let scope = self
            .enforcer
            .access_scope_with(ctx, &CHAT_RESOURCE, action, chat_id, &req)
            .await
            .map_err(map_err)?;
        Ok(with_owner(&scope, ctx.subject_id()))
    }

    /// Permission-only check for the Models API.
    ///
    /// # Errors
    /// 403 on denial, 503 on PDP failure.
    pub async fn model_check(&self, ctx: &SecurityContext, action: &str) -> Result<(), DomainError> {
        let req = AccessRequest::new().require_constraints(false);
        self.enforcer
            .access_scope_with(ctx, &MODEL_RESOURCE, action, None, &req)
            .await
            .map(|_| ())
            .map_err(map_err)
    }

    /// Scope for the quota status endpoint.
    ///
    /// # Errors
    /// 403 on denial, 503 on PDP failure.
    pub async fn quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError> {
        let req = AccessRequest::new().require_constraints(true);
        let scope = self
            .enforcer
            .access_scope_with(ctx, &USER_QUOTA_RESOURCE, "read", None, &req)
            .await
            .map_err(map_err)?;
        Ok(with_owner(&scope, ctx.subject_id()))
    }
}
