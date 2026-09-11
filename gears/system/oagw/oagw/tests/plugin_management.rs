#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for the plugin definition surface (entry 2.3).
//!
//! In-crate integration tests only (DECOMPOSITION assumption 5): no e2e suite
//! is added under `testing/e2e/gears/oagw/`. The tests drive the mounted router
//! the way the host api-gateway does — the Bearer token is already resolved, so
//! the request carries the resolved
//! [`SecurityContext`](toolkit_security::SecurityContext) — and assert the
//! status codes, the `application/problem+json` bodies, the tenant scoping, the
//! in-use conflict and the verbatim source of
//! `cpt-cf-oagw-dod-plugin-endpoints`, `cpt-cf-oagw-dod-plugin-model`,
//! `cpt-cf-oagw-dod-plugin-immutability`, `cpt-cf-oagw-dod-plugin-source-endpoint`,
//! `cpt-cf-oagw-dod-plugin-in-use-conflict` and
//! `cpt-cf-oagw-dod-plugin-binding-validation`.
//!
//! The plugin type catalog and the builtin registries themselves are covered by
//! the unit tests of `crate::infra::plugin`, which need no router.

// @cpt-begin:cpt-cf-oagw-dod-plugin-test-coverage:p2:inst-full
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

use oagw::api::rest::dto::{EXTENSION_PLUGIN_ID, EXTENSION_REFERENCED_BY};
use oagw::api::rest::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER};
use oagw::api::rest::routes::register_routes;
use oagw::domain::model::PROTOCOL_HTTP;
use oagw::domain::sharing::FlatHierarchy;
use oagw::infra::storage::OagwStore;

const TENANT: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000030");
const OTHER: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000040");

/// Base identifier of the auth plugin type.
const AUTH_BASE: &str = "gts.cf.core.oagw.auth_plugin.v1";
/// Base identifier of the guard plugin type.
const GUARD_BASE: &str = "gts.cf.core.oagw.guard_plugin.v1";
/// Base identifier of the transform plugin type.
const TRANSFORM_BASE: &str = "gts.cf.core.oagw.transform_plugin.v1";
/// Type-agnostic chain base of a `plugins.items[]` reference.
const CHAIN_BASE: &str = "gts.cf.core.oagw.plugin.v1";

/// Every scope a management caller may need.
fn all() -> Vec<String> {
    vec!["*".to_owned()]
}

/// Host OpenAPI registry double recording the registered component names.
#[derive(Default)]
struct RecordingRegistry {
    schemas: std::sync::Mutex<Vec<String>>,
    operations: std::sync::Mutex<Vec<String>>,
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

impl RecordingRegistry {
    fn operation_paths(&self) -> Vec<String> {
        self.operations.lock().expect("operations lock").clone()
    }
}

/// The mounted router over a fresh store.
fn mounted() -> (Router, Arc<OagwStore>, RecordingRegistry) {
    let store = Arc::new(OagwStore::new());
    let registry = RecordingRegistry::default();
    let router = register_routes(
        Router::new(),
        &registry,
        Arc::clone(&store),
        Arc::new(FlatHierarchy),
    );
    (router, store, registry)
}

/// A security context the host api-gateway would inject for `tenant`.
fn context(tenant: Uuid, scopes: &[String]) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_type("user")
        .subject_tenant_id(tenant)
        .token_scopes(scopes.to_vec())
        .build()
        .expect("context builds")
}

/// Serve `router` with the request the builder describes.
async fn serve(router: Router, request: Request<Body>) -> axum::response::Response {
    router.oneshot(request).await.expect("request serves")
}

