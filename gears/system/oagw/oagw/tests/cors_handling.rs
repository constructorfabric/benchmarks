//! Black-box, external-crate tests for DECOMPOSITION entry 2.7
//! (cors-handling), exercising the publicly reachable surface of `oagw::*`.
//!
//! `oagw::policy::cors` -- this feature's preflight-fast-path,
//! origin/method-validation, and response-header-injection algorithms -- is
//! declared `pub(crate) mod cors;` from `oagw::policy` (`src/policy/mod.rs`,
//! owned by DECOMPOSITION entry 2.5/2.8, outside this feature's file
//! ownership), so it is not reachable *by name* from an external test crate:
//! the module itself is invisible outside the `oagw` crate, and so is
//! `oagw::api::rest::proxy`/`routes::register_routes` (crate-private,
//! exactly as `tests/proxy_core.rs` documents for the analogous 2.5 case).
//! A full, `tower::ServiceExt`-driven, function-level HTTP proof of the
//! preflight/origin/method/response-header algorithms therefore also lives
//! as the inline `#[cfg(test)] mod tests` inside `src/policy/cors.rs`
//! itself, which -- being part of the `oagw` crate -- has the access this
//! file cannot, and this file does not duplicate that coverage.
//!
//! What *is* reachable from here, without ever naming a `pub(crate)` item,
//! is the crate's public `oagw::OagwGear` (`impl toolkit::Gear` /
//! `impl toolkit::RestApiCapability`) -- the exact same entry point the
//! platform calls in production. Driving a request through
//! `OagwGear::register_rest`'s returned `Router` reaches every route this
//! gear registers (Upstream/Route Management *and* the proxy path),
//! including this feature's preflight/actual-request hooks, entirely
//! through public API. `build_router` below does this once; the tests that
//! follow use it for a genuine black-box, full-router proof of this
//! feature's `Browser Preflight Request Flow` and `Browser Actual
//! Cross-Origin Request Flow`, seeding Upstream/Route data through the real
//! `POST /oagw/v1/upstreams`/`POST /oagw/v1/routes` endpoints (this crate's
//! own `OagwState` handle is private inside `OagwGear`, so there is no way
//! to poke the store directly from here -- nor should there be).
//!
//! One acceptance-criteria pair remains out of reach even through this
//! route: `cpt-cf-oagw-algo-cors-merge-effective-config`'s ancestor-chain
//! merge (`sharing: inherit` union / `sharing: enforce` hold, the feature
//! doc's own two unchecked Acceptance Criteria). `crate::proxy::merge::merge_cors`
//! is `pub(crate)`, and -- more fundamentally -- `OagwGear::register_rest`
//! always wires `crate::proxy::hierarchy::NoTenantHierarchy` (see that
//! module's doc comment: "production always uses `NoTenantHierarchy`"),
//! which reports zero ancestors for every tenant. The fake multi-level
//! provider needed to build a >1-level ancestor chain is only reachable via
//! the `pub(crate)` `register_routes_with_hierarchy` helper each management
//! submodule's own inline test module uses. So even a fully black-box,
//! real-router test cannot construct the scenario these two criteria
//! describe in the current wiring; see this file's final tests, which
//! document that gap rather than fabricate a test that cannot reach it. By
//! code inspection, `merge_cors`'s `Enforce`/`Inherit` branches do implement
//! the documented union/hold semantics (the `locked` flag set by an
//! ancestor's `Enforce` skips every later level, *including* the resolving
//! tenant's own contribution) -- but that inspection is not a substitute for
//! an executed assertion, and is reported as such below.
//!
//! This file also keeps the pre-existing model-level tests exercising the
//! parts of this feature's Security Defaults DoD
//! (`cpt-cf-oagw-dod-cors-security-defaults`) that are visible purely at the
//! `CorsConfig` (de)serialization level.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use http_body_util::BodyExt;
use httpmock::prelude::*;
use oagw::OagwGear;
use oagw::model::upstream::{CorsConfig, CorsMethod, Sharing};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use toolkit::api::OpenApiRegistryImpl;
use toolkit::{ClientHub, ConfigProvider, Gear, GearCtx, RestApiCapability};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

