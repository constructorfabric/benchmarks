//! REST-level tests for slice S2b: route and plugin management
//! (`/oagw/v1/routes`, `/oagw/v1/plugins`).
//!
//! These exercise the full transport stack of the two new resources — route
//! registration, handler extraction, DTO serialization, OData list parameters
//! and the RFC 9457 problem+json error surface — without a database: the
//! control plane is backed by the in-memory store, exactly as the e2e
//! configuration runs it.
//!
//! Tenant scoping is the security property under test: a resource of another
//! tenant is a 404 (never a 403), and an unauthenticated request is a 401, so
//! no management endpoint can be probed for existence.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use oagw::OagwConfig;
use oagw::api::rest::extract::MAX_BODY_BYTES;
use oagw::api::rest::routes::register_routes;
use oagw::domain::services::control_plane::ControlPlaneService;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;

/// Route base path (gear-relative, without `/api`).
const ROUTES: &str = "/oagw/v1/routes";

/// Plugin base path.
const PLUGINS: &str = "/oagw/v1/plugins";

/// Upstream base path, used to create the resources routes hang off.
const UPSTREAMS: &str = "/oagw/v1/upstreams";

/// The tenant every request in this file is authenticated for by default.
const TENANT: Uuid = Uuid::from_u128(0x6f61_6777_0000_0000_0000_0000_0000_0002);

/// A second tenant, used for the isolation assertions.
const OTHER_TENANT: Uuid = Uuid::from_u128(0x6f61_6777_0000_0000_0000_0000_0000_0003);

/// Minimal OpenAPI registry: records nothing, returns the schema name.
struct NoopOpenApiRegistry;

impl OpenApiRegistry for NoopOpenApiRegistry {
    fn register_operation(&self, _spec: &OperationSpec) {}

    fn ensure_schema_raw(
        &self,
        name: &str,
        _schemas: Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) -> String {
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A router serving the OAGW control plane.
fn router() -> Router {
    let service = ControlPlaneService::new(OagwConfig::default());
    register_routes(Router::new(), &NoopOpenApiRegistry, Arc::new(service))
}

/// A JSON upstream body for `host`.
fn upstream_body(host: &str) -> Value {
    json!({
        "server": { "endpoints": [ { "scheme": "https", "host": host } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    })
}

/// A JSON upstream body for `host` that references `plugin_id` in its plugin
/// chain.
///
/// The host is a parameter because the derived alias must be unique per tenant,
/// so two referencing upstreams in one test need distinct hosts.
fn upstream_with_plugin_body(host: &str, plugin_id: &str) -> Value {
    let mut body = upstream_body(host);
    body["plugins"] = json!({ "sharing": "private", "items": [plugin_id] });
    body
}

/// A JSON route body matching `methods` on `path` for `upstream_id`.
fn route_body(upstream_id: &str, path: &str, methods: &[&str]) -> Value {
    json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": methods, "path": path } }
    })
}

/// A JSON plugin body.
fn plugin_body(name: &str, plugin_type: &str, source: &str) -> Value {
    json!({
        "name": name,
        "plugin_type": plugin_type,
        "source_code": source
    })
}

/// The Starlark source used by the plugin tests.
const SOURCE: &str = "def on_request(ctx):\n    ctx.set_header('x-trace', ctx.request_id())\n";

/// Send a request to the router and return `(status, headers, json body)`.
///
/// `context` mirrors the `SecurityContext` extension the host gateway's auth
/// middleware inserts; `None` means the request reaches the gear unauthenticated.
async fn send(
    router: Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    context: Option<SecurityContext>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let (status, headers, bytes) = send_raw(router, method, uri, body, context).await;
    let parsed = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };

    (status, headers, parsed)
}

/// [`send`] without decoding the body, for non-JSON responses.
async fn send_raw(
    router: Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    context: Option<SecurityContext>,
) -> (StatusCode, axum::http::HeaderMap, axum::body::Bytes) {
    let builder = Request::builder().method(method).uri(uri);
    let builder = if body.is_some() {
        builder.header("content-type", "application/json")
    } else {
        builder
    };
    let body = Body::from(body.map(|value| value.to_string()).unwrap_or_default());

    send_body(router, builder, body, context).await
}

/// Send a request with an explicit [`Body`], for bodies no JSON value can carry
/// (an over-limit stream, in particular).
async fn send_body(
    router: Router,
    builder: http::request::Builder,
    body: Body,
    context: Option<SecurityContext>,
) -> (StatusCode, axum::http::HeaderMap, axum::body::Bytes) {
    let mut builder = builder;
    if let Some(context) = context {
        builder = builder.extension(context);
    }
    let request = builder.body(body).expect("request builds");

    let response = router.oneshot(request).await.expect("router responds");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();

    (status, headers, bytes)
}

/// [`send_body`] with the response decoded as JSON.
async fn send_body_json(
    router: Router,
    builder: http::request::Builder,
    body: Body,
    context: Option<SecurityContext>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let (status, headers, bytes) = send_body(router, builder, body, context).await;
    let parsed = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };

    (status, headers, parsed)
}

