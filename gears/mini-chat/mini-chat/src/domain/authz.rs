//! PEP integration with the platform `AuthZ` Resolver (PDP).
//!
//! Mini Chat acts as the Policy Enforcement Point: it builds evaluation
//! requests, sends them to the PDP, and compiles returned constraints into
//! `AccessScope` applied by Secure ORM (DESIGN.md section 3.8).

use authz_resolver_sdk::models::TenantMode;
use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_security::SecurityContext;
use toolkit_security::access_scope::AccessScope;
use toolkit_security::pep_properties;
use uuid::Uuid;

use crate::error::DomainError;

/// Chat resource type (GTS).
pub const CHAT_RESOURCE_GTS: &str =
    toolkit_gts::gts_id!("cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~");
/// Model resource type (GTS).
pub const MODEL_RESOURCE_GTS: &str =
    toolkit_gts::gts_id!("cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~");
/// User quota resource type (GTS).
pub const USER_QUOTA_RESOURCE_GTS: &str =
    toolkit_gts::gts_id!("cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~");

const CHAT_PROPERTIES: &[&str] = &[
    pep_properties::OWNER_TENANT_ID,
    pep_properties::OWNER_ID,
    pep_properties::RESOURCE_ID,
];
const MODEL_PROPERTIES: &[&str] = &[];
const USER_QUOTA_PROPERTIES: &[&str] = &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID];

/// The PEP facade.
pub struct Pep {
    enforcer: PolicyEnforcer,
}

impl Pep {
    /// Create the PEP over the resolver client.
    #[must_use]
    pub fn new(authz: std::sync::Arc<dyn authz_resolver_sdk::AuthZResolverApi>) -> Self {
        Self {
            enforcer: PolicyEnforcer::new(authz),
        }
    }

    /// Evaluate a chat-scoped action and return the compiled scope.
    ///
    /// # Errors
    ///
    /// - [`DomainError::PermissionDenied`] on PDP denial or compile failure
    /// - [`DomainError::PdpUnavailable`] on PDP evaluation failure
    pub async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError> {
        let resource = ResourceType::from_static(CHAT_RESOURCE_GTS, CHAT_PROPERTIES);
        let mut request = AccessRequest::new()
            .tenant_mode(TenantMode::RootOnly)
            .require_constraints(true);
        if chat_id.is_none() {
            // Collection operations (create/list): pass the owner properties
            // of the prospective resource.
            request = request
                .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
                .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
        }
        self.enforcer
            .access_scope_with(ctx, &resource, action, chat_id, &request)
            .await
            .map_err(map_enforcer_err)
    }

    /// Evaluate a model permission (no constraint scoping).
    ///
    /// # Errors
    ///
    /// - [`DomainError::PermissionDenied`] on PDP denial
    /// - [`DomainError::PdpUnavailable`] on PDP evaluation failure
    pub async fn model_permission(&self, ctx: &SecurityContext, action: &str) -> Result<(), DomainError> {
        let resource = ResourceType::from_static(MODEL_RESOURCE_GTS, MODEL_PROPERTIES);
        let request = AccessRequest::new()
            .tenant_mode(TenantMode::RootOnly)
            .require_constraints(false);
        self.enforcer
            .access_scope_with(ctx, &resource, action, None, &request)
            .await
            .map_err(map_enforcer_err)?;
        Ok(())
    }

    /// Evaluate the user-quota read permission and return the scope.
    ///
    /// # Errors
    ///
    /// - [`DomainError::PermissionDenied`] on PDP denial or compile failure
    /// - [`DomainError::PdpUnavailable`] on PDP evaluation failure
    pub async fn user_quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError> {
        let resource = ResourceType::from_static(USER_QUOTA_RESOURCE_GTS, USER_QUOTA_PROPERTIES);
        let request = AccessRequest::new()
            .tenant_mode(TenantMode::RootOnly)
            .require_constraints(true)
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
        self.enforcer
            .access_scope_with(ctx, &resource, "read", None, &request)
            .await
            .map_err(map_enforcer_err)
    }
}

/// Map enforcer errors onto the canonical contract (fail closed).
fn map_enforcer_err(err: EnforcerError) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => {
            DomainError::PermissionDenied
        }
        EnforcerError::EvaluationFailed(err) => {
            tracing::error!("mini-chat: PDP evaluation failed: {err}");
            DomainError::PdpUnavailable
        }
    }
}