/// `cpt-cf-oagw-dod-cors-security-defaults`: the schema default is
/// `enabled: false` -- the posture this feature's
/// `validate_origin_and_method`/`inject_response_headers` treat as
/// "perform no CORS processing" -- and the sharing mode defaults to
/// `private` (never visible to a descendant tenant's merge).
#[test]
fn cors_config_deserializes_with_the_documented_deny_by_default_posture() {
    let cors: CorsConfig = serde_json::from_value(serde_json::json!({ "enabled": false })).unwrap();
    assert!(!cors.enabled);
    assert_eq!(cors.sharing, Sharing::Private);
    assert!(cors.allowed_origins.is_empty());
    assert_eq!(
        cors.allowed_methods,
        vec![CorsMethod::Get, CorsMethod::Post]
    );
    assert!(cors.expose_headers.is_empty());
    assert!(!cors.allow_credentials);
}

/// A fully declared, enabled `cors` object round-trips every field this
/// feature's algorithms read: `allowed_origins` (exact-match input),
/// `allowed_methods` (method-check input), `expose_headers` and
/// `allow_credentials` (response-header injection input).
#[test]
fn enabled_cors_config_round_trips_every_field_this_feature_consumes() {
    let cors: CorsConfig = serde_json::from_value(serde_json::json!({
        "sharing": "inherit",
        "enabled": true,
        "allowed_origins": ["https://app.example.com", "http://localhost:3000"],
        "allowed_methods": ["GET", "POST", "DELETE"],
        "expose_headers": ["X-Request-ID"],
        "allow_credentials": true,
    }))
    .unwrap();

    assert_eq!(cors.sharing, Sharing::Inherit);
    assert!(cors.enabled);
    assert_eq!(
        cors.allowed_origins,
        vec![
            "https://app.example.com".to_owned(),
            "http://localhost:3000".to_owned(),
        ]
    );
    assert_eq!(
        cors.allowed_methods,
        vec![CorsMethod::Get, CorsMethod::Post, CorsMethod::Delete]
    );
    assert_eq!(cors.expose_headers, vec!["X-Request-ID".to_owned()]);
    assert!(cors.allow_credentials);
}

/// `http://` is a syntactically ordinary `allowed_origins` entry at the
/// model level -- nothing about the wire format singles out a plaintext
/// scheme for rejection, consistent with `cpt-cf-oagw-dod-cors-security-defaults`'s
/// literal, scheme-sensitive (not scheme-restrictive) exact-match rule this
/// feature applies at match time.
#[test]
fn plaintext_scheme_origins_are_ordinary_allowed_origins_entries() {
    let cors: CorsConfig = serde_json::from_value(serde_json::json!({
        "enabled": true,
        "allowed_origins": ["http://localhost:3000", "http://localhost:5173"],
        "allow_credentials": true,
    }))
    .unwrap();
    assert_eq!(cors.allowed_origins.len(), 2);
    assert!(
        cors.allowed_origins
            .iter()
            .all(|o| o.starts_with("http://"))
    );
}

// ---------------------------------------------------------------------
// Full-router, black-box tests driven through the real `oagw::OagwGear`
// (see the module doc comment for why this reaches routes that the rest of
// this file cannot).
// ---------------------------------------------------------------------

/// Hands back one fixed JSON blob for the `oagw` gear's config section,
/// regardless of the requested gear name matching anything else -- the
/// shape `toolkit::config::gear_config_or_default` expects is
/// `gears.<name> = { "config": {...} }`, i.e. the *whole* per-gear value,
/// not just its `config` sub-object.
struct FixedConfig(Value);

impl ConfigProvider for FixedConfig {
    fn get_gear_config(&self, gear: &str) -> Option<&Value> {
        (gear == OagwGear::MODULE_NAME).then_some(&self.0)
    }
}

/// Build the real `oagw` gear router by driving it through the exact same
/// public `toolkit::Gear`/`toolkit::RestApiCapability` entry points the
/// platform calls in production (`impl Gear for OagwGear` /
/// `impl RestApiCapability for OagwGear`, `oagw/src/lib.rs`). This never
/// names a single `pub(crate)` item.
async fn build_router(allow_http_upstream: bool) -> Router {
    let gear = OagwGear::default();
    let ctx = GearCtx::new(
        OagwGear::MODULE_NAME,
        Uuid::new_v4(),
        Arc::new(FixedConfig(json!({
            "config": {
                "proxy_timeout_secs": 5,
                "allow_http_upstream": allow_http_upstream,
            }
        }))) as Arc<dyn ConfigProvider>,
        Arc::new(ClientHub::new()),
        // `CancellationToken` implements `Default`; inferred from
        // `GearCtx::new`'s signature without naming (or depending on)
        // `tokio-util` directly.
        Default::default(),
    );
    gear.init(&ctx)
        .await
        .expect("gear init must succeed with a well-formed fixed config");
    let openapi = OpenApiRegistryImpl::new();
    gear.register_rest(&ctx, Router::new(), &openapi)
        .expect("register_rest must succeed")
}

