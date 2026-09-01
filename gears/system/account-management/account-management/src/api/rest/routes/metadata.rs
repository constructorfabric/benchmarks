//! `OperationBuilder` route registration for the
//! `/account-management/v1/tenants/{tenant_id}/metadata*`
//! endpoints.

use axum::Router;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::{OperationBuilder, OperationBuilderODataExt};

use account_management_sdk::MetadataEntryFilterField;

use crate::api::rest::{dto, handlers};

const API_TAG: &str = "Tenant Metadata";

/// Collection path: `GET /tenants/{tenant_id}/metadata`.
const COLLECTION_PATH: &str = "/account-management/v1/tenants/{tenant_id}/metadata";
/// Entry path: `GET / PUT / DELETE /tenants/{tenant_id}/metadata/{type_id}`.
const ENTRY_PATH: &str = "/account-management/v1/tenants/{tenant_id}/metadata/{type_id}";
/// Inheritance-aware read path: `GET /tenants/{tenant_id}/metadata/{type_id}/resolved`.
const RESOLVED_PATH: &str =
    "/account-management/v1/tenants/{tenant_id}/metadata/{type_id}/resolved";

#[allow(
    clippy::too_many_lines,
    reason = "five OperationBuilder chains in linear sequence"
)]
pub(super) fn register_metadata_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
) -> Router {
    // GET /account-management/v1/tenants/{tenant_id}/metadata
    router = OperationBuilder::get(COLLECTION_PATH)
        .operation_id("account_management.list_tenant_metadata")
        .summary("List tenant metadata entries")
        .description(
            "List the metadata entries written directly on a tenant. Direct-only -- \
             inherited values are not included; clients reading effective values use \
             the `/resolved` endpoint.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("tenant_id", "Tenant UUID")
        .query_param_typed(
            "limit",
            false,
            "Maximum number of metadata entries to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .handler(handlers::list_metadata)
        .json_response_with_schema::<toolkit_odata::Page<dto::TenantMetadataEntryDto>>(
            openapi,
            http::StatusCode::OK,
            "Paginated list of tenant metadata entries",
        )
        .with_odata_filter::<MetadataEntryFilterField>()
        .with_odata_orderby::<MetadataEntryFilterField>()
        .standard_errors(openapi)
        .problem_response(
            openapi,
            http::StatusCode::SERVICE_UNAVAILABLE,
            "PDP / DB / types-registry transport failure",
        )
        .register(router, openapi);

    // GET /account-management/v1/tenants/{tenant_id}/metadata/{type_id}
    router = OperationBuilder::get(ENTRY_PATH)
        .operation_id("account_management.get_tenant_metadata")
        .summary("Get a tenant metadata entry")
        .description(
            "Read the metadata entry attached directly to the tenant for the given \
             chained GTS `type_id`. Does not walk up the ancestor chain -- use the \
             `/resolved` endpoint for inheritance-aware reads.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("tenant_id", "Tenant UUID")
        .path_param("type_id", "Full chained GTS schema identifier")
        .handler(handlers::get_metadata)
        .json_response_with_schema::<dto::TenantMetadataEntryDto>(
            openapi,
            http::StatusCode::OK,
            "Tenant metadata entry",
        )
        .standard_errors(openapi)
        .problem_response(
            openapi,
            http::StatusCode::SERVICE_UNAVAILABLE,
            "PDP / DB / types-registry transport failure",
        )
        .register(router, openapi);

    // PUT /account-management/v1/tenants/{tenant_id}/metadata/{type_id}
    router = OperationBuilder::put(ENTRY_PATH)
        .operation_id("account_management.put_tenant_metadata")
        .summary("Create or replace a tenant metadata entry")
        .description(
            "Upsert the metadata entry at (tenant_id, type_id). The request body is \
             the GTS-validated payload; the chained `type_id` is the path parameter. \
             Returns HTTP 200 with the post-write entry per RFC 7231 PUT semantics; \
             the insert-vs-update distinction is preserved on the \
             `am.events:metadata_upserted` audit line.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("tenant_id", "Tenant UUID")
        .path_param("type_id", "Full chained GTS schema identifier")
        .json_request::<dto::PutTenantMetadataDto>(openapi, "GTS-validated metadata payload")
        .handler(handlers::upsert_metadata)
        .json_response_with_schema::<dto::TenantMetadataEntryDto>(
            openapi,
            http::StatusCode::OK,
            "Tenant metadata entry stored",
        )
        .standard_errors(openapi)
        .problem_response(
            openapi,
            http::StatusCode::SERVICE_UNAVAILABLE,
            "PDP / DB / types-registry transport failure",
        )
        .register(router, openapi);

    // DELETE /account-management/v1/tenants/{tenant_id}/metadata/{type_id}
    router = OperationBuilder::delete(ENTRY_PATH)
        .operation_id("account_management.delete_tenant_metadata")
        .summary("Delete a tenant metadata entry")
        .description(
            "Hard-delete the metadata entry attached directly to the tenant for the given \
             chained GTS `type_id`. Idempotent on missing rows: returns 204 whether the \
             direct entry existed and was removed or was already absent (mirrors `delete_user` \
             deprovision idempotency). The tenant-existence and schema-registration gates \
             still raise their own 404 codes.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("tenant_id", "Tenant UUID")
        .path_param("type_id", "Full chained GTS schema identifier")
        .handler(handlers::delete_metadata)
        .no_content_response(http::StatusCode::NO_CONTENT, "Metadata entry deleted")
        .standard_errors(openapi)
        .problem_response(
            openapi,
            http::StatusCode::SERVICE_UNAVAILABLE,
            "PDP / DB / types-registry transport failure",
        )
        .register(router, openapi);

    // GET /account-management/v1/tenants/{tenant_id}/metadata/{type_id}/resolved
    router = OperationBuilder::get(RESOLVED_PATH)
        .operation_id("account_management.resolve_tenant_metadata")
        .summary("Resolve the effective metadata value for a tenant and schema")
        .description(
            "Walk up the ancestor chain honouring `self_managed` barriers and the schema's \
             `inheritance_policy`. Empty resolution returns HTTP 200 with `resolved=false`; \
             a 404 is raised only if the tenant does not exist or the schema is not \
             registered.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("tenant_id", "Tenant UUID")
        .path_param("type_id", "Full chained GTS schema identifier")
        .handler(handlers::resolve_metadata)
        .json_response_with_schema::<dto::ResolvedTenantMetadataDto>(
            openapi,
            http::StatusCode::OK,
            "Effective metadata value (resolved=true) or empty (resolved=false)",
        )
        .standard_errors(openapi)
        .problem_response(
            openapi,
            http::StatusCode::SERVICE_UNAVAILABLE,
            "PDP / DB / types-registry transport failure",
        )
        .register(router, openapi);

    router
}
