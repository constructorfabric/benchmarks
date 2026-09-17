//! Integration tests for the management REST surface.
//!
//! The tests register the gear's routes exactly as the gear does — through
//! [`oagw::api::rest::routes::register_routes`] — and drive the resulting
//! `axum::Router` with `tower::ServiceExt::oneshot`, so status codes, the
//! `application/problem+json` bodies and the route wiring are verified over
//! HTTP rather than through handler signatures alone.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use toolkit::api::OpenApiRegistry;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::ControlPlane;
use oagw::api::rest::error::{ALREADY_EXISTS_TYPE, NOT_FOUND_TYPE, VALIDATION_TYPE};
use oagw::api::rest::handlers;
use oagw::api::rest::routes::register_routes;
use oagw::config::OagwConfig;

// ── Noop OpenAPI registry ───────────────────────────────────────────────

/// Accepts every registration; these tests only care about the router.
struct NoopRegistry;

impl OpenApiRegistry for NoopRegistry {
    fn register_operation(&self, _spec: &toolkit::api::operation_builder::OperationSpec) {}

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

// ── Harness ─────────────────────────────────────────────────────────────

/// A router scoped to one tenant, as the hosting server would build it.
struct Fixture {
    router: Router,
}

fn fixture() -> Fixture {
    let context = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_type("user")
        .subject_tenant_id(Uuid::new_v4())
        .build()
        .expect("security context");
    let service: Arc<ControlPlane> = control_plane();
    let router =
        register_routes(Router::new(), &NoopRegistry, service).layer(axum::Extension(context));
    Fixture { router }
}

/// The service the gear builds from its configuration section.
fn control_plane() -> Arc<ControlPlane> {
    handlers::control_plane(&OagwConfig::default())
}

/// Send a request and return `(status, content-type, body)`.
async fn send(router: Router, request: Request<Body>) -> (StatusCode, String, Value) {
    let response = router.oneshot(request).await.expect("infallible service");
    let status = response.status();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, content_type, body)
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .expect("request")
}

