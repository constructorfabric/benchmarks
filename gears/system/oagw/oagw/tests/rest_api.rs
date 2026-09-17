//! REST-level tests for the OAGW management API (`Router` + `oneshot`).
//!
//! These exercise the full transport stack of slice S1 — route registration,
//! handler extraction, DTO serialization and the RFC 9457 problem+json error
//! surface — without a database: the control plane is backed by the in-memory
//! store, exactly as the e2e configuration runs it.
//!
//! Every request is authenticated through the `SecurityContext` extension the
//! host gateway's auth middleware inserts: tenant resolution fails closed, so an
//! unauthenticated request is a 401 rather than a request against a default
//! tenant.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use oagw::OagwConfig;
use oagw::api::rest::routes::register_routes;
use oagw::domain::services::control_plane::ControlPlaneService;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;

/// Upstream base path (gear-relative, without `/api`).
const UPSTREAMS: &str = "/oagw/v1/upstreams";

/// The tenant every request in this file is authenticated for.
const TENANT: Uuid = Uuid::from_u128(0x6f61_6777_0000_0000_0000_0000_0000_0001);

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
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    if let Some(context) = context {
        builder = builder.extension(context);
    }
    let request = builder
        .body(Body::from(
            body.map(|value| value.to_string()).unwrap_or_default(),
        ))
        .expect("request builds");

    respond(router, request).await
}

