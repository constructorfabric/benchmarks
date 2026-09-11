//! REST handlers for the OAGW gear's management endpoints.

use axum::extract::Extension;
use toolkit_security::SecurityContext;
use uuid::Uuid;

pub mod plugins;
pub mod proxy;
pub mod routes;
pub mod upstreams;

/// Resolves the calling tenant id from the optional `SecurityContext`
/// extension, falling back to the nil UUID tenant when it is absent
/// (`CODE1-F-004`: previously duplicated identically in
/// `handlers/upstreams.rs`, `handlers/routes.rs`, and `handlers/plugins.rs`).
pub(crate) fn tenant_id_of(security_ctx: Option<Extension<SecurityContext>>) -> Uuid {
    security_ctx
        .map(|Extension(ctx)| ctx.subject_tenant_id())
        .unwrap_or_default()
}