fn post(path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

fn put(path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

fn delete(path: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(path)
        .body(Body::empty())
        .expect("request")
}

fn upstream_body(host: &str) -> Value {
    json!({
        "server": { "endpoints": [{ "scheme": "https", "host": host, "port": 443 }] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    })
}

fn route_body(upstream_id: &str, path: &str) -> Value {
    json!({
        "upstream_id": upstream_id,
        "match": {
            "http": { "methods": ["GET"], "path": path, "path_suffix_mode": "append" }
        },
    })
}

async fn create_upstream(router: &Router, host: &str) -> Value {
    let (status, _, created) = send(
        router.clone(),
        post("/oagw/v1/upstreams", upstream_body(host)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "created: {created}");
    created
}

// ── Tests ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn upstreams_round_trip_over_http() {
    let fx = fixture();
    let (status, content_type, created) = send(
        fx.router.clone(),
        post("/oagw/v1/upstreams", upstream_body("api.OpenAI.com")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(content_type.starts_with("application/json"));
    assert_eq!(created["alias"], "api.openai.com");
    assert!(created["id"].as_str().is_some());

    let (status, _, listed) = send(fx.router.clone(), get("/oagw/v1/upstreams")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["total"], 1);
    assert_eq!(listed["top"], 50, "the default page size applies");
    assert_eq!(listed["items"][0]["alias"], "api.openai.com");

    // Both id spellings resolve: the bare UUID and the anonymous GTS id.
    let id = created["id"].as_str().expect("id").to_owned();
    let gts_id = format!("gts.cf.core.oagw.upstream.v1~{id}");
    for path in [
        format!("/oagw/v1/upstreams/{id}"),
        format!("/oagw/v1/upstreams/{gts_id}"),
    ] {
        let (status, _, fetched) = send(fx.router.clone(), get(&path)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(fetched["id"], created["id"]);
    }

    // A full replacement keeps the identity and the derived alias.
    let mut replacement = upstream_body("api.openai.com");
    replacement["enabled"] = json!(false);
    let (status, _, replaced) = send(
        fx.router.clone(),
        put(&format!("/oagw/v1/upstreams/{id}"), replacement),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replaced["enabled"], json!(false));
    assert_eq!(replaced["alias"], "api.openai.com");

    let (status, _, _) = send(
        fx.router.clone(),
        delete(&format!("/oagw/v1/upstreams/{id}")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _, problem) =
        send(fx.router.clone(), get(&format!("/oagw/v1/upstreams/{id}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(problem["type"], NOT_FOUND_TYPE);
}

#[tokio::test]
async fn alias_conflicts_and_immutability_are_problem_documents() {
    let fx = fixture();
    let first = send(
        fx.router.clone(),
        post("/oagw/v1/upstreams", upstream_body("api.example.com")),
    )
    .await
    .2;
    let id = first["id"].as_str().expect("id").to_owned();

    // A second upstream with the same derived alias is a 409.
    let (status, content_type, problem) = send(
        fx.router.clone(),
        post("/oagw/v1/upstreams", upstream_body("api.example.com")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(content_type, "application/problem+json");
    assert_eq!(problem["type"], ALREADY_EXISTS_TYPE);
    assert_eq!(problem["context"]["resource_name"], "api.example.com");

    // The alias of a hostname upstream is derived, not chosen.
    let mut override_request = upstream_body("api.openai.com");
    override_request["alias"] = json!("something.else");
    let (status, _, problem) = send(
        fx.router.clone(),
        post("/oagw/v1/upstreams", override_request),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["type"], VALIDATION_TYPE);

    // `id` is immutable on PUT.
    let mut renamed = upstream_body("api.openai.com");
    renamed["id"] = json!(Uuid::new_v4().to_string());
    let (status, _, problem) = send(
        fx.router.clone(),
        put(&format!("/oagw/v1/upstreams/{id}"), renamed),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["context"]["reason"], "immutable_field");
}

#[tokio::test]
async fn routes_are_reachable_and_tenant_scoped() {
    let fx = fixture();
    let created = create_upstream(&fx.router, "api.example.com").await;
    let upstream_id = created["id"].as_str().expect("id").to_owned();

    let (status, _, route) = send(
        fx.router.clone(),
        post("/oagw/v1/routes", route_body(&upstream_id, "/v1")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{route}");
    assert_eq!(route["upstream_id"], created["id"]);
    assert_eq!(route["match"]["http"]["path"], "/v1");

    // A route pointing at an upstream of another tenant is not reachable.
    let (status, _, problem) = send(
        fx.router.clone(),
        post(
            "/oagw/v1/routes",
            route_body(&Uuid::new_v4().to_string(), "/v2"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(problem["type"], NOT_FOUND_TYPE);

    // Another tenant sees neither resource.
    let other = fixture();
    let (status, _, page) = send(other.router.clone(), get("/oagw/v1/upstreams")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["total"], 0);
    let (status, _, _) = send(
        other.router.clone(),
        get(&format!(
            "/oagw/v1/routes/{}",
            route["id"].as_str().expect("id")
        )),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn plugins_expose_their_source_and_their_name_uniqueness() {
    let fx = fixture();
    let plugin_body = json!({
        "name": "signer",
        "type": "guard",
        "source": "def plugin(ctx): pass",
    });
    let (status, _, plugin) = send(
        fx.router.clone(),
        post("/oagw/v1/plugins", plugin_body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{plugin}");
    let id = plugin["id"].as_str().expect("id").to_owned();

    let (status, _, source) = send(
        fx.router.clone(),
        get(&format!("/oagw/v1/plugins/{id}/source")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source["source"], "def plugin(ctx): pass");

    // The name is unique per tenant.
    let (status, _, _) = send(fx.router.clone(), post("/oagw/v1/plugins", plugin_body)).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // An unknown plugin is a 404 problem document.
    let (status, _, _) = send(
        fx.router.clone(),
        get(&format!("/oagw/v1/plugins/{}/source", Uuid::new_v4())),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _, _) = send(fx.router.clone(), delete(&format!("/oagw/v1/plugins/{id}"))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn list_parameters_are_validated_and_applied() {
    let fx = fixture();
    for host in ["a.example.com", "b.example.com", "c.example.com"] {
        create_upstream(&fx.router, host).await;
    }

    let (status, _, page) = send(
        fx.router.clone(),
        get("/oagw/v1/upstreams?$orderby=alias%20desc&$top=2&$skip=1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let aliases: Vec<&str> = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["alias"].as_str().expect("alias"))
        .collect();
    assert_eq!(aliases, ["b.example.com", "a.example.com"]);

    let (status, _, page) = send(
        fx.router.clone(),
        get("/oagw/v1/upstreams?$select=id,alias"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["total"], 3);
    let item = &page["items"][0];
    assert!(item.get("alias").is_some());
    assert!(item.get("server").is_none(), "unselected fields disappear");

    let (status, _, problem) = send(
        fx.router.clone(),
        get("/oagw/v1/upstreams?$top=5&frobnicate=1"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["type"], VALIDATION_TYPE);

    let (status, _, _) = send(fx.router.clone(), get("/oagw/v1/upstreams?$top=1000")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "$top above the maximum");
}

#[tokio::test]
async fn endpoint_pools_are_addressable_as_sub_resources() {
    let fx = fixture();
    let created = create_upstream(&fx.router, "api.example.com").await;
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, _, pool) = send(
        fx.router.clone(),
        get(&format!("/oagw/v1/upstreams/{id}/endpoints")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pool["endpoints"].as_array().map(Vec::len), Some(1));

    // The pool cannot become empty.
    let (status, _, problem) = send(
        fx.router.clone(),
        delete(&format!("/oagw/v1/upstreams/{id}/endpoints/0")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["type"], VALIDATION_TYPE);

    let (status, _, _) = send(
        fx.router.clone(),
        get(&format!("/oagw/v1/upstreams/{}/endpoints", Uuid::new_v4())),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn plugin_chains_bind_through_sub_resources() {
    let fx = fixture();
    let created = create_upstream(&fx.router, "api.example.com").await;
    let id = created["id"].as_str().expect("id").to_owned();

    let chain = json!({
        "sharing": "inherit",
        "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"],
    });
    let (status, _, bound) = send(
        fx.router.clone(),
        post(&format!("/oagw/v1/upstreams/{id}/plugins"), chain),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{bound}");
    // The chain endpoint answers with the updated upstream, whose `plugins`
    // block lists the canonical GTS ids.
    assert_eq!(
        bound["plugins"]["items"][0],
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
    );

    // A reference that resolves to nothing is a 400 validation problem.
    let (status, _, problem) = send(
        fx.router.clone(),
        post(
            &format!("/oagw/v1/upstreams/{id}/plugins"),
            json!({ "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1"] }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(problem["type"], VALIDATION_TYPE);
}

#[tokio::test]
async fn a_malformed_id_is_a_validation_problem() {
    let fx = fixture();
    let (status, content_type, problem) =
        send(fx.router.clone(), get("/oagw/v1/upstreams/not-a-uuid")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(content_type, "application/problem+json");
    assert_eq!(problem["type"], VALIDATION_TYPE);
}

#[tokio::test]
async fn a_malformed_body_is_a_validation_problem() {
    let fx = fixture();
    let response = fx
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/oagw/v1/upstreams")
                .header("content-type", "application/json")
                .body(Body::from("{ not json"))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