/// A `GET` request with the security context extension attached.
async fn get(router: Router, tenant: Uuid, scopes: &[String], uri: &str) -> axum::response::Response {
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
    scopes: &[String],
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

/// A request with no security context extension at all.
async fn anonymous(router: Router, method: &str, uri: &str) -> axum::response::Response {
    let method = axum::http::Method::from_bytes(method.as_bytes()).expect("method");
    serve(
        router,
        Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
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

/// The body of a response as raw bytes.
async fn raw_body(response: axum::response::Response) -> Vec<u8> {
    Body::new(response)
        .collect()
        .await
        .expect("body collects")
        .to_bytes()
        .to_vec()
}

/// Assert the canonical problem contract of a failure response.
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
    assert_eq!(document["instance"], json!(instance), "{document}");
    document
}

/// A valid guard plugin body.
fn plugin_body(kind: &str, name: &str) -> Value {
    json!({
        "plugin_type": kind,
        "name": name,
        "description": "rejects a request that carries no tenant header",
        "config_schema": {
            "type": "object",
            "properties": { "header": { "type": "string" } }
        },
        "phases": ["on_request", "on_response"],
        "source_code": "def apply(ctx):\n    return ctx\n"
    })
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

/// Create a definition through the API and return the stored representation.
async fn create_plugin(router: Router, tenant: Uuid, kind: &str, name: &str) -> Value {
    let response = send(
        router,
        "POST",
        "/oagw/v1/plugins",
        tenant,
        &all(),
        plugin_body(kind, name),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    json_body(response).await
}

/// Create an upstream through the API and return the stored representation.
async fn create_upstream(router: Router, tenant: Uuid, host: &str) -> Value {
    let response = send(
        router,
        "POST",
        "/oagw/v1/upstreams",
        tenant,
        &all(),
        upstream_body(host),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    json_body(response).await
}

// ---------- create ----------

#[tokio::test]
async fn create_returns_201_with_the_stored_definition() {
    let (router, _store, _registry) = mounted();

    let stored = create_plugin(router, TENANT, "guard", "tenant-guard").await;

    assert!(
        stored["id"]
            .as_str()
            .expect("id")
            .starts_with(&format!("{GUARD_BASE}~")),
        "{stored}"
    );
    assert_eq!(stored["tenant_id"], json!(TENANT));
    assert_eq!(stored["plugin_type"], json!("guard"));
    assert_eq!(stored["name"], json!("tenant-guard"));
    assert_eq!(stored["description"], json!("rejects a request that carries no tenant header"));
    assert_eq!(stored["phases"], json!(["on_request", "on_response"]));
    assert_eq!(stored["source_code"], json!("def apply(ctx):\n    return ctx\n"));
    assert!(stored["last_used_at"].is_null(), "no usage tracking");
    assert!(stored["gc_eligible_at"].is_null(), "no GC job");
}

#[tokio::test]
async fn a_definition_of_each_type_is_stored() {
    let (router, store, _registry) = mounted();

    let auth = create_plugin(router.clone(), TENANT, "auth", "custom-auth").await;
    let transform = create_plugin(router.clone(), TENANT, "transform", "custom-transform").await;
    let _guard = create_plugin(router, TENANT, "guard", "custom-guard").await;

    assert!(
        auth["id"].as_str().expect("id").starts_with(&format!("{AUTH_BASE}~")),
        "{auth}"
    );
    assert!(
        transform["id"]
            .as_str()
            .expect("id")
            .starts_with(&format!("{TRANSFORM_BASE}~")),
        "{transform}"
    );
    assert_eq!(store.list_plugins(TENANT).len(), 3, "three rows stored");
}

#[tokio::test]
async fn the_server_managed_members_of_a_body_are_ignored() {
    let (router, store, _registry) = mounted();

    let mut body = plugin_body("guard", "stamped-guard");
    body["id"] = json!("gts.cf.core.oagw.guard_plugin.v1~00000000-0000-4000-8000-000000000001");
    body["tenant_id"] = json!(OTHER);
    body["last_used_at"] = json!("2024-01-01T00:00:00Z");
    body["gc_eligible_at"] = json!("2024-01-02T00:00:00Z");

    let response = send(router, "POST", "/oagw/v1/plugins", TENANT, &all(), body).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let stored = json_body(response).await;

    assert_ne!(
        stored["id"],
        json!("gts.cf.core.oagw.guard_plugin.v1~00000000-0000-4000-8000-000000000001"),
        "the identifier is server generated"
    );
    assert_eq!(stored["tenant_id"], json!(TENANT), "the tenant is the caller's");
    assert!(stored["last_used_at"].is_null(), "server-managed, never taken from a body");
    assert!(stored["gc_eligible_at"].is_null(), "server-managed, never taken from a body");

    let row = store.list_plugins(TENANT).pop().expect("row");
    assert!(row.last_used_at.is_none() && row.gc_eligible_at.is_none());
}

#[tokio::test]
async fn a_sourceless_definition_with_a_config_schema_is_stored() {
    let (router, _store, _registry) = mounted();

    let body = json!({
        "plugin_type": "guard",
        "name": "contract-only",
        "config_schema": { "type": "object" }
    });
    let response = send(router, "POST", "/oagw/v1/plugins", TENANT, &all(), body).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let stored = json_body(response).await;
    assert_eq!(stored["config_schema"], json!({ "type": "object" }));
    assert_eq!(stored["source_code"], json!(""), "no script body");
}

#[tokio::test]
async fn an_unknown_property_is_rejected() {
    let (router, store, _registry) = mounted();

    let mut body = plugin_body("guard", "unknown-member");
    body["secret"] = json!("cred://platform/leak");
    let response = send(router, "POST", "/oagw/v1/plugins", TENANT, &all(), body).await;

    let document = assert_problem(response, 400, "validation.error.v1", "/oagw/v1/plugins").await;
    assert!(store.list_plugins(TENANT).is_empty(), "nothing stored");
    assert!(!document["detail"].as_str().expect("detail").contains("cred://"));
}

#[tokio::test]
async fn an_unknown_plugin_type_and_a_bad_phase_are_rejected() {
    let (router, store, _registry) = mounted();

    let response = send(
        router.clone(),
        "POST",
        "/oagw/v1/plugins",
        TENANT,
        &all(),
        plugin_body("sandbox", "typed"),
    )
    .await;
    let document = assert_problem(response, 400, "validation.error.v1", "/oagw/v1/plugins").await;
    assert!(
        document["detail"]
            .as_str()
            .expect("detail")
            .contains("plugin_type"),
        "{document}"
    );

    let mut bad_phase = plugin_body("guard", "bad-phase");
    bad_phase["phases"] = json!(["on_startup"]);
    let response = send(router, "POST", "/oagw/v1/plugins", TENANT, &all(), bad_phase).await;
    let document = assert_problem(response, 400, "validation.error.v1", "/oagw/v1/plugins").await;
    assert!(
        document["detail"]
            .as_str()
            .expect("detail")
            .contains("phases"),
        "{document}"
    );
    assert!(store.list_plugins(TENANT).is_empty(), "nothing stored");
}

#[tokio::test]
async fn a_config_schema_that_is_not_an_object_is_rejected() {
    let (router, _store, _registry) = mounted();

    let mut body = plugin_body("guard", "flat-schema");
    body["config_schema"] = json!(["not", "an", "object"]);
    let response = send(router, "POST", "/oagw/v1/plugins", TENANT, &all(), body).await;
    let document = assert_problem(response, 400, "validation.error.v1", "/oagw/v1/plugins").await;
    assert!(
        document["detail"].as_str().expect("detail").contains("config_schema"),
        "{document}"
    );
}

#[tokio::test]
async fn a_definition_without_a_name_is_rejected() {
    let (router, _store, _registry) = mounted();

    let mut body = plugin_body("guard", "no-name");
    body.as_object_mut()
        .expect("object")
        .remove("name")
        .expect("name present");
    let response = send(router, "POST", "/oagw/v1/plugins", TENANT, &all(), body).await;
    let document = assert_problem(response, 400, "validation.error.v1", "/oagw/v1/plugins").await;
    assert!(
        document["detail"].as_str().expect("detail").contains("name"),
        "{document}"
    );
}

#[tokio::test]
async fn a_name_is_unique_per_tenant() {
    let (router, _store, _registry) = mounted();

    let _first = create_plugin(router.clone(), TENANT, "guard", "shared-name").await;
    let response = send(
        router.clone(),
        "POST",
        "/oagw/v1/plugins",
        TENANT,
        &all(),
        plugin_body("transform", "shared-name"),
    )
    .await;
    assert_problem(response, 409, "management.alias_conflict.v1", "/oagw/v1/plugins").await;

    // The same name in another tenant is a different resource.
    let response = send(
        router,
        "POST",
        "/oagw/v1/plugins",
        OTHER,
        &all(),
        plugin_body("transform", "shared-name"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED, "another tenant");
}

#[tokio::test]
async fn create_requires_the_create_permission_of_the_requested_type() {
    let (router, _store, _registry) = mounted();

    // A caller holding the auth create permission only.
    let auth_create = vec![format!("{AUTH_BASE}~:create")];
    let response = send(
        router.clone(),
        "POST",
        "/oagw/v1/plugins",
        TENANT,
        &auth_create,
        plugin_body("guard", "not-permitted"),
    )
    .await;
    assert_problem(response, 403, "management.forbidden.v1", "/oagw/v1/plugins").await;

    let response = send(
        router,
        "POST",
        "/oagw/v1/plugins",
        TENANT,
        &auth_create,
        plugin_body("auth", "permitted-auth"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn a_request_without_a_resolvable_tenant_is_unauthenticated() {
    let (router, _store, _registry) = mounted();

    let response = anonymous(router, "POST", "/oagw/v1/plugins").await;
    assert_problem(response, 401, "auth.failed.v1", "/oagw/v1/plugins").await;
}

// ---------- list and read ----------

#[tokio::test]
async fn the_list_is_scoped_to_the_calling_tenant_and_filterable() {
    let (router, _store, _registry) = mounted();

    let guard = create_plugin(router.clone(), TENANT, "guard", "scoped-guard").await;
    let _transform = create_plugin(router.clone(), TENANT, "transform", "scoped-transform").await;
    let foreign = create_plugin(router.clone(), OTHER, "guard", "foreign-guard").await;

    let response = get(
        router.clone(),
        TENANT,
        &all(),
        "/oagw/v1/plugins?$filter=type%20eq%20'guard'",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = json_body(response).await;
    assert_eq!(page.as_array().map(Vec::len), Some(1), "{page}");
    assert_eq!(page[0]["id"], guard["id"], "{page}");

    // The foreign definition is not in the calling tenant's page.
    let response = get(router.clone(), TENANT, &all(), "/oagw/v1/plugins").await;
    let page = json_body(response).await;
    assert_eq!(page.as_array().map(Vec::len), Some(2), "{page}");
    assert!(
        !page.as_array().expect("array").iter().any(|row| row["id"] == foreign["id"]),
        "{page}"
    );

    // A projection keeps the selected members only.
    let response = get(router, TENANT, &all(), "/oagw/v1/plugins?$select=name,plugin_type").await;
    let page = json_body(response).await;
    assert_eq!(page.as_array().map(Vec::len), Some(2), "{page}");
    assert!(page[0].get("source_code").is_none(), "{page}");
}

#[tokio::test]
async fn the_list_pages_with_top_and_skip() {
    let (router, _store, _registry) = mounted();

    for name in ["p-one", "p-two", "p-three"] {
        create_plugin(router.clone(), TENANT, "guard", name).await;
    }

    let response = get(router.clone(), TENANT, &all(), "/oagw/v1/plugins?$top=2").await;
    let page = json_body(response).await;
    assert_eq!(page.as_array().map(Vec::len), Some(2), "{page}");

    let response = get(router, TENANT, &all(), "/oagw/v1/plugins?$top=2&$skip=2").await;
    let page = json_body(response).await;
    assert_eq!(page.as_array().map(Vec::len), Some(1), "{page}");
}

#[tokio::test]
async fn an_unsupported_query_expression_is_rejected() {
    let (router, _store, _registry) = mounted();

    let response = get(router, TENANT, &all(), "/oagw/v1/plugins?$orderby=name").await;
    let document = assert_problem(response, 400, "validation.error.v1", "/oagw/v1/plugins").await;
    assert!(
        document["detail"].as_str().expect("detail").contains("$orderby"),
        "{document}"
    );
}

#[tokio::test]
async fn a_definition_is_read_by_its_gts_identifier() {
    let (router, _store, _registry) = mounted();

    let stored = create_plugin(router.clone(), TENANT, "guard", "readable").await;
    let id = stored["id"].as_str().expect("id").to_owned();

    let response = get(
        router.clone(),
        TENANT,
        &all(),
        &format!("/oagw/v1/plugins/{id}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let read = json_body(response).await;
    assert_eq!(read["id"], stored["id"]);
    assert_eq!(read["source_code"], stored["source_code"]);

    // The bare UUID form resolves to the same record.
    let uuid = id.split('~').next_back().expect("instance").to_owned();
    let response = get(router.clone(), TENANT, &all(), &format!("/oagw/v1/plugins/{uuid}")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["id"], stored["id"]);

    // A foreign tenant cannot address it.
    let response = get(router.clone(), OTHER, &all(), &format!("/oagw/v1/plugins/{id}")).await;
    assert_problem(response, 404, "route.not_found.v1", &format!("/oagw/v1/plugins/{id}")).await;

    // An unknown identifier is indistinguishable from a foreign one.
    let unknown = format!("{GUARD_BASE}~00000000-0000-4000-8000-00000000dead");
    let response = get(router.clone(), TENANT, &all(), &format!("/oagw/v1/plugins/{unknown}")).await;
    assert_problem(response, 404, "route.not_found.v1", &format!("/oagw/v1/plugins/{unknown}")).await;
}

#[tokio::test]
async fn a_named_builtin_and_a_catalog_only_identifier_have_no_row() {
    let (router, _store, _registry) = mounted();
    let _stored = create_plugin(router.clone(), TENANT, "guard", "present").await;

    for identifier in [
        format!("{GUARD_BASE}~cf.core.oagw.required_headers.v1"),
        format!("{GUARD_BASE}~cf.core.oagw.timeout.v1"),
        format!("{AUTH_BASE}~cf.core.oagw.basic.v1"),
        format!("{TRANSFORM_BASE}~cf.core.oagw.logging.v1"),
    ] {
        let response = get(
            router.clone(),
            TENANT,
            &all(),
            &format!("/oagw/v1/plugins/{identifier}"),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{identifier} has no stored row"
        );
    }

    // A malformed identifier is a client error, not a miss.
    let response = get(
        router,
        TENANT,
        &all(),
        "/oagw/v1/plugins/not-an-identifier",
    )
    .await;
    assert_problem(
        response,
        400,
        "validation.error.v1",
        "/oagw/v1/plugins/not-an-identifier",
    )
    .await;
}

#[tokio::test]
async fn reading_requires_the_read_permission_of_the_definition_type() {
    let (router, _store, _registry) = mounted();

    let stored = create_plugin(router.clone(), TENANT, "transform", "gated-transform").await;
    let id = stored["id"].as_str().expect("id").to_owned();
    let guard_read = [format!("{GUARD_BASE}~:read")];

    let response = get(
        router.clone(),
        TENANT,
        &guard_read,
        &format!("/oagw/v1/plugins/{id}"),
    )
    .await;
    assert_problem(response, 403, "management.forbidden.v1", &format!("/oagw/v1/plugins/{id}")).await;

    // The family gate of the list: none of the three read permissions held.
    let response = get(
        router.clone(),
        TENANT,
        &[format!("{AUTH_BASE}~:create")],
        "/oagw/v1/plugins",
    )
    .await;
    assert_problem(response, 403, "management.forbidden.v1", "/oagw/v1/plugins").await;

    let transform_read = [format!("{TRANSFORM_BASE}~:read")];
    let response = get(
        router,
        TENANT,
        &transform_read,
        &format!("/oagw/v1/plugins/{id}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

// ---------- source ----------

#[tokio::test]
async fn the_stored_source_is_returned_verbatim() {
    let (router, _store, _registry) = mounted();

    let source = "def apply(ctx):\n    # é – unicode, quotes \" and backslash \\\n    return None\n";
    let mut body = plugin_body("guard", "sourced");
    body["source_code"] = json!(source);
    let created = send(router.clone(), "POST", "/oagw/v1/plugins", TENANT, &all(), body).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let id = json_body(created).await["id"].as_str().expect("id").to_owned();

    let response = get(
        router,
        TENANT,
        &all(),
        &format!("/oagw/v1/plugins/{id}/source"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/plain; charset=utf-8"),
        "the verbatim media type, not an active-content type"
    );
    let body = String::from_utf8(raw_body(response).await).expect("utf-8");
    assert_eq!(body, source, "byte for byte, with no JSON envelope around it");
}

#[tokio::test]
async fn a_named_builtin_has_no_stored_source() {
    let (router, _store, _registry) = mounted();

    let response = get(
        router,
        TENANT,
        &all(),
        &format!("/oagw/v1/plugins/{GUARD_BASE}~cf.core.oagw.required_headers.v1/source"),
    )
    .await;
    assert_problem(
        response,
        404,
        "route.not_found.v1",
        &format!("/oagw/v1/plugins/{GUARD_BASE}~cf.core.oagw.required_headers.v1/source"),
    )
    .await;
}

#[tokio::test]
async fn the_source_read_requires_the_read_permission() {
    let (router, _store, _registry) = mounted();

    let stored = create_plugin(router.clone(), TENANT, "guard", "sourced-guard").await;
    let id = stored["id"].as_str().expect("id").to_owned();

    let response = get(
        router,
        TENANT,
        &[format!("{TRANSFORM_BASE}~:read")],
        &format!("/oagw/v1/plugins/{id}/source"),
    )
    .await;
    assert_problem(
        response,
        403,
        "management.forbidden.v1",
        &format!("/oagw/v1/plugins/{id}/source"),
    )
    .await;
}

// ---------- delete ----------

#[tokio::test]
async fn an_unreferenced_definition_is_deleted_and_then_is_404() {
    let (router, store, _registry) = mounted();

    let stored = create_plugin(router.clone(), TENANT, "guard", "deletable").await;
    let id = stored["id"].as_str().expect("id").to_owned();
    let path = format!("/oagw/v1/plugins/{id}");

    let response = send(router.clone(), "DELETE", &path, TENANT, &all(), json!({})).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(raw_body(response).await.is_empty(), "204 carries no body");
    assert!(store.find_plugin(TENANT, &id).is_none(), "the row is gone");

    let response = get(router.clone(), TENANT, &all(), &path).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // A repeated delete of the same definition is a `404`.
    let response = send(router, "DELETE", &path, TENANT, &all(), json!({})).await;
    assert_problem(response, 404, "route.not_found.v1", &path).await;
}

#[tokio::test]
async fn a_definition_bound_to_an_upstream_is_409_with_the_extension_members() {
    let (router, store, _registry) = mounted();

    let stored = create_plugin(router.clone(), TENANT, "guard", "bound-guard").await;
    let plugin_id = stored["id"].as_str().expect("id").to_owned();

    let mut body = upstream_body("api.vendor.com");
    body["plugins"] = json!({ "items": [plugin_id] });
    let created = send(router.clone(), "POST", "/oagw/v1/upstreams", TENANT, &all(), body).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let upstream = json_body(created).await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();

    let path = format!("/oagw/v1/plugins/{plugin_id}");
    let response = send(router.clone(), "DELETE", &path, TENANT, &all(), json!({})).await;
    let document = assert_problem(response, 409, "plugin.in_use.v1", &path).await;
    assert_eq!(document[EXTENSION_PLUGIN_ID], json!(plugin_id), "{document}");
    assert_eq!(
        document[EXTENSION_REFERENCED_BY],
        json!([format!("upstreams/{upstream_id}/plugins/0")]),
        "{document}"
    );
    assert!(
        store.find_plugin(TENANT, &plugin_id).is_some(),
        "the store is untouched by a refused delete"
    );

    // Removing the binding frees the definition; the repeated delete succeeds.
    let replacement = upstream_body("api.vendor.com");
    let response = send(
        router.clone(),
        "PUT",
        &format!("/oagw/v1/upstreams/{upstream_id}"),
        TENANT,
        &all(),
        replacement,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "{:?}", json_body(response).await);

    let response = send(router, "DELETE", &path, TENANT, &all(), json!({})).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_route_binding_names_the_route_in_the_conflict() {
    let (router, _store, _registry) = mounted();

    let stored = create_plugin(router.clone(), TENANT, "transform", "route-transform").await;
    let plugin_id = stored["id"].as_str().expect("id").to_owned();

    let upstream = create_upstream(router.clone(), TENANT, "api.vendor.com").await;
    let mut route = route_body(
        upstream["id"].as_str().expect("id").parse().expect("uuid"),
        "/v1/**",
    );
    route["plugins"] = json!({ "items": [plugin_id] });
    let created = send(router.clone(), "POST", "/oagw/v1/routes", TENANT, &all(), route).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let route = json_body(created).await;
    let route_id = route["id"].as_str().expect("id").to_owned();

    let path = format!("/oagw/v1/plugins/{plugin_id}");
    let response = send(router, "DELETE", &path, TENANT, &all(), json!({})).await;
    let document = assert_problem(response, 409, "plugin.in_use.v1", &path).await;
    assert_eq!(
        document[EXTENSION_REFERENCED_BY],
        json!([format!("routes/{route_id}/plugins/0")]),
        "{document}"
    );
    assert_eq!(document[EXTENSION_PLUGIN_ID], json!(plugin_id));
}

#[tokio::test]
async fn an_auth_reference_blocks_the_delete_with_its_own_position() {
    let (router, _store, _registry) = mounted();

    let stored = create_plugin(router.clone(), TENANT, "auth", "referenced-auth").await;
    let plugin_id = stored["id"].as_str().expect("id").to_owned();

    let mut body = upstream_body("auth.vendor.com");
    body["auth"] = json!({ "type": plugin_id, "config": {} });
    let created = send(router.clone(), "POST", "/oagw/v1/upstreams", TENANT, &all(), body).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let upstream = json_body(created).await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();

    let path = format!("/oagw/v1/plugins/{plugin_id}");
    let response = send(router, "DELETE", &path, TENANT, &all(), json!({})).await;
    let document = assert_problem(response, 409, "plugin.in_use.v1", &path).await;
    assert_eq!(
        document[EXTENSION_REFERENCED_BY],
        json!([format!("upstreams/{upstream_id}/auth")]),
        "{document}"
    );
}

#[tokio::test]
async fn delete_requires_the_delete_permission_of_the_definition_type() {
    let (router, _store, _registry) = mounted();

    let stored = create_plugin(router.clone(), TENANT, "guard", "gated-delete").await;
    let id = stored["id"].as_str().expect("id").to_owned();
    let path = format!("/oagw/v1/plugins/{id}");

    let response = send(
        router.clone(),
        "DELETE",
        &path,
        TENANT,
        &[format!("{AUTH_BASE}~:delete")],
        json!({}),
    )
    .await;
    assert_problem(response, 403, "management.forbidden.v1", &path).await;

    let response = send(
        router,
        "DELETE",
        &path,
        TENANT,
        &[format!("{GUARD_BASE}~:delete")],
        json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_named_builtin_and_a_catalog_only_identifier_cannot_be_deleted() {
    let (router, _store, _registry) = mounted();

    for identifier in [
        format!("{GUARD_BASE}~cf.core.oagw.required_headers.v1"),
        format!("{GUARD_BASE}~cf.core.oagw.cors.v1"),
        format!("{TRANSFORM_BASE}~cf.core.oagw.metrics.v1"),
    ] {
        let path = format!("/oagw/v1/plugins/{identifier}");
        let response = send(router.clone(), "DELETE", &path, TENANT, &all(), json!({})).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{identifier} has no stored row"
        );
    }
}

// ---------- immutability ----------

#[tokio::test]
async fn a_definition_has_no_replace_operation() {
    let (router, store, _registry) = mounted();

    let stored = create_plugin(router.clone(), TENANT, "guard", "immutable").await;
    let id = stored["id"].as_str().expect("id").to_owned();
    let path = format!("/oagw/v1/plugins/{id}");

    let replacement = plugin_body("guard", "immutable");
    let response = send(router.clone(), "PUT", &path, TENANT, &all(), replacement).await;
    assert_eq!(
        response.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "no replace operation is registered for a definition"
    );
    assert_eq!(
        store.find_plugin(TENANT, &id).expect("row").source_code,
        "def apply(ctx):\n    return ctx\n",
        "the stored definition is unchanged"
    );
}

#[tokio::test]
async fn no_other_plugin_endpoint_is_registered() {
    let (_router, _store, registry) = mounted();

    let paths = registry.operation_paths();
    let plugin_paths = paths
        .iter()
        .filter(|path| path.contains("/plugins"))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(plugin_paths.len(), 5, "{plugin_paths:?}");
    for path in [
        "POST /oagw/v1/plugins",
        "GET /oagw/v1/plugins",
        "GET /oagw/v1/plugins/{id}",
        "DELETE /oagw/v1/plugins/{id}",
        "GET /oagw/v1/plugins/{id}/source",
    ] {
        assert!(plugin_paths.contains(&path.to_owned()), "{path} missing");
    }

    // The mounted router answers the unregistered verbs with the method the
    // router reports, never with a stored mutation.
    let (router, _store, _registry) = mounted();
    let response = anonymous(router, "PATCH", "/oagw/v1/plugins/x").await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

// ---------- binding validation ----------

#[tokio::test]
async fn an_unresolvable_typed_reference_is_rejected() {
    let (router, _store, _registry) = mounted();

    let unknown = format!("{GUARD_BASE}~00000000-0000-4000-8000-00000000beef");
    let mut body = upstream_body("api.vendor.com");
    body["plugins"] = json!({ "items": [unknown] });
    let response = send(router, "POST", "/oagw/v1/upstreams", TENANT, &all(), body).await;
    let document = assert_problem(response, 400, "validation.error.v1", "/oagw/v1/upstreams").await;
    assert!(
        document["detail"].as_str().expect("detail").contains("plugins.items"),
        "{document}"
    );
}

#[tokio::test]
async fn a_catalog_only_identifier_cannot_be_bound() {
    let (router, _store, _registry) = mounted();

    for (field, identifier) in [
        (
            "plugins.items",
            format!("{GUARD_BASE}~cf.core.oagw.timeout.v1"),
        ),
        (
            "plugins.items",
            format!("{TRANSFORM_BASE}~cf.core.oagw.metrics.v1"),
        ),
    ] {
        let mut body = upstream_body("api.vendor.com");
        body["plugins"] = json!({ "items": [identifier] });
        let response = send(
            router.clone(),
            "POST",
            "/oagw/v1/upstreams",
            TENANT,
            &all(),
            body,
        )
        .await;
        let document =
            assert_problem(response, 400, "validation.error.v1", "/oagw/v1/upstreams").await;
        assert!(
            document["detail"].as_str().expect("detail").contains(field),
            "{document}"
        );
    }
}

#[tokio::test]
async fn basic_and_bearer_are_unknown_auth_plugins() {
    let (router, _store, _registry) = mounted();

    for name in ["basic", "bearer"] {
        let mut body = upstream_body("api.vendor.com");
        body["auth"] = json!({
            "type": format!("{AUTH_BASE}~cf.core.oagw.{name}.v1"),
            "config": {}
        });
        let response = send(
            router.clone(),
            "POST",
            "/oagw/v1/upstreams",
            TENANT,
            &all(),
            body,
        )
        .await;
        let document =
            assert_problem(response, 400, "validation.error.v1", "/oagw/v1/upstreams").await;
        assert!(
            document["detail"]
                .as_str()
                .expect("detail")
                .contains("unknown auth plugin"),
            "{document}"
        );
    }
}

#[tokio::test]
async fn an_auth_plugin_cannot_occupy_a_pipeline_slot() {
    let (router, _store, _registry) = mounted();

    let stored = create_plugin(router.clone(), TENANT, "auth", "pipeline-auth").await;
    let plugin_id = stored["id"].as_str().expect("id").to_owned();

    let mut body = upstream_body("api.vendor.com");
    body["plugins"] = json!({ "items": [plugin_id] });
    let response = send(router, "POST", "/oagw/v1/upstreams", TENANT, &all(), body).await;
    let document = assert_problem(response, 400, "validation.error.v1", "/oagw/v1/upstreams").await;
    assert!(
        document["detail"]
            .as_str()
            .expect("detail")
            .contains("plugins.items"),
        "{document}"
    );
}

#[tokio::test]
async fn a_reference_whose_base_type_mismatches_the_definition_is_rejected() {
    let (router, _store, _registry) = mounted();

    // The definition is a transform plugin; the reference names the guard base.
    let stored = create_plugin(router.clone(), TENANT, "transform", "mislabelled").await;
    let uuid = stored["id"]
        .as_str()
        .expect("id")
        .split('~')
        .next_back()
        .expect("instance")
        .to_owned();

    let mut body = upstream_body("api.vendor.com");
    body["plugins"] = json!({ "items": [format!("{GUARD_BASE}~{uuid}")] });
    let response = send(router, "POST", "/oagw/v1/upstreams", TENANT, &all(), body).await;
    assert_problem(response, 400, "validation.error.v1", "/oagw/v1/upstreams").await;
}

#[tokio::test]
async fn a_reference_into_another_tenants_key_space_is_rejected() {
    let (router, _store, _registry) = mounted();

    let foreign = create_plugin(router.clone(), OTHER, "guard", "foreign-plugin").await;
    let plugin_id = foreign["id"].as_str().expect("id").to_owned();

    let mut body = upstream_body("api.vendor.com");
    body["plugins"] = json!({ "items": [plugin_id] });
    let response = send(router, "POST", "/oagw/v1/upstreams", TENANT, &all(), body).await;
    assert_problem(response, 400, "validation.error.v1", "/oagw/v1/upstreams").await;
}

#[tokio::test]
async fn a_bound_definition_stores_both_reference_columns() {
    let (router, store, _registry) = mounted();

    // A custom transform plugin: UUID-backed, so both columns are written.
    let stored = create_plugin(router.clone(), TENANT, "transform", "bound-transform").await;
    let plugin_id = stored["id"].as_str().expect("id").to_owned();
    let instance = plugin_id.split('~').next_back().expect("instance").to_owned();

    let mut body = upstream_body("api.vendor.com");
    body["plugins"] = json!({
        "items": [
            format!("{GUARD_BASE}~cf.core.oagw.required_headers.v1"),
            { "plugin_ref": plugin_id, "config": { "header": "X-Tenant" } }
        ]
    });
    let created = send(router.clone(), "POST", "/oagw/v1/upstreams", TENANT, &all(), body).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let upstream = json_body(created).await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();

    // The stored binding rows carry the reference and, only for the custom
    // plugin, the parsed UUID; the declared config is kept verbatim and never
    // validated against the definition's `config_schema`.
    let record = store
        .find_upstream(TENANT, upstream_id.parse().expect("uuid"))
        .expect("row");
    let items = record.plugins.as_ref().expect("plugins").items.clone();
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(items[0].position, 0);
    assert_eq!(
        items[0].reference,
        format!("{GUARD_BASE}~cf.core.oagw.required_headers.v1")
    );
    assert!(items[0].plugin_uuid.is_none(), "a named row keeps the UUID unset");
    assert_eq!(items[1].position, 1);
    assert_eq!(items[1].reference, plugin_id);
    assert_eq!(items[1].plugin_uuid, Some(instance.parse().expect("uuid")));
    assert_eq!(items[1].config, Some(json!({ "header": "X-Tenant" })));

    // The response reports the declared pipeline, not the internal columns.
    assert_eq!(upstream["plugins"]["items"][1]["plugin_ref"], json!(plugin_id));
}

#[tokio::test]
async fn a_named_guard_binding_keeps_the_uuid_column_unset() {
    let (router, store, _registry) = mounted();

    let mut body = upstream_body("api.vendor.com");
    body["plugins"] = json!({ "items": [format!("{GUARD_BASE}~cf.core.oagw.required_headers.v1")] });
    let created = send(router.clone(), "POST", "/oagw/v1/upstreams", TENANT, &all(), body).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let upstream = json_body(created).await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();

    let record = store
        .find_upstream(TENANT, upstream_id.parse().expect("uuid"))
        .expect("row");
    let items = record.plugins.as_ref().expect("plugins").items.clone();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(
        items[0].reference,
        format!("{GUARD_BASE}~cf.core.oagw.required_headers.v1"),
        "the reference is canonicalized to the full GTS identifier"
    );
    assert!(items[0].plugin_uuid.is_none(), "a named row keeps the UUID unset");
}

#[tokio::test]
async fn an_auth_definition_binds_through_the_scalar_auth_field() {
    let (router, store, _registry) = mounted();

    let stored = create_plugin(router.clone(), TENANT, "auth", "scalar-auth").await;
    let plugin_id = stored["id"].as_str().expect("id").to_owned();
    let instance = plugin_id.split('~').next_back().expect("instance").to_owned();

    let mut body = upstream_body("api.vendor.com");
    body["auth"] = json!({ "type": plugin_id, "config": { "audience": "platform" } });
    let created = send(router.clone(), "POST", "/oagw/v1/upstreams", TENANT, &all(), body).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let upstream = json_body(created).await;

    let record = store
        .find_upstream(TENANT, upstream["id"].as_str().expect("id").parse().expect("uuid"))
        .expect("row");
    assert_eq!(record.auth.as_ref().expect("auth").kind, plugin_id);
    assert_eq!(record.auth_plugin_ref.as_deref(), Some(plugin_id.as_str()));
    assert_eq!(record.auth_plugin_uuid, Some(instance.parse().expect("uuid")));
}

#[tokio::test]
async fn a_bare_uuid_reference_resolves_to_the_custom_definition() {
    let (router, _store, _registry) = mounted();

    let stored = create_plugin(router.clone(), TENANT, "guard", "bare-uuid").await;
    let uuid = stored["id"]
        .as_str()
        .expect("id")
        .split('~')
        .next_back()
        .expect("instance")
        .to_owned();

    let mut body = upstream_body("api.vendor.com");
    body["plugins"] = json!({ "items": [uuid] });
    let created = send(router.clone(), "POST", "/oagw/v1/upstreams", TENANT, &all(), body).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let upstream = json_body(created).await;
    assert_eq!(
        upstream["plugins"]["items"][0]["plugin_ref"],
        json!(format!("{CHAIN_BASE}~{uuid}")),
        "the reference is canonicalized to the anonymous GTS identifier"
    );

    let path = format!("/oagw/v1/plugins/{}", stored["id"].as_str().expect("id"));
    let response = send(router, "DELETE", &path, TENANT, &all(), json!({})).await;
    let document = assert_problem(response, 409, "plugin.in_use.v1", &path).await;
    assert_eq!(document[EXTENSION_PLUGIN_ID], stored["id"], "{document}");
}
// @cpt-end:cpt-cf-oagw-dod-plugin-test-coverage:p2:inst-full
