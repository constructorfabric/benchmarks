//! Route registration for the OAGW Control-Plane Management API (feature
//! `cpt-cf-oagw-feature-control-plane-api`, interface
//! `cpt-cf-oagw-interface-management-api`).
//!
//! All operations are registered **unprefixed** on the gear router
//! (`/upstreams`, `/routes`, `/plugins`); the platform api-gateway applies
//! the `/api/oagw/v1/...` prefix (DoD
//! `cpt-cf-oagw-dod-gear-foundation-rest-openapi`).  Paths mirror the DESIGN
//! §3.3 control-plane surface:
//!
//! - upstreams: `POST/GET /upstreams`, `GET/PUT/DELETE /upstreams/{id}`;
//! - routes: `POST/GET /routes`, `GET/PUT/DELETE /routes/{id}`;
//! - plugins: `POST/GET /plugins`, `GET/DELETE /plugins/{id}`,
//!   `GET /plugins/{id}/source`.
//!
//! Every operation is `authenticated` (the platform validates the tenant
//! JWT and injects the [`SecurityContext`]) with no license gate, and
//! declares the standard error responses; the wire error envelope is the
//! OAGW [`GatewayError`] RFC 9457 body (DoD
//! `cpt-cf-oagw-dod-error-semantics-envelope`).
//!
//! `/proxy` is the Data-Plane surface (feature
//! `cpt-cf-oagw-feature-data-plane-proxy`, phase p5): the five CRUD methods
//! are registered by [`register_proxy_routes`] with the same
//! `authenticated`-surface contract (the proxy handler checks the
//! `gts.cf.core.oagw.proxy.v1~:invoke` permission at `inst-dp-exec-authz`),
//! and `OPTIONS` preflights are registered as direct routes.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::{OpenApiRegistry, OperationBuilder};

use crate::domain::GearState;

use super::dto::{
    PluginCreateDto, PluginSourceDto, PluginViewDto, RouteRequestDto, RouteViewDto,
    UpstreamRequestDto, UpstreamViewDto,
};
use super::handlers;
use super::proxy;

const TAG: &str = "OAGW Control Plane";
/// OpenAPI tag for the Data-Plane Proxy surface.
const PROXY_TAG: &str = "OAGW Data Plane";

/// Sortable/filterable list fields advertised per collection (algorithm
/// `cpt-cf-oagw-algo-control-plane-api-odata`).
const UPSTREAM_LIST_FIELDS: [&str; 3] = ["id", "alias", "enabled"];
const ROUTE_LIST_FIELDS: [&str; 5] = ["id", "upstream_id", "priority", "enabled", "path"];
const PLUGIN_LIST_FIELDS: [&str; 3] = ["id", "name", "plugin_type"];

/// Extension adding the OData-style list query parameters to a collection
/// operation.  The descriptive builder methods (`query_param_typed` etc.) are
/// defined on the fully-generic builder, so a local trait lets a free helper
/// chain them without naming the concrete type-state.
trait ListParamsExt: Sized {
    fn list_params(self, fields: &[&str]) -> Self;
}

impl<S, H, R, A, L> ListParamsExt for toolkit::api::OperationBuilder<H, R, S, A, L>
where
    H: toolkit::api::operation_builder::HandlerSlot<S>,
    A: toolkit::api::operation_builder::AuthState,
    L: toolkit::api::operation_builder::LicenseState,
{
    fn list_params(self, fields: &[&str]) -> Self {
        let fields = fields.join(", ");
        self.query_param_typed(
            "$top",
            false,
            "Maximum page size (default 50, cap 100)",
            "integer",
        )
        .query_param_typed("$skip", false, "Number of items to skip", "integer")
        .query_param_typed(
            "$filter",
            false,
            format!("OData v4 equality filter on {fields} (e.g. `alias eq 'api.vendor.com'`)"),
            "string",
        )
        .query_param_typed("$select", false, "OData v4 select expression", "string")
        .query_param_typed(
            "$orderby",
            false,
            format!("OData v4 sort on {fields}, `field [asc|desc]`"),
            "string",
        )
    }
}

/// Registers every control-plane operation, the Data-Plane `/proxy` surface,
/// **and** the admin `/metrics` surface on `router`, attaching the shared
/// [`GearState`] as an `Extension` layer for the handlers.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<GearState>,
) -> Router {
    let router = register_upstream_routes(router, openapi);
    let router = register_route_routes(router, openapi);
    let router = register_plugin_routes(router, openapi);
    let router = register_proxy_routes(router, openapi);
    let router = register_metrics_route(router, openapi);
    router.layer(axum::extract::Extension(state))
}

