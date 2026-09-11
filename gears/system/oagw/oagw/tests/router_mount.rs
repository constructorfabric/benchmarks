//! Router-mount tests (`cpt-cf-oagw-dod-router-mount`,
//! `cpt-cf-oagw-dod-unmounted-path-fallback`).
//!
//! Exercises the gear's REST route registration against a real
//! `axum::Router`, proving:
//! - the mount is reachable at `/oagw/v1/...` and never at `/api/oagw/v1/...`;
//! - an unmatched path under `/oagw/v1/...` renders the standard
//!   `RouteNotFound` problem document with the error-source header;
//! - `register_rest` merges onto the router it is given rather than nesting
//!   a fresh sub-router, so a route registered by another "gear" on the same
//!   shared router keeps working untouched.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use credstore_sdk::CredStoreClientV1;
use credstore_sdk::test_util::MockCredStoreClient;
use http_body_util::BodyExt;
use oagw::api::rest::routes::register_routes;
use oagw::gear::OagwGear;
use toolkit::api::openapi_registry::OpenApiRegistryImpl;
use toolkit::{ClientHub, ConfigProvider, Gear, GearCtx, RestApiCapability};
use tower::ServiceExt;
use uuid::Uuid;

struct EmptyConfigProvider;

impl ConfigProvider for EmptyConfigProvider {
    fn get_gear_config(&self, _gear_name: &str) -> Option<&serde_json::Value> {
        None
    }
}

/// A `ClientHub` with an empty `CredStoreClientV1` double registered, so
/// `OagwGear::init`'s `deps = [credstore]` dependency
/// (`cpt-cf-oagw-dod-credential-isolation`) resolves without a real
/// credstore gear present.
fn hub_with_credstore() -> ClientHub {
    let hub = ClientHub::default();
    let client: Arc<dyn CredStoreClientV1> = Arc::new(MockCredStoreClient::empty());
    hub.register::<dyn CredStoreClientV1>(client);
    hub
}

// reason: `Default::default()` below is inferred as
// `tokio_util::sync::CancellationToken::default()` from `GearCtx::new`'s
// signature; it is deliberately not named directly since this crate does not
// take `tokio-util` as a direct dependency, and adding one purely to spell
// out a test-only token's type would be a needless new dependency.
#[allow(clippy::default_trait_access)]
fn test_ctx() -> GearCtx {
    GearCtx::new(
        "oagw",
        Uuid::new_v4(),
        Arc::new(EmptyConfigProvider),
        Arc::new(hub_with_credstore()),
        Default::default(),
    )
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("read body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("body must be JSON")
}

// @cpt-begin:cpt-cf-oagw-dod-router-mount:p1:inst-router-mount-test-01
#[tokio::test]
async fn mount_is_reachable_at_gear_relative_path_only() {
    let router = register_routes(Router::new(), &OpenApiRegistryImpl::new());

    // Reachable at the gear-relative path.
    let req = Request::builder()
        .uri("/oagw/v1/anything")
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(req).await.expect("router call");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Never reachable at the `/api`-prefixed form: axum's own default
    // (empty-body, non-problem-json) 404 proves nothing was registered
    // there.
    let req = Request::builder()
        .uri("/api/oagw/v1/anything")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(req).await.expect("router call");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_ne!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json"),
        "/api/oagw/v1/... must not be served by this gear's mount"
    );
}
// @cpt-end:cpt-cf-oagw-dod-router-mount:p1:inst-router-mount-test-01

// @cpt-begin:cpt-cf-oagw-dod-unmounted-path-fallback:p1:inst-mount-check-return-test-01
#[tokio::test]
async fn unmatched_oagw_path_returns_the_standard_route_not_found_problem() {
    let router = register_routes(Router::new(), &OpenApiRegistryImpl::new());

    let req = Request::builder()
        .uri("/oagw/v1/no-such-resource")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(req).await.expect("router call");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );

    let body = body_json(response).await;
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert!(body["title"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(body["status"], 404);
    assert!(body["detail"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(body["instance"].as_str().is_some_and(|s| !s.is_empty()));
}
// @cpt-end:cpt-cf-oagw-dod-unmounted-path-fallback:p1:inst-mount-check-return-test-01

#[tokio::test]
async fn bare_mount_path_also_falls_back_to_route_not_found() {
    let router = register_routes(Router::new(), &OpenApiRegistryImpl::new());

    let req = Request::builder()
        .uri("/oagw/v1")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(req).await.expect("router call");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Covers the "its schemas register with the types registry without error"
/// half of `cpt-cf-oagw-dod-gear-registration`'s startup criterion. This
/// crate has no separate integration with a runtime "types registry"
/// service beyond the `OpenApiRegistry` every `OperationBuilder` call feeds
/// (see `crate::api::rest::routes::register_routes`); registering every
/// route without error and finding the entity schemas present in the
/// resulting component set is the closest observable proxy for "schemas
/// register ... without error" available inside this crate.
#[tokio::test]
async fn registering_routes_populates_the_openapi_schema_registry_without_error() {
    let openapi = OpenApiRegistryImpl::new();
    let _router = register_routes(Router::new(), &openapi);

    let components = openapi.components_registry.load();
    for expected in ["Upstream", "Route", "Plugin"] {
        assert!(
            components.contains_key(expected),
            "expected schema '{expected}' to be registered, got keys: {:?}",
            components.keys().collect::<Vec<_>>()
        );
    }
    assert!(
        !openapi.operation_specs.is_empty(),
        "registering routes must record at least one operation spec"
    );
}

#[tokio::test]
async fn register_rest_merges_onto_the_given_router_without_disturbing_other_routes() {
    // Simulate another gear's route already present on the shared router.
    let router = Router::new().route("/other-gear/v1/ping", get(|| async { "pong" }));

    let ctx = test_ctx();
    let gear = OagwGear::default();
    gear.init(&ctx).await.expect("init must succeed");
    let openapi = OpenApiRegistryImpl::new();
    let router = gear
        .register_rest(&ctx, router, &openapi)
        .expect("register_rest must succeed after init");

    // The other gear's route is untouched.
    let req = Request::builder()
        .uri("/other-gear/v1/ping")
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(req).await.expect("router call");
    assert_eq!(response.status(), StatusCode::OK);

    // The oagw mount is live and falls back correctly.
    let req = Request::builder()
        .uri("/oagw/v1/whatever")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(req).await.expect("router call");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
}
