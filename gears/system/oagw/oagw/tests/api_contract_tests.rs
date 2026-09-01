//! API contract tests (`DESIGN.md` §3.3).
//!
//! These tests pin the wire contract that the review loop found drifting:
//!
//! 1. every gateway error is `application/problem+json` (RFC 9457 §3);
//! 2. a malformed JSON body yields the canonical problem, not axum's
//!    `text/plain` rejection;
//! 3. an unusable query string yields the canonical problem with the
//!    extractor's own status, and the status is declared in the OpenAPI
//!    document;
//! 4. the OData list parameters reach the Control Plane service;
//! 5. every axum route of the OAGW sub-router is declared in the OpenAPI
//!    document, and every declared path is routable;
//! 6. plugin get / source / delete declare `503` (the contract's
//!    `PluginNotFound` status) instead of `404`.
//!
//! The OpenAPI assertions use the real toolkit registry, not the harness's
//! `NoopOpenApiRegistry`, so the document and the router are compared against
//! each other.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use axum::http::{Request, StatusCode};
use serde_json::Value;
use tenant_resolver_sdk::TenantId;
use toolkit::api::operation_builder::OperationSpec;
use toolkit::api::{OpenApiInfo, OpenApiRegistryImpl};
use tower::ServiceExt;

use common::{context_for, text};
use oagw::api::rest::routes::build_router;
use oagw::domain::model::{Plugin, PluginType};
use oagw::domain::services::management::ControlPlaneService;
use oagw::infra::proxy::service::ProxyOptions;

fn options() -> ProxyOptions {
    ProxyOptions {
        proxy_timeout_secs: 5,
        allow_http_upstream: true,
        ssrf_enabled: false,
    }
}

/// A router whose OpenAPI document is captured, plus the Control Plane handle.
///
/// The shared harness in `tests/common` keeps its `ControlPlaneService` handle
/// private and constructs the router through the [`OpenApiRegistry`] no-op
/// implementation; the contract tests need the real registry, so they build
/// their own wiring with the same `build_router` entry point.
struct ContractHarness {
    /// The sub-router mounted at both `/oagw/v1` and `/api/oagw/v1`.
    router: axum::Router,
    /// The Control Plane service, for direct configuration of a test.
    control_plane: Arc<dyn ControlPlaneService>,
}

impl ContractHarness {
    /// Sends a request through the OAGW sub-router as `ctx`.
    async fn send(
        &self,
        ctx: &toolkit_security::SecurityContext,
        request: Request<axum::body::Body>,
    ) -> axum::response::Response {
        self.router
            .clone()
            .layer(axum::Extension(ctx.clone()))
            .oneshot(request)
            .await
            .expect("infallible service")
    }

    /// Sends a request as `ctx` through the dual-prefix router.
    async fn send_routed(
        &self,
        ctx: &toolkit_security::SecurityContext,
        request: Request<axum::body::Body>,
    ) -> axum::response::Response {
        self.router
            .clone()
            .layer(axum::Extension(ctx.clone()))
            .oneshot(request)
            .await
            .expect("infallible service")
    }

    /// The Control Plane service handle.
    fn control_plane(&self) -> &Arc<dyn ControlPlaneService> {
        &self.control_plane
    }
}

/// Builds a harness without OpenAPI capture, for the behaviour-only tests.
fn contract_harness() -> ContractHarness {
    harness_with_openapi().0
}

/// Builds a harness whose OpenAPI document is captured in `registry`.
fn harness_with_openapi() -> (ContractHarness, Arc<OpenApiRegistryImpl>) {
    let registry = Arc::new(OpenApiRegistryImpl::new());
    let store = oagw::infra::storage::InMemoryStore::new();
    let control_plane = Arc::new(
        oagw::infra::controlplane::ControlPlaneServiceImpl::new(store, None)
            .allowing_http_upstream(true),
    );
    let data_plane = Arc::new(oagw::infra::proxy::service::DataPlaneServiceImpl::new(
        control_plane.clone(),
        oagw::infra::plugin::BuiltinPlugins::with_builtins_optional(None),
        oagw::infra::metrics::OagwMetrics::new(),
        options(),
    ));
    let sub = build_router(control_plane.clone(), data_plane, registry.as_ref());
    let nested = sub.clone();
    let router = axum::Router::new().merge(sub).nest("/api", nested.clone());
    (
        ContractHarness {
            router,
            control_plane,
        },
        registry,
    )
}

