// Created: 2026-08-29 by Constructor Tech
//! Axum HTTP handlers (management + proxy data plane).

pub mod management;
pub mod proxy;

pub use management::{
    create_plugin, create_route, create_upstream, delete_plugin, delete_route, delete_upstream,
    disable_upstream, enable_upstream, get_plugin, get_plugin_source, get_route, get_upstream,
    list_plugins, list_routes, list_upstreams, replace_route, replace_upstream,
};

use std::sync::Arc;

use axum::Extension;
use toolkit_security::SecurityContext;

use crate::domain::services::management::ControlPlaneService;

/// Control Plane + Data Plane services handed to the handlers.
#[derive(Clone)]
pub struct Services {
    /// Control Plane.
    pub control_plane: Arc<ControlPlaneService>,
    /// Data Plane.
    pub data_plane: Arc<crate::infra::proxy::service::DataPlaneService>,
}

/// Extract the calling tenant from the `SecurityContext`, falling back to the
/// anonymous tenant (nil UUID) when the host has no authn middleware.
///
/// Security: the tenant id is the only field used for scoping; bearer tokens
/// are never read, logged or forwarded by the management API.
#[must_use]
pub fn calling_tenant(context: Option<&SecurityContext>) -> uuid::Uuid {
    context.map_or_else(uuid::Uuid::nil, |ctx| ctx.subject_tenant_id())
}

/// Authenticated subject id, falling back to the nil UUID.
#[must_use]
pub fn calling_subject(context: Option<&SecurityContext>) -> uuid::Uuid {
    context.map_or_else(uuid::Uuid::nil, |ctx| ctx.subject_id())
}

/// The optional authn middleware extension. Handlers fall back to an
/// anonymous [`SecurityContext`] so a host without authn still serves.
pub(crate) type CtxExtension = Option<Extension<SecurityContext>>;

fn context_of(extension: &CtxExtension) -> Option<&SecurityContext> {
    extension.as_ref().map(|extension| &extension.0)
}

pub(crate) fn tenant_of(extension: &CtxExtension) -> uuid::Uuid {
    calling_tenant(context_of(extension))
}

/// Security context of the data plane, falling back to the anonymous context
/// when the host has no authn middleware.
pub(crate) fn calling_security(extension: &CtxExtension) -> SecurityContext {
    context_of(extension)
        .cloned()
        .unwrap_or_else(SecurityContext::anonymous)
}
