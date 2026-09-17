//! Integration tests of the oagw management REST API.
//!
//! The tests drive the real router (`register_routes` on top of
//! `OperationBuilder`) with `tower::ServiceExt::oneshot`, asserting the status
//! codes, the RFC 9457 problem bodies and the GTS resource ids the management
//! API is specified to produce.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::needless_pass_by_value,
    clippy::doc_markdown
)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use oagw::api::rest::routes::register_routes;
use oagw::config::OagwConfig;
use oagw::domain::hierarchy::StaticTenantHierarchy;
use oagw::domain::services::OagwService;

const UPSTREAM_PREFIX: &str = "gts.cf.core.oagw.upstream.v1~";
const ROUTE_PREFIX: &str = "gts.cf.core.oagw.route.v1~";

/// A router bound to a fresh service and the tenant of the calling context.
struct Harness {
    router: Router,
    tenant: Uuid,
    other_tenant: Uuid,
}

impl Harness {
    fn new() -> Self {
        let service = Arc::new(OagwService::new(
            OagwConfig::default(),
            Arc::new(StaticTenantHierarchy::default()),
        ));
        let router = register_routes(Router::new(), &OpenApiRegistryImpl::new(), service)
            .expect("data plane composes");
        Self {
            router,
            tenant: Uuid::new_v4(),
            other_tenant: Uuid::new_v4(),
        }
    }

    fn ctx(&self) -> SecurityContext {
        self.ctx_for(self.tenant)
    }

    fn ctx_for(&self, tenant: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(tenant)
            .build()
            .expect("context")
    }

    async fn send(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.send_as(method, uri, body, self.ctx()).await
    }

    async fn send_as(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        ctx: SecurityContext,
    ) -> (StatusCode, Value) {
        let builder = Request::builder().method(method).uri(uri);
        let request = match body {
            Some(value) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&value).expect("serialize")))
                .expect("request"),
            None => builder.body(Body::empty()).expect("request"),
        };
        let mut request = request;
        request.extensions_mut().insert(ctx);
        let response = self.router.clone().oneshot(request).await.expect("served");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let parsed = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, parsed)
    }

    async fn create_upstream(&self, alias: Option<&str>, host: &str) -> Value {
        let mut body = json!({
            "server": { "endpoints": [
                { "scheme": "http", "host": host, "port": 8080 }
            ] }
        });
        if let Some(alias) = alias {
            body["alias"] = json!(alias);
        }
        let (status, created) = self.send("POST", "/oagw/v1/upstreams", Some(body)).await;
        assert_eq!(status, StatusCode::CREATED, "create upstream: {created}");
        created
    }
}