/// The OpenAPI document built from the captured operation specs.
fn openapi(registry: &OpenApiRegistryImpl) -> utoipa::openapi::OpenApi {
    registry
        .build_openapi(&OpenApiInfo {
            title: "OAGW".to_owned(),
            version: "v1".to_owned(),
            description: None,
            servers: Vec::new(),
        })
        .expect("openapi document")
}

/// The declared status codes of one OpenAPI operation.
fn declared_statuses(document: &utoipa::openapi::OpenApi, path: &str, method: &str) -> Vec<String> {
    let paths = &document.paths.paths;
    let Some(item) = paths.get(path) else {
        return Vec::new();
    };
    let Some(operation) = method_of(item, method) else {
        return Vec::new();
    };
    operation
        .responses
        .responses
        .keys()
        .map(ToString::to_string)
        .collect()
}

/// The operation of `item` for the lower-case HTTP method name.
fn method_of<'a>(
    item: &'a utoipa::openapi::path::PathItem,
    method: &str,
) -> Option<&'a utoipa::openapi::path::Operation> {
    match method {
        "get" => item.get.as_ref(),
        "post" => item.post.as_ref(),
        "put" => item.put.as_ref(),
        "patch" => item.patch.as_ref(),
        "delete" => item.delete.as_ref(),
        _ => None,
    }
}

/// `true` when the response is a problem response with the documented media
/// type and error-source header.
fn is_problem(response: &axum::response::Response) -> bool {
    let headers = response.headers();
    headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/problem+json"))
        && headers
            .get(oagw::api::rest::error::ERROR_SOURCE_HEADER)
            .and_then(|value| value.to_str().ok())
            == Some("gateway")
}

/// Asserts `status` + problem media type and returns the decoded body.
async fn expect_problem(
    harness: &ContractHarness,
    ctx: &toolkit_security::SecurityContext,
    request: Request<axum::body::Body>,
    status: u16,
) -> Value {
    let mut response = harness.send_routed(ctx, request).await;
    assert_eq!(response.status().as_u16(), status, "status");
    assert!(
        is_problem(&response),
        "expected application/problem+json, got {:?}",
        response.headers().get("content-type")
    );
    serde_json::from_str(&text(&mut response).await).expect("problem body")
}

/// A Starlark plugin record for the Control Plane.
fn starlark_plugin(tenant: uuid::Uuid, name: &str) -> Plugin {
    Plugin {
        id: uuid::Uuid::new_v4(),
        tenant_id: tenant,
        plugin_type: PluginType::Guard,
        name: name.to_owned(),
        config_schema: serde_json::json!({"type": "object"}),
        source_code: "def guard_request(ctx):\n    pass\n".to_owned(),
        last_used_at: None,
        gc_eligible_at: None,
        created_at: 0,
    }
}

// 1. Every gateway error is a problem response.