/// A `SecurityContext` inserted directly into a request's extensions,
/// standing in for the platform auth middleware that supplies it in
/// production -- the identical stand-in every inline `#[cfg(test)]` module
/// in this crate uses (e.g. `src/api/rest/proxy.rs`'s own `request()`
/// helper).
fn security_context(tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant_id)
        .build()
        .unwrap()
}

fn json_request(method: &str, uri: &str, tenant_id: Uuid, body: Value) -> Request<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    req.extensions_mut().insert(security_context(tenant_id));
    req
}

fn bare_request(
    method: &str,
    uri: &str,
    tenant_id: Uuid,
    headers: &[(&str, &str)],
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut req = builder.body(Body::empty()).unwrap();
    req.extensions_mut().insert(security_context(tenant_id));
    req
}

async fn response_json(response: Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// A bound-then-immediately-dropped TCP listener: guarantees an ephemeral
/// port nothing is listening on, so a request routed there fails fast
/// (connection refused) instead of hanging -- used to prove a CORS
/// rejection happens *before* any upstream connection is attempted (if one
/// were attempted, the test would see a connection-refused-shaped gateway
/// error instead of the clean `403` asserted below).
async fn unused_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// Create an Upstream through the real `POST /oagw/v1/upstreams` endpoint
/// with the given `cors` object, pointing at `127.0.0.1:{port}` over plain
/// `http`. Returns `(alias, upstream_id)`.
async fn create_cors_upstream(
    router: &Router,
    tenant_id: Uuid,
    alias: &str,
    port: u16,
    cors: Value,
) -> (String, Uuid) {
    let body = json!({
        "alias": alias,
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "cors": cors,
    });
    let response = router
        .clone()
        .oneshot(json_request("POST", "/oagw/v1/upstreams", tenant_id, body))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "upstream creation must succeed"
    );
    let json = response_json(response).await;
    let id: Uuid = json["id"].as_str().unwrap().parse().unwrap();
    (alias.to_owned(), id)
}

/// Create a `GET {path}` Route through the real `POST /oagw/v1/routes`
/// endpoint, bound to `upstream_id`.
async fn create_get_route(router: &Router, tenant_id: Uuid, upstream_id: Uuid, path: &str) {
    create_route_with_methods(router, tenant_id, upstream_id, path, &["GET"]).await;
}

/// Same as [`create_get_route`], but with an explicit route-level HTTP
/// method allowlist -- needed to exercise a CORS `allowed_methods` rejection
/// (`cpt-cf-oagw-dod-cors-actual-request-validation`) for a method the
/// *route* itself still matches (a route-level method mismatch is an
/// ordinary `cpt-cf-oagw-feature-proxy-core` `RouteNotFound`, a different,
/// earlier gate than this feature's own method check).
async fn create_route_with_methods(
    router: &Router,
    tenant_id: Uuid,
    upstream_id: Uuid,
    path: &str,
    methods: &[&str],
) {
    let body = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": methods, "path": path } },
        "priority": 1,
    });
    let response = router
        .clone()
        .oneshot(json_request("POST", "/oagw/v1/routes", tenant_id, body))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "route creation must succeed"
    );
}

