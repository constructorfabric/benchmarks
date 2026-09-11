//! Shared handler state and the inbound authorization gate.

use std::sync::Arc;

use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_security::{SecurityContext, pep_properties};

use crate::domain::error::{DomainError, DomainResult};
use crate::domain::services::management::ControlPlaneService;
use crate::domain::services::proxy::DataPlaneService;

/// Actions OAGW authorizes against.
pub mod actions {
    /// Create a resource.
    pub const CREATE: &str = "create";
    /// Read or list a resource.
    pub const READ: &str = "read";
    /// Replace a resource.
    pub const OVERRIDE: &str = "override";
    /// Delete a resource.
    pub const DELETE: &str = "delete";
    /// Send a request through the proxy.
    pub const INVOKE: &str = "invoke";
}

/// Everything the Axum handlers need.
pub struct OagwState {
    /// Configuration ownership and resolution.
    pub control_plane: Arc<dyn ControlPlaneService>,
    /// Proxy execution.
    pub data_plane: Arc<dyn DataPlaneService>,
    /// Policy enforcement point. `None` disables inbound authorization,
    /// which is only reachable in tests and in deployments without an
    /// authz-resolver.
    pub enforcer: Option<Arc<PolicyEnforcer>>,
}

impl OagwState {
    /// Authorize `action` on `resource_gts_type` for the caller.
    ///
    /// Constraints are not requested: OAGW projects no authorization tables,
    /// so the decision is the whole answer — every read and write is already
    /// scoped to `subject_tenant_id` at the repository boundary.
    ///
    /// # Errors
    ///
    /// `403` when the PDP denies, `503` when it cannot be evaluated.
    pub async fn authorize(
        &self,
        ctx: &SecurityContext,
        resource_gts_type: &str,
        action: &str,
    ) -> DomainResult<()> {
        let Some(enforcer) = &self.enforcer else {
            return Ok(());
        };
        let resource = ResourceType::new(
            resource_gts_type.to_owned(),
            &[pep_properties::OWNER_TENANT_ID],
        );
        let request = AccessRequest::new()
            .resource_property(
                pep_properties::OWNER_TENANT_ID,
                ctx.subject_tenant_id(),
            )
            .require_constraints(false);

        match enforcer
            .access_scope_with(ctx, &resource, action, None, &request)
            .await
        {
            Ok(_) => Ok(()),
            Err(EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_)) => {
                Err(DomainError::forbidden(format!(
                    "not permitted to {action} '{resource_gts_type}'"
                )))
            }
            Err(EnforcerError::EvaluationFailed(source)) => Err(DomainError::link_unavailable(
                format!("authorization evaluation failed: {source}"),
            )),
        }
    }
}