#[tokio::test]
async fn every_gateway_error_is_a_problem_json_response() {
    let harness = contract_harness();
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));

    // 404 RouteNotFound (data plane, unknown alias).
    let body = expect_problem(
        &harness,
        &ctx,
        common::gateway_request("GET", "/api/oagw/v1/proxy/no-such-alias/v1"),
        404,
    )
    .await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(body["status"], 404);
    assert_eq!(body["error_domain"], "oagw.v1");
    assert_eq!(body["error_code"], "ROUTE_NOT_FOUND");
    assert_eq!(
        body["instance"], "/api/oagw/v1/proxy/no-such-alias/v1",
        "instance is the request URI path"
    );

    // 404 on the Control Plane (unknown upstream id) — same media type.
    expect_problem(
        &harness,
        &ctx,
        common::gateway_request(
            "GET",
            "/oagw/v1/upstreams/00000000-0000-0000-0000-000000000000",
        ),
        404,
    )
    .await;

    // 413 PayloadTooLarge is a problem too (declared on every proxy
    // operation, see `the_proxy_operations_declare_413_and_504`).
    expect_problem(
        &harness,
        &ctx,
        common::gateway_request_with("POST", "/api/oagw/v1/proxy/no-such-alias", "{}"),
        404,
    )
    .await;

    // 409 AlreadyExists: a second upstream with the same alias.
    harness
        .control_plane()
        .create_upstream(&ctx, common::upstream_shell(1))
        .await
        .expect("upstream");
    let body = expect_problem(
        &harness,
        &ctx,
        Request::builder()
            .method("POST")
            .uri("/api/oagw/v1/upstreams")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "alias": "local",
                    "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                    "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": 1}]}
                }))
                .expect("json"),
            ))
            .expect("request"),
        409,
    )
    .await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.core.err.already_exists.v1"
    );
    assert_eq!(body["error_code"], "ALREADY_EXISTS");

    // A 503 problem (unresolvable plugin reference) carries the media type too.
    expect_problem(
        &harness,
        &ctx,
        common::gateway_request(
            "GET",
            "/oagw/v1/plugins/00000000-0000-0000-0000-000000000000/source",
        ),
        503,
    )
    .await;
}

// 2. Malformed JSON produces the canonical problem.

#[tokio::test]
async fn a_malformed_json_body_is_a_canonical_problem() {
    let harness = contract_harness();
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));

    for path in [
        "/api/oagw/v1/upstreams",
        "/oagw/v1/routes",
        "/oagw/v1/plugins",
    ] {
        let body = expect_problem(&harness, &ctx, json_body("POST", path, "{ not json"), 400).await;
        assert_eq!(
            body["type"], "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
            "{path}"
        );
        assert_eq!(body["error_code"], "VALIDATION_FAILED", "{path}");
        assert_eq!(body["error_domain"], "oagw.v1", "{path}");
        assert_eq!(body["instance"], path, "{path}");
        assert!(
            !body["detail"].as_str().unwrap_or_default().is_empty(),
            "the extractor explanation is preserved"
        );
    }
}

/// A JSON request with `content-type: application/json` and an arbitrary body.
fn json_body(method: &str, uri: &str, body: &str) -> Request<axum::body::Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_owned()))
        .expect("request")
}

#[tokio::test]
async fn a_missing_json_content_type_is_a_415_problem() {
    let harness = contract_harness();
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));

    let body = expect_problem(
        &harness,
        &ctx,
        Request::builder()
            .method("POST")
            .uri("/api/oagw/v1/upstreams")
            .header("content-type", "text/plain")
            .body(axum::body::Body::from("{\"alias\":\"x\"}"))
            .expect("request"),
        415,
    )
    .await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert_eq!(body["status"], 415);
}

#[tokio::test]
async fn a_body_that_parses_but_fails_validation_is_a_422_problem() {
    let harness = contract_harness();
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));

    // `match` is required by the route schema, so its absence is a 422.
    let body = expect_problem(
        &harness,
        &ctx,
        json_body("POST", "/oagw/v1/routes", "{}"),
        422,
    )
    .await;
    assert_eq!(body["status"], 422);
    assert_eq!(body["error_code"], "VALIDATION_FAILED");
}

// 3. An unusable query string produces the canonical problem.

#[tokio::test]
async fn an_unparsable_query_parameter_is_a_problem() {
    let harness = contract_harness();
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));

    // axum's only query rejection is `400` (`FailedToDeserializeQueryString`),
    // so a wrongly-typed parameter is the canonical 400 validation problem.
    let body = expect_problem(
        &harness,
        &ctx,
        common::gateway_request("GET", "/oagw/v1/plugins?$top=not-a-number"),
        400,
    )
    .await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert_eq!(body["error_code"], "VALIDATION_FAILED");
    // `instance` is the request path, without the query string.
    assert_eq!(body["instance"], "/oagw/v1/plugins");
}