/// Acceptance criterion 1: a preflight `OPTIONS` against an alias that does
/// not exist still receives `204` with the documented echoed/fixed headers
/// -- proving the preflight fast path never resolves an upstream at all
/// (`cpt-cf-oagw-dod-cors-preflight-fast-path`).
///
/// Regression test for RF-002: `src/api/rest/proxy.rs`'s
/// `register_routes_with_hierarchy` now mounts `.options(handle)` on
/// `PROXY_WILDCARD_PATH` alongside the other methods, so axum's router
/// dispatches an `OPTIONS` request into `handle` (and therefore
/// `cors::preflight_fast_path`, which runs before any upstream resolution)
/// instead of rejecting it with `405` beforehand.
#[tokio::test]
async fn preflight_against_a_nonexistent_alias_returns_204_without_resolving_any_upstream() {
    let router = build_router(false).await;
    let tenant_id = Uuid::new_v4();

    let response = router
        .oneshot(bare_request(
            "OPTIONS",
            "/oagw/v1/proxy/this-alias-was-never-created/anything",
            tenant_id,
            &[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "POST"),
                ("access-control-request-headers", "content-type"),
            ],
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .unwrap(),
        "https://app.example.com"
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-methods")
            .unwrap(),
        "POST"
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-headers")
            .unwrap(),
        "content-type"
    );
    assert_eq!(
        response.headers().get("access-control-max-age").unwrap(),
        "86400"
    );
    assert_eq!(
        response.headers().get("vary").unwrap(),
        "Origin, Access-Control-Request-Method, Access-Control-Request-Headers"
    );
    assert_eq!(
        response.headers().get("x-oagw-error-source").unwrap(),
        "gateway"
    );
}

