//! Route registration for the OAGW control plane (`OperationBuilder` style).

use std::sync::Arc;

use axum::Router;
use tenant_resolver_sdk::TenantResolverClient;
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder,
};

use crate::domain::service::ControlPlaneService;
use crate::infra::plugin::AuthPluginRegistry;
use crate::infra::ratelimit::RateLimiter;

use super::handlers;

const API_TAG: &str = "OAGW";

struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Registers all OAGW REST routes (management + proxy) onto `router`.
///
/// All paths are gear-relative (`/oagw/v1/...`); the host runtime nests the
/// returned router under the configured `prefix_path`.
///
/// # Convention note
///
/// Routes are registered through `OperationBuilder` (path, auth axis, handler,
/// standard errors) exactly like the other gears. The `json_request<T>` /
/// `json_response_with_schema<T>` `OpenAPI` steps are omitted: request/response
/// wire types are the domain models, which do not carry the
/// `#[toolkit_macros::api_dto]` derives — the generated `OpenAPI` document for
/// these routes therefore has path-level metadata but no component schemas.
/// Runtime behavior is unaffected.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<ControlPlaneService>,
    tenant_resolver: Option<Arc<dyn TenantResolverClient>>,
    auth: Option<Arc<AuthPluginRegistry>>,
    rate: Option<Arc<RateLimiter>>,
) -> Router {
    // --- Upstreams ------------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream service")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::create_upstream)
        .json_response(StatusCode::OK, "Success")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstream services (OData-lite)")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::list_upstreams)
        .json_response(StatusCode::OK, "Success")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Fetch an upstream service")
        .tag(API_TAG)
        .authenticated()
        .path_param("id", "Upstream UUID")
        .require_license_features::<License>([])
        .handler(handlers::get_upstream)
        .json_response(StatusCode::OK, "Success")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.update_upstream")
        .summary("Update an upstream service (alias immutable)")
        .tag(API_TAG)
        .authenticated()
        .path_param("id", "Upstream UUID")
        .require_license_features::<License>([])
        .handler(handlers::update_upstream)
        .json_response(StatusCode::OK, "Success")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream service")
        .tag(API_TAG)
        .authenticated()
        .path_param("id", "Upstream UUID")
        .require_license_features::<License>([])
        .handler(handlers::delete_upstream)
        .json_response(StatusCode::OK, "Success")
        .standard_errors(openapi)
        .register(router, openapi);

    // --- Routes ----------------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::create_route)
        .json_response(StatusCode::OK, "Success")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes (OData-lite)")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::list_routes)
        .json_response(StatusCode::OK, "Success")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Fetch a route")
        .tag(API_TAG)
        .authenticated()
        .path_param("id", "Route UUID")
        .require_license_features::<License>([])
        .handler(handlers::get_route)
        .json_response(StatusCode::OK, "Success")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.update_route")
        .summary("Update a route (upstream_id immutable)")
        .tag(API_TAG)
        .authenticated()
        .path_param("id", "Route UUID")
        .require_license_features::<License>([])
        .handler(handlers::update_route)
        .json_response(StatusCode::OK, "Success")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .tag(API_TAG)
        .authenticated()
        .path_param("id", "Route UUID")
        .require_license_features::<License>([])
        .handler(handlers::delete_route)
        .json_response(StatusCode::OK, "Success")
        .standard_errors(openapi)
        .register(router, openapi);

    // --- Plugins ---------------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::create_plugin)
        .json_response(StatusCode::OK, "Success")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List custom plugins (OData-lite)")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::list_plugins)
        .json_response(StatusCode::OK, "Success")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Fetch a custom plugin")
        .tag(API_TAG)
        .authenticated()
        .path_param("id", "Plugin UUID")
        .require_license_features::<License>([])
        .handler(handlers::get_plugin)
        .json_response(StatusCode::OK, "Success")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Fetch a plugin's source text")
        .tag(API_TAG)
        .authenticated()
        .path_param("id", "Plugin UUID")
        .require_license_features::<License>([])
        .handler(handlers::get_plugin_source)
        .json_response(StatusCode::OK, "Success")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin (409 when in use)")
        .tag(API_TAG)
        .authenticated()
        .path_param("id", "Plugin UUID")
        .require_license_features::<License>([])
        .handler(handlers::delete_plugin)
        .json_response(StatusCode::OK, "Success")
        .standard_errors(openapi)
        .register(router, openapi);

    // --- Data plane ------------------------------------------------------
    // Registered directly with axum `any(...)` because the proxy accepts
    // every HTTP method (axum method-routers are per-method); the OperationBuilder
    // style is preserved for the management routes above. `{alias}` routes to an
    // upstream; `{*rest}` is the optional path suffix.
    router = router.route(
        "/oagw/v1/proxy/{alias}/{*rest}",
        axum::routing::any(handlers::proxy),
    );

    router = router.layer(axum::Extension(service));
    // Optional tenant-resolver client (None when the provider gear is absent
    // from the binary); the proxy handler falls back to the caller's own
    // tenant for alias resolution.
    router = router.layer(axum::Extension(tenant_resolver));
    // Optional auth-plugin registry (None in router-only tests); the proxy
    // handler executes the upstream's bound auth plugin when both are present.
    router = router.layer(axum::Extension(auth));
    // Optional DP-owned rate limiter (None in router-only tests); the proxy
    // handler enforces upstream/route rate limits on the shared buckets.
    router = router.layer(axum::Extension(rate));
    router
}
