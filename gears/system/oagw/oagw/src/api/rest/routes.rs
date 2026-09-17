//! REST route registration for the OAGW gear.
//!
//! Management CRUD registers through the [`OperationBuilder`] (which records
//! each operation in the host OpenAPI registry) for `/oagw/v1/upstreams`,
//! `/oagw/v1/routes` and `/oagw/v1/plugins` including the
//! `/oagw/v1/plugins/{id}/source` reader. The proxy endpoint registers the
//! two path forms (`{alias}` and `{alias}/{*path}`) for every forwarded method
//! (GET/POST/PUT/DELETE/PATCH/OPTIONS); `OPTIONS` cannot go through
//! `OperationBuilder` (its `.handler()` yields a 405 stub for non-standard
//! methods) so it is mounted directly with `routing::options`.
//!
//! Response/request bodies are documented without schema refs: the wire
//! models carry free-form `config` objects (authoritative schemas live in the
//! gear's `docs/schemas/*.json`), so schemas are not registered here to avoid
//! dangling `$ref`s.

use std::sync::Arc;

use axum::{Extension, Router, routing};
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder,
};

use super::handlers;
use crate::domain::services::management::ControlPlaneService;
use crate::infra::proxy::DataPlaneServiceImpl;

const API_TAG: &str = "OAGW";

struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Registers all REST routes for the OAGW gear.
///
/// Takes the `ControlPlaneService` and `DataPlaneServiceImpl` so both the
/// management CRUD handlers and the proxy handler share one instance.
#[allow(clippy::needless_pass_by_value)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    cp: Arc<ControlPlaneService>,
    dp: Arc<DataPlaneServiceImpl>,
) -> Router {
    // ------------------------------------------------------------------
    // Upstreams
    // ------------------------------------------------------------------

    // POST /oagw/v1/upstreams - Create an upstream
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create an upstream")
        .description(
            "Create an upstream describing a tenant-owned backend pool. Returns the stored \
             record with the server-assigned `id` and `Location` header.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::create_upstream)
        .json_response(StatusCode::CREATED, "Upstream created")
        .problem_response(openapi, StatusCode::BAD_REQUEST, "Validation failure")
        .problem_response(
            openapi,
            StatusCode::FORBIDDEN,
            "Missing scope or bind permission",
        )
        .problem_response(openapi, StatusCode::CONFLICT, "Alias already in use")
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /oagw/v1/upstreams - List upstreams
    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description(
            "List upstreams visible to the token with OData query parameters \
             (`$filter`, `$select`, `$orderby`, `$top`, `$skip`).",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param(
            "$filter",
            false,
            "OData v4 filter expression (limited to `eq` on scalar fields)",
        )
        .query_param(
            "$select",
            false,
            "Comma-separated fields to project onto each record",
        )
        .query_param("$orderby", false, "OData v4 orderby expression")
        .query_param(
            "$top",
            false,
            "Maximum number of records to return (default 50, max 100)",
        )
        .query_param("$skip", false, "Number of records to skip")
        .handler(handlers::list_upstreams)
        .json_response(
            StatusCode::OK,
            "List response envelope with total and items",
        )
        .problem_response(
            openapi,
            StatusCode::BAD_REQUEST,
            "Unsupported query expression",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /oagw/v1/upstreams/{id} - Get an upstream
    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get an upstream by id")
        .description("Retrieve a single upstream by its server-assigned id.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The upstream UUID")
        .handler(handlers::get_upstream)
        .json_response(StatusCode::OK, "The requested upstream")
        .problem_response(
            openapi,
            StatusCode::NOT_FOUND,
            "Missing or foreign upstream",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // PUT /oagw/v1/upstreams/{id} - Replace an upstream
    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace an upstream")
        .description("Replace an upstream in full. The `alias` is immutable after creation.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The upstream UUID")
        .handler(handlers::replace_upstream)
        .json_response(StatusCode::OK, "The replaced upstream")
        .problem_response(
            openapi,
            StatusCode::BAD_REQUEST,
            "Validation or alias immutability",
        )
        .problem_response(
            openapi,
            StatusCode::NOT_FOUND,
            "Missing or foreign upstream",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/upstreams/{id} - Delete an upstream
    router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete an upstream")
        .description("Delete an upstream and cascade-delete its routes.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The upstream UUID")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .problem_response(
            openapi,
            StatusCode::NOT_FOUND,
            "Missing or foreign upstream",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // ------------------------------------------------------------------
    // Routes
    // ------------------------------------------------------------------

    // POST /oagw/v1/routes - Create a route
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create a route")
        .description(
            "Create a route binding a match pattern to an upstream owned by the token. \
             Returns the stored record with the server-assigned `id`.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::create_route)
        .json_response(StatusCode::CREATED, "Route created")
        .problem_response(openapi, StatusCode::BAD_REQUEST, "Validation failure")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Upstream not owned")
        .problem_response(openapi, StatusCode::CONFLICT, "Match pattern conflict")
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /oagw/v1/routes - List routes
    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description(
            "List routes visible to the token with OData query parameters \
             (`$filter`, `$select`, `$orderby`, `$top`, `$skip`).",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param(
            "$filter",
            false,
            "OData v4 filter expression (limited to `eq` on scalar fields)",
        )
        .query_param(
            "$select",
            false,
            "Comma-separated fields to project onto each record",
        )
        .query_param("$orderby", false, "OData v4 orderby expression")
        .query_param(
            "$top",
            false,
            "Maximum number of records to return (default 50, max 100)",
        )
        .query_param("$skip", false, "Number of records to skip")
        .handler(handlers::list_routes)
        .json_response(
            StatusCode::OK,
            "List response envelope with total and items",
        )
        .problem_response(
            openapi,
            StatusCode::BAD_REQUEST,
            "Unsupported query expression",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /oagw/v1/routes/{id} - Get a route
    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get a route by id")
        .description("Retrieve a single route by its server-assigned id.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The route UUID")
        .handler(handlers::get_route)
        .json_response(StatusCode::OK, "The requested route")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Missing or foreign route")
        .standard_errors(openapi)
        .register(router, openapi);

    // PUT /oagw/v1/routes/{id} - Replace a route
    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace a route")
        .description("Replace a route in full. The `upstream_id` is immutable after creation.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The route UUID")
        .handler(handlers::replace_route)
        .json_response(StatusCode::OK, "The replaced route")
        .problem_response(openapi, StatusCode::BAD_REQUEST, "Validation failure")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Missing or foreign route")
        .problem_response(openapi, StatusCode::CONFLICT, "Match pattern conflict")
        .standard_errors(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/routes/{id} - Delete a route
    router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete a route")
        .description("Delete a route by id.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The route UUID")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Missing or foreign route")
        .standard_errors(openapi)
        .register(router, openapi);

    // ------------------------------------------------------------------
    // Plugins (custom, UUID-backed)
    // ------------------------------------------------------------------

    // POST /oagw/v1/plugins - Create a custom plugin
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create a custom plugin")
        .description(
            "Create a custom (Starlark) plugin of family `auth`, `guard` or `transform`. \
             The name is tenant-unique; the stored source is immutable.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::create_plugin)
        .json_response(StatusCode::CREATED, "Plugin created")
        .problem_response(openapi, StatusCode::BAD_REQUEST, "Validation failure")
        .problem_response(openapi, StatusCode::CONFLICT, "Name already in use")
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /oagw/v1/plugins - List plugins
    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description(
            "List custom plugins visible to the token with OData query parameters \
             (`$filter`, `$select`, `$orderby`, `$top`, `$skip`).",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param(
            "$filter",
            false,
            "OData v4 filter expression (limited to `eq` on scalar fields)",
        )
        .query_param(
            "$select",
            false,
            "Comma-separated fields to project onto each record",
        )
        .query_param("$orderby", false, "OData v4 orderby expression")
        .query_param(
            "$top",
            false,
            "Maximum number of records to return (default 50, max 100)",
        )
        .query_param("$skip", false, "Number of records to skip")
        .handler(handlers::list_plugins)
        .json_response(
            StatusCode::OK,
            "List response envelope with total and items",
        )
        .problem_response(
            openapi,
            StatusCode::BAD_REQUEST,
            "Unsupported query expression",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /oagw/v1/plugins/{id} - Get a plugin
    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get a plugin by id")
        .description("Retrieve a single custom plugin by its server-assigned id.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The plugin UUID")
        .handler(handlers::get_plugin)
        .json_response(StatusCode::OK, "The requested plugin")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Missing or foreign plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    // GET /oagw/v1/plugins/{id}/source - Get plugin source
    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get plugin source")
        .description("Retrieve the stored Starlark source of a custom plugin.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The plugin UUID")
        .handler(handlers::get_plugin_source)
        .json_response(StatusCode::OK, "The plugin source envelope")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Missing or foreign plugin")
        .standard_errors(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/plugins/{id} - Delete a plugin
    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete a plugin")
        .description("Delete a custom plugin. Fails with 409 while bound by an upstream or route.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "The plugin UUID")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Missing or foreign plugin")
        .problem_response(
            openapi,
            StatusCode::CONFLICT,
            "Referenced by an upstream or route",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    // ------------------------------------------------------------------
    // Proxy (data plane)
    // ------------------------------------------------------------------
    // Two path forms per forwarded method: bare `{alias}` and `{alias}/{*path}`.
    // OPTIONS is registered through raw axum routing because OperationBuilder's
    // `.handler()` installs a 405 stub for methods outside GET/POST/PUT/DELETE/
    // PATCH; the data plane treats OPTIONS like any other forwarded method once
    // the preflight short-circuit (handled here, in `handlers::proxy`) passes.

    router = register_proxy_methods(router, openapi);

    router.layer(Extension(cp)).layer(Extension(dp))
}

/// Register one operation per forwarded method for both proxy path forms.
fn register_proxy_methods(router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    let mut router = router;
    for path in ["/oagw/v1/proxy/{alias}", "/oagw/v1/proxy/{alias}/{*path}"] {
        router = register_proxy_get(router, openapi, path);
        router = register_proxy_post(router, openapi, path);
        router = register_proxy_put(router, openapi, path);
        router = register_proxy_delete(router, openapi, path);
        router = register_proxy_patch(router, openapi, path);
        router = register_proxy_options(router, path);
    }
    router
}

fn register_proxy_get(router: Router, openapi: &dyn OpenApiRegistry, path: &str) -> Router {
    OperationBuilder::get(path)
        .operation_id(format!(
            "oagw.proxy.get.{}",
            path.replace(['/', '{', '}'], "_")
        ))
        .summary("Proxy a GET request")
        .description(
            "Forward a GET request to the upstream bound to the alias, applying the \
             resolved auth, guard, transform and rate-limit configuration.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("alias", "The upstream alias to route to")
        .param(toolkit::api::operation_builder::ParamSpec {
            name: "path".to_owned(),
            location: toolkit::api::operation_builder::ParamLocation::Path,
            required: false,
            description: Some("Optional path appended to the route's base path".to_owned()),
            param_type: "string".to_owned(),
            array: false,
        })
        .handler(handlers::proxy)
        .json_response(StatusCode::OK, "Upstream response (passthrough)")
        .standard_errors(openapi)
        .register(router, openapi)
}

fn register_proxy_post(router: Router, openapi: &dyn OpenApiRegistry, path: &str) -> Router {
    OperationBuilder::post(path)
        .operation_id(format!(
            "oagw.proxy.post.{}",
            path.replace(['/', '{', '}'], "_")
        ))
        .summary("Proxy a POST request")
        .description(
            "Forward a POST request to the upstream bound to the alias, applying the \
             resolved auth, guard, transform and rate-limit configuration.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("alias", "The upstream alias to route to")
        .handler(handlers::proxy)
        .json_response(StatusCode::OK, "Upstream response (passthrough)")
        .standard_errors(openapi)
        .register(router, openapi)
}

fn register_proxy_put(router: Router, openapi: &dyn OpenApiRegistry, path: &str) -> Router {
    OperationBuilder::put(path)
        .operation_id(format!(
            "oagw.proxy.put.{}",
            path.replace(['/', '{', '}'], "_")
        ))
        .summary("Proxy a PUT request")
        .description(
            "Forward a PUT request to the upstream bound to the alias, applying the \
             resolved auth, guard, transform and rate-limit configuration.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("alias", "The upstream alias to route to")
        .handler(handlers::proxy)
        .json_response(StatusCode::OK, "Upstream response (passthrough)")
        .standard_errors(openapi)
        .register(router, openapi)
}

fn register_proxy_delete(router: Router, openapi: &dyn OpenApiRegistry, path: &str) -> Router {
    OperationBuilder::delete(path)
        .operation_id(format!(
            "oagw.proxy.delete.{}",
            path.replace(['/', '{', '}'], "_")
        ))
        .summary("Proxy a DELETE request")
        .description(
            "Forward a DELETE request to the upstream bound to the alias, applying the \
             resolved auth, guard, transform and rate-limit configuration.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("alias", "The upstream alias to route to")
        .handler(handlers::proxy)
        .json_response(StatusCode::OK, "Upstream response (passthrough)")
        .standard_errors(openapi)
        .register(router, openapi)
}

fn register_proxy_patch(router: Router, openapi: &dyn OpenApiRegistry, path: &str) -> Router {
    OperationBuilder::patch(path)
        .operation_id(format!(
            "oagw.proxy.patch.{}",
            path.replace(['/', '{', '}'], "_")
        ))
        .summary("Proxy a PATCH request")
        .description(
            "Forward a PATCH request to the upstream bound to the alias, applying the \
             resolved auth, guard, transform and rate-limit configuration.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("alias", "The upstream alias to route to")
        .handler(handlers::proxy)
        .json_response(StatusCode::OK, "Upstream response (passthrough)")
        .standard_errors(openapi)
        .register(router, openapi)
}

/// Register the OPTIONS verb for the given proxy path form via raw axum routing.
fn register_proxy_options(router: Router, path: &str) -> Router {
    router.route(path, routing::options(handlers::proxy))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use axum::response::Response;
    use httpmock::MockServer;
    use toolkit::api::{OpenApiRegistryImpl, operation_builder::OperationBuilder};
    use toolkit_security::SecurityContext;
    use tower::util::ServiceExt;
    use uuid::Uuid;

    use super::register_routes;
    use crate::config::OagwConfig;
    use crate::domain::error::gts;
    use crate::domain::models::plugin_gts;
    use crate::domain::scopes;
    use crate::domain::services::management::ControlPlaneService;
    use crate::domain::test_util::MockTenantResolver;
    use crate::infra::plugin::{
        AuthPluginRegistry, GuardPluginRegistry, ResolvingPluginValidator, TransformPluginRegistry,
    };
    use crate::infra::proxy::DataPlaneServiceImpl;
    use crate::infra::storage::{ControlPlaneStore, SharedStore};

    const HTTP_PROTO: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

    /// A fully-wired gear stack: in-memory store, built-in plugin registries,
    /// both services, and the REST routes (mirrors `OagwGear::init` wiring).
    struct TestStack {
        router: axum::Router,
        creds: Arc<dyn credstore_sdk::CredStoreClientV1>,
    }

    /// Config used by most proxy tests: plain-HTTP upstreams are permitted so
    /// the httpmock servers are reachable.
    fn test_config() -> OagwConfig {
        OagwConfig {
            allow_http_upstream: true,
            ..Default::default()
        }
    }

    fn build_stack_with(
        config: OagwConfig,
        resolver: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>>,
        creds: Arc<dyn credstore_sdk::CredStoreClientV1>,
    ) -> TestStack {
        let store: SharedStore = Arc::new(ControlPlaneStore::new());
        let auth = AuthPluginRegistry::with_builtins(
            creds.clone(),
            config.token_cache,
            toolkit_http::HttpClientConfig::proxy(),
        );
        let guards = GuardPluginRegistry::with_builtins();
        let transforms = TransformPluginRegistry::with_builtins();
        let validator = Arc::new(ResolvingPluginValidator::new(
            Arc::clone(&store),
            auth.clone(),
            guards.clone(),
            transforms.clone(),
        ));
        let cfg = Arc::new(config);
        let cp = Arc::new(ControlPlaneService::new(
            Arc::clone(&store),
            resolver,
            Arc::clone(&cfg),
            validator,
        ));
        let dp = Arc::new(DataPlaneServiceImpl::new(
            Arc::clone(&cp),
            cfg,
            auth,
            guards,
            transforms,
        ));
        let router = register_routes(axum::Router::new(), &OpenApiRegistryImpl::new(), cp, dp);
        TestStack { router, creds }
    }

    fn build_stack() -> TestStack {
        build_stack_with(
            test_config(),
            None,
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty()),
        )
    }

    fn build_router() -> axum::Router {
        build_stack().router
    }

    fn ctx_with(scopes_list: &[&str], tenant: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(tenant)
            .token_scopes(scopes_list.iter().map(|s| s.to_string()).collect())
            .build()
            .expect("valid security context")
    }

    fn ctx_with_scope(scope: &str) -> SecurityContext {
        ctx_with(&[scope], Uuid::nil())
    }

    fn proxy_ctx() -> SecurityContext {
        ctx_with(&[scopes::PROXY_INVOKE], Uuid::nil())
    }

    fn req_build(method: Method, uri: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .expect("valid test request")
    }

    /// Build a request with a security context, headers and optional body.
    fn request(
        method: Method,
        uri: &str,
        ctx: &SecurityContext,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Request<Body> {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.map(str::to_owned).unwrap_or_default()))
            .expect("valid test request");
        for (name, value) in headers {
            req.headers_mut().insert(
                axum::http::header::HeaderName::from_bytes(name.as_bytes())
                    .expect("valid test header name"),
                axum::http::HeaderValue::from_str(value).expect("valid test header value"),
            );
        }
        req.extensions_mut().insert(ctx.clone());
        req
    }

    /// Serve one request through the router using tower's `oneshot`.
    async fn run(router: &axum::Router, req: Request<Body>) -> Response {
        router.clone().oneshot(req).await.expect("router serves")
    }

    /// Full HTTP call against the stack.
    async fn call(
        stack: &TestStack,
        method: Method,
        uri: &str,
        ctx: &SecurityContext,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Response {
        run(&stack.router, request(method, uri, ctx, headers, body)).await
    }

    async fn response_body(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22)
            .await
            .expect("read response body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    async fn response_json(resp: Response) -> serde_json::Value {
        serde_json::from_str(&response_body(resp).await).expect("response body is JSON")
    }

    /// Assert an RFC 9457 gateway problem response: status, `type`, the
    /// `X-OAGW-Error-Source` header and the `content-type`. Returns the body
    /// so callers can check extra fields.
    async fn assert_problem(
        resp: Response,
        status: StatusCode,
        gts_id: &str,
        source: &str,
    ) -> serde_json::Value {
        assert_eq!(resp.status(), status, "problem response status");
        assert_eq!(
            resp.headers()
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some(source),
            "x-oagw-error-source header"
        );
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/problem+json"),
            "problem+json content type"
        );
        let json = response_json(resp).await;
        assert_eq!(json["type"], gts_id, "problem type");
        assert_eq!(
            json["status"],
            status.as_u16() as u64,
            "problem status field"
        );
        json
    }

    fn upstream_json(alias: &str, host: &str, port: u16) -> serde_json::Value {
        serde_json::json!({
            "enabled": true,
            "alias": alias,
            "server": { "endpoints": [{ "scheme": "http", "host": host, "port": port }] },
            "protocol": HTTP_PROTO
        })
    }

    fn route_json(upstream_id: &str, methods: &[&str], path: &str) -> serde_json::Value {
        serde_json::json!({
            "enabled": true,
            "upstream_id": upstream_id,
            "match": { "http": { "methods": methods, "path": path } }
        })
    }

    /// POST an upstream and return its id (asserts 201).
    async fn create_upstream_as(
        stack: &TestStack,
        body: serde_json::Value,
        ctx: &SecurityContext,
    ) -> String {
        let resp = call(
            stack,
            Method::POST,
            "/oagw/v1/upstreams",
            ctx,
            &[],
            Some(body.to_string().as_str()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED, "create upstream {body}");
        let location = resp
            .headers()
            .get(header::LOCATION)
            .expect("created upstream has Location")
            .to_str()
            .expect("ascii location")
            .to_owned();
        let _ = response_body(resp).await;
        location.rsplit('/').next().unwrap().to_owned()
    }

    /// POST a route and return its id (asserts 201).
    async fn create_route_as(
        stack: &TestStack,
        body: serde_json::Value,
        ctx: &SecurityContext,
    ) -> String {
        let resp = call(
            stack,
            Method::POST,
            "/oagw/v1/routes",
            ctx,
            &[],
            Some(body.to_string().as_str()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED, "create route {body}");
        let location = resp
            .headers()
            .get(header::LOCATION)
            .expect("created route has Location")
            .to_str()
            .expect("ascii location")
            .to_owned();
        let _ = response_body(resp).await;
        location.rsplit('/').next().unwrap().to_owned()
    }

    /// Create the canonical single-endpoint http upstream + a GET-all route.
    async fn make_upstream_and_route(
        stack: &TestStack,
        ctx: &SecurityContext,
        alias: &str,
        host: &str,
        port: u16,
    ) {
        let up_id = create_upstream_as(stack, upstream_json(alias, host, port), ctx).await;
        create_route_as(stack, route_json(&up_id, &["GET"], "/"), ctx).await;
    }

    // -----------------------------------------------------------------------
    // Management API (target 1)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn management_crud_roundtrip() {
        let stack = build_stack();
        let ctx = ctx_with_scope("*");

        // List is initially empty.
        let resp = call(&stack, Method::GET, "/oagw/v1/upstreams", &ctx, &[], None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = response_json(resp).await;
        assert_eq!(json["total"], 0);
        assert!(json["items"].is_array());

        let up_body = serde_json::json!({
            "enabled": true,
            "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com", "port": 443 }] },
            "protocol": HTTP_PROTO
        });
        let up_id = create_upstream_as(&stack, up_body, &ctx).await;

        // Fetch it back by id; tenant_id must never leak.
        let resp = call(
            &stack,
            Method::GET,
            &format!("/oagw/v1/upstreams/{up_id}"),
            &ctx,
            &[],
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "get upstream");
        assert!(
            resp.headers().get("x-oagw-error-source").is_none(),
            "management success responses carry no error-source header"
        );
        let stored = response_json(resp).await;
        assert!(stored.get("tenant_id").is_none(), "tenant_id never appears");

        // Delete it.
        let resp = call(
            &stack,
            Method::DELETE,
            &format!("/oagw/v1/upstreams/{up_id}"),
            &ctx,
            &[],
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT, "delete upstream");

        // Gone now.
        let resp = call(
            &stack,
            Method::GET,
            &format!("/oagw/v1/upstreams/{up_id}"),
            &ctx,
            &[],
            None,
        )
        .await;
        assert!(resp.status().is_client_error(), "deleted upstream is gone");
    }

    #[tokio::test]
    async fn management_records_are_tenant_scoped() {
        let t1 = Uuid::new_v4();
        let t2 = Uuid::new_v4();
        let resolver: Arc<dyn tenant_resolver_sdk::TenantResolverClient> =
            Arc::new(MockTenantResolver::new([
                (t1, Vec::new()),
                (t2, Vec::new()),
            ]));
        let stack = build_stack_with(
            test_config(),
            Some(resolver),
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty()),
        );
        let ctx1 = ctx_with(&["*"], t1);
        let ctx2 = ctx_with(&["*"], t2);

        let up_id =
            create_upstream_as(&stack, upstream_json("svc-a", "127.0.0.1", 80), &ctx1).await;

        // Tenant 2 cannot read or delete tenant 1's upstream.
        let resp = call(
            &stack,
            Method::GET,
            &format!("/oagw/v1/upstreams/{up_id}"),
            &ctx2,
            &[],
            None,
        )
        .await;
        assert_problem(resp, StatusCode::NOT_FOUND, gts::ROUTE_NOT_FOUND, "gateway").await;
        let resp = call(
            &stack,
            Method::DELETE,
            &format!("/oagw/v1/upstreams/{up_id}"),
            &ctx2,
            &[],
            None,
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "foreign delete is a 404"
        );

        // Lists are tenant-scoped too.
        let resp = call(&stack, Method::GET, "/oagw/v1/upstreams", &ctx2, &[], None).await;
        let json = response_json(resp).await;
        assert_eq!(json["total"], 0, "tenant 2 sees no upstreams");
        let resp = call(&stack, Method::GET, "/oagw/v1/upstreams", &ctx1, &[], None).await;
        let json = response_json(resp).await;
        assert_eq!(json["total"], 1, "tenant 1 sees its own upstream");
    }

    #[tokio::test]
    async fn upstream_alias_is_immutable() {
        let stack = build_stack();
        let ctx = ctx_with_scope("*");
        let up_id = create_upstream_as(&stack, upstream_json("mock", "127.0.0.1", 80), &ctx).await;

        // PUT with a different alias is rejected with a validation problem.
        let mut replaced = serde_json::json!({
            "enabled": true,
            "alias": "renamed",
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 80 }] },
            "protocol": HTTP_PROTO
        });
        replaced["id"] = serde_json::json!(up_id);
        let resp = call(
            &stack,
            Method::PUT,
            &format!("/oagw/v1/upstreams/{up_id}"),
            &ctx,
            &[],
            Some(replaced.to_string().as_str()),
        )
        .await;
        assert_problem(resp, StatusCode::BAD_REQUEST, gts::VALIDATION, "gateway").await;

        // The record is unchanged.
        let resp = call(
            &stack,
            Method::GET,
            &format!("/oagw/v1/upstreams/{up_id}"),
            &ctx,
            &[],
            None,
        )
        .await;
        let json = response_json(resp).await;
        assert_eq!(json["alias"], "mock", "alias unchanged");
    }

    #[tokio::test]
    async fn disabled_upstream_is_listed_but_not_routed() {
        let stack = build_stack();
        let ctx = ctx_with_scope("*");
        let mut body = upstream_json("offline", "127.0.0.1", 80);
        body["enabled"] = serde_json::json!(false);
        let up_id = create_upstream_as(&stack, body, &ctx).await;
        create_route_as(&stack, route_json(&up_id, &["GET"], "/"), &ctx).await;

        // Visible to management ...
        let resp = call(&stack, Method::GET, "/oagw/v1/upstreams", &ctx, &[], None).await;
        let json = response_json(resp).await;
        assert_eq!(json["total"], 1, "disabled upstream is listed");

        // ... but the proxy skips it: no enabled same-alias upstream exists.
        let resp = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/offline",
            &proxy_ctx(),
            &[],
            None,
        )
        .await;
        assert_problem(resp, StatusCode::NOT_FOUND, gts::ROUTE_NOT_FOUND, "gateway").await;
    }

    #[tokio::test]
    async fn management_crud_covers_custom_plugins() {
        let stack = build_stack();
        let ctx = ctx_with_scope("*");
        let plugin_body = serde_json::json!({
            "plugin_type": "auth",
            "name": "my-auth",
            "config_schema": { "type": "object", "properties": { "key": { "type": "string" } } },
            "source_code": "def authenticate(ctx): return True"
        });
        let resp = call(
            &stack,
            Method::POST,
            "/oagw/v1/plugins",
            &ctx,
            &[],
            Some(plugin_body.to_string().as_str()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED, "create plugin");
        let location = resp
            .headers()
            .get(header::LOCATION)
            .expect("Location")
            .to_str()
            .unwrap()
            .to_owned();
        let id = location.rsplit('/').next().unwrap().to_owned();
        let _ = response_body(resp).await;

        // Source reader + list + get.
        let resp = call(
            &stack,
            Method::GET,
            &format!("/oagw/v1/plugins/{id}/source"),
            &ctx,
            &[],
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "plugin source envelope");
        let resp = call(&stack, Method::GET, "/oagw/v1/plugins", &ctx, &[], None).await;
        let json = response_json(resp).await;
        assert_eq!(json["total"], 1, "plugin listed");

        // Delete then 404.
        let resp = call(
            &stack,
            Method::DELETE,
            &format!("/oagw/v1/plugins/{id}"),
            &ctx,
            &[],
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT, "delete plugin");
        let resp = call(
            &stack,
            Method::GET,
            &format!("/oagw/v1/plugins/{id}"),
            &ctx,
            &[],
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "deleted plugin gone");
    }

    #[tokio::test]
    async fn management_missing_scope_is_403_permission_denied() {
        let stack = build_stack();
        // Unscoped token (no wildcard).
        let ctx = ctx_with(&[], Uuid::nil());
        let resp = call(&stack, Method::GET, "/oagw/v1/upstreams", &ctx, &[], None).await;
        assert_problem(
            resp,
            StatusCode::FORBIDDEN,
            gts::PERMISSION_DENIED,
            "gateway",
        )
        .await;
    }

    // -----------------------------------------------------------------------
    // Validation -> 400 problem+json (target 2) + alias rules (target 3)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn management_validation_renders_problem_plus_json() {
        let stack = build_stack();
        let ctx = ctx_with_scope("*");

        // IP-based endpoints require an explicit alias.
        let body = serde_json::json!({
            "enabled": true,
            "server": { "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 80 }] },
            "protocol": HTTP_PROTO
        });
        let resp = call(
            &stack,
            Method::POST,
            "/oagw/v1/upstreams",
            &ctx,
            &[],
            Some(body.to_string().as_str()),
        )
        .await;
        let problem =
            assert_problem(resp, StatusCode::BAD_REQUEST, gts::VALIDATION, "gateway").await;
        assert!(
            problem["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("alias"),
            "detail explains the alias rule: {problem}"
        );

        // Unknown wire fields are rejected before the handler runs: axum's
        // `Json` extractor (with serde deny_unknown_fields) answers 422
        // Unprocessable Entity for schema violations, distinct from the
        // handler-produced 400 validation problems above.
        let resp = call(
            &stack,
            Method::POST,
            "/oagw/v1/upstreams",
            &ctx,
            &[],
            Some(r#"{"enabled":true,"bogus_field":1,"protocol":""}"#),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown wire fields are rejected by the JSON extractor"
        );
    }

    #[tokio::test]
    async fn proxy_common_suffix_pool_demands_target_host() {
        let stack = build_stack();
        let ctx = ctx_with_scope("*");
        let body = serde_json::json!({
            "enabled": true,
            "server": {
                "endpoints": [
                    { "scheme": "http", "host": "us.vendor.test", "port": 8443 },
                    { "scheme": "http", "host": "eu.vendor.test", "port": 8443 }
                ]
            },
            "protocol": HTTP_PROTO
        });
        let up_id = create_upstream_as(&stack, body, &ctx).await;
        create_route_as(&stack, route_json(&up_id, &["GET"], "/"), &ctx).await;
        let pctx = proxy_ctx();

        // The two-host pool auto-derives the common-suffix alias.
        let resp = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/vendor.test:8443",
            &pctx,
            &[],
            None,
        )
        .await;
        assert_problem(
            resp,
            StatusCode::BAD_REQUEST,
            gts::MISSING_TARGET_HOST,
            "gateway",
        )
        .await;

        // Malformed target host (port embedded) is invalid.
        let resp = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/vendor.test:8443",
            &pctx,
            &[("x-oagw-target-host", "eu.vendor.test:443")],
            None,
        )
        .await;
        assert_problem(
            resp,
            StatusCode::BAD_REQUEST,
            gts::INVALID_TARGET_HOST,
            "gateway",
        )
        .await;

        // Well-formed but unknown host.
        let resp = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/vendor.test:8443",
            &pctx,
            &[("x-oagw-target-host", "other.vendor.test")],
            None,
        )
        .await;
        assert_problem(
            resp,
            StatusCode::BAD_REQUEST,
            gts::UNKNOWN_TARGET_HOST,
            "gateway",
        )
        .await;

        // A known target host passes header validation and would forward
        // (connection refused to an unroutable hostname resolves as a 503,
        // never a 400 — proving the header was accepted).
        let resp = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/vendor.test:8443",
            &pctx,
            &[("x-oagw-target-host", "us.vendor.test")],
            None,
        )
        .await;
        assert_ne!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "known host accepted"
        );
    }

    // -----------------------------------------------------------------------
    // Proxy endpoint via httpmock (target 4)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn proxy_happy_path_forwards_and_passthrough() {
        let server = MockServer::start();
        let up_mock = server.mock(|when, then| {
            when.method("GET").path("/");
            then.status(200)
                .header("content-type", "text/plain")
                .body("hello-from-upstream");
        });
        let stack = build_stack();
        let ctx = ctx_with_scope("*");
        make_upstream_and_route(&stack, &ctx, "mock", "127.0.0.1", server.port()).await;

        let resp = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/mock",
            &proxy_ctx(),
            &[],
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "forwarded response");
        assert_eq!(
            resp.headers()
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some("upstream"),
            "successful proxy responses carry error-source: upstream"
        );
        let body = response_body(resp).await;
        assert_eq!(body, "hello-from-upstream", "body passes through");
        up_mock.assert_hits(1);
    }

    #[tokio::test]
    async fn proxy_unregistered_method_returns_405() {
        // The proxy registers GET/POST/PUT/DELETE/PATCH/OPTIONS only; TRACE is
        // not registered, so axum answers 405 before the handler runs.
        let stack = build_stack();
        let resp = call(
            &stack,
            Method::TRACE,
            "/oagw/v1/proxy/mock",
            &proxy_ctx(),
            &[],
            None,
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "unregistered verb 405"
        );
    }

    #[tokio::test]
    async fn proxy_method_not_in_route_returns_404() {
        let server = MockServer::start();
        let up_mock = server.mock(|when, then| {
            when.method("POST").path("/");
            then.status(200).body("should not happen");
        });
        let stack = build_stack();
        let ctx = ctx_with_scope("*");
        make_upstream_and_route(&stack, &ctx, "mock", "127.0.0.1", server.port()).await;

        // The route only matches GET.
        let resp = call(
            &stack,
            Method::POST,
            "/oagw/v1/proxy/mock",
            &proxy_ctx(),
            &[],
            Some("x"),
        )
        .await;
        assert_problem(resp, StatusCode::NOT_FOUND, gts::ROUTE_NOT_FOUND, "gateway").await;
        up_mock.assert_hits(0);
    }

    #[tokio::test]
    async fn proxy_longest_prefix_route_wins() {
        let server = MockServer::start();
        let winning = server.mock(|when, then| {
            // route "/v1" + suffix "/v1/chat" => outbound "/v1/v1/chat".
            when.method("GET").path("/v1/v1/chat");
            then.status(200).body("longest");
        });
        let losing = server.mock(|when, then| {
            // route "/" + suffix "/v1/chat" => outbound "/v1/chat".
            when.method("GET").path("/v1/chat");
            then.status(200).body("root");
        });
        let stack = build_stack();
        let ctx = ctx_with_scope("*");
        let up_id = create_upstream_as(
            &stack,
            upstream_json("mock", "127.0.0.1", server.port()),
            &ctx,
        )
        .await;
        create_route_as(&stack, route_json(&up_id, &["GET"], "/"), &ctx).await;
        create_route_as(&stack, route_json(&up_id, &["GET"], "/v1"), &ctx).await;

        let resp = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/mock/v1/chat",
            &proxy_ctx(),
            &[],
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(response_body(resp).await, "longest");
        winning.assert_hits(1);
        losing.assert_hits(0);
    }

    #[tokio::test]
    async fn proxy_query_allowlist_filters_parameters() {
        let server = MockServer::start();
        let up_mock = server.mock(|when, then| {
            when.method("GET").path("/").query_param("page", "2");
            then.status(200).body("paged");
        });
        let stack = build_stack();
        let ctx = ctx_with_scope("*");
        let up_id = create_upstream_as(
            &stack,
            upstream_json("mock", "127.0.0.1", server.port()),
            &ctx,
        )
        .await;
        let mut route = route_json(&up_id, &["GET"], "/");
        route["match"]["http"]["query_allowlist"] = serde_json::json!(["page"]);
        create_route_as(&stack, route, &ctx).await;

        // Allowlisted parameter passes through.
        let resp = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/mock?page=2",
            &proxy_ctx(),
            &[],
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "allowlisted query forwards");
        up_mock.assert_hits(1);

        // Unknown parameter -> 400 before the network.
        let resp = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/mock?page=2&x=1",
            &proxy_ctx(),
            &[],
            None,
        )
        .await;
        assert_problem(resp, StatusCode::BAD_REQUEST, gts::VALIDATION, "gateway").await;
        up_mock.assert_hits(1);
    }

    #[tokio::test]
    async fn proxy_header_transforms_set_add_and_allowlist() {
        let server = MockServer::start();
        let up_mock = server.mock(|when, then| {
            when.method("GET")
                .path("/")
                .header("x-request-id", "req-123")
                .header("x-set", "yes")
                .header("x-add", "yes")
                .header_missing("x-drop");
            then.status(200).body("hdrs");
        });
        let stack = build_stack();
        let ctx = ctx_with_scope("*");
        let mut body = upstream_json("mock", "127.0.0.1", server.port());
        body["headers"] = serde_json::json!({
            "request": {
                "passthrough": "allowlist",
                "passthrough_allowlist": ["X-Request-Id"],
                "set": { "X-Set": "yes" },
                "add": { "X-Add": "yes" }
            }
        });
        let up_id = create_upstream_as(&stack, body, &ctx).await;
        create_route_as(&stack, route_json(&up_id, &["GET"], "/"), &ctx).await;

        let resp = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/mock",
            &proxy_ctx(),
            &[("x-request-id", "req-123"), ("x-drop", "drop-me")],
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        // The matcher pins the allowlisted + set/add headers and the absence
        // of the inbound-only x-drop header, so a misconfigured transform
        // fails the assertion.
        up_mock.assert_hits(1);
    }

    #[tokio::test]
    async fn proxy_body_validation_limits_and_mismatch() {
        let server = MockServer::start();
        let up_mock = server.mock(|when, then| {
            when.method("POST").path("/");
            then.status(200).body("ok");
        });
        let config = OagwConfig {
            allow_http_upstream: true,
            body_limit_bytes: 32,
            ..Default::default()
        };
        let stack = build_stack_with(
            config,
            None,
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty()),
        );
        let ctx = ctx_with_scope("*");
        make_upstream_and_route(&stack, &ctx, "mock", "127.0.0.1", server.port()).await;

        // Hard size limit -> 413 before reaching the data plane.
        let big_body = "x".repeat(64);
        let resp = call(
            &stack,
            Method::POST,
            "/oagw/v1/proxy/mock",
            &proxy_ctx(),
            &[],
            Some(big_body.as_str()),
        )
        .await;
        let problem = assert_problem(
            resp,
            StatusCode::PAYLOAD_TOO_LARGE,
            gts::PAYLOAD_TOO_LARGE,
            "gateway",
        )
        .await;
        assert_eq!(
            problem["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("32"),
            true
        );

        // Content-length that disagrees with the actual body -> 400.
        let resp = call(
            &stack,
            Method::POST,
            "/oagw/v1/proxy/mock",
            &proxy_ctx(),
            &[("content-length", "999")],
            Some("tiny"),
        )
        .await;
        assert_problem(resp, StatusCode::BAD_REQUEST, gts::VALIDATION, "gateway").await;

        // Unsupported transfer-encoding -> 400.
        let resp = call(
            &stack,
            Method::POST,
            "/oagw/v1/proxy/mock",
            &proxy_ctx(),
            &[("transfer-encoding", "gzip")],
            None,
        )
        .await;
        assert_problem(resp, StatusCode::BAD_REQUEST, gts::VALIDATION, "gateway").await;

        up_mock.assert_hits(0);
    }

    #[tokio::test]
    async fn proxy_connection_refused_is_503_link_unavailable() {
        // Bind and drop a listener to obtain an unused port: nothing listens
        // there, so the forward fails with a transport error -> 503.
        let dead_port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
            listener.local_addr().expect("local addr").port()
        };
        let stack = build_stack();
        let ctx = ctx_with_scope("*");
        make_upstream_and_route(&stack, &ctx, "mock", "127.0.0.1", dead_port).await;

        let resp = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/mock",
            &proxy_ctx(),
            &[],
            None,
        )
        .await;
        assert_problem(
            resp,
            StatusCode::SERVICE_UNAVAILABLE,
            gts::LINK_UNAVAILABLE,
            "gateway",
        )
        .await;
    }

    // -----------------------------------------------------------------------
    // Auth plugins (target 5)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn proxy_apikey_auth_injects_credential_header() {
        let server = MockServer::start();
        let up_mock = server.mock(|when, then| {
            when.method("GET")
                .path("/")
                .header("x-api-key", "sekret-123");
            then.status(200).body("authed");
        });
        let creds: Arc<dyn credstore_sdk::CredStoreClientV1> =
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::with_secrets(
                vec![("apikey1".to_owned(), "sekret-123".to_owned())],
            ));
        let stack = build_stack_with(test_config(), None, creds);
        let ctx = ctx_with_scope("*");
        let mut body = upstream_json("mock", "127.0.0.1", server.port());
        body["auth"] = serde_json::json!({
            "plugin_type": plugin_gts::AUTH_APIKEY,
            "config": { "key_ref": "cred://apikey1", "header_name": "x-api-key" }
        });
        let up_id = create_upstream_as(&stack, body, &ctx).await;
        create_route_as(&stack, route_json(&up_id, &["GET"], "/"), &ctx).await;

        let resp = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/mock",
            &proxy_ctx(),
            &[],
            None,
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "credential injected upstream"
        );
        up_mock.assert_hits(1);
    }

    #[tokio::test]
    async fn proxy_apikey_missing_secret_is_401() {
        let server = MockServer::start();
        let up_mock = server.mock(|when, then| {
            when.method("GET").path("/");
            then.status(200).body("never");
        });
        let creds: Arc<dyn credstore_sdk::CredStoreClientV1> =
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty());
        let stack = build_stack_with(test_config(), None, creds);
        let ctx = ctx_with_scope("*");
        let mut body = upstream_json("mock", "127.0.0.1", server.port());
        body["auth"] = serde_json::json!({
            "plugin_type": plugin_gts::AUTH_APIKEY,
            "config": { "key_ref": "cred://does-not-exist" }
        });
        let up_id = create_upstream_as(&stack, body, &ctx).await;
        create_route_as(&stack, route_json(&up_id, &["GET"], "/"), &ctx).await;

        let resp = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/mock",
            &proxy_ctx(),
            &[],
            None,
        )
        .await;
        assert_problem(
            resp,
            StatusCode::UNAUTHORIZED,
            gts::AUTHENTICATION_FAILED,
            "gateway",
        )
        .await;
        up_mock.assert_hits(0);
    }

    #[tokio::test]
    async fn proxy_oauth2_token_is_fetched_once_and_cached() {
        let token_server = MockServer::start();
        let token_mock = token_server.mock(|when, then| {
            when.method("POST").path("/token");
            then.status(200).json_body(serde_json::json!({
                "access_token": "tok-abc",
                "token_type": "Bearer",
                "expires_in": 3600
            }));
        });
        let upstream_server = MockServer::start();
        let up_mock = upstream_server.mock(|when, then| {
            when.method("GET")
                .path("/")
                .header("authorization", "Bearer tok-abc");
            then.status(200).body("oauth2-ok");
        });
        let creds: Arc<dyn credstore_sdk::CredStoreClientV1> = Arc::new(
            credstore_sdk::test_util::MockCredStoreClient::with_secrets(vec![
                ("cid".to_owned(), "client-1".to_owned()),
                ("csec".to_owned(), "secret-1".to_owned()),
            ]),
        );
        let stack = build_stack_with(test_config(), None, creds);
        let ctx = ctx_with_scope("*");
        let token_url = format!("http://127.0.0.1:{}/token", token_server.port());
        let mut body = upstream_json("oauth", "127.0.0.1", upstream_server.port());
        body["auth"] = serde_json::json!({
            "plugin_type": plugin_gts::AUTH_OAUTH2_CLIENT_CRED,
            "config": {
                "token_endpoint": token_url,
                "client_id_ref": "cred://cid",
                "client_secret_ref": "cred://csec"
            }
        });
        let up_id = create_upstream_as(&stack, body, &ctx).await;
        create_route_as(&stack, route_json(&up_id, &["GET"], "/"), &ctx).await;

        // Same security context on both calls => same cache key.
        let pctx = proxy_ctx();
        let r1 = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/oauth",
            &pctx,
            &[],
            None,
        )
        .await;
        assert_eq!(r1.status(), StatusCode::OK, "first proxied call");
        let r2 = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/oauth",
            &pctx,
            &[],
            None,
        )
        .await;
        assert_eq!(
            r2.status(),
            StatusCode::OK,
            "second proxied call reuses cached token"
        );

        token_mock.assert_hits(1);
        up_mock.assert_hits(2);
    }

    #[tokio::test]
    async fn proxy_request_id_transform_injects_and_reuses() {
        // Generated id (no inbound header).
        let gen_server = MockServer::start();
        let gen_mock = gen_server.mock(|when, then| {
            when.method("GET").path("/").header_exists("x-request-id");
            then.status(200).body("gen");
        });
        // Reused id (inbound header present and allowed through by the
        // upstream's passthrough allowlist).
        let reuse_server = MockServer::start();
        let reuse_mock = reuse_server.mock(|when, then| {
            when.method("GET")
                .path("/")
                .header("x-request-id", "inbound-abc");
            then.status(200).body("reuse");
        });
        let stack = build_stack();
        let ctx = ctx_with_scope("*");
        let transform = plugin_gts::TRANSFORM_REQUEST_ID.to_owned();
        let mut route_gen = route_json(
            &create_upstream_as(
                &stack,
                upstream_json("gen", "127.0.0.1", gen_server.port()),
                &ctx,
            )
            .await,
            &["GET"],
            "/",
        );
        route_gen["plugins"] = serde_json::json!({ "sharing": "private", "items": [transform] });
        create_route_as(&stack, route_gen, &ctx).await;

        let resp = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/gen",
            &proxy_ctx(),
            &[],
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers().contains_key("x-request-id"),
            "transform propagates a request id to the client"
        );
        gen_mock.assert_hits(1);

        // The upstream must allow the correlation id through (passthrough
        // default is `None` — an admin-controlled security posture), then the
        // transform reuses the inbound value instead of generating a new one.
        let mut reuse_upstream = upstream_json("reuse", "127.0.0.1", reuse_server.port());
        reuse_upstream["headers"] = serde_json::json!({
            "request": { "passthrough": "allowlist", "passthrough_allowlist": ["X-Request-Id"] }
        });
        let mut route_reuse = route_json(
            &create_upstream_as(&stack, reuse_upstream, &ctx).await,
            &["GET"],
            "/",
        );
        route_reuse["plugins"] = serde_json::json!({ "sharing": "private", "items": [transform] });
        create_route_as(&stack, route_reuse, &ctx).await;

        let resp = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/reuse",
            &proxy_ctx(),
            &[("x-request-id", "inbound-abc")],
            None,
        )
        .await;
        let status = resp.status();
        if status != StatusCode::OK {
            let body = response_body(resp).await;
            panic!("reuse proxy failed with {status:?}: {body}");
        }
        assert_eq!(status, StatusCode::OK);
        // The strict matcher pins the reused value; a freshly generated id
        // would not match.
        reuse_mock.assert_hits(1);
    }

    // -----------------------------------------------------------------------
    // Rate limiting over HTTP (target 6)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn proxy_rate_limit_rejects_excess_with_429_headers() {
        let server = MockServer::start();
        let up_mock = server.mock(|when, then| {
            when.method("GET").path("/");
            then.status(200).body("rl-ok");
        });
        let stack = build_stack();
        let ctx = ctx_with_scope("*");
        let mut body = upstream_json("mock", "127.0.0.1", server.port());
        body["rate_limit"] = serde_json::json!({
            "sustained": { "rate": 2, "window": "minute" },
            "burst": { "capacity": 2 },
            "scope": "tenant",
            "cost": 1
        });
        let up_id = create_upstream_as(&stack, body, &ctx).await;
        create_route_as(&stack, route_json(&up_id, &["GET"], "/"), &ctx).await;
        let pctx = proxy_ctx();

        let r1 = call(&stack, Method::GET, "/oagw/v1/proxy/mock", &pctx, &[], None).await;
        assert_eq!(r1.status(), StatusCode::OK);
        assert_eq!(
            r1.headers()
                .get("x-ratelimit-limit")
                .and_then(|v| v.to_str().ok()),
            Some("2")
        );
        assert_eq!(
            r1.headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok()),
            Some("1")
        );

        let r2 = call(&stack, Method::GET, "/oagw/v1/proxy/mock", &pctx, &[], None).await;
        assert_eq!(r2.status(), StatusCode::OK);
        assert_eq!(
            r2.headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok()),
            Some("0")
        );

        let r3 = call(&stack, Method::GET, "/oagw/v1/proxy/mock", &pctx, &[], None).await;
        let problem = assert_problem(
            r3,
            StatusCode::TOO_MANY_REQUESTS,
            gts::RATE_LIMIT_EXCEEDED,
            "gateway",
        )
        .await;
        let retry_after = problem["retry_after_seconds"].as_u64().unwrap_or(0);
        assert!(
            retry_after >= 1,
            "retry-after in problem body, got {retry_after}"
        );

        up_mock.assert_hits(2);
    }

    // -----------------------------------------------------------------------
    // CORS over HTTP (target 7)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn proxy_cors_validates_actual_cross_origin_requests() {
        let server = MockServer::start();
        let up_mock = server.mock(|when, then| {
            when.method("GET").path("/");
            then.status(200).body("cors-ok");
        });
        let stack = build_stack();
        let ctx = ctx_with_scope("*");
        let mut body = upstream_json("mock", "127.0.0.1", server.port());
        body["cors"] = serde_json::json!({
            "enabled": true,
            "allowed_origins": ["https://app.example.com"],
            "allowed_methods": ["GET", "POST"]
        });
        let up_id = create_upstream_as(&stack, body, &ctx).await;
        create_route_as(
            &stack,
            route_json(&up_id, &["GET", "POST", "DELETE"], "/"),
            &ctx,
        )
        .await;
        let pctx = proxy_ctx();

        // Disallowed origin -> 403 before any forwarding.
        let resp = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/mock",
            &pctx,
            &[("origin", "https://evil.com")],
            None,
        )
        .await;
        assert_problem(
            resp,
            StatusCode::FORBIDDEN,
            gts::CORS_ORIGIN_NOT_ALLOWED,
            "gateway",
        )
        .await;

        // Allowed origin but method not in the CORS allowlist -> 403.
        let resp = call(
            &stack,
            Method::DELETE,
            "/oagw/v1/proxy/mock",
            &pctx,
            &[("origin", "https://app.example.com")],
            None,
        )
        .await;
        assert_problem(
            resp,
            StatusCode::FORBIDDEN,
            gts::CORS_METHOD_NOT_ALLOWED,
            "gateway",
        )
        .await;

        // Allowed origin + method -> forwarded with CORS headers.
        let resp = call(
            &stack,
            Method::GET,
            "/oagw/v1/proxy/mock",
            &pctx,
            &[("origin", "https://app.example.com")],
            None,
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "allowed cross-origin request forwards"
        );
        assert_eq!(
            resp.headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("https://app.example.com"),
            "origin echoed in CORS header"
        );
        let vary = resp
            .headers()
            .get("vary")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        assert!(
            vary.contains("origin"),
            "vary includes origin, got {vary:?}"
        );

        up_mock.assert_hits(1);
    }

    // -----------------------------------------------------------------------
    // Route registration
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn route_registration_builds_without_conflicts() {
        // Building twice would panic if two OperationBuilder registrations
        // claimed the same (method, path) or the raw OPTIONS proxy routes
        // collided with the OperationBuilder ones.
        let _ = build_router();
        let _ = build_router();
    }

    #[tokio::test]
    async fn proxy_options_preflight_is_answered_204() {
        let router = build_router();
        let mut req = req_build(Method::OPTIONS, "/oagw/v1/proxy/example.org");
        // The gateway's auth middleware inserts an anonymous context before
        // the handler's `Extension<SecurityContext>` extractor runs; mirror
        // that here (the preflight branch never uses it).
        req.extensions_mut().insert(SecurityContext::anonymous());
        req.headers_mut()
            .insert(header::ORIGIN, "https://client.example".parse().unwrap());
        req.headers_mut().insert(
            header::ACCESS_CONTROL_REQUEST_METHOD,
            "GET".parse().unwrap(),
        );
        let resp = router.oneshot(req).await.expect("router serves");
        assert_eq!(
            resp.status(),
            StatusCode::NO_CONTENT,
            "preflight must short-circuit with 204, got {:?}",
            resp.status()
        );
    }
}