/// Send a raw request and decode the response.
async fn respond(
    router: Router,
    request: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let response = router.oneshot(request).await.expect("router responds");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
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

/// Assert a response is an OAGW gateway problem document.
fn assert_gateway_problem(
    status: StatusCode,
    headers: &axum::http::HeaderMap,
    problem: &Value,
    title: &str,
) {
    assert_eq!(status, StatusCode::BAD_REQUEST);
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
    assert_eq!(problem["title"], title);
    assert_eq!(problem["status"], status.as_u16());
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn create_then_get_round_trips_the_upstream() {
    let router = router();

    let (status, headers, created) = authed(
        router.clone(),
        "POST",
        UPSTREAMS,
        Some(upstream_body("api.openai.com")),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
    assert_eq!(created["alias"], "api.openai.com");
    assert_eq!(created["enabled"], true);
    assert_eq!(created["server"]["endpoints"][0]["host"], "api.openai.com");
    assert_eq!(
        created["protocol"],
        "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    );

    let id = created["id"].as_str().expect("id is a string").to_owned();
    let (status, headers, fetched) =
        authed(router.clone(), "GET", &format!("{UPSTREAMS}/{id}"), None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
    assert_eq!(fetched, created);

    // The anonymous GTS spelling of the identifier addresses the same upstream.
    let gts_id = format!("gts.cf.core.oagw.upstream.v1~{id}");
    let (status, _, by_gts_id) =
        authed(router, "GET", &format!("{UPSTREAMS}/{gts_id}"), None).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "the GTS form resolves to the record"
    );
    assert_eq!(by_gts_id, created, "the body still carries the bare UUID");
}

#[tokio::test]
async fn create_tolerates_a_missing_content_type() {
    let router = router();
    let request = Request::builder()
        .method("POST")
        .uri(UPSTREAMS)
        .extension(security_context(TENANT))
        .body(Body::from(upstream_body("api.openai.com").to_string()))
        .expect("request builds");

    let (status, headers, _) = respond(router, request).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
}

#[tokio::test]
async fn create_rejects_a_non_json_content_type_with_a_problem_document() {
    let router = router();
    let request = Request::builder()
        .method("POST")
        .uri(UPSTREAMS)
        .header("content-type", "text/plain")
        .extension(security_context(TENANT))
        .body(Body::from(upstream_body("api.openai.com").to_string()))
        .expect("request builds");

    let (status, headers, _) = respond(router, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        content_type(&headers).as_deref(),
        Some("application/problem+json")
    );
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
}

#[tokio::test]
async fn duplicate_alias_is_a_409_problem_document() {
    let router = router();

    let (_, _, first) = authed(
        router.clone(),
        "POST",
        UPSTREAMS,
        Some(upstream_body("api.openai.com")),
    )
    .await;
    assert!(first["id"].is_string());

    let (status, headers, problem) = authed(
        router,
        "POST",
        UPSTREAMS,
        Some(upstream_body("api.openai.com")),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        content_type(&headers).as_deref(),
        Some("application/problem+json")
    );
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
    assert_eq!(problem["status"], 409);
    assert_eq!(problem["title"], "Alias Conflict");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1"
    );
}

#[tokio::test]
async fn unknown_id_is_a_404_problem_document() {
    let router = router();
    let id = Uuid::new_v4();

    let (status, headers, problem) =
        authed(router, "GET", &format!("{UPSTREAMS}/{id}"), None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
    assert_eq!(problem["title"], "Upstream Not Found");
    assert_eq!(problem["status"], 404);
}

#[tokio::test]
async fn malformed_id_is_a_400_problem_document() {
    let router = router();

    for id in ["not-a-uuid", "gts.cf.core.oagw.upstream.v1~not-a-uuid"] {
        let (status, headers, problem) =
            authed(router.clone(), "GET", &format!("{UPSTREAMS}/{id}"), None).await;

        assert_gateway_problem(status, &headers, &problem, "Validation Error");
        assert!(
            problem["detail"].as_str().expect("detail").contains(id),
            "the malformed id is named in the detail"
        );
    }
}

#[tokio::test]
async fn malformed_query_parameter_is_a_400_problem_document() {
    let router = router();

    for query in ["?$top=abc", "?$skip=-1&$top=1", "?$top=101"] {
        let (status, headers, problem) =
            authed(router.clone(), "GET", &format!("{UPSTREAMS}{query}"), None).await;

        assert_gateway_problem(status, &headers, &problem, "Validation Error");
    }
}

#[tokio::test]
async fn unknown_route_is_a_404_problem_document() {
    let router = router();

    let (status, _, body) = authed(router, "GET", "/oagw/v1/unknown", None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body.is_null(),
        "unregistered paths are not OAGW problem documents"
    );
}

#[tokio::test]
async fn invalid_body_is_a_400_problem_document() {
    let router = router();

    let malformed = Request::builder()
        .method("POST")
        .uri(UPSTREAMS)
        .header("content-type", "application/json")
        .extension(security_context(TENANT))
        .body(Body::from("{"))
        .expect("request builds");
    let (status, headers, _) = respond(router.clone(), malformed).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        content_type(&headers).as_deref(),
        Some("application/problem+json")
    );

    let (status, _, problem) = authed(
        router,
        "POST",
        UPSTREAMS,
        Some(json!({ "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1" })),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["title"], "Validation Error");
}

#[tokio::test]
async fn replace_is_a_full_replacement_with_an_immutable_alias() {
    let router = router();

    let (_, _, created) = authed(
        router.clone(),
        "POST",
        UPSTREAMS,
        Some(upstream_body("api.openai.com")),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    let mut replacement = upstream_body("api.openai.com");
    replacement["tags"] = json!(["maintenance"]);
    let (status, _, replaced) = authed(
        router.clone(),
        "PUT",
        &format!("{UPSTREAMS}/{id}"),
        Some(replacement),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(replaced["id"], created["id"]);
    assert_eq!(replaced["alias"], created["alias"]);
    assert_eq!(replaced["tags"], json!(["maintenance"]));

    // A second no-op replacement is idempotent.
    let (status, _, again) = authed(
        router,
        "PUT",
        &format!("{UPSTREAMS}/{id}"),
        Some(upstream_body("api.openai.com")),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["alias"], "api.openai.com");
}

#[tokio::test]
async fn replace_cannot_rename_the_alias() {
    let router = router();

    let (_, _, created) = authed(
        router.clone(),
        "POST",
        UPSTREAMS,
        Some(upstream_body("api.openai.com")),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    let mut renamed = upstream_body("eu.openai.com");
    renamed["alias"] = json!("eu.openai.com");
    let (status, _, problem) =
        authed(router, "PUT", &format!("{UPSTREAMS}/{id}"), Some(renamed)).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["title"], "Validation Error");
}

#[tokio::test]
async fn delete_returns_204_and_hides_the_resource() {
    let router = router();

    let (_, _, created) = authed(
        router.clone(),
        "POST",
        UPSTREAMS,
        Some(upstream_body("api.openai.com")),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, headers, body) =
        authed(router.clone(), "DELETE", &format!("{UPSTREAMS}/{id}"), None).await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_null());
    assert_eq!(
        error_source(&headers).as_deref(),
        Some("gateway"),
        "ADR-0007: success responses carry the error-source header too"
    );

    let (status, _, _) = authed(router, "GET", &format!("{UPSTREAMS}/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn success_responses_are_stamped_gateway() {
    let router = router();

    let (_, headers, created) = authed(
        router.clone(),
        "POST",
        UPSTREAMS,
        Some(upstream_body("api.openai.com")),
    )
    .await;
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"), "201");
    let id = created["id"].as_str().expect("id").to_owned();

    let (_, headers, _) = authed(router.clone(), "GET", &format!("{UPSTREAMS}/{id}"), None).await;
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"), "200");

    let (_, headers, page) = authed(router.clone(), "GET", UPSTREAMS, None).await;
    assert_eq!(
        error_source(&headers).as_deref(),
        Some("gateway"),
        "200 list"
    );
    assert_eq!(page.as_array().expect("bare array").len(), 1);

    let (_, headers, _) = authed(router, "DELETE", &format!("{UPSTREAMS}/{id}"), None).await;
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"), "204");
}

#[tokio::test]
async fn list_supports_top_skip_orderby_and_select() {
    let router = router();

    for host in ["a.example.com", "b.example.com", "c.example.com"] {
        let _ = authed(router.clone(), "POST", UPSTREAMS, Some(upstream_body(host))).await;
    }

    let (status, _, page) = authed(
        router.clone(),
        "GET",
        &format!("{UPSTREAMS}?$orderby=alias%20desc&$top=2&$select=alias"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let items = page.as_array().expect("bare array");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["alias"], "c.example.com");
    assert_eq!(items[1]["alias"], "b.example.com");
    assert!(
        items[0].get("server").is_none(),
        "$select projects the item"
    );

    let (status, _, _) = authed(
        router.clone(),
        "GET",
        &format!("{UPSTREAMS}?$top=101"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "$top above the cap is rejected"
    );

    let (status, _, page) = authed(router, "GET", &format!("{UPSTREAMS}?$skip=2"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page.as_array().expect("array").len(), 1);
}

#[tokio::test]
async fn list_filters_on_alias() {
    let router = router();

    for host in ["api.openai.com", "api.anthropic.com"] {
        let _ = authed(router.clone(), "POST", UPSTREAMS, Some(upstream_body(host))).await;
    }

    let (status, _, page) = authed(
        router,
        "GET",
        &format!("{UPSTREAMS}?$filter=alias%20eq%20'api.openai.com'"),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let items = page.as_array().expect("bare array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["alias"], "api.openai.com");
}

#[tokio::test]
async fn unsupported_filter_is_a_400_problem_document() {
    let router = router();

    for filter in [
        "alias%20gt%20'api.openai.com'",
        "startswith(alias,'api')",
        "tenant_id%20eq%20'00000000-0000-0000-0000-000000000001'",
    ] {
        let (status, headers, problem) = authed(
            router.clone(),
            "GET",
            &format!("{UPSTREAMS}?$filter={filter}"),
            None,
        )
        .await;

        assert_gateway_problem(status, &headers, &problem, "Validation Error");
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

#[tokio::test]
async fn tenants_are_scoped_by_the_authenticated_caller() {
    let router = router();
    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();

    let (_, _, created) = send(
        router.clone(),
        "POST",
        UPSTREAMS,
        Some(upstream_body("api.openai.com")),
        Some(security_context(tenant_a)),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    // Tenant B cannot see (or delete) tenant A's upstream.
    let (status, _, _) = send(
        router.clone(),
        "GET",
        &format!("{UPSTREAMS}/{id}"),
        None,
        Some(security_context(tenant_b)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Tenant A still can.
    let (status, _, _) = send(
        router.clone(),
        "GET",
        &format!("{UPSTREAMS}/{id}"),
        None,
        Some(security_context(tenant_a)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The same alias is free again for tenant B.
    let (status, _, _) = send(
        router,
        "POST",
        UPSTREAMS,
        Some(upstream_body("api.openai.com")),
        Some(security_context(tenant_b)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn unauthenticated_requests_are_rejected_with_401() {
    let router = router();

    // Read, create and delete all fail closed: without a `SecurityContext` there
    // is no tenant to scope the request to, so nothing is served or mutated.
    let (status, headers, problem) = unauthenticated(router.clone(), "GET", UPSTREAMS, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        content_type(&headers).as_deref(),
        Some("application/problem+json")
    );
    assert_eq!(error_source(&headers).as_deref(), Some("gateway"));
    assert_eq!(problem["title"], "Authentication Failed");
    assert_eq!(problem["detail"], "authentication required");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1"
    );

    let (status, _, problem) = unauthenticated(
        router.clone(),
        "POST",
        UPSTREAMS,
        Some(upstream_body("api.openai.com")),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(problem["title"], "Authentication Failed");

    let (status, _, _) = unauthenticated(
        router.clone(),
        "GET",
        &format!("{UPSTREAMS}/{}", Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _, _) = unauthenticated(
        router,
        "DELETE",
        &format!("{UPSTREAMS}/{}", Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn anonymous_contexts_are_rejected_with_401() {
    let router = router();

    // A context without a tenant (`SecurityContext::anonymous()`) carries no
    // tenant id, so it is not an authenticated caller either.
    let (status, _, problem) = send(
        router,
        "GET",
        UPSTREAMS,
        None,
        Some(SecurityContext::anonymous()),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(problem["detail"], "authentication required");
}
