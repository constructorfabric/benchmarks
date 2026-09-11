//! The registered REST route tree
//! (`cpt-cf-oagw-dod-gear-foundation-rest-registration`).
//!
//! The tree is **gear-relative**: `/oagw/v1/...` with **no leading `/api`**
//! segment, because the api-gateway gear owns the router and its
//! `prefix_path` is empty in the graded configuration (`inst-gf-rest-1`).
//!
//! Entry 2.1 declares the tree and registers **nothing**: the handler bodies
//! for upstreams, routes, plugins and proxy requests belong to entries 2.2,
//! 2.3, 2.4 and 2.6. [`route_tree`] is the declarative description of the
//! four prefixes, and [`register_rest`] returns the router unchanged while
//! publishing the shared service handles, so a registered path addressed
//! before its owning entry lands serves no configuration data and makes no
//! domain service call (`inst-gf-rest-5`).
//!
//! Inbound bearer authentication on `/oagw/v1/...` is required by the
//! api-gateway's `require_auth_by_default` policy; the handlers read the
//! security context the authentication middleware resolved ahead of them and
//! reject a missing one with the `401` OAGW authentication surface
//! (`inst-gf-rest-3`/`-4`).
//!
//! Entry 2.2 fills the upstream prefix in: [`upstream_routes`] registers the
//! five operations of `/oagw/v1/upstreams`. Entry 2.3 fills the route prefix in
//! the same way: [`route_routes`] registers the five operations of
//! `/oagw/v1/routes`. Entry 2.4 fills the proxy prefix in:
//! [`proxy_routes`] registers the catch-all `/oagw/v1/proxy/{alias}` and
//! `/oagw/v1/proxy/{alias}/{*path_suffix}` pair. The plugins prefix stays
//! declared and unserviced, because its owning entry has not landed.

use std::sync::Arc;

use toolkit::api::OpenApiRegistry;
use toolkit::context::GearCtx;

use crate::domain::services::management::UpstreamManagementService;
use crate::domain::services::plugin_management::PluginManagementService;
use crate::domain::services::route_management::RouteManagementService;
use crate::domain::services::ControlPlaneService;

pub mod dto;
pub mod error;
pub mod metrics_routes;
pub mod plugin_handlers;
pub mod plugin_routes;
pub mod proxy_handlers;
pub mod proxy_routes;
pub mod route_handlers;
pub mod route_routes;
pub mod upstream_handlers;
pub mod upstream_routes;

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;

#[cfg(test)]
#[path = "error_contract_tests.rs"]
mod error_contract_tests;

#[cfg(test)]
#[path = "dto_tests.rs"]
mod dto_tests;

/// The single gear-relative prefix every OAGW path lives under.
pub const PREFIX: &str = "/oagw/v1";

/// `/oagw/v1/upstreams`
pub const UPSTREAMS_PATH: &str = "/oagw/v1/upstreams";
/// `/oagw/v1/upstreams/{id}`
pub const UPSTREAM_BY_ID_PATH: &str = "/oagw/v1/upstreams/{id}";
/// `/oagw/v1/routes`
pub const ROUTES_PATH: &str = "/oagw/v1/routes";
/// `/oagw/v1/routes/{id}`
pub const ROUTE_BY_ID_PATH: &str = "/oagw/v1/routes/{id}";
/// `/oagw/v1/plugins`
pub const PLUGINS_PATH: &str = "/oagw/v1/plugins";
/// `/oagw/v1/plugins/{id}`
pub const PLUGIN_BY_ID_PATH: &str = "/oagw/v1/plugins/{id}";
/// `GET /oagw/v1/plugins/{id}/source` — the registered source content of one
/// custom plugin (`cpt-cf-oagw-dod-plugin-system-management-api`).
pub const PLUGIN_SOURCE_PATH: &str = "/oagw/v1/plugins/{id}/source";
/// `/oagw/v1/proxy/{alias}[/{path_suffix}]`
pub const PROXY_PATH: &str = "/oagw/v1/proxy/{alias}";
/// `/oagw/v1/proxy/{alias}/{*path_suffix}`
pub const PROXY_SUFFIX_PATH: &str = "/oagw/v1/proxy/{alias}/{*path_suffix}";
/// `GET /metrics` — gear-relative and **outside** the `/oagw/v1` prefix, because
/// the exposition is an operations surface, not a configuration API
/// (`cpt-cf-oagw-dod-observability-and-state-metric-surface`).
pub const METRICS_PATH: &str = "/metrics";

