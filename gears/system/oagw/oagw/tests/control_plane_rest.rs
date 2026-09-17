//! Control-plane REST surface (DESIGN §3.3 "Management API").
//!
//! Every request is issued against the router [`oagw::api::rest::routes::register_routes`]
//! builds, so the tests exercise the real wire shapes: the OData-ish list
//! envelope, RFC 9457 problem documents, tenant scoping and the
//! `/oagw/v1` prefix the gear registers under an empty gateway prefix.
//! No socket is bound: requests go through `tower`'s `oneshot`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::extract::Request;
use http::HeaderMap;
use oagw::api::rest::handlers::OagwApi;
use oagw::api::rest::routes;
use oagw::config::OagwConfig;
use oagw::infra::management::ManagementService;
use oagw::infra::plugin::builtin_plugin_ids;
use oagw::infra::proxy::service::{DataPlane, DataPlaneDeps};
use oagw::infra::storage::memory::MemoryStore;
use oagw::infra::tenant::TenantHierarchy;
use serde_json::{json, Value};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt as _;
use uuid::Uuid;

/// Shared store; each [`router`] call wires a fresh handler over it so two
/// callers can be served by one configuration store.
struct Gateway {
    store: Arc<MemoryStore>,
}

fn gateway() -> Gateway {
    Gateway {
        store: MemoryStore::new(),
    }
}

/// A router for one caller; the api-gateway would supply this context.
fn router(gateway: &Gateway, context: SecurityContext) -> axum::Router {
    let data_plane = Arc::new(
        DataPlane::new(DataPlaneDeps {
            store: Arc::clone(&gateway.store),
            tenants: TenantHierarchy::new(None, Duration::from_secs(60)),
            credstore: None,
            config: OagwConfig::default(),
        })
        .unwrap(),
    );
    let api = OagwApi {
        management: Arc::new(ManagementService::new(
            Arc::clone(&gateway.store),
            Some(Arc::clone(&data_plane)),
            false,
        )),
        data_plane,
    };
    routes::register_routes(axum::Router::new(), &OpenApiRegistryImpl::new(), Arc::new(api))
        .layer(axum::Extension(context))
}

/// An authenticated caller owning `tenant`.
fn caller(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_type("service")
        .subject_tenant_id(tenant)
        .build()
        .unwrap()
}

async fn call(router: &axum::Router, request: Request) -> (http::StatusCode, Value, HeaderMap) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let json = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(Value::Null)
    };
    (status, json, headers)
}

fn request(method: &'static str, target: &str, body: Option<Value>) -> Request {
    let builder = Request::builder()
        .method(method)
        .uri(target)
        .header("content-type", "application/json");
    match body {
        Some(body) => builder.body(axum::body::Body::from(body.to_string())),
        None => builder.body(axum::body::Body::empty()),
    }
    .unwrap()
}

fn upstream_body(alias: &str, host: &str) -> Value {
    json!({
        "alias": alias,
        "server": {"endpoints": [{"scheme": "https", "host": host, "port": 443}]},
        // Unknown members must be tolerated, not rejected.
        "x-deployment-hint": {"region": "eu"}
    })
}

