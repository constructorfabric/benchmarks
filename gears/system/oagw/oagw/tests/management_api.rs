#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for the management API (entry 2.2).
//!
//! In-crate integration tests only (DECOMPOSITION assumption 5): no e2e suite
//! is added under `testing/e2e/gears/oagw/`. The tests drive the mounted router
//! the way the host api-gateway does — the Bearer token is already resolved, so
//! the request carries the resolved
//! [`SecurityContext`](toolkit_security::SecurityContext) — and assert the
//! status codes, the `application/problem+json` bodies and the tenant scoping
//! of `cpt-cf-oagw-dod-upstream-endpoints`, `cpt-cf-oagw-dod-route-endpoints`,
//! `cpt-cf-oagw-dod-enable-disable` and `cpt-cf-oagw-dod-error-contract`.

// @cpt-begin:cpt-cf-oagw-dod-in-crate-test-coverage:p2:inst-full
use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::api::rest::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER};
use oagw::api::rest::routes::{MOUNT_ROOT, register_routes};
use oagw::domain::model::PROTOCOL_HTTP;
use oagw::domain::sharing::{FlatHierarchy, StaticHierarchy, TenantHierarchy};
use oagw::infra::storage::OagwStore;

const TENANT: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000010");
const ANCESTOR: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000001");
const OTHER: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000020");

const UPSTREAM_ID_PREFIX: &str = "gts.cf.core.oagw.upstream.v1~";
const ROUTE_ID_PREFIX: &str = "gts.cf.core.oagw.route.v1~";

/// Host OpenAPI registry double recording the registered component names.
#[derive(Default)]
struct RecordingRegistry {
    schemas: std::sync::Mutex<Vec<String>>,
    operations: std::sync::Mutex<Vec<String>>,
}

impl RecordingRegistry {
    fn schema_names(&self) -> Vec<String> {
        self.schemas.lock().expect("schemas lock").clone()
    }

    fn operation_paths(&self) -> Vec<String> {
        self.operations.lock().expect("operations lock").clone()
    }
}

impl toolkit::api::OpenApiRegistry for RecordingRegistry {
    fn register_operation(&self, spec: &toolkit::api::operation_builder::OperationSpec) {
        self.operations
            .lock()
            .expect("operations lock")
            .push(format!("{} {}", spec.method, spec.path));
    }

    fn ensure_schema_raw(
        &self,
        name: &str,
        schemas: Vec<(String, utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>)>,
    ) -> String {
        self.schemas
            .lock()
            .expect("schemas lock")
            .extend(schemas.into_iter().map(|(name, _)| name));
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// The mounted router over a fresh store, with the host registry.
///
/// The hierarchy is the single-tenant fallback: these tests exercise the
/// transport, the mapping and the tenant scoping, none of which needs an
/// ancestor chain. The ancestor-gate tests use [`mounted_with`].
fn mounted() -> (Router, Arc<OagwStore>, RecordingRegistry) {
    mounted_with(Arc::new(FlatHierarchy))
}

/// The mounted router over a fresh store, with the given hierarchy source.
fn mounted_with(
    hierarchy: Arc<dyn TenantHierarchy>,
) -> (Router, Arc<OagwStore>, RecordingRegistry) {
    let store = Arc::new(OagwStore::new());
    let registry = RecordingRegistry::default();
    let router = register_routes(
        Router::new(),
        &registry,
        Arc::clone(&store),
        Arc::clone(&hierarchy),
    );
    (router, store, registry)
}

/// The mounted router whose ancestor chains come from `chains`, keyed by
/// calling tenant.
fn mounted_over_chains(chains: BTreeMap<Uuid, Vec<Uuid>>) -> (Router, Arc<OagwStore>) {
    let (router, store, _registry) = mounted_with(Arc::new(StaticHierarchy::new(chains)));
    (router, store)
}

/// A security context the host api-gateway would inject for `tenant`.
fn context(tenant: Uuid, scopes: &[&str]) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_type("user")
        .subject_tenant_id(tenant)
        .token_scopes(scopes.iter().map(|scope| (*scope).to_owned()).collect())
        .build()
        .expect("context builds")
}

/// Serve `router` with the request the builder describes.
async fn serve(router: Router, request: Request<Body>) -> axum::response::Response {
    router.oneshot(request).await.expect("request serves")
}

/// A `GET` request with the security context extension attached.
async fn get(router: Router, tenant: Uuid, scopes: &[&str], uri: &str) -> axum::response::Response {
    serve(
        router,
        Request::builder()
            .uri(uri)
            .extension(context(tenant, scopes))
            .body(Body::empty())
            .expect("request builds"),
    )
    .await
}

/// A request with a JSON body and the security context extension attached.
async fn send(
    router: Router,
    method: &str,
    uri: &str,
    tenant: Uuid,
    scopes: &[&str],
    body: Value,
) -> axum::response::Response {
    let method = axum::http::Method::from_bytes(method.as_bytes()).expect("method");
    serve(
        router,
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .extension(context(tenant, scopes))
            .body(Body::from(body.to_string()))
            .expect("request builds"),
    )
    .await
}

/// Error-source header value of a response, if it carries one.
fn error_source(response: &axum::response::Response) -> Option<String> {
    response
        .headers()
        .get(ERROR_SOURCE_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Deserialize the body of a response into a JSON value.
async fn json_body(response: axum::response::Response) -> Value {
    let bytes = Body::new(response)
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("body is a JSON document")
}

/// Assert the canonical problem contract of a failure response.
///
/// `cpt-cf-oagw-dod-error-contract`: every management failure is an
/// `application/problem+json` body carrying the GTS `type` identifier, the
/// standard fields and `X-OAGW-Error-Source: gateway`.
async fn assert_problem(
    response: axum::response::Response,
    status: u16,
    type_suffix: &str,
    instance: &str,
) -> Value {
    assert_eq!(response.status(), StatusCode::from_u16(status).expect("status"));
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/problem+json"),
        "{status} carries the problem content type"
    );
    assert_eq!(
        error_source(&response).as_deref(),
        Some(ERROR_SOURCE_GATEWAY),
        "{status} is classified as gateway-originated"
    );

    let document = json_body(response).await;
    assert_eq!(
        document["type"],
        json!(format!("gts://gts.cf.core.errors.err.v1~cf.oagw.{type_suffix}")),
        "{document}"
    );
    assert_eq!(document["status"], json!(status), "{document}");
    assert!(document["title"].is_string(), "{document}");
    assert!(document["detail"].is_string(), "{document}");
    assert_eq!(document["instance"], json!(instance), "{document}");
    document
}

/// A minimal valid upstream body, with the alias derived from the endpoint.
fn upstream_body(host: &str) -> Value {
    json!({
        "server": { "endpoints": [ { "scheme": "https", "host": host } ] },
        "protocol": PROTOCOL_HTTP
    })
}

/// A minimal valid route body bound to `upstream_id`.
fn route_body(upstream_id: Uuid, path: &str) -> Value {
    json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": path } }
    })
}

