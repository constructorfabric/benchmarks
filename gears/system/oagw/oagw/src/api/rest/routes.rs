//! Route registration of the OAGW management REST API.
//!
//! Every operation is registered on the shared router the gear is handed, at
//! its gear-relative and absolute path (`/oagw/v1/…`, DESIGN.md §3.3): the
//! gateway only nests the router under `prefix_path` when that prefix is
//! non-empty, so the literal path registered here is the path served.
//!
//! The registration follows the `types-registry` convention — one
//! [`OperationBuilder`] chain per operation, ending in
//! [`OperationBuilder::register`](toolkit::api::operation_builder::OperationBuilder::register)
//! — and the handlers are attached as-is, so the only state they need arrives
//! through the `Extension<Arc<ManagementService>>` layer applied to the
//! router below.
//!
//! ## Error-source header
//!
//! The `stamp_error_source` middleware adds `X-OAGW-Error-Source: gateway` to
//! any response the layer produces that does not already carry the header
//! (successful responses, and axum's own rejections). A handler that produced
//! an upstream failure replaces the value, which the data plane of phase 4
//! relies on; nothing here stamps the *shared* router, so other gears' routes
//! are untouched.

use std::sync::Arc;

use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;

use super::ManagementService;
use super::dto::{
    PluginDto, PluginRequest, PluginSourceDto, RouteDto, RouteRequest, UpstreamDto, UpstreamRequest,
};
use super::handlers;
use crate::error::{ERROR_SOURCE_HEADER_NAME, ErrorSource};

const API_TAG: &str = "OAGW Management";

/// Registers all management REST routes of the gear on the shared router.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<ManagementService>,
) -> Router {
    let api = management_router(openapi, service);

    router.merge(api)
}

/// The management API as its own router, with its service and error-source
/// middleware layered on.
fn management_router(openapi: &dyn OpenApiRegistry, service: Arc<ManagementService>) -> Router {
    let api = register_upstream_routes(Router::new(), openapi);
    let api = route_routes(api, openapi);
    let api = plugin_routes(api, openapi);

    api.layer(axum::middleware::from_fn(stamp_error_source))
        .layer(axum::Extension(service))
}

/// Registers the upstream operations (`POST`, `GET` list and
/// `GET`/`PUT`/`DELETE` by id).
fn register_upstream_routes(api: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // POST /oagw/v1/upstreams - Create upstream
    let api = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.upstreams.create")
        .summary("Create upstream")
        .description(
            "Creates an upstream in the calling tenant. The alias is derived from the endpoint \
             pool unless it cannot be derived, and the created resource is returned with its \
             anonymous GTS identifier.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<UpstreamRequest>(openapi, "Upstream to create")
        .handler(handlers::create_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::CREATED, "Created upstream")
        .standard_errors(openapi)
        .register(api, openapi);

    // GET /oagw/v1/upstreams - List upstreams
    let api = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.upstreams.list")
        .summary("List upstreams")
        .description(
            "Lists the upstreams of the calling tenant. Supports the OData parameters $filter, \
             $select, $orderby, $top (default 50, capped at 100) and $skip.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "$filter",
            false,
            "OData filter, e.g. `alias eq 'api.openai.com'`",
        )
        .query_param("$select", false, "Comma-separated fields to return")
        .query_param(
            "$orderby",
            false,
            "Comma-separated sort keys, e.g. `alias desc`",
        )
        .query_param("$top", false, "Page size, default 50, maximum 100")
        .query_param("$skip", false, "Page offset")
        .handler(handlers::list_upstreams)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "List of upstreams")
        .standard_errors(openapi)
        .register(api, openapi);

    // GET /oagw/v1/upstreams/{id} - Get upstream
    let api = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.get")
        .summary("Get upstream")
        .description(
            "Reads one upstream of the calling tenant by its anonymous GTS identifier or bare \
             UUID. An upstream owned by another tenant is not visible.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Anonymous GTS identifier or UUID of the upstream")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The requested upstream")
        .standard_errors(openapi)
        .register(api, openapi);

    // PUT /oagw/v1/upstreams/{id} - Replace upstream
    let api = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.replace")
        .summary("Replace upstream")
        .description(
            "Replaces an upstream. This is a full replacement: omitted optional members are \
             cleared back to their defaults. `id` and `tenant_id` are immutable and the alias \
             cannot change.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Anonymous GTS identifier or UUID of the upstream")
        .json_request::<UpstreamRequest>(openapi, "Replacement upstream")
        .handler(handlers::replace_upstream)
        .json_response_with_schema::<UpstreamDto>(openapi, StatusCode::OK, "The replaced upstream")
        .standard_errors(openapi)
        .register(api, openapi);

    // DELETE /oagw/v1/upstreams/{id} - Delete upstream
    OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.upstreams.delete")
        .summary("Delete upstream")
        .description(
            "Deletes an upstream of the calling tenant together with its routes, and answers \
             with 204 and no body.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Anonymous GTS identifier or UUID of the upstream")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "The upstream was deleted")
        .standard_errors(openapi)
        .register(api, openapi)
}