/// A `SecurityContext` authenticated for `tenant`.
fn security_context(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant)
        .build()
        .expect("security context builds")
}

/// An authenticated request for the file's tenant.
async fn authed(
    router: Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    send(router, method, uri, body, Some(security_context(TENANT))).await
}

/// An unauthenticated request (no `SecurityContext` extension).
async fn unauthenticated(
    router: Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    send(router, method, uri, body, None).await
}

/// `X-OAGW-Error-Source` of a response.
fn error_source(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("x-oagw-error-source")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

/// The `content-type` of a response.
fn content_type(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

/// Assert a problem document carries the gateway error-source marker.
fn assert_gateway_source(status: StatusCode, headers: &axum::http::HeaderMap, problem: &Value) {
    assert_eq!(
        content_type(headers).as_deref(),
        Some("application/problem+json"),
        "gateway errors are problem+json"
    );
    assert_eq!(
        error_source(headers).as_deref(),
        Some("gateway"),
        "gateway errors are stamped with the error source"
    );
    assert_eq!(problem["status"], status.as_u16());
}

/// Assert a response is a 400 validation problem document.
fn assert_validation_problem(status: StatusCode, headers: &axum::http::HeaderMap, problem: &Value) {
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_gateway_source(status, headers, problem);
    assert_eq!(problem["title"], "Validation Error");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

/// Assert `items[field]` is monotonic in list order.
///
/// `ascending` selects non-decreasing (`true`) or non-increasing (`false`).
/// List timestamps have second resolution, so equal keys are expected and
/// allowed to appear in any order.
fn assert_sorted_by(items: &[Value], field: &str, ascending: bool) {
    let mut previous = items
        .first()
        .and_then(|item| item[field].as_u64())
        .unwrap_or_default();
    for item in items {
        let current = item[field]
            .as_u64()
            .unwrap_or_else(|| panic!("{field} is a number"));
        if ascending {
            assert!(current >= previous, "{field} is non-decreasing: {items:?}");
        } else {
            assert!(current <= previous, "{field} is non-increasing: {items:?}");
        }
        previous = current;
    }
}

/// Create an upstream and return its id.
async fn create_upstream(router: &Router, host: &str) -> String {
    let (status, _, created) =
        authed(router.clone(), "POST", UPSTREAMS, Some(upstream_body(host))).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "the fixture upstream is created"
    );

    created["id"].as_str().expect("id is a string").to_owned()
}

/// Create a route and return the status, headers and body.
async fn create_route(router: &Router, body: Value) -> (StatusCode, axum::http::HeaderMap, Value) {
    let (status, headers, created) = authed(router.clone(), "POST", ROUTES, Some(body)).await;
    if status == StatusCode::CREATED {
        assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
    }

    (status, headers, created)
}

/// Create a plugin and return the status, headers and body.
async fn create_plugin(router: &Router, body: Value) -> (StatusCode, axum::http::HeaderMap, Value) {
    let (status, headers, created) = authed(router.clone(), "POST", PLUGINS, Some(body)).await;

    (status, headers, created)
}

// ---------------------------------------------------------------------------
// Route CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn route_crud_round_trips() {
    let router = router();
    let upstream_id = create_upstream(&router, "api.example.com").await;

    // POST - create
    let (status, _, created) = create_route(
        &router,
        route_body(&upstream_id, "/v1/chat", &["GET", "POST"]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().expect("id").to_owned();
    assert_eq!(created["upstream_id"], upstream_id.as_str());
    assert_eq!(
        created["match"]["http"]["path"], "/v1/chat",
        "the match rule round-trips under the `match` key"
    );
    assert_eq!(created["match"]["http"]["methods"], json!(["GET", "POST"]));
    assert_eq!(
        created["match"]["http"]["path_suffix_mode"], "append",
        "the schema default is applied"
    );
    assert_eq!(created["enabled"], json!(true), "routes default to enabled");

    // GET - by bare uuid and by GTS identifier
    let (status, _, fetched) = authed(router.clone(), "GET", &format!("{ROUTES}/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched, created);

    let gts_id = format!("gts.cf.core.oagw.route.v1~{id}");
    let (status, _, by_gts_id) =
        authed(router.clone(), "GET", &format!("{ROUTES}/{gts_id}"), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the GTS form resolves to the record"
    );
    assert_eq!(by_gts_id, created, "the body still carries the bare UUID");

    // LIST - a bare array holding the created route
    let (status, _, listed) = authed(router.clone(), "GET", ROUTES, None).await;
    assert_eq!(status, StatusCode::OK);
    let items = listed.as_array().expect("bare array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], created["id"]);

    // PUT - full replacement
    let mut replacement = route_body(&upstream_id, "/v2/messages", &["POST"]);
    replacement["enabled"] = json!(false);
    replacement["tags"] = json!(["chat"]);
    let (status, _, replaced) = authed(
        router.clone(),
        "PUT",
        &format!("{ROUTES}/{id}"),
        Some(replacement),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replaced["id"], created["id"], "the id is preserved");
    assert_eq!(
        replaced["created_at"], created["created_at"],
        "created_at is preserved"
    );
    assert!(
        replaced["updated_at"].as_u64().expect("updated_at")
            >= created["created_at"].as_u64().expect("created_at"),
        "updated_at is refreshed"
    );
    assert_eq!(replaced["match"]["http"]["path"], "/v2/messages");
    assert_eq!(replaced["match"]["http"]["methods"], json!(["POST"]));
    assert_eq!(
        replaced["enabled"],
        json!(false),
        "the replacement clears omitted fields"
    );
    assert_eq!(replaced["tags"], json!(["chat"]));

    // The replacement is visible through GET.
    let (status, _, fetched) = authed(router.clone(), "GET", &format!("{ROUTES}/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["match"]["http"]["path"], "/v2/messages");

    // DELETE - then a 404
    let (status, headers, body) =
        authed(router.clone(), "DELETE", &format!("{ROUTES}/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_null());
    assert_eq!(
        error_source(&headers).as_deref(),
        Some("gateway"),
        "success responses carry the error-source header too"
    );

    let (status, _, problem) = authed(router, "GET", &format!("{ROUTES}/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(problem["title"], "Route Not Found");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
}

#[tokio::test]
async fn route_for_a_foreign_upstream_is_a_404() {
    let router = router();

    // The upstream belongs to another tenant: it is not addressable, so the
    // route cannot be created (DESIGN §3.3 Tenant Scoping: 404, never 403).
    let (_, _, foreign) = send(
        router.clone(),
        "POST",
        UPSTREAMS,
        Some(upstream_body("ancestor.example.com")),
        Some(security_context(OTHER_TENANT)),
    )
    .await;
    let foreign_id = foreign["id"].as_str().expect("id").to_owned();

    let (status, headers, problem) =
        create_route(&router, route_body(&foreign_id, "/v1", &["GET"])).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_gateway_source(status, &headers, &problem);
    assert_eq!(problem["title"], "Upstream Not Found");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1"
    );

    // An upstream that exists nowhere is the same 404: existence is not
    // disclosed across tenants.
    let (status, _, problem) = create_route(
        &router,
        route_body(&Uuid::new_v4().to_string(), "/v1", &["GET"]),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(problem["title"], "Upstream Not Found");
}

#[tokio::test]
async fn duplicate_match_rule_is_a_409_and_disjoint_methods_are_not() {
    let router = router();
    let upstream_id = create_upstream(&router, "api.example.com").await;

    let (status, _, _) = create_route(
        &router,
        route_body(&upstream_id, "/v1/chat", &["GET", "POST"]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // The same path with an intersecting method set conflicts, even when the
    // route is disabled: a disabled route can be re-enabled, so it keeps its
    // claim on the match rule.
    for methods in [vec!["GET"], vec!["POST"], vec!["PATCH", "GET"]] {
        let (status, headers, problem) =
            create_route(&router, route_body(&upstream_id, "/v1/chat", &methods)).await;
        assert_eq!(status, StatusCode::CONFLICT, "methods {methods:?}");
        assert_gateway_source(status, &headers, &problem);
        assert_eq!(problem["title"], "Match Conflict");
        assert_eq!(
            problem["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.route.match_conflict.v1"
        );
    }

    // A different upstream of the same tenant may reuse the rule.
    let other_upstream = create_upstream(&router, "eu.example.com").await;
    let (status, _, _) =
        create_route(&router, route_body(&other_upstream, "/v1/chat", &["GET"])).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "another upstream is a distinct rule"
    );

    // A disjoint method set on the same path is a distinct rule.
    let (status, _, _) = create_route(
        &router.clone(),
        route_body(&upstream_id, "/v1/chat", &["DELETE"]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // The same path for another tenant is a distinct rule too.
    let (status, _, _) = send(
        router,
        "POST",
        ROUTES,
        Some(route_body(&other_upstream, "/v1/chat", &["GET"])),
        Some(security_context(OTHER_TENANT)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "tenant B cannot see tenant A's upstream"
    );
}

#[tokio::test]
async fn replace_cannot_move_a_route_to_another_upstream() {
    let router = router();
    let upstream_id = create_upstream(&router, "api.example.com").await;
    let other_upstream = create_upstream(&router, "eu.example.com").await;

    let (_, _, created) = authed(
        router.clone(),
        "POST",
        ROUTES,
        Some(route_body(&upstream_id, "/v1", &["GET"])),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, headers, problem) = authed(
        router,
        "PUT",
        &format!("{ROUTES}/{id}"),
        Some(route_body(&other_upstream, "/v1", &["GET"])),
    )
    .await;

    assert_validation_problem(status, &headers, &problem);
    let detail = problem["detail"].as_str().expect("detail").to_owned();
    assert!(
        detail.contains("immutable") && detail.contains("upstream_id"),
        "the detail names the immutable field: {detail}"
    );
    assert_eq!(
        problem["field"], "upstream_id",
        "the offending field is named"
    );
}

#[tokio::test]
async fn invalid_route_bodies_are_rejected_with_a_gateway_problem() {
    let router = router();
    let upstream_id = create_upstream(&router, "api.example.com").await;

    let unknown_field = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": "/v1" } },
        "id": Uuid::new_v4()
    });
    let empty_methods = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": [], "path": "/v1" } }
    });
    let method_outside_enum = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["TRACE"], "path": "/v1" } }
    });
    let relative_path = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": "v1" } }
    });
    let empty_path = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": "" } }
    });
    let neither_match_branch = json!({
        "upstream_id": upstream_id,
        "match": {}
    });
    let both_match_branches = json!({
        "upstream_id": upstream_id,
        "match": {
            "http": { "methods": ["GET"], "path": "/v1" },
            "grpc": { "service": "svc.V1", "method": "Get" }
        }
    });
    let invalid_tag = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": "/v1" } },
        "tags": ["Chat"]
    });
    let invalid_rate_limit = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": "/v1" } },
        "rate_limit": { "sustained": { "rate": 0 } }
    });

    for (label, body) in [
        ("unknown field", unknown_field),
        ("empty methods", empty_methods),
        ("method outside the enum", method_outside_enum),
        ("relative path", relative_path),
        ("empty path", empty_path),
        ("neither http nor grpc", neither_match_branch),
        ("both http and grpc", both_match_branches),
        ("invalid tag", invalid_tag),
        ("invalid rate limit", invalid_rate_limit),
    ] {
        let (status, headers, problem) = authed(router.clone(), "POST", ROUTES, Some(body)).await;
        assert_validation_problem(status, &headers, &problem);
        assert!(
            !problem["detail"].as_str().expect("detail").is_empty(),
            "{label} carries a diagnostic detail"
        );
    }
}

#[tokio::test]
async fn route_lists_support_the_odata_parameters() {
    let router = router();
    let first = create_upstream(&router, "api.example.com").await;
    let second = create_upstream(&router, "eu.example.com").await;

    for path in ["/v1/a", "/v1/b", "/v1/c", "/v1/d"] {
        let upstream = if path == "/v1/a" || path == "/v1/c" {
            &first
        } else {
            &second
        };
        let (status, _, _) = create_route(&router, route_body(upstream, path, &["GET"])).await;
        assert_eq!(status, StatusCode::CREATED, "fixture route for {path}");
    }

    // Disable one route so `$filter=enabled eq false` has something to find.
    let (status, _, listed) = authed(router.clone(), "GET", ROUTES, None).await;
    assert_eq!(status, StatusCode::OK);
    let items = listed.as_array().expect("bare array");
    assert_eq!(items.len(), 4);
    let id = items[0]["id"].as_str().expect("id").to_owned();
    let upstream_id = items[0]["upstream_id"]
        .as_str()
        .expect("upstream_id")
        .to_owned();
    let path = items[0]["match"]["http"]["path"]
        .as_str()
        .expect("path")
        .to_owned();
    let mut replacement = route_body(&upstream_id, &path, &["GET"]);
    replacement["enabled"] = json!(false);
    let (status, _, _) = authed(
        router.clone(),
        "PUT",
        &format!("{ROUTES}/{id}"),
        Some(replacement),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the route is disabled");

    // $top / $skip
    let (status, _, page) = authed(
        router.clone(),
        "GET",
        &format!("{ROUTES}?$top=2&$skip=1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page.as_array().expect("array").len(), 2);

    // $orderby=updated_at desc: stamps are non-increasing. Timestamps have
    // second resolution and the four fixtures are created within one second, so
    // the order of equal keys is the only part that is implementation-defined;
    // the property under test is that `$orderby` is honored at all.
    let (status, _, page) = authed(
        router.clone(),
        "GET",
        &format!("{ROUTES}?$orderby=updated_at%20desc"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = page.as_array().expect("array");
    assert_eq!(items.len(), 4);
    assert_sorted_by(items, "updated_at", false);

    // $orderby=created_at asc: non-decreasing, and the replacement keeps
    // `created_at`, so the disabled route still carries the earliest stamp.
    let (status, _, page) = authed(
        router.clone(),
        "GET",
        &format!("{ROUTES}?$orderby=created_at%20asc"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = page.as_array().expect("array");
    assert_eq!(items.len(), 4);
    assert_sorted_by(items, "created_at", true);
    assert_eq!(
        items[0]["updated_at"], items[0]["created_at"],
        "the replaced route keeps created_at while refreshing updated_at"
    );

    // $filter=upstream_id eq '<uuid>'
    let (status, _, page) = authed(
        router.clone(),
        "GET",
        &format!("{ROUTES}?$filter=upstream_id%20eq%20'{second}'"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = page.as_array().expect("array");
    assert_eq!(items.len(), 2);
    for item in items {
        assert_eq!(item["upstream_id"], second.as_str());
    }

    // $filter=enabled eq false
    let (status, _, page) = authed(
        router.clone(),
        "GET",
        &format!("{ROUTES}?$filter=enabled%20eq%20false"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = page.as_array().expect("array");
    assert_eq!(items.len(), 1, "only the disabled route matches");
    assert_eq!(items[0]["id"], id.as_str());
    assert!(!items[0]["enabled"].as_bool().expect("enabled"));

    // $select projects the item.
    let (status, _, page) = authed(
        router.clone(),
        "GET",
        &format!("{ROUTES}?$select=id,upstream_id"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for item in page.as_array().expect("array") {
        let keys: Vec<&str> = item
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys.len(), 2, "projected to id and upstream_id: {keys:?}");
    }

    // $top above the cap is rejected.
    let (status, headers, problem) =
        authed(router, "GET", &format!("{ROUTES}?$top=101"), None).await;
    assert_validation_problem(status, &headers, &problem);
}

#[tokio::test]
async fn unsupported_route_filter_is_a_400_problem_document() {
    let router = router();

    for filter in [
        "alias%20eq%20'api.example.com'",
        "tenant_id%20eq%20'00000000-0000-0000-0000-000000000001'",
        "upstream_id%20gt%20'00000000-0000-0000-0000-000000000001'",
    ] {
        let (status, headers, problem) = authed(
            router.clone(),
            "GET",
            &format!("{ROUTES}?$filter={filter}"),
            None,
        )
        .await;

        assert_validation_problem(status, &headers, &problem);
        assert!(
            problem["detail"]
                .as_str()
                .expect("detail")
                .contains("$filter"),
            "the rejected expression is named: {}",
            problem["detail"]
        );
    }
}

// ---------------------------------------------------------------------------
// Plugin CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plugin_crud_round_trips() {
    let router = router();

    // POST - create
    let (status, _, created) = create_plugin(
        &router.clone(),
        plugin_body("request_validator", "guard_plugin", SOURCE),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().expect("id").to_owned();
    assert_eq!(created["name"], "request_validator");
    assert_eq!(created["plugin_type"], "guard_plugin");
    assert_eq!(created["source_code"], SOURCE);
    assert!(created["last_used_at"].is_null(), "not used yet");
    assert!(
        created["gc_eligible_at"].is_null(),
        "still referenced or never used"
    );
    assert!(
        created["config_schema"].is_null(),
        "omitted optional fields are absent"
    );

    // GET - by uuid and by GTS identifier
    let (status, _, fetched) =
        authed(router.clone(), "GET", &format!("{PLUGINS}/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched, created);

    let gts_id = format!("gts.cf.core.oagw.guard_plugin.v1~{id}");
    let (status, _, by_gts) =
        authed(router.clone(), "GET", &format!("{PLUGINS}/{gts_id}"), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the GTS form resolves to the record"
    );
    assert_eq!(by_gts, created);

    // LIST
    let (status, _, listed) = authed(router.clone(), "GET", PLUGINS, None).await;
    assert_eq!(status, StatusCode::OK);
    let items = listed.as_array().expect("bare array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], created["id"]);

    // SOURCE - the raw Starlark, verbatim
    let (status, headers, source) = send_raw(
        router.clone(),
        "GET",
        &format!("{PLUGINS}/{id}/source"),
        None,
        Some(security_context(TENANT)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        content_type(&headers).as_deref(),
        Some("text/plain; charset=utf-8"),
        "the source is served as plain text"
    );
    assert_eq!(
        error_source(&headers).as_deref(),
        Some("gateway"),
        "the success response is stamped with the error source"
    );
    assert_eq!(source, SOURCE.as_bytes(), "the source is returned verbatim");

    // DELETE - then the plugin is no longer addressable. `PluginNotFound` is a
    // 503 in the DESIGN §3.3 error table (the control plane is the only
    // component able to resolve a plugin id); it is still never a 403, so
    // tenant isolation is preserved.
    let (status, _, _) = authed(router.clone(), "DELETE", &format!("{PLUGINS}/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _, problem) = authed(router, "GET", &format!("{PLUGINS}/{id}"), None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(problem["title"], "Plugin Not Found");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
    );
}

#[tokio::test]
async fn plugin_bodies_are_validated() {
    let router = router();

    // Duplicate name within the tenant.
    let (status, _, _) = create_plugin(
        &router.clone(),
        plugin_body("validator", "guard_plugin", SOURCE),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, headers, problem) = create_plugin(
        &router.clone(),
        plugin_body("validator", "guard_plugin", SOURCE),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_gateway_source(status, &headers, &problem);
    assert_eq!(problem["title"], "Alias Conflict");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1"
    );
    let detail = problem["detail"].as_str().expect("detail").to_owned();
    assert!(
        detail.contains("name"),
        "the detail names the field: {detail}"
    );
    assert_eq!(problem["field"], "name");

    // The same name is free for another tenant.
    let (status, _, _) = send(
        router.clone(),
        "POST",
        PLUGINS,
        Some(plugin_body("validator", "guard_plugin", SOURCE)),
        Some(security_context(OTHER_TENANT)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    for (label, body) in [
        (
            "blank source_code",
            plugin_body("blank", "guard_plugin", ""),
        ),
        (
            "whitespace source_code",
            plugin_body("blank", "guard_plugin", "   \n "),
        ),
        (
            "unknown plugin_type",
            plugin_body("odd", "middleware", SOURCE),
        ),
        ("empty plugin_type", plugin_body("odd", "", SOURCE)),
        ("empty name", plugin_body("", "guard_plugin", SOURCE)),
        (
            "missing name",
            json!({ "plugin_type": "guard_plugin", "source_code": SOURCE }),
        ),
        (
            "missing source_code",
            json!({ "name": "no-source", "plugin_type": "guard_plugin" }),
        ),
        (
            "unknown field",
            json!({
                "name": "extra",
                "plugin_type": "guard_plugin",
                "source_code": SOURCE,
                "id": Uuid::new_v4()
            }),
        ),
        (
            "config_schema is not an object",
            json!({
                "name": "schema",
                "plugin_type": "guard_plugin",
                "source_code": SOURCE,
                "config_schema": "string"
            }),
        ),
    ] {
        let (status, _, problem) = authed(router.clone(), "POST", PLUGINS, Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label} is a 400");
        assert_eq!(problem["title"], "Validation Error", "{label}");
    }
}

#[tokio::test]
async fn plugin_lists_support_the_odata_parameters() {
    let router = router();

    for (name, plugin_type) in [
        ("alpha", "guard_plugin"),
        ("beta", "transform_plugin"),
        ("gamma", "guard_plugin"),
    ] {
        let (status, _, _) =
            create_plugin(&router.clone(), plugin_body(name, plugin_type, SOURCE)).await;
        assert_eq!(status, StatusCode::CREATED, "fixture plugin {name}");
    }

    let (status, _, page) = authed(
        router.clone(),
        "GET",
        &format!("{PLUGINS}?$filter=plugin_type%20eq%20'guard_plugin'&$orderby=name%20asc"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = page.as_array().expect("bare array");
    assert_eq!(items.len(), 2, "only the guard plugins match");
    assert_eq!(items[0]["name"], "alpha");
    assert_eq!(items[1]["name"], "gamma");

    let (status, _, page) = authed(
        router.clone(),
        "GET",
        &format!("{PLUGINS}?$filter=name%20eq%20'beta'"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = page.as_array().expect("array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "beta");

    let (status, _, page) = authed(
        router.clone(),
        "GET",
        &format!("{PLUGINS}?$top=2&$skip=1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page.as_array().expect("array").len(), 2);

    let (status, _, problem) = authed(router, "GET", &format!("{PLUGINS}?$top=101"), None).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "$top above the cap is rejected"
    );
    assert_eq!(problem["title"], "Validation Error");
}

#[tokio::test]
async fn deleting_a_referenced_plugin_is_a_409_with_the_reference_list() {
    let router = router();

    // Referenced by one upstream of the calling tenant.
    let (_, _, plugin) = create_plugin(
        &router.clone(),
        plugin_body("upstream_guard", "guard_plugin", SOURCE),
    )
    .await;
    assert!(plugin["id"].as_str().is_some());
    let plugin_id = plugin["id"].as_str().expect("id").to_owned();
    let gts_id = format!("gts.cf.core.oagw.guard_plugin.v1~{plugin_id}");

    let (status, _, upstream) = authed(
        router.clone(),
        "POST",
        UPSTREAMS,
        Some(upstream_with_plugin_body("api.example.com", &plugin_id)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "the upstream binds the plugin");
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();

    let (status, headers, problem) = authed(
        router.clone(),
        "DELETE",
        &format!("{PLUGINS}/{plugin_id}"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_gateway_source(status, &headers, &problem);
    assert_eq!(problem["title"], "Plugin In Use");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
    );
    assert_eq!(
        problem["detail"],
        "Plugin is referenced by 1 upstream(s) and 0 route(s)"
    );
    assert_eq!(problem["plugin_id"], gts_id.as_str());
    assert_eq!(
        problem["referenced_by"]["upstreams"],
        json!([format!("gts.cf.core.oagw.upstream.v1~{upstream_id}")])
    );
    assert_eq!(problem["referenced_by"]["routes"], json!([]));

    // The reference counts the bare-UUID spelling too.
    let (status, _, upstream) = authed(
        router.clone(),
        "POST",
        UPSTREAMS,
        Some(upstream_with_plugin_body("eu.example.com", &gts_id)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let second_upstream = upstream["id"].as_str().expect("id").to_owned();

    let (status, _, problem) = authed(
        router.clone(),
        "DELETE",
        &format!("{PLUGINS}/{plugin_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        problem["detail"],
        "Plugin is referenced by 2 upstream(s) and 0 route(s)"
    );
    assert_eq!(
        problem["referenced_by"]["upstreams"],
        json!([
            format!("gts.cf.core.oagw.upstream.v1~{upstream_id}"),
            format!("gts.cf.core.oagw.upstream.v1~{second_upstream}")
        ])
    );

    // Deleting the referencing upstreams frees the plugin.
    for id in [upstream_id, second_upstream] {
        let (status, _, _) =
            authed(router.clone(), "DELETE", &format!("{UPSTREAMS}/{id}"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    let (status, _, _) = authed(router, "DELETE", &format!("{PLUGINS}/{plugin_id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn deleting_a_route_referenced_plugin_is_a_409() {
    let router = router();
    let upstream_id = create_upstream(&router, "api.example.com").await;

    let (_, _, plugin) =
        create_plugin(&router, plugin_body("route_guard", "guard_plugin", SOURCE)).await;
    let plugin_id = plugin["id"].as_str().expect("id").to_owned();

    let mut route = route_body(&upstream_id, "/v1", &["GET"]);
    route["plugins"] = json!({ "sharing": "private", "items": [plugin_id] });
    let (status, _, created) = authed(router.clone(), "POST", ROUTES, Some(route)).await;
    assert_eq!(status, StatusCode::CREATED);
    let route_id = created["id"].as_str().expect("id").to_owned();

    let (status, _, problem) = authed(
        router.clone(),
        "DELETE",
        &format!("{PLUGINS}/{plugin_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        problem["detail"],
        "Plugin is referenced by 0 upstream(s) and 1 route(s)"
    );
    assert_eq!(problem["referenced_by"]["upstreams"], json!([]));
    assert_eq!(
        problem["referenced_by"]["routes"],
        json!([format!("gts.cf.core.oagw.route.v1~{route_id}")])
    );

    // Deleting the referencing route frees the plugin.
    let (status, _, _) = authed(
        router.clone(),
        "DELETE",
        &format!("{ROUTES}/{route_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _, _) = authed(router, "DELETE", &format!("{PLUGINS}/{plugin_id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn deleting_a_plugin_referenced_by_an_upstream_and_a_route_reports_both() {
    let router = router();
    let upstream_id = create_upstream(&router, "api.example.com").await;

    let (_, _, plugin) =
        create_plugin(&router, plugin_body("shared", "transform_plugin", SOURCE)).await;
    let plugin_id = plugin["id"].as_str().expect("id").to_owned();

    let (status, _, upstream) = authed(
        router.clone(),
        "POST",
        UPSTREAMS,
        Some(upstream_with_plugin_body("eu.example.com", &plugin_id)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let referencing_upstream = upstream["id"].as_str().expect("id").to_owned();

    let mut route = route_body(&upstream_id, "/v1", &["GET"]);
    route["plugins"] = json!({ "sharing": "private", "items": [plugin_id] });
    let (status, _, created) = authed(router.clone(), "POST", ROUTES, Some(route)).await;
    assert_eq!(status, StatusCode::CREATED);
    let route_id = created["id"].as_str().expect("id").to_owned();

    let (status, _, problem) =
        authed(router, "DELETE", &format!("{PLUGINS}/{plugin_id}"), None).await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        problem["detail"],
        "Plugin is referenced by 1 upstream(s) and 1 route(s)"
    );
    assert_eq!(
        problem["referenced_by"]["upstreams"],
        json!([format!(
            "gts.cf.core.oagw.upstream.v1~{referencing_upstream}"
        )])
    );
    assert_eq!(
        problem["referenced_by"]["routes"],
        json!([format!("gts.cf.core.oagw.route.v1~{route_id}")])
    );
}

#[tokio::test]
async fn plugins_have_no_put_endpoint() {
    let router = router();

    let (_, _, created) =
        create_plugin(&router, plugin_body("immutable", "guard_plugin", SOURCE)).await;
    assert!(created["id"].as_str().is_some());
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, headers, problem) = send(
        router,
        "PUT",
        &format!("{PLUGINS}/{id}"),
        Some(plugin_body("renamed", "guard_plugin", SOURCE)),
        Some(security_context(TENANT)),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "plugins are immutable: no PUT is registered"
    );

    // The 405 is the gateway error surface, not axum's plain-text default: a
    // client that mistypes the method reads a problem document, and the type
    // names the mistake (DESIGN §2.1 / ADR-0007).
    assert_gateway_source(status, &headers, &problem);
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.method.not_allowed.v1"
    );
}

// ---------------------------------------------------------------------------
// Body size
// ---------------------------------------------------------------------------

/// A body one MiB above the documented hard limit.
///
/// The body is streamed so the whole payload is never built in one allocation:
/// every frame repeats the same one-MiB chunk (`Bytes::clone` only bumps a
/// refcount), while the length is still `MAX_BODY_BYTES + 1` bytes — enough for
/// the limit check to trip, at the cost of a single chunk of memory.
fn oversized_body() -> Body {
    const CHUNK_BYTES: usize = 1024 * 1024;
    let chunk = Bytes::from(vec![b'x'; CHUNK_BYTES]);
    let frames = MAX_BODY_BYTES / CHUNK_BYTES + 1;

    use futures_util::StreamExt;

    Body::from_stream(
        futures_util::stream::repeat_with(move || Ok::<_, std::convert::Infallible>(chunk.clone()))
            .take(frames),
    )
}

/// Assert a response is the 413 problem document for an over-limit body.
fn assert_payload_too_large(
    label: &str,
    status: StatusCode,
    headers: &axum::http::HeaderMap,
    problem: &Value,
) {
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{label}: {problem}");
    assert_gateway_source(status, headers, problem);
    assert_eq!(
        problem["type"], "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
        "{label}"
    );
    assert!(
        problem["trace_id"].as_str().is_some(),
        "the 413 carries a trace_id: {problem}"
    );
}

/// The 413 is reachable over HTTP on every body-accepting operation.
///
/// Axum's own 2 MB default would reject the body before the handler with a
/// plain-text response, so the management router raises the limit to
/// [`MAX_BODY_BYTES`] and the handler enforces it while buffering: the client
/// reads a problem document, whatever the operation (DESIGN §2.2
/// `cpt-cf-oagw-constraint-body-limit`, ADR-0007).
#[tokio::test]
async fn an_oversized_body_is_a_413_problem_document() {
    let router = router();
    let upstream_id = create_upstream(&router, "api.example.com").await;
    let (_, _, route) = authed(
        router.clone(),
        "POST",
        ROUTES,
        Some(route_body(&upstream_id, "/v1", &["GET"])),
    )
    .await;
    let route_id = route["id"].as_str().expect("id").to_owned();

    for (method, uri) in [
        ("POST", UPSTREAMS),
        ("PUT", &format!("{UPSTREAMS}/{upstream_id}")),
        ("POST", ROUTES),
        ("PUT", &format!("{ROUTES}/{route_id}")),
        ("POST", PLUGINS),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        let (status, headers, problem) = send_body_json(
            router.clone(),
            request,
            oversized_body(),
            Some(security_context(TENANT)),
        )
        .await;

        assert_payload_too_large(&format!("{method} {uri}"), status, &headers, &problem);
    }
}

// ---------------------------------------------------------------------------
// Tenant scoping and authentication
// ---------------------------------------------------------------------------

#[tokio::test]
async fn routes_and_plugins_are_scoped_to_the_calling_tenant() {
    let router = router();
    let upstream_id = create_upstream(&router, "api.example.com").await;

    let (_, _, route) = authed(
        router.clone(),
        "POST",
        ROUTES,
        Some(route_body(&upstream_id, "/v1", &["GET"])),
    )
    .await;
    let route_id = route["id"].as_str().expect("id").to_owned();

    let (_, _, plugin) = create_plugin(
        &router.clone(),
        plugin_body("tenant_scoped", "guard_plugin", SOURCE),
    )
    .await;
    let plugin_id = plugin["id"].as_str().expect("id").to_owned();

    // The other tenant sees neither resource, and an empty list for both.
    // Route lookups are 404; plugin lookups are the documented 503
    // `PluginNotFound` (DESIGN §3.3). Both are existence-neutral: neither
    // status is a 403, and neither leaks that the id exists elsewhere.
    for (path, id, missing, problem_type) in [
        (
            ROUTES,
            &route_id,
            StatusCode::NOT_FOUND,
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
        ),
        (
            PLUGINS,
            &plugin_id,
            StatusCode::SERVICE_UNAVAILABLE,
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1",
        ),
    ] {
        let (status, _, problem) = send(
            router.clone(),
            "GET",
            &format!("{path}/{id}"),
            None,
            Some(security_context(OTHER_TENANT)),
        )
        .await;
        assert_eq!(status, missing, "{path} is tenant scoped");
        assert_ne!(status, StatusCode::FORBIDDEN, "existence is not disclosed");
        assert_eq!(problem["type"], problem_type, "{path}");

        let (status, _, listed) = send(
            router.clone(),
            "GET",
            path,
            None,
            Some(security_context(OTHER_TENANT)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(listed.as_array().expect("bare array").is_empty(), "{path}");

        let (status, _, _) = send(
            router.clone(),
            "DELETE",
            &format!("{path}/{id}"),
            None,
            Some(security_context(OTHER_TENANT)),
        )
        .await;
        assert_eq!(status, missing, "{path} delete is tenant scoped");
    }

    // The owning tenant still sees both, and can delete the plugin.
    let (status, _, listed) = authed(router.clone(), "GET", ROUTES, None).await;
    assert_eq!(listed.as_array().expect("array").len(), 1);
    assert_eq!(status, StatusCode::OK);
    let (status, _, listed) = authed(router.clone(), "GET", PLUGINS, None).await;
    assert_eq!(listed.as_array().expect("array").len(), 1);
    assert_eq!(status, StatusCode::OK);

    let (status, _, _) = authed(
        router.clone(),
        "DELETE",
        &format!("{PLUGINS}/{plugin_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn route_and_plugin_requests_fail_closed_without_a_context() {
    let router = router();
    let upstream_id = create_upstream(&router, "api.example.com").await;

    for (method, path, body) in [
        (
            "POST",
            ROUTES,
            Some(route_body(&upstream_id, "/v1", &["GET"])),
        ),
        ("GET", ROUTES, None),
        ("GET", &format!("{ROUTES}/{}", Uuid::new_v4()), None),
        (
            "POST",
            PLUGINS,
            Some(plugin_body("anon", "guard_plugin", SOURCE)),
        ),
        ("GET", PLUGINS, None),
        ("DELETE", &format!("{PLUGINS}/{}", Uuid::new_v4()), None),
        ("GET", &format!("{PLUGINS}/{}/source", Uuid::new_v4()), None),
    ] {
        let (status, headers, problem) = unauthenticated(router.clone(), method, path, body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
        assert_eq!(
            content_type(&headers).as_deref(),
            Some("application/problem+json")
        );
        assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
        assert_eq!(problem["detail"], "authentication required");
    }

    // Nothing was written by the rejected requests.
    let (status, _, listed) = authed(router, "GET", ROUTES, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(listed.as_array().expect("array").is_empty());
}
