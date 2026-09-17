//! REST route registration for the OAGW management plane.
//!
//! Routes mount at `/oagw/v1/...` — gear-relative, without the `/api` prefix
//! the `DESIGN.md` table shows, which is a platform-level prefix applied by
//! the hosting gateway.
use axum::{Extension, Router};
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder,
};

use super::dto::{
    CreatePluginRequest, EndpointPoolDto, PluginChainDto, PluginDto, PluginListDto,
    PluginSourceDto, RouteDto, RouteListDto, RouteRequest, UpstreamDto, UpstreamListDto,
    UpstreamRequest,
};
use super::handlers::{self, SharedControlPlane};

const UPSTREAM_TAG: &str = "OAGW Upstreams";
const ROUTE_TAG: &str = "OAGW Routes";
const PLUGIN_TAG: &str = "OAGW Plugins";

/// The `$filter` / `$select` / `$orderby` / `$top` / `$skip` parameters every
/// list endpoint accepts.
///
/// The platform's OData extractor rejects `$skip`, so the gear parses the list
/// parameters itself (see [`crate::api::rest::params`]); the parameters are
/// still declared here so the OpenAPI document matches the wire.
macro_rules! list_params {
    ($builder:expr) => {
        $builder
            .query_param("$filter", false, "OData filter expression")
            .query_param("$select", false, "Comma-separated fields to project")
            .query_param("$orderby", false, "Sort expression, e.g. `created_at desc`")
            .query_param("$top", false, "Page size (default 50, maximum 100)")
            .query_param("$skip", false, "Page offset")
    };
}

struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Registers all management REST routes for the OAGW gear.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: SharedControlPlane,
) -> Router {
    let router = upstreams(router, openapi);
    let router = endpoint_pools(router, openapi);
    let router = plugin_chains(router, openapi);
    let router = routes_group(router, openapi);
    let router = plugins(router, openapi);
    let router = crate::api::proxy::routes::register_proxy(router, openapi);
    router.layer(Extension(service))
}

