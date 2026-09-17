//! Router-level tests of the `/oagw/v1` management surface.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use toolkit::api::{OpenApiInfo, OpenApiRegistryImpl};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use crate::api::routes::register_routes;
use crate::controlplane::service::ControlPlaneService;
use crate::controlplane::store::ControlPlaneStore;
use crate::domain::plugin::{AUTH_NOOP, GUARD_REQUIRED_HEADERS, TRANSFORM_REQUEST_ID};

/// Fixed tenant of every request in this module.
fn tenant_id() -> Uuid {
    Uuid::from_u128(0x0A6D)
}

/// A `SecurityContext` bound to [`tenant_id`], as the auth middleware would
/// inject it.
fn context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(0x5EB))
        .subject_tenant_id(tenant_id())
        .build()
        .unwrap()
}

/// A router, its OpenAPI registry and the shared control-plane service.
struct Harness {
    router: Router,
    openapi: OpenApiRegistryImpl,
    svc: Arc<ControlPlaneService>,
}

fn harness() -> Harness {
    let svc = Arc::new(ControlPlaneService::new(Arc::new(ControlPlaneStore::new())));
    let openapi = OpenApiRegistryImpl::new();
    let router = register_routes(Router::new(), &openapi, Arc::clone(&svc));
    Harness {
        router,
        openapi,
        svc,
    }
}

/// Builds a JSON request carrying the tenant's `SecurityContext` extension.
fn request(method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body = match body {
        Some(value) => Body::from(serde_json::to_vec(&value).unwrap()),
        None => Body::empty(),
    };
    let mut request = builder.body(body).unwrap();
    request.extensions_mut().insert(context());
    request
}

/// Deserializes a JSON body (or `Value::Null` for an empty one).
async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// The `code` field of a problem body.
fn code_of(body: &Value) -> Option<&str> {
    body.get("code").and_then(Value::as_str)
}

/// A minimal valid upstream body: one hostname endpoint, HTTP protocol.
///
/// An explicit `alias` is only legal for an IP-based endpoint pool
/// (`docs/PRD.md` "Configure Upstream": hostname pools always auto-derive).
fn upstream_body(alias: Option<&str>) -> Value {
    let mut body = match alias {
        None => json!({
            "server": { "endpoints": [ { "host": "api.example.com" } ] },
            "protocol": "http",
        }),
        Some(alias) => json!({
            "server": { "endpoints": [ { "scheme": "http", "host": "10.0.0.7", "port": 8080 } ] },
            "protocol": "http",
            "alias": alias,
        }),
    };
    if let Some(alias) = alias {
        body["alias"] = Value::String(alias.to_owned());
    }
    body
}

/// A minimal valid HTTP route body for `upstream_id`.
fn route_body(upstream_id: Uuid) -> Value {
    json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
    })
}

/// Creates one upstream through the REST surface and returns its id.
async fn seed_upstream(h: &Harness) -> Uuid {
    let response = h
        .router
        .clone()
        .oneshot(request(
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body(None)),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

// ── upstreams ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_upstream_returns_201_with_location_and_body() {
    let h = harness();
    let response = h
        .router
        .oneshot(request(
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body(Some("openai.example.com"))),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let location = response
        .headers()
        .get("location")
        .expect("Location header")
        .to_str()
        .unwrap()
        .to_owned();
    let body = body_json(response).await;
    assert_eq!(body["alias"], "openai.example.com");
    assert_eq!(body["enabled"], true);
    assert_eq!(body["protocol"], "http");
    assert_eq!(body["tenant_id"], tenant_id().to_string());
    let created = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    assert_eq!(
        location,
        format!("/oagw/v1/upstreams/{}", created.as_simple())
    );
}

#[tokio::test]
async fn create_upstream_derives_the_alias_from_a_hostname_pool() {
    let h = harness();
    let response = h
        .router
        .oneshot(request(
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body(None)),
        ))
        .await
        .unwrap();
    let body = body_json(response).await;
    assert_eq!(body["alias"], "api.example.com");
}

#[tokio::test]
async fn create_upstream_rejects_a_duplicate_alias_with_409() {
    let h = harness();
    let first = h
        .router
        .clone()
        .oneshot(request(
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body(None)),
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let second = h
        .router
        .oneshot(request(
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body(None)),
        ))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CONFLICT);
    assert_eq!(
        second.headers().get("x-oagw-error-source").unwrap(),
        "gateway"
    );
    assert_eq!(
        second.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    let body = body_json(second).await;
    assert_eq!(body["status"], 409);
    assert_eq!(code_of(&body), Some("AliasConflict"));
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.alias_conflict.v1"
    );
}

