//! REST route registration of the plugin management surface (FEATURE entry
//! 2.6).
//!
//! The five operations live on the gear-relative paths entry 2.1 declared, and
//! every one of them is registered `authenticated()`, because the caller
//! identity the handlers read is produced by the platform authentication
//! middleware ahead of the handler. There is deliberately **no** `PUT` or
//! `PATCH` operation: a custom plugin is immutable after creation.
//!
//! The list operation documents the OData system query options the upstream
//! and route surfaces do, because all three lists are served by the one parser
//! entry 2.2 delivered.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::OpenApiRegistry;

use super::dto::{PluginListResponse, PluginRequest, PluginResponse, PluginSourceResponse};
use super::plugin_handlers;
use crate::domain::services::plugin_management::PluginManagementService;

/// The OpenAPI tag of the plugin management surface.
const TAG: &str = "OAGW Plugins";

/// Register the five plugin operations of the management surface.
pub fn register_plugin_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<PluginManagementService>,
) -> Router {
    let router = toolkit::api::OperationBuilder::post(super::PLUGINS_PATH)
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description("Create a custom plugin the authenticated tenant owns. `plugin_type` names one of the three plugin base types, `name` is unique within the tenant, and the server assigns the identifier; the record is immutable from creation onward and its source content is stored as an opaque reference artifact that is never interpreted or executed.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<PluginRequest>(openapi, "The plugin record. `id` is server-assigned and rejected when supplied.")
        .handler(plugin_handlers::create)
        .json_response_with_schema::<PluginResponse>(
            openapi,
            StatusCode::CREATED,
            "The created plugin",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = toolkit::api::OperationBuilder::get(super::PLUGINS_PATH)
        .operation_id("oagw.list_plugins")
        .summary("List custom plugins")
        .description("List the custom plugins the authenticated tenant owns, filtered, projected and paginated by the OData system query options `$filter`, `$select`, `$top` and `$skip`; `type eq 'guard'` selects the guard plugins.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "An OData filter expression, such as `type eq 'guard'`.")
        .query_param("$select", false, "A comma-separated list of plugin record fields to project.")
        .query_param("$top", false, "The page size; default 50, maximum 100.")
        .query_param("$skip", false, "The non-negative offset of the first record returned.")
        .handler(plugin_handlers::list)
        .json_response_with_schema::<PluginListResponse>(
            openapi,
            StatusCode::OK,
            "The projected list and the count actually returned",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = toolkit::api::OperationBuilder::get(super::PLUGIN_BY_ID_PATH)
        .operation_id("oagw.get_plugin")
        .summary("Get a custom plugin by identifier")
        .description("Read one custom plugin the authenticated tenant owns. A missing, foreign, ancestor-owned or removed identifier is not-found and discloses nothing.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The plugin identifier")
        .handler(plugin_handlers::get)
        .json_response_with_schema::<PluginResponse>(
            openapi,
            StatusCode::OK,
            "The stored plugin",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    let router = toolkit::api::OperationBuilder::get(super::PLUGIN_SOURCE_PATH)
        .operation_id("oagw.get_plugin_source")
        .summary("Get the registered source of a custom plugin")
        .description("Read the registered source content of one custom plugin the authenticated tenant owns, as an opaque reference artifact. A named plugin is resolved by the in-process registry and carries no stored source, and a missing, foreign or ancestor-owned identifier is not-found.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The plugin identifier")
        .handler(plugin_handlers::source)
        .json_response_with_schema::<PluginSourceResponse>(
            openapi,
            StatusCode::OK,
            "The stored source reference",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    toolkit::api::OperationBuilder::delete(super::PLUGIN_BY_ID_PATH)
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin")
        .description("Delete a custom plugin the authenticated tenant owns that no upstream auth block, upstream plugin binding or route plugin binding references; a still-referenced plugin is a `409` carrying the `referenced_by` set, and the record and every binding are left unchanged.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "The identifier of the plugin to delete")
        .handler(plugin_handlers::delete)
        .no_content_response(StatusCode::NO_CONTENT, "The plugin was deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi)
        .layer(axum::Extension(service))
}
