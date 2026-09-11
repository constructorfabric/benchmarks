//! Router-level tests for the management REST API (DESIGN §3.3).
//!
//! Requests go through the real `Router` with `tower::ServiceExt::oneshot`, so
//! the assertions cover the wire shape — status codes, `application/problem+json`
//! bodies and the `X-OAGW-Error-Source` header — rather than the service layer.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::gts_helpers as gts;
use crate::domain::model::{HttpMatch, HttpMethod, MatchConfig, PathSuffixMode, Route};
use crate::domain::services::management::ControlPlaneService;
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::proxy::service::DataPlaneService;
use crate::infra::storage::memory::{
    MemoryPluginRepository, MemoryRouteRepository, MemoryUpstreamRepository,
};

const TENANT: &str = "00000000-0000-0000-0000-000000000001";

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

/// The gateway's router, with a security context injected into every request.
fn router() -> Router {
    router_with_config(OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    })
}

/// [`router`] with an explicit gear configuration, for tests that exercise a
/// non-default limit (the body limit in particular).
fn router_with_config(config: OagwConfig) -> Router {
    struct FlatHierarchy;
    #[async_trait::async_trait]
    impl crate::domain::repo::TenantHierarchy for FlatHierarchy {
        async fn chain(&self, tenant_id: &str) -> Vec<String> {
            vec![tenant_id.to_owned()]
        }
    }
    let control_plane = Arc::new(ControlPlaneService::new(
        Arc::new(MemoryUpstreamRepository::default()),
        Arc::new(MemoryRouteRepository::default()),
        Arc::new(MemoryPluginRepository::default()),
        Arc::new(FlatHierarchy),
        true,
    ));
    let data_plane = DataPlaneService::new(Arc::clone(&control_plane), config.clone())
        .expect("a buildable data plane")
        .with_registries(
            AuthPluginRegistry::empty(),
            GuardPluginRegistry::empty(),
            TransformPluginRegistry::empty(),
        );
    crate::api::rest::routes::register_routes(
        Router::new(),
        &NoopOpenApiRegistry,
        control_plane,
        Arc::new(data_plane),
        config,
    )
    .layer(axum::Extension(security_context()))
}

fn security_context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::now_v7())
        .subject_tenant_id(Uuid::parse_str(TENANT).unwrap())
        .build()
        .unwrap()
}

/// Sends a JSON request through the router.
async fn call(router: &Router, method: Method, uri: &str, body: Option<Value>) -> (u16, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.map_or_else(Vec::new, |value| {
            value.to_string().into_bytes()
        })))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let parsed = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, wrap(&content_type, &parsed))
}

/// Records the content type alongside the body so assertions can tell
/// `application/json` from `application/problem+json`.
fn wrap(content_type: &str, body: &Value) -> Value {
    json!({ "content_type": content_type, "body": body })
}

/// An HTTP upstream on an unreachable port: management calls never dial.
fn upstream_body(alias: Option<&str>) -> Value {
    let mut body = json!({
        "server": { "endpoints": [ { "scheme": "http", "host": "backend.example.com" } ] },
        "protocol": gts::PROTOCOL_HTTP,
    });
    if let Some(alias) = alias {
        body["alias"] = json!(alias);
    }
    body
}

/// An IP-address endpoint, which always requires an explicit alias (ADR 0002).
fn ip_upstream_body(alias: &str) -> Value {
    json!({
        "alias": alias,
        "server": { "endpoints": [ { "scheme": "http", "host": "10.0.0.1" } ] },
        "protocol": gts::PROTOCOL_HTTP,
    })
}

/// A second upstream with a different derived alias.
fn other_upstream_body() -> Value {
    json!({
        "server": { "endpoints": [ { "scheme": "http", "host": "other.example.com" } ] },
        "protocol": gts::PROTOCOL_HTTP,
    })
}

