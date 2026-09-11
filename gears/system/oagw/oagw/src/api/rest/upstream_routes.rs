//! REST route registration of the upstream management surface (FEATURE entry
//! 2.2).
//!
//! The five operations live on the gear-relative paths entry 2.1 declared, and
//! every one of them is registered `authenticated()`, because the caller
//! identity the handlers read is produced by the platform authentication
//! middleware ahead of the handler.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::OpenApiRegistry;

use super::dto::{UpstreamListResponse, UpstreamRequest, UpstreamResponse};
use super::upstream_handlers;
use crate::domain::services::management::UpstreamManagementService;

/// The OpenAPI tag of the upstream management surface.
const TAG: &str = "OAGW Upstreams";

/// A create whose body violates a shape is a `400`; a same-tenant alias
/// collision is a `409`.
const CREATE_BODY_DESCRIPTION: &str = "The upstream record. `id` and `tenant_id` are server-assigned and are rejected when supplied; an absent `alias` is derived from the endpoint pool.";

/// Register the five upstream operations of the management surface.
pub fn register_upstream_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<UpstreamManagementService>,
) -> Router {
    let router = toolkit::api::OperationBuilder::post(super::UPSTREAMS_PATH)
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description("Create an upstream for the authenticated tenant. The alias is derived from the endpoint pool unless it is supplied, and an alias that matches an ancestor tenant's upstream is a bind requiring `oagw:upstream:bind`.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<UpstreamRequest>(openapi, CREATE_BODY_DESCRIPTION)
        .handler(upstream_handlers::create)
        .json_response_with_schema::<UpstreamResponse>(
            openapi,
            StatusCode::CREATED,
            "The stored upstream with its effective enablement",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = toolkit::api::OperationBuilder::get(super::UPSTREAMS_PATH)
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List the upstreams the authenticated tenant owns, filtered, projected, ordered and paginated by the OData system query options `$filter`, `$select`, `$orderby`, `$top` and `$skip`.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "An OData filter expression, such as `alias eq 'api.vendor.com'`.")
        .query_param("$select", false, "A comma-separated list of upstream record fields to project.")
        .query_param("$orderby", false, "A field name with an optional `asc` or `desc` direction.")
        .query_param("$top", false, "The page size; default 50, maximum 100.")
        .query_param("$skip", false, "The non-negative offset of the first record returned.")
        .handler(upstream_handlers::list)
        .json_response_with_schema::<UpstreamListResponse>(
            openapi,
            StatusCode::OK,
            "The projected list and the count actually returned",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = toolkit::api::OperationBuilder::get(super::UPSTREAM_BY_ID_PATH)
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream by identifier")
        .description("Read one upstream the authenticated tenant owns. A missing or foreign identifier is not-found and discloses nothing.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The upstream identifier")
        .handler(upstream_handlers::get)
        .json_response_with_schema::<UpstreamResponse>(
            openapi,
            StatusCode::OK,
            "The stored upstream with its effective enablement",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = toolkit::api::OperationBuilder::put(super::UPSTREAM_BY_ID_PATH)
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description("Apply a full replacement. Omitted sub-configuration blocks are cleared, the alias is immutable, and `enabled` is settable only here; this is the enable and disable path.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The identifier of the upstream to replace")
        .json_request::<UpstreamRequest>(openapi, CREATE_BODY_DESCRIPTION)
        .handler(upstream_handlers::replace)
        .json_response_with_schema::<UpstreamResponse>(
            openapi,
            StatusCode::OK,
            "The replaced upstream with its effective enablement",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    toolkit::api::OperationBuilder::delete(super::UPSTREAM_BY_ID_PATH)
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream the authenticated tenant owns, together with its dependent route, tag and plugin binding rows.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The identifier of the upstream to delete")
        .handler(upstream_handlers::delete)
        .no_content_response(StatusCode::NO_CONTENT, "The upstream was deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi)
        .layer(axum::Extension(service))
}