/// Every scope a management caller may need.
const ALL: &[&str] = &["*"];

/// Create an upstream through the API and return the stored representation.
async fn create_upstream(router: Router, tenant: Uuid, host: &str) -> Value {
    let response = send(
        router,
        "POST",
        "/oagw/v1/upstreams",
        tenant,
        ALL,
        upstream_body(host),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    json_body(response).await
}

// ---------- upstream endpoints ----------

#[tokio::test]
async fn create_upstream_returns_201_with_the_stored_representation() {
    let (router, _store, _registry) = mounted();

    let response = send(
        router,
        "POST",
        "/oagw/v1/upstreams",
        TENANT,
        ALL,
        upstream_body("api.vendor.com"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        error_source(&response).as_deref(),
        Some(ERROR_SOURCE_GATEWAY),
        "the mounted layer classifies every gear response"
    );

    let stored = json_body(response).await;
    assert_eq!(stored["alias"], json!("api.vendor.com"), "{stored}");
    assert_eq!(stored["enabled"], json!(true), "recorded default");
    assert_eq!(stored["tenant_id"], json!(TENANT));
    assert!(stored["id"].is_string());
    assert!(stored["created_at"].is_string());
    assert!(stored["auth"].is_null(), "absent optional stays absent");
}

#[tokio::test]
async fn a_projection_never_carries_credential_material() {
    let (router, _store, _registry) = mounted();

    let mut body = upstream_body("api.vendor.com");
    body["auth"] = json!({
        "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1",
        "config": { "client_secret": "cred://platform/vendor-oauth2/secret" }
    });
    let response = send(
        router.clone(),
        "POST",
        "/oagw/v1/upstreams",
        TENANT,
        ALL,
        body,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let stored = json_body(response).await;
    assert_eq!(
        stored["auth"]["config"]["client_secret"],
        json!("cred://platform/vendor-oauth2/secret"),
        "the reference is echoed, never dereferenced"
    );

    // A projection that selects the auth block still carries the reference and
    // nothing resolved behind it.
    let response = get(
        router,
        TENANT,
        ALL,
        "/oagw/v1/upstreams?$select=alias,auth",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = json_body(response).await;
    assert_eq!(page.as_array().map(Vec::len), Some(1), "{page}");
    assert_eq!(
        page[0]["auth"]["config"]["client_secret"],
        json!("cred://platform/vendor-oauth2/secret")
    );
    assert_eq!(page[0]["alias"], json!("api.vendor.com"), "{page}");
    assert!(page[0].get("server").is_none(), "unselected: {page}");
}

#[tokio::test]
async fn create_upstream_rejects_an_unknown_property_with_400() {
    let (router, _store, _registry) = mounted();

    let mut body = upstream_body("api.vendor.com");
    body["unexpected"] = json!("member");
    let response = send(router, "POST", "/oagw/v1/upstreams", TENANT, ALL, body).await;
    let document = assert_problem(response, 400, "validation.error.v1", "/oagw/v1/upstreams").await;
    assert!(
        document["detail"]
            .as_str()
            .expect("detail")
            .contains("unexpected"),
        "the detail names the offending property: {document}"
    );
}

#[tokio::test]
async fn create_upstream_rejects_an_invalid_body_with_400() {
    let (router, _store, _registry) = mounted();

    // `alias` is derived from the endpoint; an explicit different one is a 400.
    let mut body = upstream_body("api.vendor.com");
    body["alias"] = json!("other.example.com");
    let response = send(router, "POST", "/oagw/v1/upstreams", TENANT, ALL, body).await;
    assert_problem(response, 400, "validation.error.v1", "/oagw/v1/upstreams").await;
}

#[tokio::test]
async fn a_body_with_a_wrong_content_type_is_a_problem_document() {
    let (router, _store, _registry) = mounted();

    let response = serve(
        router,
        Request::builder()
            .method(axum::http::Method::POST)
            .uri("/oagw/v1/upstreams")
            .header(header::CONTENT_TYPE, "text/plain")
            .extension(context(TENANT, ALL))
            .body(Body::from("{}"))
            .expect("request builds"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/problem+json")
    );
}

#[tokio::test]
async fn a_request_without_a_resolvable_tenant_is_rejected_before_any_store_access() {
    let (router, store, _registry) = mounted();

    // No security context at all.
    let response = serve(
        router.clone(),
        Request::builder()
            .uri("/oagw/v1/upstreams")
            .body(Body::empty())
            .expect("request builds"),
    )
    .await;
    assert_problem(
        response,
        401,
        "auth.failed.v1",
        "/oagw/v1/upstreams",
    )
    .await;

    // The anonymous context the host injects for an unauthenticated call.
    let response = serve(
        router.clone(),
        Request::builder()
            .uri("/oagw/v1/upstreams")
            .extension(SecurityContext::anonymous())
            .body(Body::empty())
            .expect("request builds"),
    )
    .await;
    assert_problem(
        response,
        401,
        "auth.failed.v1",
        "/oagw/v1/upstreams",
    )
    .await;

    assert_eq!(store.list_upstreams(TENANT).len(), 0, "no record written");
}

#[tokio::test]
async fn create_upstream_requires_the_create_permission() {
    let (router, store, _registry) = mounted();

    let response = send(
        router,
        "POST",
        "/oagw/v1/upstreams",
        TENANT,
        &["gts.cf.core.oagw.upstream.v1~:read"],
        upstream_body("api.vendor.com"),
    )
    .await;
    let document =
        assert_problem(response, 403, "management.forbidden.v1", "/oagw/v1/upstreams").await;
    assert!(
        document["detail"]
            .as_str()
            .expect("detail")
            .contains(":create"),
        "the detail names the missing permission: {document}"
    );
    assert_eq!(store.list_upstreams(TENANT).len(), 0, "no record written");
}

#[tokio::test]
async fn list_and_read_return_the_stored_representations() {
    let (router, _store, _registry) = mounted();
    let created = create_upstream(router.clone(), TENANT, "api.vendor.com").await;
    let id = created["id"].as_str().expect("id").to_owned();

    // Read by bare UUID.
    let response = get(
        router.clone(),
        TENANT,
        ALL,
        &format!("/oagw/v1/upstreams/{id}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let document = json_body(response).await;
    assert_eq!(document["id"], json!(id));

    // Read by the anonymous GTS identifier of the same instance part.
    let response = get(
        router.clone(),
        TENANT,
        ALL,
        &format!("/oagw/v1/upstreams/{UPSTREAM_ID_PREFIX}{id}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let document = json_body(response).await;
    assert_eq!(document["id"], json!(id));

    // List returns the same record.
    let response = get(router.clone(), TENANT, ALL, "/oagw/v1/upstreams").await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = json_body(response).await;
    assert_eq!(page.as_array().map(Vec::len), Some(1), "{page}");
    assert_eq!(page[0]["id"], json!(id));

    // An unknown identifier is `404` through the mapping layer.
    let unknown = Uuid::new_v4();
    let response = get(
        router,
        TENANT,
        ALL,
        &format!("/oagw/v1/upstreams/{unknown}"),
    )
    .await;
    assert_problem(
        response,
        404,
        "route.not_found.v1",
        &format!("/oagw/v1/upstreams/{unknown}"),
    )
    .await;
}

#[tokio::test]
async fn a_foreign_or_ancestor_upstream_is_indistinguishable_from_a_missing_one() {
    let (router, _store, _registry) = mounted();
    let own = create_upstream(router.clone(), TENANT, "api.vendor.com").await;
    let _foreign = create_upstream(router.clone(), OTHER, "other.example.com").await;

    let foreign = _foreign_id(&_foreign_owned(router.clone(), OTHER).await);
    let own_id = own["id"].as_str().expect("id").to_owned();

    // Another tenant's record is not addressable by this tenant.
    let response = get(
        router.clone(),
        TENANT,
        ALL,
        &format!("/oagw/v1/upstreams/{foreign}"),
    )
    .await;
    let document = assert_problem(
        response,
        404,
        "route.not_found.v1",
        &format!("/oagw/v1/upstreams/{foreign}"),
    )
    .await;
    let rendered = document.to_string();
    assert!(!rendered.contains(&OTHER.to_string()), "{rendered}");
    assert!(!rendered.contains("other.example.com"), "{rendered}");

    // The list is scoped to the calling tenant before any filtering.
    let response = get(router.clone(), TENANT, ALL, "/oagw/v1/upstreams").await;
    let page = json_body(response).await;
    assert_eq!(page.as_array().map(Vec::len), Some(1), "{page}");
    assert_eq!(page[0]["id"], json!(own_id));

    // A list request of the other tenant holds no record of this one.
    let response = get(router, OTHER, ALL, "/oagw/v1/upstreams").await;
    let page = json_body(response).await;
    assert_eq!(page.as_array().map(Vec::len), Some(1), "{page}");
    assert_ne!(page[0]["id"], json!(own_id));
}

/// The upstream a tenant owns, through its own list.
async fn _foreign_owned(router: Router, tenant: Uuid) -> Value {
    let response = get(router, tenant, ALL, "/oagw/v1/upstreams").await;
    let page = json_body(response).await;
    page.as_array()
        .and_then(|records| records.first().cloned())
        .expect("the tenant holds one record")
}

fn _foreign_id(record: &Value) -> String {
    record["id"].as_str().expect("id").to_owned()
}

#[tokio::test]
async fn replace_upstream_returns_200_with_the_replaced_representation() {
    let (router, store, _registry) = mounted();
    let created = create_upstream(router.clone(), TENANT, "api.vendor.com").await;
    let id = created["id"].as_str().expect("id").to_owned();

    let mut body = upstream_body("api.vendor.com");
    body["enabled"] = json!(false);
    body["tags"] = json!(["team-a"]);
    let response = send(
        router.clone(),
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        TENANT,
        ALL,
        body,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let replaced = json_body(response).await;
    assert_eq!(replaced["id"], json!(id), "identity is preserved");
    assert_eq!(replaced["tenant_id"], json!(TENANT));
    assert_eq!(replaced["alias"], json!("api.vendor.com"), "alias immutable");
    assert_eq!(replaced["enabled"], json!(false), "the boolean is stored");
    assert_eq!(replaced["tags"], json!(["team-a"]));

    let stored = store
        .find_upstream(TENANT, Uuid::parse_str(&id).expect("uuid"))
        .expect("stored");
    assert!(!stored.enabled, "the store holds the replaced value");

    // A changed alias is a 400, not a rename.
    let mut body = upstream_body("api.vendor.com");
    body["alias"] = json!("api.other.com");
    let response = send(
        router,
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        TENANT,
        ALL,
        body,
    )
    .await;
    assert_problem(
        response,
        400,
        "validation.error.v1",
        &format!("/oagw/v1/upstreams/{id}"),
    )
    .await;
}

#[tokio::test]
async fn replace_upstream_requires_the_override_permission_and_resolves_the_identifier() {
    let (router, _store, _registry) = mounted();
    let created = create_upstream(router.clone(), TENANT, "api.vendor.com").await;
    let id = created["id"].as_str().expect("id").to_owned();

    let response = send(
        router.clone(),
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        TENANT,
        &["gts.cf.core.oagw.upstream.v1~:read"],
        upstream_body("api.vendor.com"),
    )
    .await;
    assert_problem(
        response,
        403,
        "management.forbidden.v1",
        &format!("/oagw/v1/upstreams/{id}"),
    )
    .await;

    let unknown = Uuid::new_v4();
    let response = send(
        router,
        "PUT",
        &format!("/oagw/v1/upstreams/{unknown}"),
        TENANT,
        ALL,
        upstream_body("api.vendor.com"),
    )
    .await;
    assert_problem(
        response,
        404,
        "route.not_found.v1",
        &format!("/oagw/v1/upstreams/{unknown}"),
    )
    .await;
}

#[tokio::test]
async fn delete_upstream_returns_204_and_cascades_its_routes() {
    let (router, store, _registry) = mounted();
    let created = create_upstream(router.clone(), TENANT, "api.vendor.com").await;
    let id = Uuid::parse_str(created["id"].as_str().expect("id")).expect("uuid");

    let route = send(
        router.clone(),
        "POST",
        "/oagw/v1/routes",
        TENANT,
        ALL,
        route_body(id, "/v1"),
    )
    .await;
    assert_eq!(route.status(), StatusCode::CREATED);

    let response = serve(
        router.clone(),
        Request::builder()
            .method(axum::http::Method::DELETE)
            .uri(format!("/oagw/v1/upstreams/{id}"))
            .extension(context(TENANT, ALL))
            .body(Body::empty())
            .expect("request builds"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let bytes = Body::new(response)
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    assert!(bytes.is_empty(), "a 204 carries no body");

    assert_eq!(store.list_routes(TENANT).len(), 0, "the cascade ran");

    // A second delete is `404`, not `204`.
    let response = serve(
        router,
        Request::builder()
            .method(axum::http::Method::DELETE)
            .uri(format!("/oagw/v1/upstreams/{id}"))
            .extension(context(TENANT, ALL))
            .body(Body::empty())
            .expect("request builds"),
    )
    .await;
    assert_problem(
        response,
        404,
        "route.not_found.v1",
        &format!("/oagw/v1/upstreams/{id}"),
    )
    .await;
}

#[tokio::test]
async fn delete_upstream_requires_the_delete_permission() {
    let (router, _store, _registry) = mounted();
    let created = create_upstream(router.clone(), TENANT, "api.vendor.com").await;
    let id = created["id"].as_str().expect("id");

    let response = serve(
        router,
        Request::builder()
            .method(axum::http::Method::DELETE)
            .uri(format!("/oagw/v1/upstreams/{id}"))
            .extension(context(TENANT, &["gts.cf.core.oagw.upstream.v1~:read"]))
            .body(Body::empty())
            .expect("request builds"),
    )
    .await;
    assert_problem(
        response,
        403,
        "management.forbidden.v1",
        &format!("/oagw/v1/upstreams/{id}"),
    )
    .await;
}

// ---------- route endpoints ----------

#[tokio::test]
async fn create_route_returns_201_and_resolves_the_upstream_reference() {
    let (router, store, _registry) = mounted();
    let upstream = create_upstream(router.clone(), TENANT, "api.vendor.com").await;
    let upstream_id = Uuid::parse_str(upstream["id"].as_str().expect("id")).expect("uuid");

    let response = send(
        router.clone(),
        "POST",
        "/oagw/v1/routes",
        TENANT,
        ALL,
        route_body(upstream_id, "/v1"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let stored = json_body(response).await;
    assert_eq!(stored["upstream_id"], json!(upstream_id));
    assert_eq!(stored["match_type"], json!("http"));
    assert_eq!(stored["match"]["http"]["path"], json!("/v1"));
    assert_eq!(stored["enabled"], json!(true), "recorded default");
    assert_eq!(stored["priority"], json!(0), "recorded default");

    // An ancestor-owned upstream resolves as missing and is a `400`.
    let ancestor = create_upstream(router.clone(), ANCESTOR, "ancestor.example.com").await;
    let ancestor_id = Uuid::parse_str(ancestor["id"].as_str().expect("id")).expect("uuid");
    let response = send(
        router.clone(),
        "POST",
        "/oagw/v1/routes",
        TENANT,
        ALL,
        route_body(ancestor_id, "/v2"),
    )
    .await;
    let document =
        assert_problem(response, 400, "validation.error.v1", "/oagw/v1/routes").await;
    assert!(
        document["detail"]
            .as_str()
            .expect("detail")
            .contains("upstream_id"),
        "the detail names the offending field: {document}"
    );

    // A duplicate match rule of an enabled route is a `409`.
    let response = send(
        router,
        "POST",
        "/oagw/v1/routes",
        TENANT,
        ALL,
        route_body(upstream_id, "/v1"),
    )
    .await;
    assert_problem(response, 409, "management.route_match_conflict.v1", "/oagw/v1/routes").await;

    assert_eq!(store.list_routes(TENANT).len(), 1);
}

#[tokio::test]
async fn create_route_rejects_a_bad_body_with_400() {
    let (router, _store, _registry) = mounted();
    let upstream = create_upstream(router.clone(), TENANT, "api.vendor.com").await;
    let upstream_id = Uuid::parse_str(upstream["id"].as_str().expect("id")).expect("uuid");

    // Both match members present is a `400`.
    let body = json!({
        "upstream_id": upstream_id,
        "match": {
            "http": { "methods": ["GET"], "path": "/v1" },
            "grpc": { "service": "svc.Svc", "method": "Get" }
        }
    });
    let response = send(router, "POST", "/oagw/v1/routes", TENANT, ALL, body).await;
    assert_problem(response, 400, "validation.error.v1", "/oagw/v1/routes").await;
}

#[tokio::test]
async fn routes_support_list_and_read_with_the_odata_contract() {
    let (router, _store, _registry) = mounted();
    let upstream = create_upstream(router.clone(), TENANT, "api.vendor.com").await;
    let upstream_id = Uuid::parse_str(upstream["id"].as_str().expect("id")).expect("uuid");

    // One of the three carries a tag the `$filter` expression can isolate.
    let mut tagged = route_body(upstream_id, "/v1");
    tagged["tags"] = json!(["edge"]);
    for body in [tagged, route_body(upstream_id, "/v2"), route_body(upstream_id, "/v3")] {
        let response = send(
            router.clone(),
            "POST",
            "/oagw/v1/routes",
            TENANT,
            ALL,
            body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    // `$filter` narrows, `$select` projects.
    let response = get(
        router.clone(),
        TENANT,
        ALL,
        "/oagw/v1/routes?$filter=tags%20eq%20'edge'&$select=id,match",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = json_body(response).await;
    assert_eq!(page.as_array().map(Vec::len), Some(1), "{page}");
    assert!(
        page[0].get("match").is_some(),
        "the projection keeps the selected field: {page}"
    );
    assert!(page[0].get("priority").is_none(), "{page}");

    // `$orderby` with `$top`/`$skip` pages the collection.
    let response = get(
        router.clone(),
        TENANT,
        ALL,
        "/oagw/v1/routes?$orderby=created_at%20desc&$top=2&$skip=1",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = json_body(response).await;
    let paths: Vec<&str> = page
        .as_array()
        .expect("array")
        .iter()
        .map(|route| route["match"]["http"]["path"].as_str().expect("path"))
        .collect();
    assert_eq!(paths, vec!["/v2", "/v1"], "{page}");

    // An unsupported expression is `400`.
    for uri in [
        "/oagw/v1/routes?$filter=path%20ne%20'/v2'",
        "/oagw/v1/routes?$orderby=nothing",
        "/oagw/v1/routes?$select=secret",
        "/oagw/v1/routes?$top=1000",
        "/oagw/v1/routes?$skip=-1",
        "/oagw/v1/routes?$unsupported=1",
    ] {
        let response = get(router.clone(), TENANT, ALL, uri).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/problem+json"),
            "{uri}"
        );
    }

    // A read of one route by its GTS identifier.
    let response = get(router.clone(), TENANT, ALL, "/oagw/v1/routes").await;
    let page = json_body(response).await;
    let id = page[0]["id"].as_str().expect("id").to_owned();
    let response = get(
        router.clone(),
        TENANT,
        ALL,
        &format!("/oagw/v1/routes/{ROUTE_ID_PREFIX}{id}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["id"], json!(id));

    // An unknown route of this tenant is `404`.
    let unknown = Uuid::new_v4();
    let response = get(router, TENANT, ALL, &format!("/oagw/v1/routes/{unknown}")).await;
    assert_problem(
        response,
        404,
        "route.not_found.v1",
        &format!("/oagw/v1/routes/{unknown}"),
    )
    .await;
}

#[tokio::test]
async fn replace_route_accepts_an_echoed_upstream_id_and_rejects_a_changed_one() {
    let (router, _store, _registry) = mounted();
    let upstream = create_upstream(router.clone(), TENANT, "api.vendor.com").await;
    let upstream_id = Uuid::parse_str(upstream["id"].as_str().expect("id")).expect("uuid");
    let created = send(
        router.clone(),
        "POST",
        "/oagw/v1/routes",
        TENANT,
        ALL,
        route_body(upstream_id, "/v1"),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let stored = json_body(created).await;
    let id = stored["id"].as_str().expect("id").to_owned();

    // Echoing the stored reference is accepted.
    let mut body = route_body(upstream_id, "/v1");
    body["enabled"] = json!(false);
    body["priority"] = json!(7);
    let response = send(
        router.clone(),
        "PUT",
        &format!("/oagw/v1/routes/{id}"),
        TENANT,
        ALL,
        body,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let replaced = json_body(response).await;
    assert_eq!(replaced["id"], json!(id));
    assert_eq!(replaced["upstream_id"], json!(upstream_id));
    assert_eq!(replaced["enabled"], json!(false));
    assert_eq!(replaced["priority"], json!(7));

    // A different reference is a `400`, and the stored route is untouched.
    let other = create_upstream(router.clone(), TENANT, "other.example.com").await;
    let other_id = Uuid::parse_str(other["id"].as_str().expect("id")).expect("uuid");
    let body = route_body(other_id, "/v1");
    let response = send(
        router,
        "PUT",
        &format!("/oagw/v1/routes/{id}"),
        TENANT,
        ALL,
        body,
    )
    .await;
    assert_problem(
        response,
        400,
        "validation.error.v1",
        &format!("/oagw/v1/routes/{id}"),
    )
    .await;
}

#[tokio::test]
async fn delete_route_returns_204() {
    let (router, store, _registry) = mounted();
    let upstream = create_upstream(router.clone(), TENANT, "api.vendor.com").await;
    let upstream_id = Uuid::parse_str(upstream["id"].as_str().expect("id")).expect("uuid");
    let created = send(
        router.clone(),
        "POST",
        "/oagw/v1/routes",
        TENANT,
        ALL,
        route_body(upstream_id, "/v1"),
    )
    .await;
    let id = json_body(created).await["id"]
        .as_str()
        .expect("id")
        .to_owned();

    let response = serve(
        router,
        Request::builder()
            .method(axum::http::Method::DELETE)
            .uri(format!("/oagw/v1/routes/{id}"))
            .extension(context(TENANT, ALL))
            .body(Body::empty())
            .expect("request builds"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(store.list_routes(TENANT).len(), 0);
}

#[tokio::test]
async fn route_endpoints_require_their_own_permissions() {
    let (router, _store, _registry) = mounted();
    let upstream = create_upstream(router.clone(), TENANT, "api.vendor.com").await;
    let upstream_id = Uuid::parse_str(upstream["id"].as_str().expect("id")).expect("uuid");

    // The upstream `read` scope does not grant the route `create`.
    let response = send(
        router.clone(),
        "POST",
        "/oagw/v1/routes",
        TENANT,
        &["gts.cf.core.oagw.upstream.v1~:read"],
        route_body(upstream_id, "/v1"),
    )
    .await;
    assert_problem(response, 403, "management.forbidden.v1", "/oagw/v1/routes").await;

    let created = send(
        router.clone(),
        "POST",
        "/oagw/v1/routes",
        TENANT,
        ALL,
        route_body(upstream_id, "/v1"),
    )
    .await;
    let id = json_body(created).await["id"]
        .as_str()
        .expect("id")
        .to_owned();

    // `read` alone does not grant the route `override` nor `delete`.
    let scopes = ["gts.cf.core.oagw.route.v1~:read"];
    let response = send(
        router.clone(),
        "PUT",
        &format!("/oagw/v1/routes/{id}"),
        TENANT,
        &scopes,
        route_body(upstream_id, "/v1"),
    )
    .await;
    assert_problem(
        response,
        403,
        "management.forbidden.v1",
        &format!("/oagw/v1/routes/{id}"),
    )
    .await;

    let response = serve(
        router,
        Request::builder()
            .method(axum::http::Method::DELETE)
            .uri(format!("/oagw/v1/routes/{id}"))
            .extension(context(TENANT, &["gts.cf.core.oagw.route.v1~:read"]))
            .body(Body::empty())
            .expect("request builds"),
    )
    .await;
    assert_problem(
        response,
        403,
        "management.forbidden.v1",
        &format!("/oagw/v1/routes/{id}"),
    )
    .await;
}

#[tokio::test]
async fn a_route_of_another_tenant_is_not_addressable() {
    let (router, _store, _registry) = mounted();
    let upstream = create_upstream(router.clone(), TENANT, "api.vendor.com").await;
    let upstream_id = Uuid::parse_str(upstream["id"].as_str().expect("id")).expect("uuid");
    let created = send(
        router.clone(),
        "POST",
        "/oagw/v1/routes",
        TENANT,
        ALL,
        route_body(upstream_id, "/v1"),
    )
    .await;
    let id = json_body(created).await["id"].as_str().expect("id").to_owned();

    let response = get(
        router.clone(),
        OTHER,
        ALL,
        &format!("/oagw/v1/routes/{id}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = serve(
        router,
        Request::builder()
            .method(axum::http::Method::DELETE)
            .uri(format!("/oagw/v1/routes/{id}"))
            .extension(context(OTHER, ALL))
            .body(Body::empty())
            .expect("request builds"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------- enabled and disabled write semantics ----------

// @cpt-begin:cpt-cf-oagw-dod-enable-disable:p1:inst-full
#[tokio::test]
async fn enabled_defaults_to_true_and_is_stored_by_the_owning_tenant() {
    let (router, store, _registry) = mounted();
    let upstream = create_upstream(router.clone(), TENANT, "api.vendor.com").await;
    assert_eq!(upstream["enabled"], json!(true), "recorded default");

    let mut body = upstream_body("backup.vendor.com");
    body["enabled"] = json!(false);
    let response = send(
        router.clone(),
        "POST",
        "/oagw/v1/upstreams",
        TENANT,
        ALL,
        body,
    )
    .await;
    assert_eq!(json_body(response).await["enabled"], json!(false));

    let id = Uuid::parse_str(upstream["id"].as_str().expect("id")).expect("uuid");
    assert!(
        store.find_upstream(TENANT, id).expect("stored").enabled,
        "the first record stays enabled"
    );

    // The owning tenant's replace stores the boolean both ways.
    let mut body = upstream_body("api.vendor.com");
    body["enabled"] = json!(false);
    let response = send(
        router.clone(),
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        TENANT,
        ALL,
        body,
    )
    .await;
    assert_eq!(json_body(response).await["enabled"], json!(false));

    let response = send(
        router.clone(),
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        TENANT,
        ALL,
        upstream_body("api.vendor.com"),
    )
    .await;
    assert_eq!(json_body(response).await["enabled"], json!(true));
    let stored = store.find_upstream(TENANT, id).expect("stored");
    assert!(stored.enabled, "re-enabled by the owning tenant");
}

#[tokio::test]
async fn an_ancestor_disabled_resource_cannot_be_re_enabled_by_a_descendant() {
    let (router, _store, _registry) = mounted();
    let ancestor = create_upstream(router.clone(), ANCESTOR, "ancestor.example.com").await;
    let id = ancestor["id"].as_str().expect("id").to_owned();

    // The descendant addressing the ancestor's record is a `404`, before any
    // field is read, so the inherited disabled state cannot be lifted here.
    for uri in [
        format!("/oagw/v1/upstreams/{id}"),
        format!("/oagw/v1/upstreams/{UPSTREAM_ID_PREFIX}{id}"),
    ] {
        let response = get(
            router.clone(),
            TENANT,
            ALL,
            &format!("/oagw/v1/upstreams/{UPSTREAM_ID_PREFIX}{id}"),
        )
        .await;
        let _ = uri;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    let response = send(
        router.clone(),
        "PUT",
        &format!("/oagw/v1/upstreams/{id}"),
        TENANT,
        ALL,
        upstream_body("ancestor.example.com"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = serve(
        router,
        Request::builder()
            .method(axum::http::Method::DELETE)
            .uri(format!("/oagw/v1/upstreams/{id}"))
            .extension(context(TENANT, ALL))
            .body(Body::empty())
            .expect("request builds"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
// @cpt-end:cpt-cf-oagw-dod-enable-disable:p1:inst-full

// ---------- mount and OpenAPI registration ----------

#[tokio::test]
async fn an_unmatched_subpath_is_still_the_canonical_fallback() {
    let (router, _store, _registry) = mounted();

    for uri in [
        "/oagw/v1/upstreams/a/b",
        "/oagw/v1/routes/a/b",
        "/oagw/v1/payments",
        "/oagw/v1",
    ] {
        let response = get(router.clone(), TENANT, ALL, uri).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/problem+json"),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn the_mount_step_registers_the_management_operations_and_schemas() {
    let (_router, _store, registry) = mounted();

    let operations = registry.operation_paths();
    assert_eq!(operations.len(), 27, "{operations:?}");
    for (method, path) in [
        ("POST", "/oagw/v1/upstreams"),
        ("GET", "/oagw/v1/upstreams"),
        ("GET", "/oagw/v1/upstreams/{id}"),
        ("PUT", "/oagw/v1/upstreams/{id}"),
        ("DELETE", "/oagw/v1/upstreams/{id}"),
        ("POST", "/oagw/v1/routes"),
        ("GET", "/oagw/v1/routes"),
        ("GET", "/oagw/v1/routes/{id}"),
        ("PUT", "/oagw/v1/routes/{id}"),
        ("DELETE", "/oagw/v1/routes/{id}"),
        // @cpt-begin:cpt-cf-oagw-flow-plugin-catalog-bootstrap:p2:inst-pcat-09
        ("POST", "/oagw/v1/plugins"),
        ("GET", "/oagw/v1/plugins"),
        ("GET", "/oagw/v1/plugins/{id}"),
        ("DELETE", "/oagw/v1/plugins/{id}"),
        ("GET", "/oagw/v1/plugins/{id}/source"),
        // @cpt-end:cpt-cf-oagw-flow-plugin-catalog-bootstrap:p2:inst-pcat-09
        // The data plane is registered by the same mount step: every forwarded
        // method on both proxy paths plus the preflight pair.
        ("GET", "/oagw/v1/proxy/{alias}"),
        ("POST", "/oagw/v1/proxy/{alias}"),
        ("PUT", "/oagw/v1/proxy/{alias}"),
        ("DELETE", "/oagw/v1/proxy/{alias}"),
        ("PATCH", "/oagw/v1/proxy/{alias}"),
        ("OPTIONS", "/oagw/v1/proxy/{alias}"),
        ("GET", "/oagw/v1/proxy/{alias}/{path_suffix}"),
        ("POST", "/oagw/v1/proxy/{alias}/{path_suffix}"),
        ("PUT", "/oagw/v1/proxy/{alias}/{path_suffix}"),
        ("DELETE", "/oagw/v1/proxy/{alias}/{path_suffix}"),
        ("PATCH", "/oagw/v1/proxy/{alias}/{path_suffix}"),
        ("OPTIONS", "/oagw/v1/proxy/{alias}/{path_suffix}"),
    ] {
        assert!(
            operations.contains(&format!("{method} {path}")),
            "{method} {path} missing: {operations:?}"
        );
    }

    let schemas = registry.schema_names();
    for name in [
        "OagwUpstream",
        "OagwUpstreamBody",
        "OagwRoute",
        "OagwRouteBody",
        "OagwPlugin",
        "OagwPluginBody",
    ] {
        assert!(schemas.contains(&name.to_owned()), "{name} missing");
    }
}

#[test]
fn the_mount_root_stays_gear_relative() {
    assert_eq!(MOUNT_ROOT, "/oagw/v1");
}

// ---------- ancestor gates over a resolved chain ----------
//
// The mounts below carry a `StaticHierarchy` in place of the tenant-resolver
// adapter, so the ancestor chain of `cpt-cf-oagw-algo-sharing-mode-validate` is
// resolved for the request exactly as the wired gear resolves it from the
// client hub: the gates now fire on the mounted router, not only in the domain
// unit tests.

/// A caller holding the create permission and nothing else.
const CREATE_ONLY: &[&str] = &["gts.cf.core.oagw.upstream.v1~:create"];

const OAUTH2: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";

/// An ancestor upstream the descendant tenant inherits from, created through
/// the API under the ancestor's own context.
async fn seed_ancestor(router: Router, body: Value) {
    let response = send(
        router,
        "POST",
        "/oagw/v1/upstreams",
        ANCESTOR,
        ALL,
        body,
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "the ancestor's own create is not a bind"
    );
}

#[tokio::test]
async fn a_binding_create_requires_the_bind_permission() {
    let (router, store) = mounted_over_chains(BTreeMap::from([(TENANT, vec![ANCESTOR])]));
    seed_ancestor(router.clone(), upstream_body("api.vendor.com")).await;

    let response = send(
        router,
        "POST",
        "/oagw/v1/upstreams",
        TENANT,
        CREATE_ONLY,
        upstream_body("api.vendor.com"),
    )
    .await;
    let document =
        assert_problem(response, 403, "management.forbidden.v1", "/oagw/v1/upstreams").await;
    assert!(
        document["detail"]
            .as_str()
            .expect("detail")
            .contains("oagw:upstream:bind"),
        "the detail names the bind gate: {document}"
    );
    assert_eq!(store.list_upstreams(TENANT).len(), 0, "no record written");
}

#[tokio::test]
async fn a_granted_bind_unions_the_ancestor_tags_add_only() {
    let (router, _store) = mounted_over_chains(BTreeMap::from([
        (ANCESTOR, Vec::new()),
        (TENANT, vec![ANCESTOR]),
    ]));

    let mut ancestor = upstream_body("api.vendor.com");
    ancestor["tags"] = json!(["platform", "edge"]);
    seed_ancestor(router.clone(), ancestor).await;

    let mut own = upstream_body("api.vendor.com");
    own["tags"] = json!(["edge", "descendant"]);
    let response = send(router, "POST", "/oagw/v1/upstreams", TENANT, ALL, own).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let bound = json_body(response).await;
    assert_eq!(
        bound["tags"],
        json!(["descendant", "edge", "platform"]),
        "the inherited tags survive and none is dropped: {bound}"
    );
}

#[tokio::test]
async fn an_enforced_ancestor_block_rejects_the_descendant_override() {
    let (router, _store) = mounted_over_chains(BTreeMap::from([(TENANT, vec![ANCESTOR])]));

    let mut ancestor = upstream_body("api.vendor.com");
    ancestor["auth"] = json!({
        "type": OAUTH2,
        "sharing": "enforce",
        "config": { "client_secret": "cred://platform/ancestor-oauth2/secret" }
    });
    seed_ancestor(router.clone(), ancestor).await;

    let mut own = upstream_body("api.vendor.com");
    own["auth"] = json!({
        "type": OAUTH2,
        "config": { "client_secret": "cred://platform/descendant-oauth2/secret" }
    });
    let response = send(router, "POST", "/oagw/v1/upstreams", TENANT, ALL, own).await;
    let document = assert_problem(response, 400, "validation.error.v1", "/oagw/v1/upstreams").await;
    assert!(
        document["detail"]
            .as_str()
            .expect("detail")
            .contains("enforces `auth`"),
        "the detail names the enforced block: {document}"
    );
}

#[tokio::test]
async fn a_private_ancestor_block_is_invisible_to_a_descendant_write() {
    let (router, _store) = mounted_over_chains(BTreeMap::from([(TENANT, vec![ANCESTOR])]));
    // Holds the bind gate of the shared alias, not the auth-override one.
    let bind_only = &[CREATE_ONLY[0], oagw::domain::sharing::PERM_UPSTREAM_BIND];

    // `private` is the recorded default of a block, so the ancestor's auth is
    // never offered to the descendant.
    let mut ancestor = upstream_body("api.vendor.com");
    ancestor["auth"] = json!({
        "type": OAUTH2,
        "config": { "client_secret": "cred://platform/ancestor-oauth2/secret" }
    });
    seed_ancestor(router.clone(), ancestor).await;

    let mut own = upstream_body("api.vendor.com");
    own["auth"] = json!({
        "type": OAUTH2,
        "config": { "client_secret": "cred://platform/descendant-oauth2/secret" }
    });
    let response = send(
        router,
        "POST",
        "/oagw/v1/upstreams",
        TENANT,
        // No `oagw:upstream:override_auth`: the private block grants nothing to
        // inherit, so the override permission is never demanded.
        bind_only,
        own,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let stored = json_body(response).await;
    assert_eq!(
        stored["auth"]["config"]["client_secret"],
        json!("cred://platform/descendant-oauth2/secret"),
        "the descendant's own declaration is stored verbatim: {stored}"
    );
}

#[tokio::test]
async fn an_inherited_ancestor_auth_requires_the_override_auth_permission() {
    let (router, _store) = mounted_over_chains(BTreeMap::from([(TENANT, vec![ANCESTOR])]));
    let bind_only = &[CREATE_ONLY[0], oagw::domain::sharing::PERM_UPSTREAM_BIND];

    // `inherit` publishes the block to the descendant, which then needs the
    // override permission to shadow it.
    let mut ancestor = upstream_body("api.vendor.com");
    ancestor["auth"] = json!({
        "type": OAUTH2,
        "sharing": "inherit",
        "config": { "client_secret": "cred://platform/ancestor-oauth2/secret" }
    });
    seed_ancestor(router.clone(), ancestor).await;

    let mut own = upstream_body("api.vendor.com");
    own["auth"] = json!({
        "type": OAUTH2,
        "config": { "client_secret": "cred://platform/descendant-oauth2/secret" }
    });
    let response = send(
        router,
        "POST",
        "/oagw/v1/upstreams",
        TENANT,
        bind_only,
        own,
    )
    .await;
    let document =
        assert_problem(response, 403, "management.forbidden.v1", "/oagw/v1/upstreams").await;
    assert!(
        document["detail"]
            .as_str()
            .expect("detail")
            .contains("oagw:upstream:override_auth"),
        "the detail names the auth-override gate: {document}"
    );
}

#[tokio::test]
async fn an_appended_plugin_requires_the_add_plugins_permission() {
    let (router, _store) = mounted_over_chains(BTreeMap::from([(TENANT, vec![ANCESTOR])]));
    // Holds the bind gate of the shared alias, not the plugin-append one.
    let bind_only = &[CREATE_ONLY[0], oagw::domain::sharing::PERM_UPSTREAM_BIND];

    let plugin = "gts.cf.core.oagw.plugin.v1~0f0e0d0c-0b0a-4918-8276-7a1e4b0f1122";
    let appended = "gts.cf.core.oagw.plugin.v1~1f0e0d0c-0b0a-4918-8276-7a1e4b0f1122";
    let mut ancestor = upstream_body("api.vendor.com");
    ancestor["plugins"] = json!({ "sharing": "inherit", "items": [plugin] });
    seed_ancestor(router.clone(), ancestor).await;

    let mut own = upstream_body("api.vendor.com");
    own["plugins"] = json!({ "items": [plugin, appended] });
    let response = send(
        router,
        "POST",
        "/oagw/v1/upstreams",
        TENANT,
        bind_only,
        own,
    )
    .await;
    let document =
        assert_problem(response, 403, "management.forbidden.v1", "/oagw/v1/upstreams").await;
    assert!(
        document["detail"]
            .as_str()
            .expect("detail")
            .contains("oagw:upstream:add_plugins"),
        "the detail names the append gate: {document}"
    );
}

#[tokio::test]
async fn an_ancestor_owned_record_stays_404_when_a_chain_is_resolved() {
    let (router, _store) = mounted_over_chains(BTreeMap::from([(TENANT, vec![ANCESTOR])]));

    let ancestor = create_upstream(router.clone(), ANCESTOR, "ancestor.example.com").await;
    let id = ancestor["id"].as_str().expect("id").to_owned();
    let path = format!("/oagw/v1/upstreams/{id}");

    // The chain lets the descendant *bind* to the alias; it does not widen the
    // addressable key space, so every verb on the record stays a `404`.
    for (method, uri, body) in [
        ("GET", path.clone(), None),
        (
            "PUT",
            path.clone(),
            Some(upstream_body("ancestor.example.com")),
        ),
        ("DELETE", path, None),
    ] {
        let payload = body.unwrap_or_else(|| json!({}));
        let response = send(router.clone(), method, &uri, TENANT, ALL, payload).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} {uri} is not addressable by the descendant"
        );
    }

    // The list of the descendant holds only its own records.
    let own = create_upstream(router.clone(), TENANT, "api.vendor.com").await;
    let response = get(router, TENANT, ALL, "/oagw/v1/upstreams").await;
    let page = json_body(response).await;
    assert_eq!(page.as_array().map(Vec::len), Some(1), "{page}");
    assert_eq!(page[0]["id"], own["id"]);
}

#[tokio::test]
async fn without_a_chain_the_same_create_is_not_a_bind() {
    // The single-tenant fallback: no ancestor chain is resolvable, so the very
    // same create that is a bind above is an ordinary create here. This is the
    // observable difference the startup log reports.
    let (router, _store, _registry) = mounted();
    seed_ancestor(router.clone(), upstream_body("api.vendor.com")).await;

    let response = send(
        router,
        "POST",
        "/oagw/v1/upstreams",
        TENANT,
        CREATE_ONLY,
        upstream_body("api.vendor.com"),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "no chain resolved, so no bind gate fires"
    );
}

// @cpt-end:cpt-cf-oagw-dod-in-crate-test-coverage:p2:inst-full