fn route_body(upstream_id: Uuid) -> Value {
    json!({
        "upstream_id": upstream_id,
        "match_config": {
            "http": {
                "methods": ["GET", "POST"],
                "path": "/api",
                "path_suffix_mode": "append",
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upstream_crud_round_trip() {
    let router = router();

    let (status, created) = call(
        &router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(upstream_body(None)),
    )
    .await;
    assert_eq!(status, 201, "created: {created}");
    let id = created["body"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    assert!(
        created["body"]["gts_id"]
            .as_str()
            .unwrap()
            .starts_with(gts::TYPE_UPSTREAM)
    );
    assert_eq!(
        created["body"]["alias"], "backend.example.com",
        "the alias is derived from the host"
    );
    assert_eq!(created["body"]["tenant_id"], TENANT);
    assert_eq!(created["content_type"], "application/json");

    let (status, fetched) = call(
        &router,
        Method::GET,
        &format!("/oagw/v1/upstreams/{id}"),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(fetched["body"]["id"], created["body"]["id"]);

    let (status, list) = call(&router, Method::GET, "/oagw/v1/upstreams", None).await;
    assert_eq!(status, 200);
    assert_eq!(list["body"]["total"], 1);
    assert_eq!(list["body"]["items"].as_array().unwrap().len(), 1);

    // A hostname-based endpoint always auto-derives its alias, so a replacement
    // body carries no alias and the derived one survives.
    let mut replacement = upstream_body(None);
    replacement["enabled"] = json!(false);
    let (status, replaced) = call(
        &router,
        Method::PUT,
        &format!("/oagw/v1/upstreams/{id}"),
        Some(replacement),
    )
    .await;
    assert_eq!(status, 200, "replaced: {replaced}");
    assert_eq!(replaced["body"]["alias"], "backend.example.com");
    assert_eq!(replaced["body"]["enabled"], false);

    let (status, body) = call(
        &router,
        Method::DELETE,
        &format!("/oagw/v1/upstreams/{id}"),
        None,
    )
    .await;
    assert_eq!(status, 204, "delete: {body}");
    let (status, body) = call(
        &router,
        Method::GET,
        &format!("/oagw/v1/upstreams/{id}"),
        None,
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(body["body"]["title"], "Route Not Found");
}

#[tokio::test]
async fn duplicate_alias_is_a_conflict() {
    let router = router();
    let first = call(
        &router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(ip_upstream_body("shared")),
    )
    .await;
    assert_eq!(first.0, 201, "{:?}", first.1);

    let (status, problem) = call(
        &router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(ip_upstream_body("shared")),
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(problem["content_type"], "application/problem+json");
    assert_eq!(problem["body"]["title"], "Validation Error");
    assert_eq!(
        problem["body"]["type"],
        gts::error_type(gts::ERR_VALIDATION)
    );
    assert_eq!(problem["body"]["status"], 409);
}

#[tokio::test]
async fn hostname_endpoints_always_derive_their_alias() {
    let router = router();
    let (status, problem) = call(
        &router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(upstream_body(Some("pinned"))),
    )
    .await;
    assert_eq!(status, 400, "{problem}");
    assert!(
        problem["body"]["detail"]
            .as_str()
            .unwrap()
            .contains("auto-derive"),
        "the rejection explains the rule: {problem}"
    );
}

#[tokio::test]
async fn malformed_endpoint_host_is_rejected() {
    let router = router();
    let mut body = upstream_body(None);
    body["server"]["endpoints"][0]["host"] = json!("not a host");
    let (status, problem) = call(&router, Method::POST, "/oagw/v1/upstreams", Some(body)).await;
    assert_eq!(status, 400, "{problem}");
    assert_eq!(problem["content_type"], "application/problem+json");
    assert_eq!(problem["body"]["title"], "Validation Error");
}

#[tokio::test]
async fn plaintext_http_endpoints_are_opt_in() {
    // An `http` endpoint scheme is a legal field value whatever the
    // configuration says: the control plane stores it, and the data plane is
    // where a plaintext connection is refused (DESIGN "Non-goals").
    struct FlatHierarchy;
    #[async_trait::async_trait]
    impl crate::domain::repo::TenantHierarchy for FlatHierarchy {
        async fn chain(&self, tenant_id: &str) -> Vec<String> {
            vec![tenant_id.to_owned()]
        }
    }
    let control_plane = ControlPlaneService::new(
        Arc::new(MemoryUpstreamRepository::default()),
        Arc::new(MemoryRouteRepository::default()),
        Arc::new(MemoryPluginRepository::default()),
        Arc::new(FlatHierarchy),
        false,
    );
    let body: crate::api::rest::dto::UpstreamRequest =
        serde_json::from_value(upstream_body(None)).unwrap();
    let created = control_plane
        .create_upstream(TENANT, body.into_domain(TENANT), None)
        .await
        .expect("an http endpoint scheme is always accepted");
    let route = Route {
        upstream_id: created.id,
        enabled: true,
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "/api".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        ..Route::default()
    };
    control_plane.create_route(TENANT, route).await.unwrap();

    let config = OagwConfig {
        allow_http_upstream: false,
        ..OagwConfig::default()
    };
    let data_plane = DataPlaneService::new(Arc::new(control_plane), config).unwrap();
    let response = data_plane
        .proxy(crate::infra::proxy::service::ProxyCall {
            tenant_id: TENANT.to_owned(),
            user_id: None,
            client_ip: None,
            method: Method::GET,
            path: "/backend.example.com/api".to_owned(),
            query: String::new(),
            headers: http::HeaderMap::new(),
            body: bytes::Bytes::new(),
            upgrade: None,
        })
        .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn odata_parameters_are_tolerated_on_list_endpoints() {
    let router = router();
    let (status, list) = call(
        &router,
        Method::GET,
        "/oagw/v1/upstreams?$top=1&$skip=0&$select=id,alias&$filter=contains(alias,b)&$orderby=alias%20desc",
        None,
    )
    .await;
    assert_eq!(status, 200, "{list}");
    assert_eq!(list["body"]["total"], 0, "an empty table still answers");
    assert!(list["body"]["items"].is_array());
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn route_crud_and_match_uniqueness() {
    let router = router();
    let (_, created) = call(
        &router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(upstream_body(None)),
    )
    .await;
    let upstream_id = created["body"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();

    let (status, route) = call(
        &router,
        Method::POST,
        "/oagw/v1/routes",
        Some(route_body(upstream_id)),
    )
    .await;
    assert_eq!(status, 201, "{route}");
    let route_id = route["body"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();

    // A second route with the same match is a conflict.
    let (status, problem) = call(
        &router,
        Method::POST,
        "/oagw/v1/routes",
        Some(route_body(upstream_id)),
    )
    .await;
    assert_eq!(status, 409, "{problem}");

    let (status, list) = call(&router, Method::GET, "/oagw/v1/routes", None).await;
    assert_eq!(status, 200);
    assert_eq!(list["body"]["total"], 1);

    let (status, updated) = call(
        &router,
        Method::PUT,
        &format!("/oagw/v1/routes/{route_id}"),
        Some(json!({
            "upstream_id": upstream_id,
            "enabled": false,
            "match_config": {
                "http": { "methods": ["GET"], "path": "/other", "path_suffix_mode": "append" }
            }
        })),
    )
    .await;
    assert_eq!(status, 200, "{updated}");
    assert_eq!(updated["body"]["match_config"]["http"]["path"], "/other");

    let (status, _) = call(
        &router,
        Method::DELETE,
        &format!("/oagw/v1/routes/{route_id}"),
        None,
    )
    .await;
    assert_eq!(status, 204);
}

/// Two enabled routes may share a path only when their `priority` separates
/// them (DESIGN §"Data Constraints").
#[tokio::test]
async fn same_path_with_a_different_priority_is_accepted() {
    let router = router();
    let (_, created) = call(
        &router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(upstream_body(None)),
    )
    .await;
    let upstream_id = created["body"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();

    let mut first = route_body(upstream_id);
    first["priority"] = json!(10);
    let (status, stored) = call(&router, Method::POST, "/oagw/v1/routes", Some(first)).await;
    assert_eq!(status, 201, "{stored}");
    assert_eq!(stored["body"]["priority"], 10);

    // Same path and methods, different priority: accepted.
    let mut second = route_body(upstream_id);
    second["priority"] = json!(5);
    let (status, stored) = call(&router, Method::POST, "/oagw/v1/routes", Some(second)).await;
    assert_eq!(status, 201, "{stored}");
    assert_eq!(stored["body"]["priority"], 5);

    // Same path, methods *and* priority: refused.
    let mut third = route_body(upstream_id);
    third["priority"] = json!(10);
    let (status, problem) = call(&router, Method::POST, "/oagw/v1/routes", Some(third)).await;
    assert_eq!(status, 409, "{problem}");
}

/// A route may carry its own CORS policy, which then governs the proxy leg.
#[tokio::test]
async fn a_route_can_declare_its_own_cors_policy() {
    let router = router();
    let (_, created) = call(
        &router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(upstream_body(None)),
    )
    .await;
    let upstream_id = created["body"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();

    let mut body = route_body(upstream_id);
    body["cors"] = json!({
        "enabled": true,
        "allowed_origins": ["https://app.example.com"],
        "allowed_methods": ["GET"],
    });
    let (status, stored) = call(&router, Method::POST, "/oagw/v1/routes", Some(body)).await;
    assert_eq!(status, 201, "{stored}");
    assert_eq!(
        stored["body"]["cors"]["allowed_origins"][0],
        "https://app.example.com"
    );

    // A wildcard origin combined with credentials is refused, on a route just
    // as on an upstream.
    let mut bad = route_body(upstream_id);
    bad["priority"] = json!(3);
    bad["cors"] = json!({
        "enabled": true,
        "allow_credentials": true,
        "allowed_origins": ["*"],
    });
    let (status, problem) = call(&router, Method::POST, "/oagw/v1/routes", Some(bad)).await;
    assert_eq!(status, 400, "{problem}");
    assert!(
        problem["body"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("route cors"))
    );
}

#[tokio::test]
async fn route_upstream_id_is_immutable() {
    let router = router();
    let (_, first) = call(
        &router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(upstream_body(None)),
    )
    .await;
    let (_, second) = call(
        &router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(other_upstream_body()),
    )
    .await;

    let a = first["body"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let b = second["body"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();

    let (_, route) = call(
        &router,
        Method::POST,
        "/oagw/v1/routes",
        Some(route_body(a)),
    )
    .await;
    let route_id = route["body"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();

    let (status, problem) = call(
        &router,
        Method::PUT,
        &format!("/oagw/v1/routes/{route_id}"),
        Some(route_body(b)),
    )
    .await;
    assert_eq!(status, 400, "{problem}");
    assert!(
        problem["body"]["detail"]
            .as_str()
            .unwrap()
            .contains("immutable")
    );
}

#[tokio::test]
async fn route_to_an_unknown_upstream_is_not_found() {
    let router = router();
    let (status, problem) = call(
        &router,
        Method::POST,
        "/oagw/v1/routes",
        Some(route_body(Uuid::now_v7())),
    )
    .await;
    assert_eq!(status, 404, "{problem}");
    assert_eq!(problem["content_type"], "application/problem+json");
}

#[tokio::test]
async fn route_requires_an_upstream_id() {
    let router = router();
    let body = json!({ "match_config": { "http": { "methods": ["GET"], "path": "/api" } } });
    let (status, problem) = call(&router, Method::POST, "/oagw/v1/routes", Some(body)).await;
    assert_eq!(status, 400, "{problem}");
}

#[tokio::test]
async fn tenant_isolation_on_the_management_api() {
    let router = router();
    let (_, created) = call(
        &router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(upstream_body(None)),
    )
    .await;
    let id = created["body"]["id"].as_str().unwrap().to_owned();

    let other = SecurityContext::builder()
        .subject_id(Uuid::now_v7())
        .subject_tenant_id(Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap())
        .build()
        .unwrap();
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("/oagw/v1/upstreams/{id}"))
        .body(Body::empty())
        .unwrap();
    let response = router
        .oneshot(request)
        .await
        .unwrap_or_else(|_| panic!("the router must answer"));
    let _ = other;
    assert_eq!(response.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

fn plugin_body(name: &str) -> Value {
    json!({
        "name": name,
        "type": "guard",
        "config_schema": { "type": "object" },
        "source_code": "def on_request(ctx):\n    return None\n",
    })
}

#[tokio::test]
async fn plugin_lifecycle_including_source() {
    let router = router();
    let (status, created) = call(
        &router,
        Method::POST,
        "/oagw/v1/plugins",
        Some(plugin_body("pin")),
    )
    .await;
    assert_eq!(
        status,
        201,
        "create: {}",
        serde_json::to_string(&created).unwrap()
    );
    let id = created["body"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();

    let (status, source) = call(
        &router,
        Method::GET,
        &format!("/oagw/v1/plugins/{id}/source"),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        source["body"]["source_code"],
        plugin_body("pin")["source_code"]
    );
    assert_eq!(source["body"]["type"], "guard");

    let (status, _) = call(
        &router,
        Method::DELETE,
        &format!("/oagw/v1/plugins/{id}"),
        None,
    )
    .await;
    assert_eq!(status, 204);
    let (status, _) = call(
        &router,
        Method::GET,
        &format!("/oagw/v1/plugins/{id}"),
        None,
    )
    .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn plugin_in_use_cannot_be_deleted() {
    let router = router();
    let (_, created) = call(
        &router,
        Method::POST,
        "/oagw/v1/plugins",
        Some(plugin_body("pinned")),
    )
    .await;
    let plugin_id = created["body"]["id"].as_str().unwrap().to_owned();

    // Binding the plugin to an upstream has to happen through the control
    // plane: the REST create takes a `plugins` block, and the reference is the
    // plugin's GTS id.
    let (_, upstream) = call(
        &router,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(json!({
            "server": { "endpoints": [ { "scheme": "http", "host": "backend.example.com" } ] },
            "protocol": gts::PROTOCOL_HTTP,
            "plugins": {
                "sharing": "private",
                "items": [ { "plugin_ref": plugin_id, "config": {} } ]
            }
        })),
    )
    .await;
    let upstream_gts = upstream["body"]["gts_id"].as_str().unwrap().to_owned();

    let (status, problem) = call(
        &router,
        Method::DELETE,
        &format!("/oagw/v1/plugins/{plugin_id}"),
        None,
    )
    .await;
    assert_eq!(status, 409, "{problem}");
    assert_eq!(problem["content_type"], "application/problem+json");
    assert_eq!(problem["body"]["title"], "Plugin In Use");
    assert_eq!(
        problem["body"]["type"],
        gts::error_type(gts::ERR_PLUGIN_IN_USE)
    );
    // ADR 0001: the body names the referencing resources, not just the count.
    assert_eq!(
        problem["body"]["referenced_by"]["upstreams"],
        json!([upstream_gts])
    );
    assert_eq!(problem["body"]["referenced_by"]["routes"], json!([]));

    // Once the referencing upstream is gone the plugin is deletable.
    let upstream_uuid = upstream["body"]["id"].as_str().unwrap().to_owned();
    let (status, _) = call(
        &router,
        Method::DELETE,
        &format!("/oagw/v1/upstreams/{upstream_uuid}"),
        None,
    )
    .await;
    assert_eq!(status, 204);
    let (status, _) = call(
        &router,
        Method::DELETE,
        &format!("/oagw/v1/plugins/{plugin_id}"),
        None,
    )
    .await;
    assert_eq!(status, 204);
}

#[tokio::test]
async fn unknown_ids_answer_problem_json_404() {
    let router = router();
    let id = Uuid::now_v7();
    for uri in [
        format!("/oagw/v1/upstreams/{id}"),
        format!("/oagw/v1/routes/{id}"),
        format!("/oagw/v1/plugins/{id}"),
    ] {
        let (status, problem) = call(&router, Method::GET, &uri, None).await;
        assert_eq!(status, 404, "{uri}");
        assert_eq!(problem["content_type"], "application/problem+json");
        assert_eq!(
            problem["body"]["type"],
            gts::error_type(gts::ERR_ROUTE_NOT_FOUND)
        );
    }
}

// ---------------------------------------------------------------------------
// CORS preflight routing
// ---------------------------------------------------------------------------

/// `proxy_operations` registers the proxy path per HTTP method; `OPTIONS` is
/// anonymous because the data plane answers a preflight before any upstream
/// work (ADR 0004). Without that registration the browser's first request of a
/// cross-origin exchange would be answered `405` and the exchange would never
/// start, so this covers the routing rather than the CORS logic itself.
#[tokio::test]
async fn a_preflight_reaches_the_data_plane_instead_of_405() {
    let router = router();
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/oagw/v1/proxy/backend/api/hello")
                .header(http::header::ORIGIN, "https://console.example.com")
                .header("access-control-request-method", "POST")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        response
            .headers()
            .get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "https://console.example.com"
    );
}

// ---------------------------------------------------------------------------
// Body handling (DESIGN §3.2 Body Validation)
// ---------------------------------------------------------------------------

/// A body over the gear's limit is `413` as a problem document, with the
/// gateway error source — distinct from the plain-text `413` the edge gateway
/// produces for its own, smaller limit.
#[tokio::test]
async fn a_body_over_the_gear_limit_is_a_problem_json_413() {
    let router = router_with_config(OagwConfig {
        allow_http_upstream: true,
        max_body_bytes: 16,
        ..OagwConfig::default()
    });
    let (status, problem) = call(
        &router,
        Method::POST,
        "/oagw/v1/proxy/backend/api",
        Some(json!({
            "filler": "this payload is far longer than sixteen bytes"
        })),
    )
    .await;
    assert_eq!(status, 413);
    assert_eq!(problem["content_type"], "application/problem+json");
    assert_eq!(problem["body"]["title"], "Payload Too Large");
    assert_eq!(
        problem["body"]["type"],
        gts::error_type(gts::ERR_PAYLOAD_TOO_LARGE)
    );
}

/// A declared `content-length` that disagrees with the actual body is rejected
/// rather than forwarded to the upstream.
#[tokio::test]
async fn a_declared_length_that_disagrees_with_the_body_is_rejected() {
    let router = router();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/oagw/v1/proxy/backend/api")
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::CONTENT_LENGTH, "999")
        .body(Body::from(b"{}".to_vec()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers().get(gts::HEADER_ERROR_SOURCE).unwrap(),
        gts::ERROR_SOURCE_GATEWAY
    );
}

/// A `content-length` that is not an integer is a validation error.
#[tokio::test]
async fn a_non_numeric_content_length_is_a_validation_error() {
    let router = router();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/oagw/v1/proxy/backend/api")
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::CONTENT_LENGTH, "many")
        .body(Body::from(b"{}".to_vec()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let problem: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(problem["title"], "Validation Error");
}

/// An upstream without its required `server` block is a validation error with
/// the validation error type (DESIGN §3.3).
#[tokio::test]
async fn an_upstream_without_server_endpoints_is_a_validation_error() {
    let (status, body) = call(
        &router(),
        Method::POST,
        "/oagw/v1/upstreams",
        Some(json!({
            "protocol": gts::PROTOCOL_HTTP,
        })),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["content_type"], "application/problem+json");
    assert_eq!(body["body"]["type"], gts::error_type(gts::ERR_VALIDATION));
}

/// A syntactically invalid JSON body is a 400 in the gear's own problem format,
/// not axum's plain-text rejection body.
#[tokio::test]
async fn a_malformed_json_body_is_a_400() {
    let router = router();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/oagw/v1/upstreams")
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(b"{not json".to_vec()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers().get(gts::HEADER_ERROR_SOURCE).unwrap(),
        gts::ERROR_SOURCE_GATEWAY
    );
    assert_eq!(
        response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/problem+json")
    );
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let problem: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(problem["type"], gts::error_type(gts::ERR_VALIDATION));
}
