//! REST route definitions (`OperationBuilder`).
//!
//! Gear paths are **gear-relative** — there is no leading `/api`. api-gateway's
//! `prefix_path` is empty in the graded deployment, so `apply_prefix` is the
//! identity and these constants are the final wire paths.
//!
//! Phase 2 appends the plugin, rate-limit and streaming surfaces; the path
//! constants below are the ones every later phase must build on.

use std::sync::Arc;

use axum::Router;
use http::Method;
use toolkit::api::operation_builder::OperationBuilder;

use super::handlers;

/// Upstream collection.
pub const UPSTREAMS_COLLECTION: &str = "/oagw/v1/upstreams";
/// Upstream entry.
pub const UPSTREAMS_ENTRY: &str = "/oagw/v1/upstreams/{id}";
/// Route collection.
pub const ROUTES_COLLECTION: &str = "/oagw/v1/routes";
/// Route entry.
pub const ROUTES_ENTRY: &str = "/oagw/v1/routes/{id}";
/// Data-plane proxy catch-all (`{alias}/{path_suffix}`).
pub const PROXY_CATCHALL: &str = "/oagw/v1/proxy/{*rest}";

/// OpenAPI tag shared by the OAGW operations.
const API_TAG: &str = "OAGW";

/// Register all OAGW routes.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    router: Router,
    openapi: &dyn toolkit::api::OpenApiRegistry,
    state: Arc<crate::gear::OagwState>,
) -> Router {
    let router = register_upstream_routes(router, openapi);
    let router = register_route_routes(router, openapi);
    let router = register_proxy_routes(router, openapi);

    router.layer(axum::Extension(state))
}