/// Registers the route collection operations (`POST`, `GET` list and
/// `GET`/`PUT`/`DELETE` by id).
fn route_routes(api: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // POST /oagw/v1/routes - Create route
    let api = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.routes.create")
        .summary("Create route")
        .description(
            "Creates a route in the calling tenant. `upstream_id` must address an upstream of \
             the calling tenant, and the match rule must be unique within that upstream.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<RouteRequest>(openapi, "Route to create")
        .handler(handlers::create_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::CREATED, "Created route")
        .standard_errors(openapi)
        .register(api, openapi);

    // GET /oagw/v1/routes - List routes
    let api = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.routes.list")
        .summary("List routes")
        .description(
            "Lists the routes of the calling tenant. Supports the OData parameters $filter, \
             $select, $orderby, $top (default 50, capped at 100) and $skip.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "$filter",
            false,
            "OData filter, e.g. `upstream_id eq '<uuid>'`",
        )
        .query_param("$select", false, "Comma-separated fields to return")
        .query_param(
            "$orderby",
            false,
            "Comma-separated sort keys, e.g. `priority desc`",
        )
        .query_param("$top", false, "Page size, default 50, maximum 100")
        .query_param("$skip", false, "Page offset")
        .handler(handlers::list_routes)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "List of routes")
        .standard_errors(openapi)
        .register(api, openapi);

    // GET /oagw/v1/routes/{id} - Get route
    let api = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.get")
        .summary("Get route")
        .description(
            "Reads one route of the calling tenant by its anonymous GTS identifier or bare UUID.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Anonymous GTS identifier or UUID of the route")
        .handler(handlers::get_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The requested route")
        .standard_errors(openapi)
        .register(api, openapi);

    // PUT /oagw/v1/routes/{id} - Replace route
    let api = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.replace")
        .summary("Replace route")
        .description(
            "Replaces a route. This is a full replacement: omitted optional members are cleared \
             back to their defaults. `id`, `tenant_id` and `upstream_id` are immutable.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Anonymous GTS identifier or UUID of the route")
        .json_request::<RouteRequest>(openapi, "Replacement route")
        .handler(handlers::replace_route)
        .json_response_with_schema::<RouteDto>(openapi, StatusCode::OK, "The replaced route")
        .standard_errors(openapi)
        .register(api, openapi);

    // DELETE /oagw/v1/routes/{id} - Delete route
    OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.routes.delete")
        .summary("Delete route")
        .description("Deletes a route of the calling tenant, and answers with 204 and no body.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Anonymous GTS identifier or UUID of the route")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "The route was deleted")
        .standard_errors(openapi)
        .register(api, openapi)
}

/// Registers the custom-plugin operations (`POST`, `GET` list, `GET`/`DELETE`
/// by id and the Starlark source read).
fn plugin_routes(api: Router, openapi: &dyn OpenApiRegistry) -> Router {
    // POST /oagw/v1/plugins - Create custom plugin
    let api = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.plugins.create")
        .summary("Create custom plugin")
        .description(
            "Stores a custom Starlark plugin (ADR 0002). Plugins are immutable, so an update is \
             performed by creating a new plugin and re-binding the references.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<PluginRequest>(openapi, "Plugin definition")
        .handler(handlers::create_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::CREATED, "Created plugin")
        .standard_errors(openapi)
        .register(api, openapi);

    // GET /oagw/v1/plugins - List plugins
    let api = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.plugins.list")
        .summary("List plugins")
        .description(
            "Lists the custom plugins of the calling tenant. Supports the OData parameters \
             $filter, $select, $orderby, $top (default 50, capped at 100) and $skip.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "$filter",
            false,
            "OData filter, e.g. `plugin_type eq 'guard'`",
        )
        .query_param("$select", false, "Comma-separated fields to return")
        .query_param(
            "$orderby",
            false,
            "Comma-separated sort keys, e.g. `name desc`",
        )
        .query_param("$top", false, "Page size, default 50, maximum 100")
        .query_param("$skip", false, "Page offset")
        .handler(handlers::list_plugins)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::OK, "List of plugins")
        .standard_errors(openapi)
        .register(api, openapi);

    // GET /oagw/v1/plugins/{id} - Get plugin
    let api = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.plugins.get")
        .summary("Get plugin")
        .description(
            "Reads one custom plugin of the calling tenant, including its Starlark source and \
             configuration schema.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Anonymous GTS identifier or UUID of the plugin")
        .handler(handlers::get_plugin)
        .json_response_with_schema::<PluginDto>(openapi, StatusCode::OK, "The requested plugin")
        .standard_errors(openapi)
        .register(api, openapi);

    // GET /oagw/v1/plugins/{id}/source - Get the Starlark source of a plugin
    let api = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.plugins.source")
        .summary("Get the source of a plugin")
        .description("Returns the Starlark source of a custom plugin of the calling tenant.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Anonymous GTS identifier or UUID of the plugin")
        .handler(handlers::get_plugin_source)
        .json_response_with_schema::<PluginSourceDto>(
            openapi,
            StatusCode::OK,
            "Starlark source of the plugin",
        )
        .standard_errors(openapi)
        .register(api, openapi);

    // DELETE /oagw/v1/plugins/{id} - Delete plugin
    OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.plugins.delete")
        .summary("Delete plugin")
        .description(
            "Deletes a custom plugin of the calling tenant, unless an upstream or a route still \
             references it, in which case the request is answered with 409 and the references.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Anonymous GTS identifier or UUID of the plugin")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "The plugin was deleted")
        .standard_errors(openapi)
        .register(api, openapi)
}

