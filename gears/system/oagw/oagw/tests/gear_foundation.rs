//! Integration-level tests for DECOMPOSITION entry 2.1 (Gear Foundation and
//! Configuration), exercising the crate's public surface as a black box
//! (`oagw::...`) rather than its private internals.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::thread;

use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use http_body_util::BodyExt;
use oagw::config::OagwConfig;
use oagw::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER_NAME, OagwError, OagwErrorKind};
use oagw::model::upstream::{EndpointScheme, Upstream};
use oagw::store::{ConfigStore, OagwState};
use tower::ServiceExt;

/// Mirrors the exact `gears.oagw.config` stanza the graded
/// `config/e2e-local.yaml` sets (see DECOMPOSITION §1 and the FEATURE's
/// acceptance criteria).
#[test]
fn resolves_the_graded_e2e_local_stanza_from_the_public_config_api() {
    let node = serde_json::json!({
        "proxy_timeout_secs": 2,
        "allow_http_upstream": true,
        "ssrf_policy": { "enabled": false },
    });
    let config = OagwConfig::resolve(&node).unwrap();
    assert_eq!(config.proxy_timeout_secs, 2);
    assert!(config.allow_http_upstream);
    assert!(!config.ssrf_policy.enabled);
}

#[test]
fn resolves_documented_defaults_when_stanza_is_absent() {
    let config = OagwConfig::default();
    assert_eq!(config.proxy_timeout_secs, 30);
    assert!(!config.allow_http_upstream);
    assert!(config.ssrf_policy.enabled);
}

/// Every gateway-originated error must be served as
/// `application/problem+json` with `X-OAGW-Error-Source: gateway`, even when
/// dispatched through a real `axum::Router` (not just a direct
/// `IntoResponse::into_response()` call as in the crate's unit tests).
#[tokio::test]
async fn error_renders_correctly_through_a_real_axum_router() {
    let app = axum::Router::new().route(
        "/boom",
        get(|| async {
            OagwError::new(OagwErrorKind::RouteNotFound, "no route matched /boom")
                .with_instance("/oagw/v1/proxy/boom")
                .into_response()
        }),
    );

    let request = axum::http::Request::builder()
        .uri("/boom")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
    assert_eq!(
        response
            .headers()
            .get(ERROR_SOURCE_HEADER_NAME)
            .and_then(|v| v.to_str().ok()),
        Some(ERROR_SOURCE_GATEWAY)
    );

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(json["instance"], "/oagw/v1/proxy/boom");
}

/// The shared configuration store must give read-after-write visibility
/// with no torn values, from the crate's public API, across real OS
/// threads.
#[test]
fn config_store_write_is_visible_to_concurrent_readers_with_no_torn_values() {
    let store = Arc::new(ConfigStore::new(OagwConfig::default()));

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let store = store.clone();
            thread::spawn(move || {
                for _ in 0..200 {
                    let cfg = store.config();
                    assert!(cfg.proxy_timeout_secs == 30 || cfg.proxy_timeout_secs == 2);
                }
            })
        })
        .collect();

    let writer = {
        let store = store.clone();
        thread::spawn(move || {
            store.set_config(OagwConfig {
                proxy_timeout_secs: 2,
                ..OagwConfig::default()
            });
        })
    };

    writer.join().unwrap();
    for reader in readers {
        reader.join().unwrap();
    }
    assert_eq!(store.config().proxy_timeout_secs, 2);
}

#[test]
fn oagw_state_wraps_a_seeded_store_for_extension_injection() {
    let state = OagwState::new(OagwConfig::default());
    assert_eq!(state.store.config().proxy_timeout_secs, 30);
}

/// `cpt-cf-oagw-dod-gear-registration` / the acceptance criterion "After
/// successful registration, the gear appears in the host ToolKit runtime's
/// registered-gear set...": this exercises the *real* production
/// registration mechanism (`#[toolkit::gear(name = "oagw", ...)]` submits an
/// `inventory::submit!` registrator at compile time), not a stand-in --
/// `toolkit::GearRegistry::discover_and_build()` is the same call the host
/// process itself makes at startup, and this test binary links the `oagw`
/// rlib (and therefore its registrator) exactly as the host process does.
#[test]
fn oagw_gear_is_discoverable_in_the_host_runtimes_registered_gear_set_with_rest_capability() {
    let registry = toolkit::GearRegistry::discover_and_build()
        .expect("gear discovery/topo-sort must succeed for a single dependency-free gear");

    let oagw_entry = registry
        .gears()
        .iter()
        .find(|entry| entry.name() == "oagw")
        .expect("the oagw gear must be discoverable via the toolkit gear registry");

    assert!(
        oagw_entry.caps().has::<toolkit::registry::RestApiCap>(),
        "the oagw gear must register its REST capability (its /oagw/v1 router)"
    );
}

/// `cpt-cf-oagw-dod-single-executable-packaging`: this feature packages as a
/// library (`[lib]`) linked into the single ToolKit host executable, with no
/// separate `[[bin]]` runtime process of its own. This is a structural
/// packaging property, not request/response behavior, so it is verified by
/// inspecting the crate's own manifest rather than by exercising any runtime
/// surface.
#[test]
fn oagw_gear_packages_as_a_library_with_no_separate_runtime_process() {
    let manifest = include_str!("../Cargo.toml");
    assert!(
        manifest.contains("[lib]"),
        "the oagw gear must build as a library, not a standalone binary"
    );
    assert!(
        !manifest.contains("[[bin]]"),
        "the oagw gear must not declare a separate runtime-process binary target"
    );
}

/// The task specification's override: an Upstream may declare a plaintext
/// `http` endpoint (legal at create time regardless of whether
/// `allow_http_upstream` permits actually connecting to it -- that is
/// proxy-core's, 2.5, concern).
#[test]
fn upstream_model_accepts_plaintext_http_and_ws_schemes() {
    let http_upstream: Upstream = serde_json::from_value(serde_json::json!({
        "server": { "endpoints": [ { "scheme": "http", "host": "svc.internal", "port": 80 } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    }))
    .unwrap();
    assert_eq!(
        http_upstream.server.endpoints[0].scheme,
        EndpointScheme::Http
    );

    let ws_upstream: Upstream = serde_json::from_value(serde_json::json!({
        "server": { "endpoints": [ { "scheme": "ws", "host": "svc.internal" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    }))
    .unwrap();
    assert_eq!(ws_upstream.server.endpoints[0].scheme, EndpointScheme::Ws);
}