#[tokio::test]
async fn create_upstream_rejects_an_invalid_payload_with_400() {
    let h = harness();
    let response = h
        .router
        .oneshot(request(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({"protocol": "http"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(code_of(&body), Some("ValidationError"));
}

#[tokio::test]
async fn list_get_and_replace_an_upstream() {
    let h = harness();
    let id = seed_upstream(&h).await;

    let list = h
        .router
        .clone()
        .oneshot(request("GET", "/oagw/v1/upstreams", None))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let items = body_json(list).await;
    assert_eq!(items.as_array().map(Vec::len), Some(1));

    let get = h
        .router
        .clone()
        .oneshot(request("GET", &format!("/oagw/v1/upstreams/{id}"), None))
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::OK);
    assert_eq!(body_json(get).await["id"], id.to_string());

    let replaced = h
        .router
        .clone()
        .oneshot(request(
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(json!({
                "server": { "endpoints": [ { "host": "renamed.example.com" } ] },
                "protocol": "http",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(replaced.status(), StatusCode::OK);
    let body = body_json(replaced).await;
    assert_eq!(body["alias"], "renamed.example.com");
    assert_eq!(body["id"], id.to_string());
}

#[tokio::test]
async fn list_upstreams_applies_odata_parameters() {
    let h = harness();
    seed_upstream(&h).await;
    let response = h
        .router
        .oneshot(request(
            "GET",
            "/oagw/v1/upstreams?$filter=alias%20eq%20'api.example.com'&$select=id,alias",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let items = body_json(response).await;
    assert_eq!(items.as_array().map(Vec::len), Some(1));
    assert_eq!(items[0]["alias"], "api.example.com");
    assert!(
        items[0].get("protocol").is_none(),
        "$select drops the other fields"
    );
}

#[tokio::test]
async fn get_upstream_of_another_tenant_is_404() {
    let h = harness();
    let id = seed_upstream(&h).await;

    let mut foreign = request("GET", &format!("/oagw/v1/upstreams/{id}"), None);
    foreign.extensions_mut().insert(
        SecurityContext::builder()
            .subject_id(Uuid::from_u128(0x5EB))
            .subject_tenant_id(Uuid::from_u128(0x0A6E))
            .build()
            .unwrap(),
    );
    let response = h.router.oneshot(foreign).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(code_of(&body), Some("RouteNotFound"));
}

#[tokio::test]
async fn delete_upstream_returns_204_then_404() {
    let h = harness();
    let id = seed_upstream(&h).await;

    let deleted = h
        .router
        .clone()
        .oneshot(request("DELETE", &format!("/oagw/v1/upstreams/{id}"), None))
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    let gone = h
        .router
        .oneshot(request("DELETE", &format!("/oagw/v1/upstreams/{id}"), None))
        .await
        .unwrap();
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_upstream_referenced_by_a_route_is_409() {
    let h = harness();
    let upstream_id = seed_upstream(&h).await;
    let created = h
        .router
        .clone()
        .oneshot(request(
            "POST",
            "/oagw/v1/routes",
            Some(route_body(upstream_id)),
        ))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);

    let response = h
        .router
        .oneshot(request(
            "DELETE",
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(code_of(&body_json(response).await).is_some());
}

// ── routes ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_route_returns_201_and_lists_in_registration_order() {
    let h = harness();
    let upstream_id = seed_upstream(&h).await;

    let first = h
        .router
        .clone()
        .oneshot(request(
            "POST",
            "/oagw/v1/routes",
            Some(route_body(upstream_id)),
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_body = body_json(first).await;
    assert_eq!(first_body["upstream_id"], upstream_id.to_string());
    assert_eq!(first_body["enabled"], true);
    assert_eq!(first_body["match"]["http"]["path"], "/v1/chat");

    let second = h
        .router
        .clone()
        .oneshot(request(
            "POST",
            "/oagw/v1/routes",
            Some(json!({
                "upstream_id": upstream_id,
                "match": { "http": { "methods": ["POST"], "path": "/v1/complete" } },
            })),
        ))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CREATED);

    let list = h
        .router
        .clone()
        .oneshot(request("GET", "/oagw/v1/routes", None))
        .await
        .unwrap();
    let items = body_json(list).await;
    let paths: Vec<&str> = items
        .as_array()
        .unwrap()
        .iter()
        .map(|route| route["match"]["http"]["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["/v1/chat", "/v1/complete"]);
}

#[tokio::test]
async fn duplicate_route_match_rule_is_409() {
    let h = harness();
    let upstream_id = seed_upstream(&h).await;
    let create = |path: &'static str, methods: &[&str]| {
        let body = json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": methods, "path": path } },
        });
        h.router
            .clone()
            .oneshot(request("POST", "/oagw/v1/routes", Some(body)))
    };
    assert_eq!(
        create("/v1/chat", &["GET", "POST"]).await.unwrap().status(),
        StatusCode::CREATED
    );
    let duplicate = create("/v1/chat", &["POST", "GET"]).await.unwrap();
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
    assert_eq!(code_of(&body_json(duplicate).await), Some("Conflict"));
}

#[tokio::test]
async fn route_for_an_unknown_upstream_is_404() {
    let h = harness();
    let response = h
        .router
        .oneshot(request(
            "POST",
            "/oagw/v1/routes",
            Some(route_body(Uuid::new_v4())),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(code_of(&body_json(response).await).is_some());
}

#[tokio::test]
async fn replace_route_keeps_the_upstream_immutable() {
    let h = harness();
    let upstream_id = seed_upstream(&h).await;
    let created = h
        .router
        .clone()
        .oneshot(request(
            "POST",
            "/oagw/v1/routes",
            Some(route_body(upstream_id)),
        ))
        .await
        .unwrap();
    let route_id: Uuid = Uuid::parse_str(body_json(created).await["id"].as_str().unwrap()).unwrap();

    let replaced = h
        .router
        .clone()
        .oneshot(request(
            "PUT",
            &format!("/oagw/v1/routes/{route_id}"),
            Some(json!({
                "match": { "http": { "methods": ["GET"], "path": "/v2/chat" } },
                "enabled": false,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(replaced.status(), StatusCode::OK);
    let body = body_json(replaced).await;
    assert_eq!(body["match"]["http"]["path"], "/v2/chat");
    assert_eq!(body["enabled"], false);
    assert_eq!(body["upstream_id"], upstream_id.to_string());
}

#[tokio::test]
async fn get_replace_and_delete_a_route() {
    let h = harness();
    let upstream_id = seed_upstream(&h).await;
    let created = h
        .router
        .clone()
        .oneshot(request(
            "POST",
            "/oagw/v1/routes",
            Some(route_body(upstream_id)),
        ))
        .await
        .unwrap();
    let route_id: Uuid = Uuid::parse_str(body_json(created).await["id"].as_str().unwrap()).unwrap();

    let get = h
        .router
        .clone()
        .oneshot(request("GET", &format!("/oagw/v1/routes/{route_id}"), None))
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::OK);

    let deleted = h
        .router
        .clone()
        .oneshot(request(
            "DELETE",
            &format!("/oagw/v1/routes/{route_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    let gone = h
        .router
        .oneshot(request("GET", &format!("/oagw/v1/routes/{route_id}"), None))
        .await
        .unwrap();
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_route_frees_the_upstream_for_deletion() {
    let h = harness();
    let upstream_id = seed_upstream(&h).await;
    let created = h
        .router
        .clone()
        .oneshot(request(
            "POST",
            "/oagw/v1/routes",
            Some(route_body(upstream_id)),
        ))
        .await
        .unwrap();
    let route_id: Uuid = Uuid::parse_str(body_json(created).await["id"].as_str().unwrap()).unwrap();

    let deleted = h
        .router
        .clone()
        .oneshot(request(
            "DELETE",
            &format!("/oagw/v1/routes/{route_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    let removed = h
        .router
        .oneshot(request(
            "DELETE",
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
}

// ── plugins ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn register_list_get_and_delete_a_custom_plugin() {
    let h = harness();
    let registered = h
        .router
        .clone()
        .oneshot(request(
            "POST",
            "/oagw/v1/plugins",
            Some(json!({"kind": "guard", "name": "ip-allow", "source": "def apply(ctx): pass"})),
        ))
        .await
        .unwrap();
    assert_eq!(registered.status(), StatusCode::CREATED);
    let body = body_json(registered).await;
    assert_eq!(body["builtin"], false);
    assert_eq!(body["kind"], "guard");
    assert!(body["bindable"].as_bool().unwrap());
    assert_eq!(body["type"], "gts.cf.core.oagw.guard_plugin.v1~");
    // `DESIGN.md` §3.1 "Custom plugins": the API form is `{plugin_type}{uuid}`,
    // and that instance id is accepted by every `{id}` sub-resource.
    let instance_id = body["id"].as_str().unwrap().to_owned();

    let list = h
        .router
        .clone()
        .oneshot(request("GET", "/oagw/v1/plugins", None))
        .await
        .unwrap();
    let items = body_json(list).await;
    let array = items.as_array().unwrap();
    assert!(array.iter().any(|plugin| plugin["builtin"] == false));
    assert!(array.iter().any(|plugin| plugin["id"] == AUTH_NOOP));

    let get = h
        .router
        .clone()
        .oneshot(request(
            "GET",
            &format!("/oagw/v1/plugins/{instance_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::OK);
    assert_eq!(body_json(get).await["name"], "ip-allow");

    let source = h
        .router
        .clone()
        .oneshot(request(
            "GET",
            &format!("/oagw/v1/plugins/{instance_id}/source"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(source.status(), StatusCode::OK);
    assert_eq!(
        body_json(source).await["source_code"],
        "def apply(ctx): pass"
    );

    let deleted = h
        .router
        .oneshot(request(
            "DELETE",
            &format!("/oagw/v1/plugins/{instance_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn duplicate_plugin_name_is_409() {
    let h = harness();
    let plugin = json!({"kind": "guard", "name": "ip-allow", "source": "def apply(ctx): pass"});
    let first = h
        .router
        .clone()
        .oneshot(request("POST", "/oagw/v1/plugins", Some(plugin.clone())))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let second = h
        .router
        .oneshot(request("POST", "/oagw/v1/plugins", Some(plugin)))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn deleting_a_plugin_in_use_is_409() {
    let h = harness();
    let registered = h
        .router
        .clone()
        .oneshot(request(
            "POST",
            "/oagw/v1/plugins",
            Some(json!({"kind": "guard", "name": "ip-allow", "source": "def apply(ctx): pass"})),
        ))
        .await
        .unwrap();
    let instance_id = body_json(registered).await["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let created = h
        .router
        .clone()
        .oneshot(request(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "server": { "endpoints": [ { "host": "api.example.com" } ] },
                "protocol": "http",
                "plugins": { "items": [instance_id] },
            })),
        ))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);

    let response = h
        .router
        .oneshot(request(
            "DELETE",
            &format!("/oagw/v1/plugins/{instance_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(code_of(&body_json(response).await), Some("PluginInUse"));
}

#[tokio::test]
async fn built_in_plugin_catalog_and_source_are_served() {
    let h = harness();
    let descriptor = h
        .router
        .clone()
        .oneshot(request(
            "GET",
            &format!("/oagw/v1/plugins/{AUTH_NOOP}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(descriptor.status(), StatusCode::OK);
    let body = body_json(descriptor).await;
    assert_eq!(body["builtin"], true);
    assert_eq!(body["bindable"], true);

    let source = h
        .router
        .clone()
        .oneshot(request(
            "GET",
            &format!("/oagw/v1/plugins/{AUTH_NOOP}/source"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(source.status(), StatusCode::OK);
    let body = body_json(source).await;
    assert_eq!(body["id"], AUTH_NOOP);
    assert!(body["source_code"].as_str().unwrap().contains("noop"));

    let unknown = h
        .router
        .clone()
        .oneshot(request("GET", "/oagw/v1/plugins/not-a-uuid", None))
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);

    let missing = h
        .router
        .oneshot(request(
            "GET",
            &format!("/oagw/v1/plugins/{GUARD_REQUIRED_HEADERS}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::OK);
}

// ── contract-level assertions ───────────────────────────────────────────────

#[tokio::test]
async fn an_unknown_identifier_is_404_with_a_problem_body() {
    let h = harness();
    let id = Uuid::new_v4();
    for uri in [
        format!("/oagw/v1/upstreams/{id}"),
        format!("/oagw/v1/routes/{id}"),
        format!("/oagw/v1/plugins/{id}"),
        format!("/oagw/v1/plugins/{id}/source"),
    ] {
        let response = h
            .router
            .clone()
            .oneshot(request("GET", &uri, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(
            response.headers().get("x-oagw-error-source").unwrap(),
            "gateway",
            "{uri}"
        );
        let body = body_json(response).await;
        assert_eq!(body["status"], 404, "{uri}");
        assert_eq!(code_of(&body), Some("RouteNotFound"), "{uri}");
        assert_eq!(body["instance"], uri.as_str(), "{uri}");
    }
}

#[tokio::test]
async fn every_management_operation_is_published_in_openapi() {
    let h = harness();
    let doc = h.openapi.build_openapi(&OpenApiInfo::default()).unwrap();
    let paths: Vec<&str> = doc.paths.paths.keys().map(String::as_str).collect();
    for expected in [
        "/oagw/v1/upstreams",
        "/oagw/v1/upstreams/{id}",
        "/oagw/v1/routes",
        "/oagw/v1/routes/{id}",
        "/oagw/v1/plugins",
        "/oagw/v1/plugins/{id}",
        "/oagw/v1/plugins/{id}/source",
    ] {
        assert!(paths.contains(&expected), "missing {expected} in {paths:?}");
    }
    let list = doc.paths.paths.get("/oagw/v1/upstreams").unwrap();
    assert!(list.get.is_some());
    assert!(list.post.is_some());
    assert!(
        list.get
            .as_ref()
            .unwrap()
            .parameters
            .as_ref()
            .map(|params| params.len())
            .unwrap_or_default()
            >= 5,
        "the list operation documents the OData parameters"
    );
}

#[tokio::test]
async fn an_http_endpoint_pool_is_accepted_and_a_grpc_route_can_be_bound() {
    let h = harness();
    let created = h
        .router
        .clone()
        .oneshot(request(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "server": { "endpoints": [
                    { "scheme": "http", "host": "10.0.0.5", "port": 8080 },
                    { "scheme": "http", "host": "10.0.0.6", "port": 8080 },
                ] },
                "protocol": "grpc",
                "alias": "billing.internal",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let body = body_json(created).await;
    assert_eq!(body["alias"], "billing.internal");
    assert_eq!(
        body["server"]["endpoints"].as_array().map(Vec::len),
        Some(2)
    );
    let upstream_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    let route = h
        .router
        .oneshot(request(
            "POST",
            "/oagw/v1/routes",
            Some(json!({
                "upstream_id": upstream_id,
                "match": { "grpc": { "service": "cf.billing.v1.Billing", "method": "Charge" } },
            })),
        ))
        .await
        .unwrap();
    assert_eq!(route.status(), StatusCode::CREATED);
    let body = body_json(route).await;
    assert_eq!(body["match"]["grpc"]["service"], "cf.billing.v1.Billing");
}

#[tokio::test]
async fn the_tenant_comes_from_the_security_context() {
    let h = harness();
    let response = h
        .router
        .oneshot(request(
            "POST",
            "/oagw/v1/upstreams",
            Some(upstream_body(None)),
        ))
        .await
        .unwrap();
    let body = body_json(response).await;
    assert_eq!(body["tenant_id"], tenant_id().to_string());
    assert_eq!(h.svc.list_upstreams(tenant_id()).len(), 1);
    assert!(
        h.svc.list_upstreams(Uuid::from_u128(0x0A6E)).is_empty(),
        "another tenant sees nothing"
    );
}

#[tokio::test]
async fn plugin_references_from_the_catalog_are_resolved() {
    let h = harness();
    let created = h
        .router
        .clone()
        .oneshot(request(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "server": { "endpoints": [ { "host": "api.example.com" } ] },
                "protocol": "http",
                "plugins": { "items": [TRANSFORM_REQUEST_ID] },
            })),
        ))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let body = body_json(created).await;
    assert_eq!(body["plugins"]["items"][0], TRANSFORM_REQUEST_ID);
}