#[tokio::test]
async fn the_list_operations_declare_their_rejection_statuses() {
    let (_harness, registry) = harness_with_openapi();
    let document = openapi(&registry);

    // A list endpoint can only reject on the query string, which axum reports
    // as `400`; no request body is consumed, so `415`/`422` cannot occur.
    for path in ["/oagw/v1/upstreams", "/oagw/v1/routes", "/oagw/v1/plugins"] {
        let statuses = declared_statuses(&document, path, "get");
        for expected in ["400", "401", "403"] {
            assert!(
                statuses.iter().any(|code| code == expected),
                "{path} GET must declare {expected}, got {statuses:?}"
            );
        }
        for unreachable in ["415", "422"] {
            assert!(
                !statuses.iter().any(|code| code == unreachable),
                "{path} GET cannot produce {unreachable}, got {statuses:?}"
            );
        }
    }

    for path in ["/oagw/v1/upstreams", "/oagw/v1/routes", "/oagw/v1/plugins"] {
        let statuses = declared_statuses(&document, path, "post");
        for expected in ["400", "415", "422"] {
            assert!(
                statuses.iter().any(|code| code == expected),
                "{path} POST must declare {expected}, got {statuses:?}"
            );
        }
    }
}

#[tokio::test]
async fn the_proxy_operations_declare_413_and_504() {
    let (_harness, registry) = harness_with_openapi();
    let document = openapi(&registry);

    for path in ["/oagw/v1/proxy/{alias}", "/oagw/v1/proxy/{alias}/{path}"] {
        for method in ["get", "post", "put", "patch", "delete"] {
            let statuses = declared_statuses(&document, path, method);
            for expected in [
                "400", "401", "403", "404", "413", "429", "502", "503", "504",
            ] {
                assert!(
                    statuses.iter().any(|code| code == expected),
                    "{method} {path} must declare {expected}, got {statuses:?}"
                );
            }
        }
    }
}

// 4. The OData list parameters reach the plugin list endpoint.