fn route_body(upstream_id: Uuid, suffix_mode: &str, plugins: Value) -> Value {
    json!({
        "upstream_id": upstream_id,
        "match": {
            "http": {
                "methods": ["GET"],
                "path": "/v1/*",
                "query_allowlist": ["page"],
                "path_suffix_mode": suffix_mode
            }
        },
        "plugins": plugins
    })
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upstream_defaults_survive_the_wire_with_unknown_fields() {
    let gw = gateway();
    let router = router(&gw, caller(Uuid::new_v4()));

    let (status, created, _) = call(
        &router,
        request("POST", "/oagw/v1/upstreams", Some(upstream_body("api.example.com", "api.example.com"))),
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "{created}");
    assert_eq!(created["alias"], "api.example.com");
    assert_eq!(created["enabled"], true, "enabled defaults to true");
    assert_eq!(
        created["protocol"],
        "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "protocol defaults to the OAGW HTTP identifier"
    );
    assert_eq!(created["alias_explicit"], true);

    let (status, page, _) = call(&router, request("GET", "/oagw/v1/upstreams", None)).await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(page["count"], 1);
    assert_eq!(page["items"][0]["id"], created["id"]);

    let (status, found, _) = call(
        &router,
        request(
            "GET",
            &format!("/oagw/v1/upstreams/{}", created["id"].as_str().unwrap()),
            None,
        ),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(found["alias"], "api.example.com");

    let (status, problem, headers) = call(
        &router,
        request("GET", &format!("/oagw/v1/upstreams/{}", Uuid::new_v4()), None),
    )
    .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(problem["context"], json!({}), "the canonical context object is always present");
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
}

#[tokio::test]
async fn aliases_are_scoped_per_tenant_and_records_are_invisible_across_tenants() {
    let gw = gateway();
    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();
    let router_a = router(&gw, caller(tenant_a));
    let router_b = router(&gw, caller(tenant_b));

    let (status, first, _) = call(
        &router_a,
        request("POST", "/oagw/v1/upstreams", Some(upstream_body("shared.example", "a.example.com"))),
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "{first}");

    // The same alias is free in another tenant.
    let (status, second, _) = call(
        &router_b,
        request("POST", "/oagw/v1/upstreams", Some(upstream_body("shared.example", "b.example.com"))),
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "{second}");

    // ...but not twice in the same tenant.
    let (status, conflict, _) = call(
        &router_a,
        request("POST", "/oagw/v1/upstreams", Some(upstream_body("shared.example", "c.example.com"))),
    )
    .await;
    assert_eq!(status, http::StatusCode::CONFLICT);
    assert_eq!(
        conflict["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.alias.conflict.v1"
    );

    let (_, page_a, _) = call(&router_a, request("GET", "/oagw/v1/upstreams", None)).await;
    assert_eq!(page_a["count"], 1);
    assert_eq!(page_a["items"][0]["server"]["endpoints"][0]["host"], "a.example.com");
    let (_, page_b, _) = call(&router_b, request("GET", "/oagw/v1/upstreams", None)).await;
    assert_eq!(page_b["count"], 1);
    assert_eq!(page_b["items"][0]["server"]["endpoints"][0]["host"], "b.example.com");

    // A foreign record id is indistinguishable from a missing one.
    let foreign_id = first["id"].as_str().unwrap();
    let (status, problem, _) = call(&router_b, request("GET", &format!("/oagw/v1/upstreams/{foreign_id}"), None)).await;
    assert_eq!(status, http::StatusCode::NOT_FOUND, "{problem}");
}

#[tokio::test]
async fn an_upstream_without_a_derivable_alias_is_rejected() {
    let gw = gateway();
    let router = router(&gw, caller(Uuid::new_v4()));

    // An IP endpoint cannot derive an alias and none is supplied.
    let body = json!({
        "server": {"endpoints": [{"scheme": "https", "host": "10.0.0.1", "port": 443}]}
    });
    let (status, problem, _) = call(&router, request("POST", "/oagw/v1/upstreams", Some(body))).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(
        problem["detail"].as_str().unwrap().contains("alias is required"),
        "{}",
        problem["detail"]
    );

    // Plaintext endpoints are rejected while `allow_http_upstream` is off.
    let body = json!({
        "alias": "plain.example",
        "server": {"endpoints": [{"scheme": "http", "host": "plain.example", "port": 80}]}
    });
    let (status, problem, _) = call(&router, request("POST", "/oagw/v1/upstreams", Some(body))).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("allow_http_upstream"),
        "{}",
        problem["detail"]
    );
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn route_wire_shape_carries_path_suffix_mode_and_bound_plugins() {
    let gw = gateway();
    let tenant = Uuid::new_v4();
    let router = router(&gw, caller(tenant));

    let (status, upstream, _) = call(
        &router,
        request("POST", "/oagw/v1/upstreams", Some(upstream_body("routes.example", "routes.example"))),
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "{upstream}");
    let upstream_id = Uuid::parse_str(upstream["id"].as_str().unwrap()).unwrap();

    let plugins = json!({
        "sharing": "private",
        "items": [
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
            {
                "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                "config": {"required_request_headers": "x-correlation-id"}
            }
        ]
    });
    let (status, created, _) = call(
        &router,
        request(
            "POST",
            "/oagw/v1/routes",
            Some(route_body(upstream_id, "disabled", plugins.clone())),
        ),
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "{created}");
    assert_eq!(created["match"]["http"]["path_suffix_mode"], "disabled");
    assert_eq!(created["plugins"]["items"][0], plugins["items"][0]);
    assert_eq!(
        created["plugins"]["items"][1]["config"]["required_request_headers"],
        "x-correlation-id",
        "the bound configuration is stored"
    );

    let route_id = created["id"].as_str().unwrap().to_owned();
    // `upstream_id` is immutable, so a replacement naming another upstream is a
    // validation error rather than a silent move.
    let moved = json!({
        "upstream_id": Uuid::new_v4(),
        "match": {"http": {"methods": ["GET"], "path": "/v1/*", "query_allowlist": [], "path_suffix_mode": "append"}}
    });
    let (status, problem, _) = call(
        &router,
        request("PUT", &format!("/oagw/v1/routes/{route_id}"), Some(moved)),
    )
    .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{problem}");
    assert!(problem["detail"].as_str().unwrap().contains("immutable"));

    let (status, deleted, _) = call(
        &router,
        request("DELETE", &format!("/oagw/v1/routes/{route_id}"), None),
    )
    .await;
    assert_eq!(status, http::StatusCode::NO_CONTENT);
    assert_eq!(deleted, Value::Null);
}

#[tokio::test]
async fn conflicting_match_rules_are_rejected_and_protect_the_upstream() {
    let gw = gateway();
    let router = router(&gw, caller(Uuid::new_v4()));

    let (status, upstream, _) = call(
        &router,
        request("POST", "/oagw/v1/upstreams", Some(upstream_body("lifecycle.example", "lifecycle.example"))),
    )
    .await;
    assert_eq!(status, http::StatusCode::CREATED, "{upstream}");
    let upstream_id = Uuid::parse_str(upstream["id"].as_str().unwrap()).unwrap();
    let route = route_body(upstream_id, "append", json!({"items": [], "sharing": "private"}));

    let (status, created, _) = call(&router, request("POST", "/oagw/v1/routes", Some(route.clone()))).await;
    assert_eq!(status, http::StatusCode::CREATED, "{created}");
    let route_id = created["id"].as_str().unwrap().to_owned();

    let (status, conflict, _) = call(&router, request("POST", "/oagw/v1/routes", Some(route))).await;
    assert_eq!(status, http::StatusCode::CONFLICT);
    assert_eq!(
        conflict["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.match.conflict.v1"
    );

    // An upstream with routes cannot be deleted.
    let (status, problem, _) = call(
        &router,
        request("DELETE", &format!("/oagw/v1/upstreams/{upstream_id}"), None),
    )
    .await;
    assert_eq!(status, http::StatusCode::CONFLICT, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.match.conflict.v1"
    );

    let (status, _, _) = call(&router, request("DELETE", &format!("/oagw/v1/routes/{route_id}"), None)).await;
    assert_eq!(status, http::StatusCode::NO_CONTENT);
    // Now unreferenced, the upstream can go.
    let (status, _, _) = call(
        &router,
        request("DELETE", &format!("/oagw/v1/upstreams/{upstream_id}"), None),
    )
    .await;
    assert_eq!(status, http::StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plugin_catalog_is_readable_and_builtins_cannot_be_deleted() {
    let gw = gateway();
    let router = router(&gw, caller(Uuid::new_v4()));

    let (status, page, _) = call(&router, request("GET", "/oagw/v1/plugins", None)).await;
    assert_eq!(status, http::StatusCode::OK);
    let catalogued = builtin_plugin_ids();
    assert_eq!(
        page["count"].as_u64().unwrap() as usize,
        catalogued.len() - 6,
        "only implemented plugins are served, not the catalog-only ids"
    );

    let apikey = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
    let (status, plugin, _) = call(&router, request("GET", &format!("/oagw/v1/plugins/{apikey}"), None)).await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(plugin["name"], "cf.core.oagw.apikey.v1");
    assert_eq!(plugin["plugin_type"], "auth");

    let (status, problem, _) = call(&router, request("DELETE", &format!("/oagw/v1/plugins/{apikey}"), None)).await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
    assert!(problem["detail"].as_str().unwrap().contains("cannot be deleted"));

    let (status, problem, _) = call(
        &router,
        request("GET", &format!("/oagw/v1/plugins/{}", Uuid::new_v4()), None),
    )
    .await;
    assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
    );
}

#[tokio::test]
async fn custom_plugins_are_created_read_and_deleted() {
    let gw = gateway();
    let router = router(&gw, caller(Uuid::new_v4()));

    let source = "def guard_request(ctx):\n    return {\"allow\": True}\n";
    let body = json!({
        "name": "tenant-guard",
        "description": "custom guard",
        "plugin_type": "guard",
        "config_schema": {"type": "object"},
        "source_code": source
    });
    let (status, created, _) = call(&router, request("POST", "/oagw/v1/plugins", Some(body))).await;
    assert_eq!(status, http::StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();
    assert!(
        id.starts_with("gts.cf.core.oagw.guard_plugin.v1~"),
        "{id}"
    );

    let (status, page, _) = call(&router, request("GET", "/oagw/v1/plugins", None)).await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(page["count"].as_u64().unwrap(), 7, "six built-ins plus the custom one");

    let (status, source_response, _) = call(
        &router,
        request("GET", &format!("/oagw/v1/plugins/{id}/source"), None),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(source_response["language"], "starlark");
    assert_eq!(source_response["source"], source);

    let (status, _, _) = call(&router, request("DELETE", &format!("/oagw/v1/plugins/{id}"), None)).await;
    assert_eq!(status, http::StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------------
// Lists, paging and prefix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_endpoints_page_filter_and_order() {
    let gw = gateway();
    let router = router(&gw, caller(Uuid::new_v4()));
    for alias in ["b.example.com", "a.example.com", "c.example.com"] {
        let (status, created, _) = call(
            &router,
            request("POST", "/oagw/v1/upstreams", Some(upstream_body(alias, alias))),
        )
        .await;
        assert_eq!(status, http::StatusCode::CREATED, "{created}");
    }

    let (status, page, _) = call(
        &router,
        request("GET", "/oagw/v1/upstreams?$orderby=alias%20asc", None),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
    let aliases: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["alias"].as_str().unwrap())
        .collect();
    assert_eq!(aliases, vec!["a.example.com", "b.example.com", "c.example.com"]);

    let (status, page, _) = call(
        &router,
        request(
            "GET",
            "/oagw/v1/upstreams?$top=1&$skip=1&$orderby=alias%20asc",
            None,
        ),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(page["count"], 1, "$top bounds the page");
    assert_eq!(page["items"][0]["alias"], "b.example.com");

    let (status, page, _) = call(
        &router,
        request("GET", "/oagw/v1/upstreams?$orderby=alias%20desc", None),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(page["items"][0]["alias"], "c.example.com");

    let (status, page, _) = call(
        &router,
        request("GET", "/oagw/v1/upstreams?$filter=alias%20eq%20%27a.example.com%27", None),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(page["count"], 1);
    assert_eq!(page["items"][0]["alias"], "a.example.com");

    let (status, page, _) = call(
        &router,
        request("GET", "/oagw/v1/upstreams?$filter=bogus%20eq%20%27x%27", None),
    )
    .await;
    assert_eq!(status, http::StatusCode::OK, "an unknown field matches nothing");
    assert_eq!(page["count"], 0);

    let (status, problem, _) = call(
        &router,
        request("GET", "/oagw/v1/upstreams?$filter=bogus", None),
    )
    .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
    assert!(problem["detail"].as_str().unwrap().contains("unsupported $filter"));
}

#[tokio::test]
async fn the_gear_serves_oagw_v1_without_an_extra_api_prefix() {
    let gw = gateway();
    let router = router(&gw, caller(Uuid::new_v4()));

    // The gear registers gear-relative paths; the api-gateway prefix is empty
    // in this deployment, so an `/api` prefixed path is simply unrouted.
    let (status, _, _) = call(&router, request("GET", "/api/oagw/v1/upstreams", None)).await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);

    let (status, problem, _) = call(&router, request("GET", "/oagw/v1/does-not-exist", None)).await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
    assert_eq!(problem, Value::Null, "unrouted paths are not problem documents");

    // An empty endpoint list is a payload problem, not a routing problem.
    let (status, problem, _) = call(
        &router,
        request(
            "POST",
            "/oagw/v1/upstreams",
            Some(json!({
                "alias": "x.example",
                "server": {"endpoints": []}
            })),
        ),
    )
    .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST);
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}
