//! REST route registration of the plugin management API.
//!
//! Five gear-relative operations under `/oagw/v1/plugins`, documented under the
//! `oagw` OpenAPI tag and layered with the shared
//! [`PluginApiState`](super::plugin_handlers::PluginApiState). There is no
//! `PUT`/`PATCH`: plugin definitions are immutable, and axum answers an
//! unsupported method on a registered path with 405.
//!
//! Paths are **gear-relative**: the first segment is the gear name and there is
//! no `/api` prefix — the gateway mounts the gear's router itself.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::{OpenApiRegistry, OperationBuilder};

use super::plugin_handlers::{
    PluginApiState, PluginDto, PluginRequestDto, PluginSourceDto, create_plugin, delete_plugin,
    get_plugin, get_plugin_source, list_plugins,
};

/// The single OpenAPI tag of every OAGW management operation.
const TAG: &str = "oagw";

/// Registers the plugin management routes onto `router`, layered with the
/// shared [`PluginApiState`] extension.
pub fn register_plugin_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<PluginApiState>,
) -> Router {
    let router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a plugin")
        .description(
            "Create an immutable Starlark plugin owned by the calling tenant. Definitions are \
             never updated: a change is a new plugin plus re-binding, so there is no `PUT`.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<PluginRequestDto>(openapi, "Plugin definition")
        .handler(create_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::CREATED, "Plugin created")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description("List the plugin definitions of the calling tenant with `OData` parameters.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "$filter",
            false,
            "Filter expression: a single `<field> eq|ne 'value'` comparison",
        )
        .query_param("$select", false, "Comma-separated fields to project")
        .query_param("$orderby", false, "Sort expression: `<field> [asc|desc]`")
        .query_param("$top", false, "Page size, 50 by default, at most 100")
        .query_param("$skip", false, "Number of leading items to skip")
        .handler(list_plugins)
        .json_array_response_with_schema::<PluginDto>(openapi, StatusCode::OK, "Plugin page")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a plugin")
        .description("Retrieve one plugin definition of the calling tenant by id.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID")
        .handler(get_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::OK, "Plugin found")
        .standard_errors(openapi)
        .error_404(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get the source of a plugin")
        .description("Retrieve the Starlark source of one plugin as a JSON document.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID")
        .handler(get_plugin_source)
        .json_response_with_schema::<PluginSourceDto>(openapi, StatusCode::OK, "Plugin source")
        .standard_errors(openapi)
        .error_404(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description(
            "Delete a plugin definition of the calling tenant. A plugin still bound to an \
             upstream or route is refused with 409 and the binding list in the problem context.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID")
        .handler(delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .standard_errors(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .register(router, openapi);

    router.layer(axum::Extension(state))
}

#[cfg(test)]
#[path = "plugin_routes_tests.rs"]
mod tests;