/// Adds `X-OAGW-Error-Source: gateway` to responses that do not carry it yet.
///
/// Handlers and [`GatewayError`](crate::error::GatewayError) already stamp the
/// header; this covers the responses that bypass them (successes produced
/// before the error plumbing runs, and axum's own rejections).
async fn stamp_error_source(request: Request, next: Next) -> Response {
    let response = next.run(request).await;

    if response
        .headers()
        .contains_key(ERROR_SOURCE_HEADER_NAME.as_str())
    {
        response
    } else {
        ErrorSource::Gateway.on(response)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::Arc;

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{HeaderMap, Request, StatusCode};
    use serde_json::{Value, json};
    use toolkit::api::OpenApiRegistryImpl;
    use toolkit_security::SecurityContext;
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::register_routes;
    use crate::OagwConfig;
    use crate::api::rest::ManagementService;
    use crate::domain::ConfigService;

    const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
    const UPSTREAM_BASE: &str = "gts.cf.core.oagw.upstream.v1";
    const ROUTE_BASE: &str = "gts.cf.core.oagw.route.v1";
    const VALIDATION_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
    const PLUGIN_IN_USE_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1";

    const TENANT: Uuid = Uuid::from_u128(0x0BEE);
    const ANCESTOR: Uuid = Uuid::from_u128(0x0BAB);

    fn context(tenant: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::from_u128(0x00C0_FFEE))
            .subject_type("service")
            .subject_tenant_id(tenant)
            .build()
            .unwrap()
    }

    fn service() -> Arc<ManagementService> {
        Arc::new(ManagementService::new(Arc::new(ConfigService::new(
            OagwConfig::default(),
        ))))
    }

    fn router(service: Arc<ManagementService>) -> Router {
        let openapi = OpenApiRegistryImpl::new();

        register_routes(Router::new(), &openapi, service)
    }

    /// Sends a request, returning the status, headers and parsed body.
    async fn send(
        router: &Router,
        method: &str,
        uri: &str,
        tenant: Option<Uuid>,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let payload = body.map(|body| Body::from(serde_json::to_vec(&body).unwrap()));
        let mut request = builder.body(payload.unwrap_or_else(Body::empty)).unwrap();
        if let Some(tenant) = tenant {
            request.extensions_mut().insert(context(tenant));
        }

        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 1024 * 128).await.unwrap();

        (
            status,
            headers,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn upstream_body(host: &str, port: u16) -> Value {
        json!({
            "server": {
                "endpoints": [{ "scheme": "https", "host": host, "port": port }]
            },
            "protocol": HTTP_PROTOCOL,
            "tags": ["llm"],
        })
    }

    fn plugin_body(name: &str) -> Value {
        json!({
            "name": name,
            "plugin_type": "guard",
            "phases": ["on_request"],
            "source_code": "def on_request(ctx):\n    return ctx.next()\n",
        })
    }

    /// An upstream body carrying an explicit alias, which IP-literal hosts need.
    fn aliased_upstream_body(alias: &str, host: &str, port: u16) -> Value {
        let mut body = upstream_body(host, port);
        body["alias"] = json!(alias);
        body
    }

    /// Creates an upstream and returns its wire identifier.
    async fn create_upstream(router: &Router, tenant: Uuid, host: &str) -> String {
        let (status, _, body) = send(
            router,
            "POST",
            "/oagw/v1/upstreams",
            Some(tenant),
            Some(upstream_body(host, 443)),
        )
        .await;

        assert_eq!(status, StatusCode::CREATED, "{body}");

        body["id"].as_str().unwrap().to_owned()
    }

    /// Creates an upstream whose alias differs from its host name.
    async fn create_aliased_upstream(
        router: &Router,
        tenant: Uuid,
        alias: &str,
        host: &str,
    ) -> String {
        let (status, _, body) = send(
            router,
            "POST",
            "/oagw/v1/upstreams",
            Some(tenant),
            Some(aliased_upstream_body(alias, host, 443)),
        )
        .await;

        assert_eq!(status, StatusCode::CREATED, "{body}");

        body["id"].as_str().unwrap().to_owned()
    }

    /// Creates a route bound to `upstream_id` and returns its wire identifier.
    async fn create_route(router: &Router, tenant: Uuid, upstream_id: &str) -> String {
        create_route_matching(router, tenant, upstream_id, "POST", "/v1/chat/completions").await
    }

    /// Creates a route with an explicit match rule and returns its wire
    /// identifier, so a test can own several routes on one upstream.
    async fn create_route_matching(
        router: &Router,
        tenant: Uuid,
        upstream_id: &str,
        method: &str,
        path: &str,
    ) -> String {
        let body = json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": [method], "path": path } },
            "tags": ["chat"],
        });
        let (status, _, document) =
            send(router, "POST", "/oagw/v1/routes", Some(tenant), Some(body)).await;

        assert_eq!(status, StatusCode::CREATED, "{document}");

        document["id"].as_str().unwrap().to_owned()
    }

    /// Creates a route whose plugin chain references a custom plugin.
    async fn create_plugin_bound_route(
        router: &Router,
        tenant: Uuid,
        upstream_id: &str,
        plugin_instance: &str,
    ) -> String {
        let body = json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["POST"], "path": "/v1/chat/completions" } },
            "plugins": { "sharing": "private", "items": [plugin_instance] },
        });
        let (status, _, document) =
            send(router, "POST", "/oagw/v1/routes", Some(tenant), Some(body)).await;

        assert_eq!(status, StatusCode::CREATED, "{document}");

        document["id"].as_str().unwrap().to_owned()
    }

    /// Creates a plugin and returns its wire identifier.
    async fn create_plugin(router: &Router, tenant: Uuid, name: &str) -> String {
        let (status, _, body) = send(
            router,
            "POST",
            "/oagw/v1/plugins",
            Some(tenant),
            Some(plugin_body(name)),
        )
        .await;

        assert_eq!(status, StatusCode::CREATED, "{body}");

        body["id"].as_str().unwrap().to_owned()
    }

    fn uuid_of(wire: &str) -> Uuid {
        Uuid::parse_str(wire.rsplit('~').next().unwrap()).unwrap()
    }

    fn header_value<'a>(headers: &'a HeaderMap, name: &'a str) -> Option<&'a str> {
        headers.get(name).and_then(|value| value.to_str().ok())
    }

    fn assert_problem(document: &Value, status: u16, problem_type: &str) {
        assert_eq!(document["type"].as_str(), Some(problem_type), "{document}");
        assert_eq!(
            document["status"].as_u64(),
            Some(u64::from(status)),
            "{document}"
        );
        assert!(document["title"].as_str().is_some(), "{document}");
        assert!(document["detail"].as_str().is_some(), "{document}");
    }

    // -- upstreams ---------------------------------------------------------

    #[tokio::test]
    async fn test_create_upstream_answers_201_with_wire_id_and_header() {
        let router = router(service());

        let (status, headers, body) = send(
            &router,
            "POST",
            "/oagw/v1/upstreams",
            Some(TENANT),
            Some(upstream_body("api.openai.com", 443)),
        )
        .await;

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            header_value(&headers, "x-oagw-error-source"),
            Some("gateway")
        );
        assert_eq!(body["alias"].as_str(), Some("api.openai.com"));
        assert!(body["id"].as_str().unwrap().starts_with(UPSTREAM_BASE));
        assert_eq!(
            body["tenant_id"].as_str(),
            Some(TENANT.to_string().as_str())
        );
        assert_eq!(body["server"]["endpoints"][0]["port"].as_u64(), Some(443));
    }

    #[tokio::test]
    async fn test_get_upstream_accepts_both_identifier_forms() {
        let router = router(service());
        let wire = create_upstream(&router, TENANT, "api.openai.com").await;
        let instance = uuid_of(&wire).to_string();

        let (_, _, by_wire) = send(
            &router,
            "GET",
            &format!("/oagw/v1/upstreams/{wire}"),
            Some(TENANT),
            None,
        )
        .await;
        let (status, _, by_uuid) = send(
            &router,
            "GET",
            &format!("/oagw/v1/upstreams/{instance}"),
            Some(TENANT),
            None,
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{by_uuid}");
        assert_eq!(by_wire["id"], by_uuid["id"]);
        assert_eq!(by_wire["id"].as_str(), Some(wire.as_str()));
    }

    #[tokio::test]
    async fn test_replace_upstream_is_a_full_replacement() {
        let router = router(service());
        let wire = create_upstream(&router, TENANT, "api.openai.com").await;

        let mut replacement = upstream_body("api.openai.com", 443);
        replacement["tags"] = json!(["llm", "flagship"]);
        let (status, _, body) = send(
            &router,
            "PUT",
            &format!("/oagw/v1/upstreams/{wire}"),
            Some(TENANT),
            Some(replacement),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["tags"], json!(["llm", "flagship"]));
        assert_eq!(body["alias"].as_str(), Some("api.openai.com"));
        assert!(
            body["auth"].is_null(),
            "omitted members are cleared: {body}"
        );
        assert!(body["rate_limit"].is_null(), "{body}");

        // A replacement that omits the alias entirely keeps the stored one.
        let (status, _, body) = send(
            &router,
            "PUT",
            &format!("/oagw/v1/upstreams/{wire}"),
            Some(TENANT),
            Some(json!({
                "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com", "port": 443 }] },
                "protocol": HTTP_PROTOCOL,
            })),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["alias"].as_str(), Some("api.openai.com"));
        assert_eq!(body["tags"], json!([]));
    }

    #[tokio::test]
    async fn test_replace_upstream_rejects_server_owned_members() {
        let router = router(service());
        let wire = create_upstream(&router, TENANT, "api.openai.com").await;

        for mutation in [
            json!({ "id": wire }),
            json!({ "tenant_id": Uuid::new_v4().to_string() }),
        ] {
            let body = {
                let mut body = upstream_body("api.openai.com", 443);
                for (key, value) in mutation.as_object().unwrap() {
                    body[key.as_str()] = value.clone();
                }
                body
            };
            let (status, _, document) = send(
                &router,
                "PUT",
                &format!("/oagw/v1/upstreams/{wire}"),
                Some(TENANT),
                Some(body),
            )
            .await;

            assert_eq!(status, StatusCode::BAD_REQUEST, "{document}");
            assert_problem(&document, 400, VALIDATION_TYPE);
        }
    }

    #[tokio::test]
    async fn test_replace_upstream_rejects_an_alias_change() {
        let router = router(service());
        let wire = create_aliased_upstream(&router, TENANT, "edge-1", "10.0.1.1").await;

        let (status, _, document) = send(
            &router,
            "PUT",
            &format!("/oagw/v1/upstreams/{wire}"),
            Some(TENANT),
            Some(json!({
                "alias": "renamed",
                "server": { "endpoints": [{ "scheme": "https", "host": "10.0.1.1", "port": 443 }] },
                "protocol": HTTP_PROTOCOL,
            })),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{document}");
        assert_eq!(document["field"].as_str(), Some("alias"));
    }

    #[tokio::test]
    async fn test_delete_upstream_answers_204_with_an_empty_body() {
        let router = router(service());
        let wire = create_upstream(&router, TENANT, "api.openai.com").await;
        create_route(&router, TENANT, &wire).await;

        let (status, headers, body) = send(
            &router,
            "DELETE",
            &format!("/oagw/v1/upstreams/{wire}"),
            Some(TENANT),
            None,
        )
        .await;

        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(
            header_value(&headers, "x-oagw-error-source"),
            Some("gateway")
        );
        assert!(body.is_null(), "a 204 must have no body: {body}");

        let (status, _, _) = send(
            &router,
            "GET",
            &format!("/oagw/v1/upstreams/{wire}"),
            Some(TENANT),
            None,
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_invalid_upstream_is_a_validation_problem() {
        let router = router(service());

        let (status, headers, document) = send(
            &router,
            "POST",
            "/oagw/v1/upstreams",
            Some(TENANT),
            Some(json!({
                "server": { "endpoints": [] },
                "protocol": HTTP_PROTOCOL,
            })),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            header_value(&headers, "content-type"),
            Some("application/problem+json")
        );
        assert_problem(&document, 400, VALIDATION_TYPE);
    }

    #[tokio::test]
    async fn test_malformed_body_is_a_validation_problem() {
        let router = router(service());
        let request = Request::builder()
            .method("POST")
            .uri("/oagw/v1/upstreams")
            .header("content-type", "application/json")
            .body(Body::from("{not json"))
            .unwrap();

        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        let document: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_problem(&document, 400, VALIDATION_TYPE);
    }

    // -- tenancy -----------------------------------------------------------

    #[tokio::test]
    async fn test_list_upstreams_is_scoped_to_the_caller_tenant() {
        let router = router(service());
        create_upstream(&router, TENANT, "api.openai.com").await;
        create_upstream(&router, ANCESTOR, "api.anthropic.com").await;

        let (status, _, mine) =
            send(&router, "GET", "/oagw/v1/upstreams", Some(TENANT), None).await;
        let (_, _, theirs) = send(&router, "GET", "/oagw/v1/upstreams", Some(ANCESTOR), None).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(mine["total"].as_u64(), Some(1), "{mine}");
        assert_eq!(
            mine["upstreams"][0]["alias"].as_str(),
            Some("api.openai.com")
        );
        assert_eq!(theirs["total"].as_u64(), Some(1), "{theirs}");
        assert_eq!(
            theirs["upstreams"][0]["alias"].as_str(),
            Some("api.anthropic.com")
        );
    }

    #[tokio::test]
    async fn test_ancestor_owned_resource_is_invisible_not_forbidden() {
        let router = router(service());
        let wire = create_upstream(&router, ANCESTOR, "api.openai.com").await;

        let (status, headers, document) = send(
            &router,
            "GET",
            &format!("/oagw/v1/upstreams/{wire}"),
            Some(TENANT),
            None,
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            header_value(&headers, "x-oagw-error-source"),
            Some("gateway")
        );
        assert_problem(
            &document,
            404,
            "gts.cf.core.errors.err.v1~cf.oagw.config.not_found.v1",
        );
    }

    #[tokio::test]
    async fn test_unknown_identifier_is_a_not_found_problem() {
        let router = router(service());
        let unknown = format!("{UPSTREAM_BASE}~{}", Uuid::new_v4());

        let (status, _, document) = send(
            &router,
            "GET",
            &format!("/oagw/v1/upstreams/{unknown}"),
            Some(TENANT),
            None,
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_problem(
            &document,
            404,
            "gts.cf.core.errors.err.v1~cf.oagw.config.not_found.v1",
        );
    }

    #[tokio::test]
    async fn test_malformed_identifier_is_a_bad_request_problem() {
        let router = router(service());

        let (status, _, document) = send(
            &router,
            "GET",
            "/oagw/v1/upstreams/not-an-identifier",
            Some(TENANT),
            None,
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(document["field"].as_str(), Some("id"), "{document}");
    }

    #[tokio::test]
    async fn test_missing_security_context_falls_back_to_the_anonymous_tenant() {
        let router = router(service());
        create_upstream(&router, TENANT, "api.openai.com").await;
        let unknown = format!("{UPSTREAM_BASE}~{}", Uuid::new_v4());

        let (status, _, empty) = send(&router, "GET", "/oagw/v1/upstreams", None, None).await;
        let (missing, _, _) = send(
            &router,
            "GET",
            &format!("/oagw/v1/upstreams/{unknown}"),
            None,
            None,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(empty["total"].as_u64(), Some(0), "{empty}");
        assert_eq!(
            missing,
            StatusCode::NOT_FOUND,
            "the anonymous tenant owns nothing"
        );
    }

    // -- list parameters ---------------------------------------------------

    #[tokio::test]
    async fn test_list_upstreams_supports_odata_parameters() {
        let router = router(service());
        create_upstream(&router, TENANT, "api.openai.com").await;
        create_upstream(&router, TENANT, "api.anthropic.com").await;
        create_upstream(&router, TENANT, "api.mistral.ai").await;

        let (status, _, filtered) = send(
            &router,
            "GET",
            "/oagw/v1/upstreams?$filter=alias%20eq%20%27api.openai.com%27",
            Some(TENANT),
            None,
        )
        .await;
        let (_, _, ordered) = send(
            &router,
            "GET",
            "/oagw/v1/upstreams?$orderby=alias%20desc",
            Some(TENANT),
            None,
        )
        .await;
        let (_, _, paged) = send(
            &router,
            "GET",
            "/oagw/v1/upstreams?$top=2&$skip=1",
            Some(TENANT),
            None,
        )
        .await;
        let (_, _, selected) = send(
            &router,
            "GET",
            "/oagw/v1/upstreams?$select=alias,enabled&$orderby=alias",
            Some(TENANT),
            None,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(filtered["total"].as_u64(), Some(1), "{filtered}");
        assert_eq!(
            filtered["upstreams"][0]["alias"].as_str(),
            Some("api.openai.com")
        );
        let aliases: Vec<&str> = ordered["upstreams"]
            .as_array()
            .unwrap()
            .iter()
            .map(|upstream| upstream["alias"].as_str().unwrap())
            .collect();
        assert_eq!(
            aliases,
            ["api.openai.com", "api.mistral.ai", "api.anthropic.com"]
        );
        assert_eq!(paged["upstreams"].as_array().unwrap().len(), 2, "{paged}");
        assert_eq!(paged["count"].as_u64(), Some(2));
        let projected: Vec<&str> = selected["upstreams"]
            .as_array()
            .unwrap()
            .iter()
            .map(|upstream| upstream["alias"].as_str().unwrap())
            .collect();
        assert_eq!(
            projected,
            ["api.anthropic.com", "api.mistral.ai", "api.openai.com"]
        );
        assert!(
            selected["upstreams"][0].get("server").is_none(),
            "{}",
            selected["upstreams"][0]
        );
    }

    #[tokio::test]
    async fn test_top_is_capped_and_zero_is_honoured() {
        let router = router(service());
        create_upstream(&router, TENANT, "api.openai.com").await;
        create_upstream(&router, TENANT, "api.anthropic.com").await;
        create_upstream(&router, TENANT, "api.mistral.ai").await;

        let (_, _, capped) = send(
            &router,
            "GET",
            "/oagw/v1/upstreams?$top=500",
            Some(TENANT),
            None,
        )
        .await;
        let (_, _, empty) = send(
            &router,
            "GET",
            "/oagw/v1/upstreams?$top=0",
            Some(TENANT),
            None,
        )
        .await;

        assert!(
            capped["upstreams"].as_array().unwrap().len() <= 100,
            "{capped}"
        );
        assert_eq!(capped["upstreams"].as_array().unwrap().len(), 3);
        assert_eq!(empty["upstreams"].as_array().unwrap().len(), 0, "{empty}");
    }

    #[tokio::test]
    async fn test_invalid_list_parameters_are_rejected() {
        let router = router(service());
        create_upstream(&router, TENANT, "api.openai.com").await;

        for query in [
            "$top=not-a-number",
            "$skip=-1",
            "$filter=hostname%20eq%20%27api.openai.com%27",
            "$select=secret",
            "$orderby=alias%20sideways",
            "$filter=",
        ] {
            let (status, _, document) = send(
                &router,
                "GET",
                &format!("/oagw/v1/upstreams?{query}"),
                Some(TENANT),
                None,
            )
            .await;

            assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {document}");
            assert_problem(&document, 400, VALIDATION_TYPE);
        }
    }

    #[tokio::test]
    async fn test_filter_matches_identifiers_and_tags() {
        let router = router(service());
        let wire = create_upstream(&router, TENANT, "api.openai.com").await;
        create_upstream(&router, TENANT, "api.anthropic.com").await;

        for (filter, expected) in [
            (format!("id eq {wire}"), 1),
            (format!("id eq '{}'", uuid_of(&wire)), 1),
            (format!("id ne {wire}"), 1),
            ("enabled eq true".to_owned(), 2),
            ("enabled ne false".to_owned(), 2),
            ("tags eq 'llm'".to_owned(), 2),
            ("tags ne 'missing'".to_owned(), 2),
            ("tenant_id eq '".to_owned() + &TENANT.to_string() + "'", 2),
        ] {
            let encoded = filter.replace(' ', "%20").replace('\'', "%27");
            let (status, _, document) = send(
                &router,
                "GET",
                &format!("/oagw/v1/upstreams?$filter={encoded}"),
                Some(TENANT),
                None,
            )
            .await;

            assert_eq!(status, StatusCode::OK, "{filter}: {document}");
            assert_eq!(
                document["total"].as_u64(),
                Some(expected),
                "{filter}: {document}"
            );
        }
    }

    // -- routes ------------------------------------------------------------

    #[tokio::test]
    async fn test_route_crud_round_trip() {
        let router = router(service());
        let upstream = create_upstream(&router, TENANT, "api.openai.com").await;

        let route = create_route(&router, TENANT, &upstream).await;
        assert!(route.starts_with(ROUTE_BASE), "{route}");

        let (_, _, read) = send(
            &router,
            "GET",
            &format!("/oagw/v1/routes/{route}"),
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(
            read["match"]["http"]["path"].as_str(),
            Some("/v1/chat/completions")
        );
        assert_eq!(read["upstream_id"].as_str(), Some(upstream.as_str()));

        let (status, _, replaced) = send(
            &router,
            "PUT",
            &format!("/oagw/v1/routes/{route}"),
            Some(TENANT),
            Some(json!({
                "upstream_id": upstream,
                "match": { "http": { "methods": ["GET"], "path": "/v1/models" } },
                "priority": 5,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{replaced}");
        assert_eq!(
            replaced["match"]["http"]["methods"][0].as_str(),
            Some("GET")
        );
        assert_eq!(replaced["priority"].as_u64(), Some(5));
        assert_eq!(replaced["tags"], json!([]), "omitted tags are cleared");

        let (status, _, body) = send(
            &router,
            "DELETE",
            &format!("/oagw/v1/routes/{route}"),
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(body.is_null(), "{body}");

        let (status, _, _) = send(
            &router,
            "GET",
            &format!("/oagw/v1/routes/{route}"),
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_route_requires_an_owned_upstream() {
        let router = router(service());
        let ancestor_upstream = create_upstream(&router, ANCESTOR, "api.openai.com").await;

        let (missing, _, document) = send(
            &router,
            "POST",
            "/oagw/v1/routes",
            Some(TENANT),
            Some(json!({
                "upstream_id": ancestor_upstream,
                "match": { "http": { "methods": ["GET"], "path": "/v1/models" } },
            })),
        )
        .await;
        assert_eq!(missing, StatusCode::NOT_FOUND, "{document}");

        let (absent, _, document) = send(
            &router,
            "POST",
            "/oagw/v1/routes",
            Some(TENANT),
            Some(json!({
                "match": { "http": { "methods": ["GET"], "path": "/v1/models" } },
            })),
        )
        .await;
        assert_eq!(absent, StatusCode::BAD_REQUEST, "{document}");
    }

    #[tokio::test]
    async fn test_replace_route_rejects_an_upstream_change() {
        let router = router(service());
        let first = create_upstream(&router, TENANT, "api.openai.com").await;
        let second = create_upstream(&router, TENANT, "api.anthropic.com").await;
        let route = create_route(&router, TENANT, &first).await;

        let (status, _, document) = send(
            &router,
            "PUT",
            &format!("/oagw/v1/routes/{route}"),
            Some(TENANT),
            Some(json!({
                "upstream_id": second,
                "match": { "http": { "methods": ["GET"], "path": "/v1/models" } },
            })),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{document}");
        assert_eq!(
            document["field"].as_str(),
            Some("upstream_id"),
            "{document}"
        );
    }

    #[tokio::test]
    async fn test_duplicate_match_rule_conflicts() {
        let router = router(service());
        let upstream = create_aliased_upstream(&router, TENANT, "edge-1", "10.0.1.1").await;
        let body = json!({
            "upstream_id": upstream,
            "match": { "http": { "methods": ["POST"], "path": "/v1/chat/completions" } },
        });
        let (first, _, _) = send(
            &router,
            "POST",
            "/oagw/v1/routes",
            Some(TENANT),
            Some(body.clone()),
        )
        .await;
        let (second, _, document) =
            send(&router, "POST", "/oagw/v1/routes", Some(TENANT), Some(body)).await;

        assert_eq!(first, StatusCode::CREATED);
        assert_eq!(second, StatusCode::CONFLICT, "{document}");
        assert_problem(
            &document,
            409,
            "gts.cf.core.errors.err.v1~cf.oagw.config.conflict.v1",
        );
    }

    #[tokio::test]
    async fn test_route_listing_is_scoped_to_owned_upstreams() {
        let router = router(service());
        let mine = create_upstream(&router, TENANT, "api.openai.com").await;
        let other = create_upstream(&router, ANCESTOR, "api.anthropic.com").await;
        create_route(&router, TENANT, &mine).await;
        create_route(&router, ANCESTOR, &other).await;

        let (status, _, listed) = send(&router, "GET", "/oagw/v1/routes", Some(TENANT), None).await;

        assert_eq!(status, StatusCode::OK, "{listed}");
        assert_eq!(listed["total"].as_u64(), Some(1), "{listed}");
        assert_eq!(
            listed["routes"][0]["upstream_id"].as_str(),
            Some(mine.as_str())
        );
    }

    #[tokio::test]
    async fn test_list_routes_supports_odata_parameters() {
        let router = router(service());
        let openai = create_upstream(&router, TENANT, "api.openai.com").await;
        let anthropic = create_upstream(&router, TENANT, "api.anthropic.com").await;
        create_route_matching(&router, TENANT, &openai, "POST", "/v1/chat/completions").await;
        create_route_matching(&router, TENANT, &anthropic, "GET", "/v1/models").await;

        let (_, _, filtered) = send(
            &router,
            "GET",
            &format!("/oagw/v1/routes?$filter=upstream_id%20eq%20{openai}"),
            Some(TENANT),
            None,
        )
        .await;
        let (_, _, ascending) = send(
            &router,
            "GET",
            "/oagw/v1/routes?$orderby=upstream_id&$top=1",
            Some(TENANT),
            None,
        )
        .await;
        let (_, _, descending) = send(
            &router,
            "GET",
            "/oagw/v1/routes?$orderby=upstream_id%20desc&$top=1",
            Some(TENANT),
            None,
        )
        .await;

        let (low, high) = if openai.as_str() < anthropic.as_str() {
            (openai.as_str(), anthropic.as_str())
        } else {
            (anthropic.as_str(), openai.as_str())
        };

        assert_eq!(filtered["total"].as_u64(), Some(1), "{filtered}");
        assert_eq!(
            filtered["routes"][0]["upstream_id"].as_str(),
            Some(openai.as_str())
        );
        assert_eq!(
            ascending["routes"].as_array().unwrap().len(),
            1,
            "{ascending}"
        );
        assert_eq!(ascending["count"].as_u64(), Some(1));
        assert_eq!(ascending["routes"][0]["upstream_id"].as_str(), Some(low));
        assert_eq!(descending["routes"][0]["upstream_id"].as_str(), Some(high));
    }

    // -- plugins -----------------------------------------------------------

    #[tokio::test]
    async fn test_plugin_lifecycle_answers_201_200_and_204() {
        let router = router(service());

        let wire = create_plugin(&router, TENANT, "request_validator").await;
        assert!(
            wire.starts_with("gts.cf.core.oagw.guard_plugin.v1~"),
            "{wire}"
        );

        let (_, _, read) = send(
            &router,
            "GET",
            &format!("/oagw/v1/plugins/{wire}"),
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(read["name"].as_str(), Some("request_validator"));
        assert_eq!(read["plugin_type"].as_str(), Some("guard"));
        assert_eq!(read["phases"], json!(["on_request"]));
        assert_eq!(
            read["tenant_id"].as_str(),
            Some(TENANT.to_string().as_str())
        );

        let (status, _, source) = send(
            &router,
            "GET",
            &format!("/oagw/v1/plugins/{wire}/source"),
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(source["plugin_id"].as_str(), Some(wire.as_str()));
        assert!(
            source["source_code"]
                .as_str()
                .unwrap()
                .contains("on_request")
        );

        let (status, _, body) = send(
            &router,
            "DELETE",
            &format!("/oagw/v1/plugins/{wire}"),
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(body.is_null(), "{body}");

        let (status, _, _) = send(
            &router,
            "GET",
            &format!("/oagw/v1/plugins/{wire}"),
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_list_plugins_supports_odata_parameters() {
        let router = router(service());
        create_plugin(&router, TENANT, "zeta_guard").await;
        create_plugin(&router, TENANT, "alpha_guard").await;
        create_plugin(&router, ANCESTOR, "foreign_guard").await;

        let (status, _, listed) = send(
            &router,
            "GET",
            "/oagw/v1/plugins?$orderby=name%20asc&$select=name,plugin_type",
            Some(TENANT),
            None,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let names: Vec<&str> = listed["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .map(|plugin| plugin["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["alpha_guard", "zeta_guard"], "{listed}");
        assert!(
            listed["plugins"][0].get("source_code").is_none(),
            "{listed}"
        );

        let (_, _, filtered) = send(
            &router,
            "GET",
            "/oagw/v1/plugins?$filter=plugin_type%20eq%20%27guard%27",
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(filtered["total"].as_u64(), Some(2), "{filtered}");
    }

    #[tokio::test]
    async fn test_referenced_plugin_cannot_be_deleted() {
        let router = router(service());
        let plugin = create_plugin(&router, TENANT, "request_validator").await;
        let instance = uuid_of(&plugin).to_string();
        let mut bound = upstream_body("api.openai.com", 443);
        bound["plugins"] = json!({ "sharing": "private", "items": [instance] });
        let (status, _, upstream) = send(
            &router,
            "POST",
            "/oagw/v1/upstreams",
            Some(TENANT),
            Some(bound),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{upstream}");
        let upstream_id = upstream["id"].as_str().unwrap().to_owned();
        let route = create_plugin_bound_route(&router, TENANT, &upstream_id, &instance).await;

        let (status, headers, document) = send(
            &router,
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin}"),
            Some(TENANT),
            None,
        )
        .await;

        assert_eq!(status, StatusCode::CONFLICT, "{document}");
        assert_eq!(
            header_value(&headers, "content-type"),
            Some("application/problem+json")
        );
        assert_eq!(
            header_value(&headers, "x-oagw-error-source"),
            Some("gateway")
        );
        assert_eq!(document["type"].as_str(), Some(PLUGIN_IN_USE_TYPE));
        assert_eq!(document["status"].as_u64(), Some(409));
        assert_eq!(document["plugin_id"].as_str(), Some(plugin.as_str()));
        let referenced_by = &document["referenced_by"];
        assert_eq!(
            referenced_by["upstreams"].as_array().unwrap().len(),
            1,
            "{document}"
        );
        assert_eq!(referenced_by["routes"].as_array().unwrap().len(), 1);
        assert_eq!(
            referenced_by["routes"][0].as_str(),
            Some(route.as_str()),
            "{document}"
        );

        // Unbinding the route still leaves the upstream binding in place.
        let (status, _, _) = send(
            &router,
            "DELETE",
            &format!("/oagw/v1/routes/{route}"),
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, _, document) = send(
            &router,
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin}"),
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{document}");
        assert_eq!(
            document["referenced_by"]["upstreams"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            document["referenced_by"]["routes"]
                .as_array()
                .unwrap()
                .len(),
            0
        );

        // Deleting the last referencing upstream frees the plugin.
        let (status, _, _) = send(
            &router,
            "DELETE",
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, _, _) = send(
            &router,
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin}"),
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn test_plugin_delete_reports_both_reference_arrays() {
        let router = router(service());
        let plugin = create_plugin(&router, TENANT, "unused_guard").await;

        // Deleting an unrelated upstream first leaves the plugin untouched, so
        // the conflict body must still carry both, possibly empty, arrays.
        let upstream = create_upstream(&router, TENANT, "api.openai.com").await;
        let (status, _, _) = send(
            &router,
            "DELETE",
            &format!("/oagw/v1/upstreams/{upstream}"),
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, _, document) = send(
            &router,
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin}"),
            Some(TENANT),
            None,
        )
        .await;

        assert_eq!(status, StatusCode::NO_CONTENT, "{document}");
        assert!(document.is_null());
    }

    #[tokio::test]
    async fn test_plugin_submission_is_validated() {
        let router = router(service());

        for body in [
            json!({ "name": "  ", "plugin_type": "guard", "source_code": "def on_request(ctx):\n    return ctx.next()\n" }),
            json!({ "name": "no_source", "plugin_type": "guard" }),
            json!({ "name": "bad_family", "plugin_type": "middleware", "source_code": "x" }),
            json!({ "name": "server_owned", "plugin_type": "guard", "id": "mine", "source_code": "x" }),
        ] {
            let (status, _, document) = send(
                &router,
                "POST",
                "/oagw/v1/plugins",
                Some(TENANT),
                Some(body),
            )
            .await;

            assert_eq!(status, StatusCode::BAD_REQUEST, "{document}");
            assert_problem(&document, 400, VALIDATION_TYPE);
        }
    }

    #[tokio::test]
    async fn test_plugin_of_another_tenant_is_invisible() {
        let router = router(service());
        let plugin = create_plugin(&router, ANCESTOR, "foreign_guard").await;

        let (status, _, document) = send(
            &router,
            "DELETE",
            &format!("/oagw/v1/plugins/{plugin}"),
            Some(TENANT),
            None,
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND, "{document}");
        assert_problem(
            &document,
            404,
            "gts.cf.core.errors.err.v1~cf.oagw.config.not_found.v1",
        );
    }

    // -- wire contract -----------------------------------------------------

    #[tokio::test]
    async fn test_routes_are_served_without_an_api_prefix() {
        let router = router(service());

        let (status, headers, body) =
            send(&router, "GET", "/oagw/v1/upstreams", Some(TENANT), None).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            header_value(&headers, "x-oagw-error-source"),
            Some("gateway")
        );

        let (status, _, _) =
            send(&router, "GET", "/api/oagw/v1/upstreams", Some(TENANT), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "/api must not be registered");
    }

    #[tokio::test]
    async fn test_proxy_route_is_not_registered_yet() {
        let router = router(service());

        for method in ["GET", "POST"] {
            let (status, _, _) = send(
                &router,
                method,
                "/oagw/v1/proxy/api.openai.com/v1/chat/completions",
                Some(TENANT),
                None,
            )
            .await;

            assert_eq!(status, StatusCode::NOT_FOUND, "{method}");
        }
    }

    #[tokio::test]
    async fn test_every_response_carries_the_error_source_header() {
        let router = router(service());

        for (method, uri, body) in [
            ("GET", "/oagw/v1/upstreams", None),
            (
                "POST",
                "/oagw/v1/upstreams",
                Some(json!({ "server": { "endpoints": [] }, "protocol": HTTP_PROTOCOL })),
            ),
            ("GET", "/oagw/v1/routes/unknown-identifier", None),
            ("DELETE", "/oagw/v1/routes/unknown-identifier", None),
            ("GET", "/oagw/v1/plugins", None),
        ] {
            let (_, headers, _) = send(&router, method, uri, Some(TENANT), body).await;

            assert_eq!(
                header_value(&headers, "x-oagw-error-source"),
                Some("gateway"),
                "{method} {uri}"
            );
        }
    }

    #[test]
    fn test_every_operation_is_registered_and_documented() {
        let openapi = OpenApiRegistryImpl::new();
        let router = register_routes(Router::new(), &openapi, service());

        // The 15 operations of DESIGN.md's management table across 7 literal
        // paths, none of them carrying an /api prefix.
        let keys: Vec<String> = openapi
            .operation_specs
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        let mut paths: Vec<&str> = keys
            .iter()
            .map(|key| key.split_once(':').unwrap().1)
            .collect();
        paths.sort_unstable();
        paths.dedup();

        assert_eq!(keys.len(), 15, "{keys:?}");
        assert_eq!(
            paths,
            [
                "/oagw/v1/plugins",
                "/oagw/v1/plugins/{id}",
                "/oagw/v1/plugins/{id}/source",
                "/oagw/v1/routes",
                "/oagw/v1/routes/{id}",
                "/oagw/v1/upstreams",
                "/oagw/v1/upstreams/{id}",
            ]
        );
        assert!(
            keys.iter().all(|key| !key.contains("/api/")),
            "no /api prefix: {keys:?}"
        );
        assert!(router.has_routes(), "every documented operation is routed");
    }
}