/// Acceptance criterion 3: an actual cross-origin request whose `Origin` is
/// not in the effective `allowed_origins` is rejected `403` with the
/// documented problem body, before any upstream connection is opened --
/// proven end to end by pointing the upstream at an unused port: a `403`
/// (rather than a connection-refused-shaped gateway error) is only possible
/// if the rejection happened before the forwarding step
/// (`cpt-cf-oagw-dod-cors-actual-request-validation`).
#[tokio::test]
async fn disallowed_origin_is_rejected_403_before_any_upstream_connection_is_opened() {
    let router = build_router(true).await;
    let tenant_id = Uuid::new_v4();
    let port = unused_port().await;
    let (alias, upstream_id) = create_cors_upstream(
        &router,
        tenant_id,
        "cors-origin-reject-svc",
        port,
        json!({
            "enabled": true,
            "allowed_origins": ["https://good.example"],
            "allowed_methods": ["GET"],
        }),
    )
    .await;
    create_get_route(&router, tenant_id, upstream_id, "/data").await;

    let response = router
        .oneshot(bare_request(
            "GET",
            &format!("/oagw/v1/proxy/{alias}/data"),
            tenant_id,
            &[("origin", "https://evil.example")],
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response.headers().get("x-oagw-error-source").unwrap(),
        "gateway"
    );
    assert_eq!(response.headers().get("vary").unwrap(), "Origin");
    let json = response_json(response).await;
    assert_eq!(
        json["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
    );
}

/// Acceptance criterion 4: the method-checking counterpart of the previous
/// test, same "no upstream connection opened" proof via an unused port.
#[tokio::test]
async fn disallowed_method_is_rejected_403_before_any_upstream_connection_is_opened() {
    let router = build_router(true).await;
    let tenant_id = Uuid::new_v4();
    let port = unused_port().await;
    let (alias, upstream_id) = create_cors_upstream(
        &router,
        tenant_id,
        "cors-method-reject-svc",
        port,
        json!({
            "enabled": true,
            "allowed_origins": ["https://good.example"],
            "allowed_methods": ["GET"],
        }),
    )
    .await;
    // The route itself matches both methods; only the *CORS* config
    // restricts cross-origin access to `GET`.
    create_route_with_methods(&router, tenant_id, upstream_id, "/data", &["GET", "DELETE"]).await;

    let response = router
        .oneshot(bare_request(
            "DELETE",
            &format!("/oagw/v1/proxy/{alias}/data"),
            tenant_id,
            &[("origin", "https://good.example")],
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let json = response_json(response).await;
    assert_eq!(
        json["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
    );
}

/// Acceptance criteria 2 and 5: an allowed origin/method reaches the real
/// upstream, and the final response the browser receives carries
/// `Access-Control-Allow-Origin`, `Access-Control-Expose-Headers`,
/// `Access-Control-Allow-Credentials: true` and `Vary: Origin` --
/// end-to-end through the real router and a real upstream, not just the
/// header-injection function in isolation
/// (`cpt-cf-oagw-dod-cors-response-headers`).
#[tokio::test]
async fn allowed_cross_origin_request_is_forwarded_and_the_response_carries_documented_cors_headers()
 {
    let server = MockServer::start();
    let _m = server.mock(|when, then| {
        when.method(GET).path("/hello");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"ok":true}"#);
    });

    let router = build_router(true).await;
    let tenant_id = Uuid::new_v4();
    let (alias, upstream_id) = create_cors_upstream(
        &router,
        tenant_id,
        "cors-allowed-svc",
        server.port(),
        json!({
            "enabled": true,
            "allowed_origins": ["https://app.example.com"],
            "allowed_methods": ["GET"],
            "expose_headers": ["X-Request-Id"],
            "allow_credentials": true,
        }),
    )
    .await;
    create_get_route(&router, tenant_id, upstream_id, "/hello").await;

    let response = router
        .oneshot(bare_request(
            "GET",
            &format!("/oagw/v1/proxy/{alias}/hello"),
            tenant_id,
            &[("origin", "https://app.example.com")],
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .unwrap(),
        "https://app.example.com"
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-expose-headers")
            .unwrap(),
        "X-Request-Id"
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-credentials")
            .unwrap(),
        "true"
    );
    assert_eq!(response.headers().get("vary").unwrap(), "Origin");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), br#"{"ok":true}"#);
}

/// Acceptance criterion 6 / Security Defaults DoD: with the effective
/// `cors.enabled` resolving to `false` (no `cors` object declared at all),
/// no `Access-Control-*`/`Vary` header is added and the request forwards
/// exactly as an ordinary, non-CORS request would -- even carrying an
/// `Origin` header that would otherwise be rejected.
#[tokio::test]
async fn cors_disabled_upstream_adds_no_cors_headers_and_skips_validation_entirely() {
    let server = MockServer::start();
    let _m = server.mock(|when, then| {
        when.method(GET).path("/plain");
        then.status(200).body("plain-ok");
    });

    let router = build_router(true).await;
    let tenant_id = Uuid::new_v4();
    let body = json!({
        "alias": "cors-disabled-svc",
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": server.port() } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    let response = router
        .clone()
        .oneshot(json_request("POST", "/oagw/v1/upstreams", tenant_id, body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = response_json(response).await;
    let upstream_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    create_get_route(&router, tenant_id, upstream_id, "/plain").await;

    let response = router
        .oneshot(bare_request(
            "GET",
            "/oagw/v1/proxy/cors-disabled-svc/plain",
            tenant_id,
            // An origin that would be rejected under any enabled `cors`
            // configuration, to prove validation genuinely never runs.
            &[("origin", "https://anything.example")],
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );
    assert!(response.headers().get("vary").is_none());
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), b"plain-ok");
}

/// `cpt-cf-oagw-algo-cors-merge-effective-config` (Acceptance Criteria 9
/// and 10, both unchecked in `docs/features/cors-handling.md`): the
/// ancestor-chain merge is genuinely unreachable from this external test
/// crate. `crate::proxy::merge::merge_cors` is `pub(crate)`, and
/// `OagwGear::register_rest` (the real router this file otherwise drives)
/// always wires `crate::proxy::hierarchy::NoTenantHierarchy`, which reports
/// zero ancestors for every tenant -- so there is no way, even through the
/// fully public gear entry point, to construct the >1-level ancestor chain
/// these two criteria describe. The `TenantHierarchyProvider` fake needed
/// to do so is itself `pub(crate)`, used only by each management
/// submodule's own inline `#[cfg(test)]` module.
///
/// This is recorded as a coverage gap, not a defect: by code inspection,
/// `merge_cors`'s `Sharing::Enforce` branch sets a `locked` flag that skips
/// every subsequent (more specific) level -- including the resolving
/// tenant's own last-level contribution, which is otherwise unconditionally
/// unioned in -- which is the documented "hold fixed" semantics (AC10), and
/// its `Sharing::Inherit` branch unions `allowed_origins` with the running
/// effective value while overriding the other fields (AC9). No test in this
/// crate (inline or external) exercises either branch; see this file's
/// module doc comment and the final report for the full reasoning.
#[test]
fn hierarchical_cors_merge_is_untestable_from_this_external_crate_see_doc_comment() {
    // Intentionally empty: this test exists so `cargo test`'s output lists
    // the gap by name, next to the passing tests above, rather than only in
    // prose. See the doc comment immediately above and this file's module
    // doc comment for the full reasoning; see the task report for the
    // acceptance-criteria checklist this maps to.
}