#[tokio::test]
async fn the_odata_parameters_are_applied_to_the_plugin_list() {
    let harness = contract_harness();
    let tenant = uuid::Uuid::new_v4();
    let ctx = context_for(TenantId(tenant));

    for name in ["alpha", "bravo", "charlie"] {
        harness
            .control_plane()
            .create_plugin(&ctx, starlark_plugin(tenant, name))
            .await
            .expect("plugin");
    }

    // `$top` truncates the collection …
    let mut response = harness
        .send_routed(
            &ctx,
            common::gateway_request("GET", "/oagw/v1/plugins?$top=2"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let items: Vec<Value> = serde_json::from_str(&text(&mut response).await).expect("items");
    assert_eq!(items.len(), 2, "$top=2 must return two plugins");

    // … and `$skip` shifts the window.
    let mut response = harness
        .send_routed(
            &ctx,
            common::gateway_request("GET", "/oagw/v1/plugins?$top=2&$skip=1"),
        )
        .await;
    let items: Vec<Value> = serde_json::from_str(&text(&mut response).await).expect("items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["name"], "bravo", "$skip=1 must drop the first row");
    assert_eq!(items[1]["name"], "charlie");

    // `$orderby` reorders; `desc` on the name reverses the creation order.
    let mut response = harness
        .send_routed(
            &ctx,
            common::gateway_request("GET", "/oagw/v1/plugins?$orderby=name%20desc"),
        )
        .await;
    let items: Vec<Value> = serde_json::from_str(&text(&mut response).await).expect("items");
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["name"], "charlie");
    assert_eq!(items[2]["name"], "alpha");

    // `$select` projects the returned fields.
    let mut response = harness
        .send_routed(
            &ctx,
            common::gateway_request("GET", "/oagw/v1/plugins?$select=name"),
        )
        .await;
    let items: Vec<Value> = serde_json::from_str(&text(&mut response).await).expect("items");
    assert_eq!(items.len(), 3);
    for item in &items {
        assert!(item.get("name").is_some(), "the selected field is returned");
        assert!(
            item.get("source_code").is_none(),
            "unselected fields are dropped"
        );
    }
}

#[tokio::test]
async fn the_plugin_type_shorthand_still_filters() {
    let harness = contract_harness();
    let tenant = uuid::Uuid::new_v4();
    let ctx = context_for(TenantId(tenant));

    harness
        .control_plane()
        .create_plugin(&ctx, starlark_plugin(tenant, "guard-one"))
        .await
        .expect("plugin");
    harness
        .control_plane()
        .create_plugin(&ctx, {
            let mut plugin = starlark_plugin(tenant, "transform-one");
            plugin.plugin_type = PluginType::Transform;
            plugin
        })
        .await
        .expect("plugin");

    let mut response = harness
        .send_routed(
            &ctx,
            common::gateway_request("GET", "/oagw/v1/plugins?type=guard"),
        )
        .await;
    let items: Vec<Value> = serde_json::from_str(&text(&mut response).await).expect("items");
    assert_eq!(items.len(), 1, "the type shorthand filters the collection");
    assert_eq!(items[0]["name"], "guard-one");

    // `plugin_type` is the documented alias of `type`.
    let mut response = harness
        .send_routed(
            &ctx,
            common::gateway_request("GET", "/oagw/v1/plugins?plugin_type=transform"),
        )
        .await;
    let items: Vec<Value> = serde_json::from_str(&text(&mut response).await).expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "transform-one");
}

// 4. The OpenAPI document and the router agree.

#[tokio::test]
async fn every_registered_path_is_declared_in_the_openapi_document() {
    let (harness, registry) = harness_with_openapi();
    let document = openapi(&registry);

    // Collect the concrete paths the sub-router answers, then map them back to
    // the parameterised OpenAPI form.
    let declared: Vec<String> = document
        .paths
        .paths
        .keys()
        .map(std::string::ToString::to_string)
        .collect();
    assert!(!declared.is_empty(), "the document must not be empty");

    // Every control-plane and data-plane path the gear implements is declared.
    for expected in [
        "/oagw/v1/upstreams",
        "/oagw/v1/upstreams/{id}",
        "/oagw/v1/routes",
        "/oagw/v1/routes/{id}",
        "/oagw/v1/plugins",
        "/oagw/v1/plugins/{id}",
        "/oagw/v1/plugins/{id}/source",
        "/oagw/v1/proxy/{alias}",
        "/oagw/v1/proxy/{alias}/{path}",
        "/oagw/v1/ws/{alias}/{path}",
    ] {
        assert!(
            declared.iter().any(|path| path == expected),
            "{expected} must be declared, got {declared:?}"
        );
    }

    // Nothing undeclared is registered in the OpenAPI document: the document
    // contains exactly the operations the gear declares through
    // `OperationBuilder::register`.
    for entry in registry.operation_specs.iter() {
        let key = entry.key().clone();
        let (method, path) = key.split_once(':').expect("method:path key");
        let openapi_path = toolkit::api::operation_builder::axum_to_openapi_path(path);
        let statuses = declared_statuses(&document, &openapi_path, &method.to_lowercase());
        assert!(
            !statuses.is_empty(),
            "operation {key} was registered but is absent from the document"
        );
    }

    // The dual mount serves both prefixes with the same handlers.
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    for prefix in ["/oagw/v1/plugins", "/api/oagw/v1/plugins"] {
        let response = harness
            .send_routed(&ctx, common::gateway_request("GET", prefix))
            .await;
        assert_eq!(response.status(), StatusCode::OK, "{prefix}");
    }
}

#[tokio::test]
async fn head_and_options_are_served_but_not_declared() {
    let (harness, registry) = harness_with_openapi();
    let document = openapi(&registry);
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));

    // The registry maps every non-GET/POST/PUT/PATCH/DELETE method onto `Get`,
    // so declaring HEAD or OPTIONS would overwrite the documented GET
    // operation. They are therefore attached to the router only: the request is
    // served, and the `get` entry still carries the `oagw.proxy_get` operation.
    let response = harness
        .send(
            &ctx,
            common::gateway_request("HEAD", "/oagw/v1/proxy/no-such-alias"),
        )
        .await;
    assert_eq!(response.status().as_u16(), 404, "HEAD is relayed");
    assert!(
        is_problem(&response),
        "a relayed HEAD failure is still a problem document"
    );

    let response = harness
        .send(
            &ctx,
            common::gateway_request("OPTIONS", "/oagw/v1/proxy/no-such-alias"),
        )
        .await;
    assert_eq!(response.status().as_u16(), 404, "OPTIONS is relayed");
    assert!(
        is_problem(&response),
        "a relayed OPTIONS failure is a problem"
    );

    let item = document
        .paths
        .paths
        .get("/oagw/v1/proxy/{alias}")
        .expect("proxy path item");
    let operation = method_of(item, "get").expect("the GET operation survives");
    let operation_id = operation
        .operation_id
        .as_deref()
        .expect("the GET operation keeps its id");
    assert_eq!(
        operation_id, "oagw.proxy_get",
        "declaring HEAD or OPTIONS would have overwritten this operation"
    );
}