/// Registers the admin `/metrics` surface (feature
/// `cpt-cf-oagw-feature-observability-audit`, flow
/// `cpt-cf-oagw-flow-observability-audit-scrape`; DoD
/// `cpt-cf-oagw-dod-observability-audit-metrics`): `GET /metrics`, served at
/// the `/api/oagw/v1/metrics` admin surface by the platform prefix.  The
/// operation is `authenticated` (the platform requires authN per the OpenAPI
/// registry and injects the [`SecurityContext`]); the handler defensively
/// re-checks the extension and rejects with 401 before any exposition
/// (`inst-ob-scrape-authz`).
fn register_metrics_route(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    OperationBuilder::get("/metrics")
        .operation_id("oagw.metrics")
        .summary("Expose OAGW Prometheus metrics")
        .description(
            "Admin-only Prometheus text exposition of the DESIGN §4.2 vocabulary \
             (request counts, duration histogram, in-flight, errors by source, \
             circuit-breaker state/transitions, rate-limit exceedance and usage, \
             routing target-host/endpoint selection, upstream availability/connections).",
        )
        .tag("OAGW Observability")
        .authenticated()
        .no_license_required()
        .handler(super::metrics::metrics)
        .text_response(
            StatusCode::OK,
            "Prometheus text exposition",
            "text/plain; version=0.0.4",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .register(router, openapi)
}

fn register_upstream_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Create an upstream for the authenticated tenant. The alias is auto-derived from \
             hostname pools (or must be supplied explicitly for IP/non-derivable pools) and is \
             immutable once set.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<UpstreamRequestDto>(openapi, "Upstream configuration")
        .handler(handlers::create_upstream)
        .json_response_with_schema::<UpstreamViewDto>(
            openapi,
            StatusCode::CREATED,
            "Created upstream",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("List the authenticated tenant's upstreams with OData-style paging.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .list_params(&UPSTREAM_LIST_FIELDS)
        .handler(handlers::list_upstreams)
        .json_array_response_with_schema::<UpstreamViewDto>(
            openapi,
            StatusCode::OK,
            "List of upstreams",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<UpstreamViewDto>(openapi, StatusCode::OK, "The upstream")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description(
            "Whole-value replace of an existing upstream. The alias is immutable once set; a \
             replacement whose resolved alias differs from the stored one is rejected (400).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID")
        .json_request::<UpstreamRequestDto>(openapi, "Replacement configuration")
        .handler(handlers::replace_upstream)
        .json_response_with_schema::<UpstreamViewDto>(openapi, StatusCode::OK, "Replaced upstream")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream (cascades to its routes).")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi)
}

fn register_route_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description(
            "Create a route bound to an existing upstream. gRPC match rules are reserved \
             (Phase 3) and rejected; two live routes sharing a method and an equal \
             (path, priority) on the same upstream conflict (409).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<RouteRequestDto>(openapi, "Route configuration")
        .handler(handlers::create_route)
        .json_response_with_schema::<RouteViewDto>(openapi, StatusCode::CREATED, "Created route")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("List the authenticated tenant's routes with OData-style paging.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .list_params(&ROUTE_LIST_FIELDS)
        .handler(handlers::list_routes)
        .json_array_response_with_schema::<RouteViewDto>(openapi, StatusCode::OK, "List of routes")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get a route")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID")
        .handler(handlers::get_route)
        .json_response_with_schema::<RouteViewDto>(openapi, StatusCode::OK, "The route")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put("/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description(
            "Whole-value replace of an existing route. upstream_id is immutable; a replacement \
             targeting a different upstream is rejected (400).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID")
        .json_request::<RouteRequestDto>(openapi, "Replacement configuration")
        .handler(handlers::replace_route)
        .json_response_with_schema::<RouteViewDto>(openapi, StatusCode::OK, "Replaced route")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi)
}

fn register_plugin_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = OperationBuilder::post("/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description(
            "Register a UUID-backed custom plugin (auth/guard/transform). A duplicate \
             (tenant, name) conflicts (409).",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .json_request::<PluginCreateDto>(openapi, "Plugin definition")
        .handler(handlers::create_plugin)
        .json_response_with_schema::<PluginViewDto>(openapi, StatusCode::CREATED, "Created plugin")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List custom plugins")
        .description("List the authenticated tenant's custom plugins with OData-style paging.")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .list_params(&PLUGIN_LIST_FIELDS)
        .handler(handlers::list_plugins)
        .json_array_response_with_schema::<PluginViewDto>(
            openapi,
            StatusCode::OK,
            "List of plugins",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a custom plugin")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID")
        .handler(handlers::get_plugin)
        .json_response_with_schema::<PluginViewDto>(openapi, StatusCode::OK, "The plugin")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get("/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get a custom plugin's source")
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID")
        .handler(handlers::get_plugin_source)
        .json_response_with_schema::<PluginSourceDto>(openapi, StatusCode::OK, "Plugin source")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    OperationBuilder::delete("/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a custom plugin")
        .description(
            "Delete a custom plugin. A plugin still bound by upstreams or routes is refused \
             with 409 plugin.in_use carrying the referencing bindings.",
        )
        .tag(TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi)
}

/// Registers the Data-Plane Proxy surface (feature
/// `cpt-cf-oagw-feature-data-plane-proxy`, flow
/// `cpt-cf-oagw-flow-gear-foundation-plane-routing`):
///
/// - `POST/GET/PUT/PATCH/DELETE /proxy/{alias}` **and**
///   `/proxy/{alias}/{*rest}` — both path forms are registered because axum's
///   `{*rest}` wildcard does not match the bare `/proxy/{alias}` form; the
///   handler rebuilds the normalized rest path (leading `/`) internally
///   (algorithm `cpt-cf-oagw-algo-data-plane-proxy-match-route`);
/// - `OPTIONS` preflight on both path forms, registered as direct routes so
///   the CORS preflight (flow `cpt-cf-oagw-flow-cors-handling-preflight`)
///   answers 204 instead of the toolkit 405 fallback.
///
/// Every operation is `authenticated` with no license gate (the platform
/// injects the [`SecurityContext`] extension); the handler checks the
/// `gts.cf.core.oagw.proxy.v1~:invoke` permission.  Responses stream through
/// unchanged; the declared error surface mirrors the Data Plane failure
/// catalog (ADR 0007 gateway envelopes).
fn register_proxy_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let router = register_proxy_method_routes(router, openapi);
    // OPTIONS preflight (CORS) — direct routes so they bypass the toolkit 405
    // fallback for the five CRUD methods.
    let router = router.route(
        "/proxy/{alias}",
        axum::routing::options(proxy::proxy_preflight),
    );

    router.route(
        "/proxy/{alias}/{*rest}",
        axum::routing::options(proxy::proxy_preflight),
    )
}

/// Registers the five CRUD `OperationBuilder` blocks for each of the two
/// `/proxy` path forms (unique operation ids per path — the OpenAPI registry
/// rejects duplicates).
fn register_proxy_method_routes(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let description = |op| {
        format!(
            "{op} a request to the target alias. The outbound upstream is selected by the \
             X-OAGW-Target-Host matrix (ADR 0001); the response streams back unchanged \
             (ADR 0007 `X-OAGW-Error-Source: upstream`)."
        )
    };

    let mut router = router;
    for (method, operation_id) in [
        ("post", "oagw.proxy.post"),
        ("get", "oagw.proxy.get"),
        ("put", "oagw.proxy.put"),
        ("patch", "oagw.proxy.patch"),
        ("delete", "oagw.proxy.delete"),
    ] {
        let bare = "/proxy/{alias}".to_string();
        let wildcard = "/proxy/{alias}/{*rest}".to_string();
        let rest_op = format!("{operation_id}.rest");

        let build = |router: Router, path: String, operation_id: String, has_rest: bool| {
            let mut builder = match method {
                "post" => OperationBuilder::post(&path),
                "get" => OperationBuilder::get(&path),
                "put" => OperationBuilder::put(&path),
                "patch" => OperationBuilder::patch(&path),
                _ => OperationBuilder::delete(&path),
            }
            .operation_id(operation_id)
            .summary(format!("Proxy a {method} request to the target alias"))
            .description(description(method.to_uppercase()))
            .tag(PROXY_TAG)
            .authenticated()
            .no_license_required()
            .path_param("alias", "The target upstream alias");
            if has_rest {
                builder = builder.path_param(
                    "rest",
                    "Remainder of the path forwarded to the upstream (without leading slash)",
                );
            }
            builder
                .handler(proxy::proxy_request)
                .text_response(
                    StatusCode::OK,
                    "Streaming passthrough response",
                    "text/plain",
                )
                .problem_response(
                    openapi,
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Request body exceeds the 100 MB cap (413 payload.too_large)",
                )
                .error_400(openapi)
                .error_401(openapi)
                .error_403(openapi)
                .error_404(openapi)
                .error_429(openapi)
                .error_500(openapi)
                .error_502(openapi)
                .error_503(openapi)
                .error_504(openapi)
                .register(router, openapi)
        };

        router = build(router, bare, operation_id.to_owned(), false);
        router = build(router, wildcard, rest_op, true);
    }
    router
}