#[tokio::test]
async fn upstreams_round_trip_through_the_rest_api() {
    let harness = Harness::new();
    let (status, created, location) = {
        let mut request = Request::builder()
            .method("POST")
            .uri("/oagw/v1/upstreams")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "tags": ["llm"],
                    "server": { "endpoints": [
                        { "scheme": "https", "host": "api.openai.com" }
                    ] }
                }))
                .expect("serialize"),
            ))
            .expect("request");
        request.extensions_mut().insert(harness.ctx());
        let response = harness
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("served");
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let created: Value = serde_json::from_slice(&bytes).expect("created");
        (status, created, location)
    };
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().expect("id").to_owned();
    assert!(id.starts_with(UPSTREAM_PREFIX), "unexpected id {id}");
    assert_eq!(created["alias"], "api.openai.com");
    assert_eq!(created["enabled"], true);
    assert_eq!(created["server"]["endpoints"][0]["host"], "api.openai.com");
    assert_eq!(location, format!("/oagw/v1/upstreams/{id}"));

    let (status, fetched) = harness
        .send("GET", &format!("/oagw/v1/upstreams/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["id"], json!(id));
    assert_eq!(fetched["alias"], "api.openai.com");
    assert_eq!(fetched["tags"], json!(["llm"]));

    // Replacement clears omitted optional fields and keeps the identity.
    let (status, replaced) = harness
        .send(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "alias": "api.openai.com",
                "server": { "endpoints": [
                    { "scheme": "https", "host": "api.openai.com" }
                ] }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "replace: {replaced}");
    assert_eq!(replaced["id"], json!(id));
    assert!(replaced["tags"].as_array().expect("tags").is_empty());

    let (status, _) = harness
        .send("DELETE", &format!("/oagw/v1/upstreams/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, problem) = harness
        .send("GET", &format!("/oagw/v1/upstreams/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        problem_type(&problem),
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1"
    );
}

/// Convenience reader of the `type` field of a problem body.
fn problem_type(problem: &Value) -> String {
    problem["type"].as_str().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn validation_errors_are_bad_requests() {
    let harness = Harness::new();
    let (status, problem) = harness
        .send(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "server": { "endpoints": [
                    { "scheme": "http", "host": "10.0.0.1", "port": 8080 }
                ] }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(&problem),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert_eq!(problem["error_source"], "gateway");

    // Server managed fields are refused, not silently dropped.
    let (status, problem) = harness
        .send(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "id": "cf.core.oagw.upstream.v1~abc",
                "server": { "endpoints": [
                    { "scheme": "https", "host": "api.openai.com" }
                ] }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(problem["detail"].as_str().expect("detail").contains("id"));

    // A malformed body is a client error as well.
    let (status, problem) = harness
        .send(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({ "nonsense": true })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
}

#[tokio::test]
async fn alias_conflicts_are_conflicts() {
    let harness = Harness::new();
    harness
        .create_upstream(Some("api.openai.com"), "10.0.0.1")
        .await;
    let (status, problem) = harness
        .send(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "api.openai.com",
                "server": { "endpoints": [
                    { "scheme": "http", "host": "10.0.0.9", "port": 80 }
                ] }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        problem_type(&problem),
        "gts.cf.core.errors.err.v1~cf.oagw.conflict.v1"
    );
}

#[tokio::test]
async fn reads_are_scoped_to_the_calling_tenant() {
    let harness = Harness::new();
    let created = harness
        .create_upstream(Some("api.openai.com"), "10.0.0.1")
        .await;
    let id = created["id"].as_str().expect("id").to_owned();

    let (status, problem) = harness
        .send_as(
            "GET",
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            harness.ctx_for(harness.other_tenant),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        problem_type(&problem),
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1"
    );

    let (status, body) = harness
        .send_as(
            "GET",
            "/oagw/v1/upstreams",
            None,
            harness.ctx_for(harness.other_tenant),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["items"].as_array().expect("items").is_empty());

    let (status, _) = harness
        .send_as(
            "DELETE",
            &format!("/oagw/v1/upstreams/{id}"),
            None,
            harness.ctx_for(harness.other_tenant),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_upstreams_honour_the_odata_parameters() {
    let harness = Harness::new();
    for alias in ["zeta.example.com", "alpha.example.com", "mid.example.com"] {
        harness.create_upstream(Some(alias), "10.0.0.1").await;
    }

    let (status, page) = harness.send("GET", "/oagw/v1/upstreams", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"].as_array().expect("items").len(), 3);
    assert_eq!(page["page_info"]["limit"], 50);
    assert_eq!(page["page_info"]["next_cursor"], Value::Null);
    assert_eq!(page["page_info"]["prev_cursor"], Value::Null);

    let (status, page) = harness
        .send(
            "GET",
            "/oagw/v1/upstreams?$filter=alias%20eq%20'mid.example.com'",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let items = page["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["alias"], "mid.example.com");

    let (status, page) = harness
        .send("GET", "/oagw/v1/upstreams?$orderby=alias%20desc", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let aliases: Vec<&str> = page["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["alias"].as_str().expect("alias"))
        .collect();
    assert_eq!(
        aliases,
        ["zeta.example.com", "mid.example.com", "alpha.example.com"]
    );

    let (status, page) = harness
        .send(
            "GET",
            "/oagw/v1/upstreams?$select=alias&$top=2&$skip=1",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let items = page["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0], json!({ "alias": "mid.example.com" }));
    assert!(items[0].get("id").is_none());

    let (status, problem) = harness
        .send("GET", "/oagw/v1/upstreams?$unknown=1", None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        problem_type(&problem),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );

    let (status, _) = harness
        .send("GET", "/oagw/v1/upstreams?$top=not-a-number", None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn routes_round_trip_through_the_rest_api() {
    let harness = Harness::new();
    let upstream = harness
        .create_upstream(Some("api.openai.com"), "10.0.0.1")
        .await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();

    let (status, created) = harness
        .send(
            "POST",
            "/oagw/v1/routes",
            Some(json!({
                "upstream_id": upstream_id,
                "tags": ["chat"],
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let route_id = created["id"].as_str().expect("id").to_owned();
    assert!(
        route_id.starts_with(ROUTE_PREFIX),
        "unexpected id {route_id}"
    );
    assert_eq!(created["upstream_id"], json!(upstream_id));
    assert_eq!(created["match"]["http"]["path"], "/v1");

    let (status, fetched) = harness
        .send("GET", &format!("/oagw/v1/routes/{route_id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["upstream_id"], json!(upstream_id));

    let (status, page) = harness
        .send("GET", "/oagw/v1/routes?$filter=tags%20eq%20'chat'", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"].as_array().expect("items").len(), 1);

    let (status, replaced) = harness
        .send(
            "PUT",
            &format!("/oagw/v1/routes/{route_id}"),
            Some(json!({
                "tags": ["chat"],
                "match": { "http": { "methods": ["POST"], "path": "/v1" } }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(replaced["upstream_id"], json!(upstream_id));
    assert_eq!(replaced["match"]["http"]["methods"], json!(["POST"]));

    let (status, _) = harness
        .send("DELETE", &format!("/oagw/v1/routes/{route_id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = harness
        .send("GET", &format!("/oagw/v1/routes/{route_id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn route_conflicts_and_unknown_upstreams_are_reported() {
    let harness = Harness::new();
    let upstream = harness
        .create_upstream(Some("api.openai.com"), "10.0.0.1")
        .await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();

    let body = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": "/v1" } }
    });
    let (status, _) = harness
        .send("POST", "/oagw/v1/routes", Some(body.clone()))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, problem) = harness.send("POST", "/oagw/v1/routes", Some(body)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        problem_type(&problem),
        "gts.cf.core.errors.err.v1~cf.oagw.conflict.v1"
    );

    let (status, problem) = harness
        .send(
            "POST",
            "/oagw/v1/routes",
            Some(json!({
                "upstream_id": format!("{UPSTREAM_PREFIX}{}", Uuid::new_v4()),
                "match": { "http": { "methods": ["GET"], "path": "/v2" } }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        problem_type(&problem),
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1"
    );

    // A malformed match rule is a client error.
    let (status, _) = harness
        .send(
            "POST",
            "/oagw/v1/routes",
            Some(json!({
                "upstream_id": upstream_id,
                "match": { "http": { "methods": [], "path": "/v3" } }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn deleting_an_upstream_cascades_its_routes() {
    let harness = Harness::new();
    let upstream = harness
        .create_upstream(Some("api.openai.com"), "10.0.0.1")
        .await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();
    let (status, created) = harness
        .send(
            "POST",
            "/oagw/v1/routes",
            Some(json!({
                "upstream_id": upstream_id,
                "match": { "http": { "methods": ["GET"], "path": "/v1" } }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let route_id = created["id"].as_str().expect("id").to_owned();

    let (status, _) = harness
        .send("DELETE", &format!("/oagw/v1/upstreams/{upstream_id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = harness
        .send("GET", &format!("/oagw/v1/routes/{route_id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn plugins_cannot_be_deleted_while_bound() {
    let harness = Harness::new();
    let (status, plugin) = harness
        .send(
            "POST",
            "/oagw/v1/plugins",
            Some(json!({
                "plugin_type": "guard_plugin",
                "name": "tenant-guard",
                "source_code": "def on_request(ctx): pass"
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{plugin}");
    let plugin_id = plugin["id"].as_str().expect("id").to_owned();

    let (status, source) = harness
        .send("GET", &format!("/oagw/v1/plugins/{plugin_id}/source"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source["plugin_type"], "guard_plugin");
    assert_eq!(source["source_code"], "def on_request(ctx): pass");

    // Bindings may carry the bare instance id of the plugin.
    let instance = plugin_id.rsplit('~').next().expect("instance").to_owned();
    let reference = format!("gts.cf.core.oagw.guard_plugin.v1~{instance}");
    let (status, upstream) = harness
        .send(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "api.openai.com",
                "plugins": { "sharing": "private", "items": [reference] },
                "server": { "endpoints": [
                    { "scheme": "http", "host": "10.0.0.1", "port": 8080 }
                ] }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{upstream}");
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();

    let (status, problem) = harness
        .send("DELETE", &format!("/oagw/v1/plugins/{plugin_id}"), None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        problem_type(&problem),
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
    );

    let (status, _) = harness
        .send("DELETE", &format!("/oagw/v1/upstreams/{upstream_id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = harness
        .send("DELETE", &format!("/oagw/v1/plugins/{plugin_id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn every_management_route_is_declared_in_the_openapi_document() {
    let harness = Harness::new();
    // A method a path does not serve is a 405, so the probe below asserts the
    // router knows every declared path under the served method.
    let (status, _) = harness.send("PATCH", "/oagw/v1/upstreams", None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);

    for (method, path) in [
        ("POST", "/oagw/v1/upstreams"),
        ("GET", "/oagw/v1/upstreams"),
        ("GET", "/oagw/v1/upstreams/unknown"),
        ("PUT", "/oagw/v1/upstreams/unknown"),
        ("DELETE", "/oagw/v1/upstreams/unknown"),
        ("POST", "/oagw/v1/routes"),
        ("GET", "/oagw/v1/routes"),
        ("GET", "/oagw/v1/routes/unknown"),
        ("PUT", "/oagw/v1/routes/unknown"),
        ("DELETE", "/oagw/v1/routes/unknown"),
        ("POST", "/oagw/v1/plugins"),
        ("GET", "/oagw/v1/plugins"),
        ("GET", "/oagw/v1/plugins/unknown"),
        ("DELETE", "/oagw/v1/plugins/unknown"),
        ("GET", "/oagw/v1/plugins/unknown/source"),
    ] {
        let (status, _) = harness.send(method, path, None).await;
        assert_ne!(status, StatusCode::NOT_IMPLEMENTED, "{method} {path}");
        assert_ne!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "router must know {method} {path}"
        );
    }
}