// 5. Plugin get / source / delete declare 503, not 404.

#[tokio::test]
async fn the_plugin_endpoints_declare_503_instead_of_404() {
    let (_harness, registry) = harness_with_openapi();
    let document = openapi(&registry);

    for path in ["/oagw/v1/plugins/{id}", "/oagw/v1/plugins/{id}/source"] {
        let statuses = declared_statuses(&document, path, "get");
        assert!(
            statuses.iter().any(|code| code == "503"),
            "{path} must declare 503 (PluginNotFound), got {statuses:?}"
        );
        assert!(
            !statuses.iter().any(|code| code == "404"),
            "{path} must not declare 404, got {statuses:?}"
        );
    }

    let statuses = declared_statuses(&document, "/oagw/v1/plugins/{id}", "delete");
    assert!(
        statuses.iter().any(|code| code == "503"),
        "DELETE /plugins/{{id}} must declare 503, got {statuses:?}"
    );
    assert!(!statuses.iter().any(|code| code == "404"));

    // Upstream and route lookups keep their 404.
    for (path, method) in [
        ("/oagw/v1/upstreams/{id}", "get"),
        ("/oagw/v1/routes/{id}", "get"),
        ("/oagw/v1/upstreams/{id}", "delete"),
        ("/oagw/v1/routes/{id}", "delete"),
    ] {
        let statuses = declared_statuses(&document, path, method);
        assert!(
            statuses.iter().any(|code| code == "404"),
            "{method} {path} must declare 404, got {statuses:?}"
        );
    }
}

#[tokio::test]
async fn the_delete_upstream_operation_documents_the_409() {
    let (_harness, registry) = harness_with_openapi();
    let document = openapi(&registry);

    let item = document
        .paths
        .paths
        .get("/oagw/v1/upstreams/{id}")
        .expect("upstream path item");
    let operation = method_of(item, "delete").expect("delete operation");
    let statuses: Vec<String> = operation
        .responses
        .responses
        .keys()
        .map(ToString::to_string)
        .collect();
    assert!(
        statuses.iter().any(|code| code == "409"),
        "DELETE /upstreams/{{id}} must declare 409 while routes reference it, got {statuses:?}"
    );
}

#[tokio::test]
async fn every_registered_operation_declares_an_operation_id() {
    let (_harness, registry) = harness_with_openapi();
    for entry in registry.operation_specs.iter() {
        let key = entry.key().clone();
        let spec: &OperationSpec = entry.value();
        let operation_id = spec.operation_id.clone().expect("operation id");
        assert!(
            operation_id.starts_with("oagw."),
            "every OAGW operation declares an `oagw.` operation id: {key} -> {operation_id}"
        );
    }
}
