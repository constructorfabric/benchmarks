//! Shared handler state and the authorization gate in front of it.

use std::sync::Arc;

use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_security::{SecurityContext, pep_properties};

use crate::config::OagwConfig;
use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::gts;
use crate::domain::model::PluginKind;
use crate::domain::services::ControlPlaneService;
use crate::infra::proxy::DataPlaneService;
use crate::infra::storage::InMemoryStore;

/// Constraint properties this PEP can compile. OAGW resources are all
/// tenant-owned, so the owner tenant is the only axis a policy can clamp on.
const SUPPORTED_PROPERTIES: &[&str] = &[pep_properties::OWNER_TENANT_ID];

/// Actions evaluated against the OAGW resource types.
pub mod actions {
    pub const CREATE: &str = "create";
    pub const READ: &str = "read";
    pub const OVERRIDE: &str = "override";
    pub const DELETE: &str = "delete";
    pub const INVOKE: &str = "invoke";
}

/// Everything the REST handlers need.
pub struct OagwState {
    pub control: Arc<ControlPlaneService>,
    pub data_plane: Arc<DataPlaneService>,
    pub store: Arc<InMemoryStore>,
    /// Absent when the deployment runs without an authorization resolver.
    pub authz: Option<PolicyEnforcer>,
    pub config: OagwConfig,
}

impl std::fmt::Debug for OagwState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OagwState")
            .field("authz", &self.authz.is_some())
            .finish_non_exhaustive()
    }
}

impl OagwState {
    /// Authorize `action` on `resource_type` for the caller.
    ///
    /// Constraints are optional: OAGW scopes every read and write to the
    /// caller's tenant itself, so the PDP's role here is the allow/deny
    /// decision rather than a row filter.
    ///
    /// # Errors
    ///
    /// `403` when the policy denies, `503` when the PDP cannot be reached.
    pub async fn authorize(
        &self,
        ctx: &SecurityContext,
        resource_type: &'static str,
        action: &str,
    ) -> OagwResult<()> {
        let Some(enforcer) = self.authz.as_ref() else {
            return Ok(());
        };
        let resource = ResourceType::from_static(resource_type, SUPPORTED_PROPERTIES);
        let request = AccessRequest::new()
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .require_constraints(false);

        enforcer
            .access_scope_with(ctx, &resource, action, None, &request)
            .await
            .map(|_scope| ())
            .map_err(|err| match err {
                EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => {
                    OagwError::new(
                        ErrorKind::PermissionDenied,
                        format!("not permitted to {action} {resource_type}"),
                    )
                }
                EnforcerError::EvaluationFailed(source) => OagwError::new(
                    ErrorKind::LinkUnavailable,
                    format!("authorization evaluation failed: {source}"),
                )
                .with_retry_after(5),
            })
    }

    /// Retire custom plugins that have been unlinked for longer than the
    /// configured TTL.
    ///
    /// The sweep is amortized onto the management API rather than run from a
    /// timer: it is cheap over an in-process store, and the moments a plugin
    /// can become unlinked — a binding removed, an upstream or route deleted —
    /// are exactly the calls that land here.
    pub fn sweep_unlinked_plugins(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let collected = self.store.run_gc(now, self.config.plugin_gc_ttl_secs);
        if !collected.is_empty() {
            tracing::info!(
                target: "oagw.plugin",
                count = collected.len(),
                "collected unlinked custom plugins"
            );
        }
    }

    /// Resource type identifier for a plugin kind.
    #[must_use]
    pub fn plugin_resource(kind: PluginKind) -> &'static str {
        match kind {
            PluginKind::Auth => gts::AUTH_PLUGIN_BASE,
            PluginKind::Guard => gts::GUARD_PLUGIN_BASE,
            PluginKind::Transform => gts::TRANSFORM_PLUGIN_BASE,
        }
    }
}
