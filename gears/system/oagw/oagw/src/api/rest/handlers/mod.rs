//! REST handlers.

use toolkit_security::SecurityContext;

pub mod proxy;
pub mod routes;
pub mod upstreams;

pub(crate) use proxy::proxy_catchall;
pub(crate) use proxy::proxy_preflight;
pub(crate) use routes::create_route;
pub(crate) use routes::delete_route;
pub(crate) use routes::get_route;
pub(crate) use routes::list_routes;
pub(crate) use routes::replace_route;
pub(crate) use upstreams::create_upstream;
pub(crate) use upstreams::delete_upstream;
pub(crate) use upstreams::get_upstream;
pub(crate) use upstreams::list_upstreams;
pub(crate) use upstreams::replace_upstream;

/// Tenant of the caller (management API scoping).
pub(crate) fn caller_tenant(ctx: &SecurityContext) -> uuid::Uuid {
    ctx.subject_tenant_id()
}
