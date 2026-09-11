//! REST route registration of the route management surface (FEATURE entry
//! 2.3).
//!
//! The five operations live on the gear-relative paths entry 2.1 declared, and
//! every one of them is registered `authenticated()`, because the caller
//! identity the handlers read is produced by the platform authentication
//! middleware ahead of the handler. The list operation documents the same
//! OData system query options the upstream surface does, because both are
//! served by the one parser entry 2.2 delivered.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::OpenApiRegistry;

use super::dto::{RouteListResponse, RouteRequest, RouteResponse, RouteUpdateRequest};
use super::route_handlers;
use crate::domain::services::route_management::RouteManagementService;

/// The OpenAPI tag of the route management surface.
const TAG: &str = "OAGW Routes";

/// A create whose body violates a shape, a match block or a binding is a `400`;
/// a match-rule collision is a `409`.
const CREATE_BODY_DESCRIPTION: &str = "The route record. `id` and `tenant_id` are server-assigned and are rejected when supplied; `priority` defaults to `0` and `enabled` to `true`; exactly one of `match.http` or `match.grpc` is required.";

/// A replacement body, which carries no `upstream_id`.
const UPDATE_BODY_DESCRIPTION: &str = "The replacement route record. `upstream_id` is immutable and is rejected when supplied; omitted `rate_limit`, `cors`, `plugins` and `tags` are cleared; `priority` defaults to `0` and `enabled` to `true`, which re-enables a disabled route.";

/// Register the five route operations of the management surface.
pub fn register_route_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<RouteManagementService>,
) -> Router {
    let router = toolkit::api::OperationBuilder::post(super::ROUTES_PATH)
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description("Create a route of an upstream the authenticated tenant owns. The match block carries exactly one of `http` or `grpc`, `upstream_id` must resolve inside the calling tenant, and every `plugins.items[]` entry is resolved at binding time.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<RouteRequest>(openapi, CREATE_BODY_DESCRIPTION)
        .handler(route_handlers::create)
        .json_response_with_schema::<RouteResponse>(
            openapi,
            StatusCode::CREATED,
            "The stored route",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = toolkit::api::OperationBuilder::get(super::ROUTES_PATH)
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List the routes the authenticated tenant owns, filtered, projected, ordered and paginated by the OData system query options `$filter`, `$select`, `$orderby`, `$top` and `$skip`; `upstream_id eq '{uuid}'` selects one upstream's routes.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "An OData filter expression, such as `upstream_id eq '00000000-0000-0000-0000-000000000000'`.")
        .query_param("$select", false, "A comma-separated list of route record fields to project.")
        .query_param("$orderby", false, "A field name with an optional `asc` or `desc` direction.")
        .query_param("$top", false, "The page size; default 50, maximum 100.")
        .query_param("$skip", false, "The non-negative offset of the first record returned.")
        .handler(route_handlers::list)
        .json_response_with_schema::<RouteListResponse>(
            openapi,
            StatusCode::OK,
            "The projected list and the count actually returned",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = toolkit::api::OperationBuilder::get(super::ROUTE_BY_ID_PATH)
        .operation_id("oagw.get_route")
        .summary("Get a route by identifier")
        .description("Read one route the authenticated tenant owns. A missing, foreign or removed identifier is not-found and discloses nothing.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The route identifier")
        .handler(route_handlers::get)
        .json_response_with_schema::<RouteResponse>(
            openapi,
            StatusCode::OK,
            "The stored route",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = toolkit::api::OperationBuilder::put(super::ROUTE_BY_ID_PATH)
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Apply a full replacement. `upstream_id` is immutable and is rejected when supplied, omitted optional blocks are cleared, and `enabled` is settable only here; this is the enable and disable path.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The identifier of the route to replace")
        .json_request::<RouteUpdateRequest>(openapi, UPDATE_BODY_DESCRIPTION)
        .handler(route_handlers::replace)
        .json_response_with_schema::<RouteResponse>(
            openapi,
            StatusCode::OK,
            "The replaced route",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    toolkit::api::OperationBuilder::delete(super::ROUTE_BY_ID_PATH)
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete a route the authenticated tenant owns, together with its dependent match, tag and plugin binding rows. Deleting the owning upstream removes the route as well.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The identifier of the route to delete")
        .handler(route_handlers::delete)
        .no_content_response(StatusCode::NO_CONTENT, "The route was deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
        .layer(axum::Extension(service))
}