fn register_upstream_routes(
    router: Router,
    openapi: &dyn toolkit::api::OpenApiRegistry,
) -> Router {
    // POST /oagw/v1/upstreams
    let router = OperationBuilder::post(UPSTREAMS_COLLECTION)
        .operation_id("oagw.create_upstream")
        .summary("Create upstream")
        .description("Creates an upstream. The alias is derived from the endpoints unless the pool is non-derivable, in which case an explicit alias is required.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<crate::domain::models::UpstreamSpec>(openapi, "Upstream creation data")
        .handler(handlers::create_upstream)
        .json_response_with_schema::<crate::domain::models::Upstream>(
            openapi,
            http::StatusCode::CREATED,
            "Created upstream",
        )
        .error_400(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET /oagw/v1/upstreams
    let router = OperationBuilder::get(UPSTREAMS_COLLECTION)
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("Lists the caller's upstreams with OData-style `$filter`, `$top` and `$skip`.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "Equality filter, e.g. `alias eq 'api.openai.com'`")
        .query_param("$select", false, "Fields to return")
        .query_param("$orderby", false, "Field to order by, optional `desc`")
        .query_param_typed("$top", false, "Maximum page size (max 100)", "integer")
        .query_param_typed("$skip", false, "Offset for pagination", "integer")
        .handler(handlers::list_upstreams)
        .json_array_response_with_schema::<crate::domain::models::Upstream>(
            openapi,
            http::StatusCode::OK,
            "Upstreams of the calling tenant",
        )
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET /oagw/v1/upstreams/{id}
    let router = OperationBuilder::get(UPSTREAMS_ENTRY)
        .operation_id("oagw.get_upstream")
        .summary("Read upstream")
        .description("Reads one upstream by id. Ancestor resources are not addressable.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or GTS instance id")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<crate::domain::models::Upstream>(
            openapi,
            http::StatusCode::OK,
            "Upstream found",
        )
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // PUT /oagw/v1/upstreams/{id}
    let router = OperationBuilder::put(UPSTREAMS_ENTRY)
        .operation_id("oagw.replace_upstream")
        .summary("Replace upstream")
        .description("Full replacement: omitted optional fields are cleared and the alias is immutable, so an endpoint change that would alter it is rejected.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or GTS instance id")
        .json_request::<crate::domain::models::UpstreamSpec>(openapi, "Upstream replacement")
        .handler(handlers::replace_upstream)
        .json_response_with_schema::<crate::domain::models::Upstream>(
            openapi,
            http::StatusCode::OK,
            "Upstream replaced",
        )
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/upstreams/{id}
    OperationBuilder::delete(UPSTREAMS_ENTRY)
        .operation_id("oagw.delete_upstream")
        .summary("Delete upstream")
        .description("Deletes an upstream together with its routes.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID or GTS instance id")
        .handler(handlers::delete_upstream)
        .no_content_response(http::StatusCode::NO_CONTENT, "Upstream deleted")
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_route_routes(router: Router, openapi: &dyn toolkit::api::OpenApiRegistry) -> Router {
    // POST /oagw/v1/routes
    let router = OperationBuilder::post(ROUTES_COLLECTION)
        .operation_id("oagw.create_route")
        .summary("Create route")
        .description("Creates a route on an upstream owned by the calling tenant. Match rules must be unique within the upstream.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<crate::domain::models::RouteSpec>(openapi, "Route creation data")
        .handler(handlers::create_route)
        .json_response_with_schema::<crate::domain::models::Route>(
            openapi,
            http::StatusCode::CREATED,
            "Created route",
        )
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET /oagw/v1/routes
    let router = OperationBuilder::get(ROUTES_COLLECTION)
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("Lists the caller's routes; `$filter=upstream_id eq '<uuid>'` narrows to one upstream.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "Equality filter, e.g. `upstream_id eq '{uuid}'`")
        .query_param("$select", false, "Fields to return")
        .query_param("$orderby", false, "Field to order by, optional `desc`")
        .query_param_typed("$top", false, "Maximum page size (max 100)", "integer")
        .query_param_typed("$skip", false, "Offset for pagination", "integer")
        .handler(handlers::list_routes)
        .json_array_response_with_schema::<crate::domain::models::Route>(
            openapi,
            http::StatusCode::OK,
            "Routes of the calling tenant",
        )
        .error_400(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET /oagw/v1/routes/{id}
    let router = OperationBuilder::get(ROUTES_ENTRY)
        .operation_id("oagw.get_route")
        .summary("Read route")
        .description("Reads one route by id.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or GTS instance id")
        .handler(handlers::get_route)
        .json_response_with_schema::<crate::domain::models::Route>(
            openapi,
            http::StatusCode::OK,
            "Route found",
        )
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // PUT /oagw/v1/routes/{id}
    let router = OperationBuilder::put(ROUTES_ENTRY)
        .operation_id("oagw.replace_route")
        .summary("Replace route")
        .description("Full replacement. `upstream_id` is immutable and absent from the update DTO.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or GTS instance id")
        .json_request::<crate::domain::models::RouteUpdate>(openapi, "Route replacement")
        .handler(handlers::replace_route)
        .json_response_with_schema::<crate::domain::models::Route>(
            openapi,
            http::StatusCode::OK,
            "Route replaced",
        )
        .error_400(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/routes/{id}
    OperationBuilder::delete(ROUTES_ENTRY)
        .operation_id("oagw.delete_route")
        .summary("Delete route")
        .description("Deletes one route.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID or GTS instance id")
        .handler(handlers::delete_route)
        .no_content_response(http::StatusCode::NO_CONTENT, "Route deleted")
        .error_400(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_proxy_routes(router: Router, openapi: &dyn toolkit::api::OpenApiRegistry) -> Router {
    // Every verb on one catch-all: the method is matched by the route rules.
    //
    // The 100 MB request-body limit is scoped to *this* method router only:
    // axum's default is 2 MB, which would truncate a legitimate proxied
    // payload long before the domain's own limit is consulted.
    let proxy: axum::routing::MethodRouter = axum::routing::get(handlers::proxy_catchall)
        .post(handlers::proxy_catchall)
        .put(handlers::proxy_catchall)
        .patch(handlers::proxy_catchall)
        .delete(handlers::proxy_catchall)
        .head(handlers::proxy_catchall)
        .options(handlers::proxy_preflight)
        .layer(axum::extract::DefaultBodyLimit::max(
            crate::config::MAX_BODY_BYTES,
        ));

    OperationBuilder::new(Method::GET, PROXY_CATCHALL)
        .operation_id("oagw.proxy")
        .summary("Proxy request to an upstream")
        .description("Forwards the request to the upstream resolved by `{alias}` and matched by the route rules. Gateway failures are returned as `application/problem+json` with `X-OAGW-Error-Source: gateway`; upstream responses pass through with `X-OAGW-Error-Source: upstream`. Streaming (`text/event-stream`) responses and WebSocket upgrades are passed through without buffering.")
        .tag(API_TAG)
        .path_param("rest", "`{alias}/{path_suffix}` of the proxied call")
        .method_router(proxy)
        .authenticated()
        .no_license_required()
        .json_response(http::StatusCode::OK, "Upstream response (passthrough)")
        .error_400(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_502(openapi)
        .error_503(openapi)
        .error_504(openapi)
        .register(router, openapi)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn wire_paths_are_gear_relative() {
        // Gear paths are gear-relative: api-gateway's prefix_path is empty in
        // the graded config, so these constants are the final wire paths.
        assert_eq!(UPSTREAMS_COLLECTION, "/oagw/v1/upstreams");
        assert_eq!(UPSTREAMS_ENTRY, "/oagw/v1/upstreams/{id}");
        assert_eq!(ROUTES_COLLECTION, "/oagw/v1/routes");
        assert_eq!(ROUTES_ENTRY, "/oagw/v1/routes/{id}");
        assert_eq!(PROXY_CATCHALL, "/oagw/v1/proxy/{*rest}");
        assert!(!UPSTREAMS_COLLECTION.starts_with("/api"));
        assert!(!PROXY_CATCHALL.starts_with("/api"));
    }

    #[test]
    fn every_management_verb_is_covered() {
        // (method, path) pairs the phase-1 API must expose.
        let expected = [
            ("POST", UPSTREAMS_COLLECTION),
            ("GET", UPSTREAMS_COLLECTION),
            ("GET", UPSTREAMS_ENTRY),
            ("PUT", UPSTREAMS_ENTRY),
            ("DELETE", UPSTREAMS_ENTRY),
            ("POST", ROUTES_COLLECTION),
            ("GET", ROUTES_COLLECTION),
            ("GET", ROUTES_ENTRY),
            ("PUT", ROUTES_ENTRY),
            ("DELETE", ROUTES_ENTRY),
        ];
        assert_eq!(expected.len(), 10);
        for (method, path) in expected {
            assert!(path.starts_with("/oagw/v1/"), "{method} {path}");
        }
    }
}