/// The four top-level prefixes the FEATURE names
/// (`cpt-cf-oagw-dod-gear-foundation-rest-registration`).
pub const REGISTRABLE_PREFIXES: [&str; 4] =
    [UPSTREAMS_PATH, ROUTES_PATH, PLUGINS_PATH, PROXY_PATH];

/// One declaratively registered (method, path) pair of the route tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisteredRoute {
    /// The HTTP method, or `"*"` for a path the tree registers for every
    /// method.
    pub method: &'static str,
    /// The full gear-relative path, with no leading `/api` segment.
    pub path: &'static str,
}

/// The declarative route tree this entry registers.
///
/// The proxy prefix accepts every HTTP method; the management prefixes are the
/// collection and by-identifier paths entries 2.2 and 2.3 fill in.
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-rest-registration:p1:inst-gf-rest-3
// Every path is gear-relative (`/oagw/v1/...`) with no `/api` segment and no
// gear-supplied prefix, and the proxy pair is registered for every HTTP
// method because the proxy path is method-agnostic.
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-rest-registration:p1:inst-gf-rest-5
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-rest-registration:p1:inst-gf-rest-6
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-rest-registration:p1:inst-gf-rest-3
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-rest-registration:p1:inst-gf-rest-2
#[must_use]
pub fn route_tree() -> Vec<RegisteredRoute> {
    let mut routes = vec![
        RegisteredRoute { method: "POST", path: UPSTREAMS_PATH },
        RegisteredRoute { method: "GET", path: UPSTREAMS_PATH },
        RegisteredRoute { method: "GET", path: UPSTREAM_BY_ID_PATH },
        RegisteredRoute { method: "PUT", path: UPSTREAM_BY_ID_PATH },
        RegisteredRoute { method: "DELETE", path: UPSTREAM_BY_ID_PATH },
        RegisteredRoute { method: "POST", path: ROUTES_PATH },
        RegisteredRoute { method: "GET", path: ROUTES_PATH },
        RegisteredRoute { method: "GET", path: ROUTE_BY_ID_PATH },
        RegisteredRoute { method: "PUT", path: ROUTE_BY_ID_PATH },
        RegisteredRoute { method: "DELETE", path: ROUTE_BY_ID_PATH },
        RegisteredRoute { method: "POST", path: PLUGINS_PATH },
        RegisteredRoute { method: "GET", path: PLUGINS_PATH },
        RegisteredRoute { method: "GET", path: PLUGIN_BY_ID_PATH },
        // The source path is the fifth plugin operation; there is deliberately
        // no `PUT` or `PATCH` on the plugin path, because a custom plugin is
        // immutable after creation.
        RegisteredRoute { method: "GET", path: PLUGIN_SOURCE_PATH },
        RegisteredRoute { method: "DELETE", path: PLUGIN_BY_ID_PATH },
    ];
    for method in ["GET", "POST", "PUT", "DELETE", "PATCH"] {
        routes.push(RegisteredRoute { method, path: PROXY_PATH });
        routes.push(RegisteredRoute { method, path: PROXY_SUFFIX_PATH });
    }
    routes
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-rest-registration:p1:inst-gf-rest-2
}
//
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-rest-registration:p1:inst-gf-rest-6
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-rest-registration:p1:inst-gf-rest-5
//

/// Whether `path` carries a leading `/api` segment — always a defect here.
#[must_use]
pub fn has_api_prefix(path: &str) -> bool {
    path.starts_with("/api/")
        || path == "/api"
        || path.starts_with("/api/oagw")
}