fn upstreams(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // POST /oagw/v1/upstreams
    let router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create upstream")
        .description(
            "Create an upstream. The alias is derived from the endpoint hostnames unless it is \
             supplied explicitly; uniqueness is enforced per tenant.",
        )
        .tag(UPSTREAM_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<UpstreamRequest>(openapi, "Upstream to create")
        .handler(handlers::create_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::CREATED, "Created upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /oagw/v1/upstreams
    let router = list_params!(
        OperationBuilder::get("/oagw/v1/upstreams")
            .operation_id("oagw.list_upstreams")
            .summary("List upstreams")
            .description(
                "List the calling tenant's upstreams with OData-style `$filter`, `$select`, \
             `$orderby`, `$top` and `$skip` parameters.",
            )
            .tag(UPSTREAM_TAG)
            .authenticated()
            .require_license_features::<License>([])
    )
    .handler(handlers::list_upstreams)
    .json_response_with_schema::<UpstreamListDto>(openapi, StatusCode::OK, "One page")
    .standard_errors(openapi)
    .register(router, openapi);

    // GET /oagw/v1/upstreams/{id}
    let router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get upstream")
        .description("Retrieve a single upstream by id.")
        .tag(UPSTREAM_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the upstream")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not found")
        .standard_errors(openapi)
        .register(router, openapi);

    // PUT /oagw/v1/upstreams/{id}
    let router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace upstream")
        .description(
            "Full replacement. Omitted optional fields are cleared and the alias is \
             re-validated against the derivation rules.",
        )
        .tag(UPSTREAM_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the upstream")
        .json_request::<UpstreamRequest>(openapi, "Replacement upstream")
        .handler(handlers::replace_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The replaced upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/upstreams/{id}
    let router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete upstream")
        .description("Delete an upstream and cascade the delete to its routes.")
        .tag(UPSTREAM_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the upstream")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    // POST /oagw/v1/upstreams/{id}/enable
    let router = OperationBuilder::post("/oagw/v1/upstreams/{id}/enable")
        .operation_id("oagw.enable_upstream")
        .summary("Enable upstream")
        .description("Enable an upstream so the proxy can match it again.")
        .tag(UPSTREAM_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the upstream")
        .handler(handlers::enable_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The enabled upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    // POST /oagw/v1/upstreams/{id}/disable
    OperationBuilder::post("/oagw/v1/upstreams/{id}/disable")
        .operation_id("oagw.disable_upstream")
        .summary("Disable upstream")
        .description("Disable an upstream so the proxy stops matching it.")
        .tag(UPSTREAM_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the upstream")
        .handler(handlers::disable_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The disabled upstream")
        .standard_errors(openapi)
        .register(router, openapi)
}

fn endpoint_pools(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // GET /oagw/v1/upstreams/{id}/endpoints
    let router = OperationBuilder::get("/oagw/v1/upstreams/{id}/endpoints")
        .operation_id("oagw.get_endpoints")
        .summary("Get the endpoint pool")
        .description("Read the endpoint pool of an upstream.")
        .tag(UPSTREAM_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the upstream")
        .handler(handlers::get_endpoints)
        .json_response_with_schema::<EndpointPoolDto>(openapi, StatusCode::OK, "The endpoint pool")
        .standard_errors(openapi)
        .register(router, openapi);

    // POST /oagw/v1/upstreams/{id}/endpoints
    let router = OperationBuilder::post("/oagw/v1/upstreams/{id}/endpoints")
        .operation_id("oagw.add_endpoints")
        .summary("Append endpoints")
        .description("Append endpoints to the pool. Already-pooled endpoints are ignored.")
        .tag(UPSTREAM_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the upstream")
        .json_request::<EndpointPoolDto>(openapi, "Endpoints to append")
        .handler(handlers::add_endpoints)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The updated upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    // PUT /oagw/v1/upstreams/{id}/endpoints
    let router = OperationBuilder::put("/oagw/v1/upstreams/{id}/endpoints")
        .operation_id("oagw.replace_endpoints")
        .summary("Replace the endpoint pool")
        .description("Replace the whole endpoint pool of an upstream.")
        .tag(UPSTREAM_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the upstream")
        .json_request::<EndpointPoolDto>(openapi, "The new endpoint pool")
        .handler(handlers::replace_endpoints)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The updated upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/upstreams/{id}/endpoints/{position}
    OperationBuilder::delete("/oagw/v1/upstreams/{id}/endpoints/{position}")
        .operation_id("oagw.delete_endpoint")
        .summary("Remove an endpoint")
        .description("Remove one endpoint from the pool by its position in the pool.")
        .tag(UPSTREAM_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the upstream")
        .path_param(
            "position",
            "Zero-based position of the endpoint in the pool",
        )
        .handler(handlers::delete_endpoint)
        .no_content_response(StatusCode::NO_CONTENT, "Removed")
        .standard_errors(openapi)
        .register(router, openapi)
}

fn plugin_chains(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // GET /oagw/v1/upstreams/{id}/plugins
    let router = OperationBuilder::get("/oagw/v1/upstreams/{id}/plugins")
        .operation_id("oagw.get_upstream_plugins")
        .summary("Get the upstream plugin chain")
        .description("Read the ordered plugin chain of an upstream.")
        .tag(UPSTREAM_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the upstream")
        .handler(handlers::get_upstream_plugins)
        .json_response_with_schema::<PluginChainDto>(openapi, StatusCode::OK, "The plugin chain")
        .standard_errors(openapi)
        .register(router, openapi);

    // POST /oagw/v1/upstreams/{id}/plugins
    let router = OperationBuilder::post("/oagw/v1/upstreams/{id}/plugins")
        .operation_id("oagw.add_upstream_plugins")
        .summary("Append plugin bindings")
        .description("Append plugin references to the end of the upstream's chain.")
        .tag(UPSTREAM_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the upstream")
        .json_request::<PluginChainDto>(openapi, "Plugin references to append")
        .handler(handlers::add_upstream_plugins)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The updated upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    // PUT /oagw/v1/upstreams/{id}/plugins
    let router = OperationBuilder::put("/oagw/v1/upstreams/{id}/plugins")
        .operation_id("oagw.replace_upstream_plugins")
        .summary("Replace the upstream plugin chain")
        .description("Replace the whole plugin chain of an upstream.")
        .tag(UPSTREAM_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the upstream")
        .json_request::<PluginChainDto>(openapi, "The new plugin chain")
        .handler(handlers::put_upstream_plugins)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The updated upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/upstreams/{id}/plugins/{position}
    let router = OperationBuilder::delete("/oagw/v1/upstreams/{id}/plugins/{position}")
        .operation_id("oagw.delete_upstream_plugin")
        .summary("Remove a plugin binding")
        .description("Remove one binding from the upstream's chain by position.")
        .tag(UPSTREAM_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the upstream")
        .path_param(
            "position",
            "Zero-based position of the binding in the chain",
        )
        .handler(handlers::delete_upstream_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Removed")
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /oagw/v1/routes/{id}/plugins
    let router = OperationBuilder::get("/oagw/v1/routes/{id}/plugins")
        .operation_id("oagw.get_route_plugins")
        .summary("Get the route plugin chain")
        .description("Read the ordered plugin chain of a route.")
        .tag(ROUTE_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the route")
        .handler(handlers::get_route_plugins)
        .json_response_with_schema::<PluginChainDto>(openapi, StatusCode::OK, "The plugin chain")
        .standard_errors(openapi)
        .register(router, openapi);

    // POST /oagw/v1/routes/{id}/plugins
    let router = OperationBuilder::post("/oagw/v1/routes/{id}/plugins")
        .operation_id("oagw.add_route_plugins")
        .summary("Append plugin bindings")
        .description("Append plugin references to the end of the route's chain.")
        .tag(ROUTE_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the route")
        .json_request::<PluginChainDto>(openapi, "Plugin references to append")
        .handler(handlers::add_route_plugins)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The updated route")
        .standard_errors(openapi)
        .register(router, openapi);

    // PUT /oagw/v1/routes/{id}/plugins
    let router = OperationBuilder::put("/oagw/v1/routes/{id}/plugins")
        .operation_id("oagw.replace_route_plugins")
        .summary("Replace the route plugin chain")
        .description("Replace the whole plugin chain of a route.")
        .tag(ROUTE_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the route")
        .json_request::<PluginChainDto>(openapi, "The new plugin chain")
        .handler(handlers::put_route_plugins)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The updated route")
        .standard_errors(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/routes/{id}/plugins/{position}
    OperationBuilder::delete("/oagw/v1/routes/{id}/plugins/{position}")
        .operation_id("oagw.delete_route_plugin")
        .summary("Remove a plugin binding")
        .description("Remove one binding from the route's chain by position.")
        .tag(ROUTE_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the route")
        .path_param(
            "position",
            "Zero-based position of the binding in the chain",
        )
        .handler(handlers::delete_route_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Removed")
        .standard_errors(openapi)
        .register(router, openapi)
}

fn routes_group(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // POST /oagw/v1/routes
    let router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create route")
        .description(
            "Create a route against an upstream of the calling tenant. Ancestor upstreams are \
             not addressable and yield 404.",
        )
        .tag(ROUTE_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<RouteRequest>(openapi, "Route to create")
        .handler(handlers::create_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::CREATED, "Created route")
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /oagw/v1/routes
    let router = list_params!(
        OperationBuilder::get("/oagw/v1/routes")
            .operation_id("oagw.list_routes")
            .summary("List routes")
            .description(
                "List the calling tenant's routes with OData-style `$filter`, `$select`, \
             `$orderby`, `$top` and `$skip` parameters.",
            )
            .tag(ROUTE_TAG)
            .authenticated()
            .require_license_features::<License>([])
    )
    .handler(handlers::list_routes)
    .json_response_with_schema::<RouteListDto>(openapi, StatusCode::OK, "One page")
    .standard_errors(openapi)
    .register(router, openapi);

    // GET /oagw/v1/routes/{id}
    let router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get route")
        .description("Retrieve a single route by id.")
        .tag(ROUTE_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the route")
        .handler(handlers::get_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Route not found")
        .standard_errors(openapi)
        .register(router, openapi);

    // PUT /oagw/v1/routes/{id}
    let router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace route")
        .description(
            "Full replacement. `upstream_id` is immutable and re-declaring it with a different \
             value is rejected.",
        )
        .tag(ROUTE_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the route")
        .json_request::<RouteRequest>(openapi, "Replacement route")
        .handler(handlers::replace_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The replaced route")
        .standard_errors(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/routes/{id}
    let router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete route")
        .description("Delete a route of the calling tenant.")
        .tag(ROUTE_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the route")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    // POST /oagw/v1/routes/{id}/enable
    let router = OperationBuilder::post("/oagw/v1/routes/{id}/enable")
        .operation_id("oagw.enable_route")
        .summary("Enable route")
        .description("Enable a route so the proxy matches it again.")
        .tag(ROUTE_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the route")
        .handler(handlers::enable_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The enabled route")
        .standard_errors(openapi)
        .register(router, openapi);

    // POST /oagw/v1/routes/{id}/disable
    OperationBuilder::post("/oagw/v1/routes/{id}/disable")
        .operation_id("oagw.disable_route")
        .summary("Disable route")
        .description("Disable a route so the proxy stops matching it.")
        .tag(ROUTE_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the route")
        .handler(handlers::disable_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The disabled route")
        .standard_errors(openapi)
        .register(router, openapi)
}

fn plugins(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // POST /oagw/v1/plugins
    let router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create plugin")
        .description("Create a custom Starlark plugin. Plugins are immutable once created.")
        .tag(PLUGIN_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<CreatePluginRequest>(openapi, "Plugin to create")
        .handler(handlers::create_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::CREATED, "Created plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /oagw/v1/plugins
    let router = list_params!(
        OperationBuilder::get("/oagw/v1/plugins")
            .operation_id("oagw.list_plugins")
            .summary("List plugins")
            .description(
                "List the calling tenant's plugins with OData-style `$filter`, `$select`, \
             `$orderby`, `$top` and `$skip` parameters.",
            )
            .tag(PLUGIN_TAG)
            .authenticated()
            .require_license_features::<License>([])
    )
    .handler(handlers::list_plugins)
    .json_response_with_schema::<PluginListDto>(openapi, StatusCode::OK, "One page")
    .standard_errors(openapi)
    .register(router, openapi);

    // GET /oagw/v1/plugins/{id}
    let router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get plugin")
        .description("Retrieve a plugin row, without its Starlark source.")
        .tag(PLUGIN_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the plugin")
        .handler(handlers::get_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::OK, "The plugin")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Plugin not found")
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /oagw/v1/plugins/{id}/source
    let router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get plugin source")
        .description("Retrieve the sandboxed Starlark source of a plugin.")
        .tag(PLUGIN_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the plugin")
        .handler(handlers::get_plugin_source)
        .json_response_with_schema::<PluginSourceDto>(openapi, StatusCode::OK, "The plugin source")
        .standard_errors(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/plugins/{id}
    OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete plugin")
        .description(
            "Delete an unreferenced plugin. A plugin still referenced by an upstream or a route \
             is rejected with 409 and the referencing resources.",
        )
        .tag(PLUGIN_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Anonymous GTS id or UUID of the plugin")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi)
}
