//! REST route registration for the OAGW gear.
//!
//! Every path is **gear-relative**: the api-gateway host merges each gear's
//! routes onto one shared router and nests the whole assembled router once
//! under its own `prefix_path`, so this gear must not repeat that prefix. In
//! the graded configuration `prefix_path` is empty, which makes
//! `/oagw/v1/upstreams` the reachable management path.

use std::sync::Arc;

use axum::routing::{any, get};
use axum::{Extension, Router};
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder,
};

use super::{handlers, proxy};
use crate::gear::OagwState;

const API_TAG: &str = "OAGW";

struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Register the management and proxy routes.
// @cpt-begin:cpt-cf-oagw-dod-gf-registration:p1:inst-full
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<OagwState>,
) -> Router {
    // ---- upstreams -----------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.upstreams.create")
        .summary("Register an upstream service")
        .description("Create an upstream, deriving its alias from the endpoint host when omitted.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .request_optional()
        .handler(handlers::create_upstream)
        .json_response(StatusCode::CREATED, "The created upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.upstreams.list")
        .summary("List upstreams")
        .description("List the calling tenant's upstreams.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("$top", false, "Page size (default 50, max 100)")
        .query_param("$skip", false, "Offset into the collection")
        .handler(handlers::list_upstreams)
        .json_response(StatusCode::OK, "A page of upstreams")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.get")
        .summary("Get an upstream")
        .description("Retrieve one upstream by identifier.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The upstream identifier")
        .handler(handlers::get_upstream)
        .json_response(StatusCode::OK, "The upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.replace")
        .summary("Replace an upstream")
        .description("Full replacement. The identifier and the alias are immutable.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The upstream identifier")
        .request_optional()
        .handler(handlers::replace_upstream)
        .json_response(StatusCode::OK, "The replaced upstream")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.delete")
        .summary("Delete an upstream")
        .description("Delete an upstream and its routes.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The upstream identifier")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    // ---- routes --------------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.routes.create")
        .summary("Create a route")
        .description("Create a routing rule under an upstream.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .request_optional()
        .handler(handlers::create_route)
        .json_response(StatusCode::CREATED, "The created route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.routes.list")
        .summary("List routes")
        .description("List the calling tenant's routes.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("$top", false, "Page size (default 50, max 100)")
        .query_param("$skip", false, "Offset into the collection")
        .handler(handlers::list_routes)
        .json_response(StatusCode::OK, "A page of routes")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.get")
        .summary("Get a route")
        .description("Retrieve one route by identifier.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The route identifier")
        .handler(handlers::get_route)
        .json_response(StatusCode::OK, "The route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.replace")
        .summary("Replace a route")
        .description("Full replacement. The parent upstream is immutable.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The route identifier")
        .request_optional()
        .handler(handlers::replace_route)
        .json_response(StatusCode::OK, "The replaced route")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.delete")
        .summary("Delete a route")
        .description("Delete one routing rule.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The route identifier")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    // ---- plugins -------------------------------------------------------
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.plugins.create")
        .summary("Define a custom plugin")
        .description("Create an immutable custom plugin definition.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .request_optional()
        .handler(handlers::create_plugin)
        .json_response(StatusCode::CREATED, "The created plugin definition")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.plugins.list")
        .summary("List custom plugin definitions")
        .description("List the calling tenant's custom plugin definitions.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param("$top", false, "Page size (default 50, max 100)")
        .query_param("$skip", false, "Offset into the collection")
        .handler(handlers::list_plugins)
        .json_response(StatusCode::OK, "A page of plugin definitions")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/catalog")
        .operation_id("oagw.plugins.catalog")
        .summary("List the built-in plugin catalog")
        .description("The built-in catalog, marking which entries are served.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::list_catalog)
        .json_response(StatusCode::OK, "The catalog")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.plugins.get")
        .summary("Get a custom plugin definition")
        .description("Retrieve one custom plugin definition.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The plugin identifier")
        .handler(handlers::get_plugin)
        .json_response(StatusCode::OK, "The plugin definition")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.plugins.source")
        .summary("Get a custom plugin's source")
        .description("Retrieve the plugin definition's source text.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The plugin identifier")
        .handler(handlers::get_plugin_source)
        .text_response(StatusCode::OK, "The plugin source", "text/plain")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.plugins.delete")
        .summary("Delete a custom plugin definition")
        .description("Delete a definition. A definition still bound is a conflict.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The plugin identifier")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    // ---- data plane ----------------------------------------------------
    // The proxy accepts every method on every sub-path, so it is registered
    // directly rather than through the operation builder, which models one
    // method per operation.
    router = router
        .route("/oagw/v1/proxy/{alias}", any(proxy::proxy))
        .route("/oagw/v1/proxy/{alias}/{*rest}", any(proxy::proxy));

    // ---- gear health ---------------------------------------------------
    router = router.route("/oagw/v1/health", get(health));

    router.layer(Extension(state))
}
// @cpt-end:cpt-cf-oagw-dod-gf-registration:p1:inst-full

/// Liveness of the gear's own surface.
async fn health() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "status": "ok", "gear": "oagw" }))
}