/// Publish the shared service handles and return the router unchanged.
///
/// The shared `ControlPlaneService` is published to the client hub rather
/// than layered onto the router as extension state: routes the later entries
/// add would not be covered by a layer applied now
/// (`inst-gf-rest-4`).
///
/// # Errors
///
/// Never errors for entry 2.1: the registration is declarative and the
/// router is returned as it was handed over.
pub fn register_rest(
    ctx: &GearCtx,
    router: axum::Router,
    _openapi: &dyn OpenApiRegistry,
    service: &Arc<dyn ControlPlaneService>,
) -> anyhow::Result<axum::Router> {
    // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-rest-registration:p1:inst-gf-rest-4
    // `inst-gf-rest-4`: the shared handles are attached so entries 2.2, 2.3,
    // 2.4 and 2.6 can supply handler bodies without altering the registration
    // skeleton. `inst-gf-rest-5`/`-6`: no handler is registered here, so a
    // path addressed before its owning entry lands takes the framework's
    // unmatched-response behavior and no repository or service call is made.
    ctx.client_hub().register::<dyn ControlPlaneService>(Arc::clone(service));
    // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-rest-registration:p1:inst-gf-rest-7
    Ok(router)
    // @cpt-end:cpt-cf-oagw-flow-gear-foundation-rest-registration:p1:inst-gf-rest-7
    // @cpt-end:cpt-cf-oagw-flow-gear-foundation-rest-registration:p1:inst-gf-rest-4
}

/// Register the upstream management surface of entry 2.2.
///
/// The management aggregate is handed over ready-built: the registration adds
/// the five operations of `/oagw/v1/upstreams` and nothing else, so the
/// declarative route tree of [`route_tree`] stays the description of what the
/// gear publishes.
pub fn register_upstream_management(
    router: axum::Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<UpstreamManagementService>,
) -> axum::Router {
    upstream_routes::register_upstream_routes(router, openapi, service)
}

/// Register the route management surface of entry 2.3.
///
/// The route aggregate is handed over ready-built: the registration adds the
/// five operations of `/oagw/v1/routes` and nothing else, so the declarative
/// route tree of [`route_tree`] stays the description of what the gear
/// publishes.
pub fn register_route_management(
    router: axum::Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<RouteManagementService>,
) -> axum::Router {
    route_routes::register_route_routes(router, openapi, service)
}

/// Register the plugin management surface of entry 2.6.
///
/// The plugin-catalog aggregate is handed over ready-built: the registration
/// adds the five operations of `/oagw/v1/plugins` and nothing else, so the
/// declarative route tree of [`route_tree`] stays the description of what the
/// gear publishes.
pub fn register_plugin_management(
    router: axum::Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<PluginManagementService>,
) -> axum::Router {
    plugin_routes::register_plugin_routes(router, openapi, service)
}

/// Register the proxy data-plane surface of entry 2.4.
///
/// The data-plane pipeline is handed over ready-built: the registration adds
/// the two catch-all proxy paths and nothing else, and documents no OpenAPI
/// operation for them.
///
/// The shared error contract of entry 2.5 is layered on here, last, so it
/// wraps every route registered before it — the two management prefixes and
/// the proxy catch-all — and no surface registered after it can bypass it.
pub fn register_proxy_data_plane(
    router: axum::Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<crate::infra::proxy::DataPlaneServiceImpl>,
    observability: Arc<crate::infra::observability::Observability>,
) -> axum::Router {
    // @cpt-begin:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-1
    // `inst-eh-render-1` .. `-2`: every `/oagw/v1/...` endpoint renders its
    // errors through the one mapping layer and the one serializer, so the
    // completing layer is applied once, over the whole tree.
    let proxied = proxy_routes::register_proxy_routes(router, openapi, service)
        .layer(axum::Extension(observability));
    error::error_contract(proxied)
    // @cpt-end:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-1
}

/// Register the metrics surface of entry 2.9: `GET /metrics`, gear-relative and
/// outside the `/oagw/v1` prefix, behind the admin authorization boundary.
///
/// The registration adds the one operation, the registry and the gate as its
/// extension state. It is applied **before** the proxy surface so the
/// extension layers cover this route only, while the error contract the proxy
/// registration applies afterwards still wraps it.
pub fn register_metrics_surface(
    router: axum::Router,
    openapi: &dyn OpenApiRegistry,
    registry: Arc<crate::infra::metrics::MetricsRegistry>,
    gate: Arc<crate::infra::authorization::MetricsGate>,
) -> axum::Router {
    metrics_routes::register_metrics_routes(router, openapi, registry, gate)
}

#[cfg(test)]
#[path = "proxy_handler_tests.rs"]
mod proxy_handler_tests;
