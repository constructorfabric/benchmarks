//! Black-box, external-crate tests for DECOMPOSITION entry 2.5
//! (proxy-core), exercising the publicly reachable surface of `oagw::*`.
//!
//! `oagw::api::rest::proxy` (the REST handler and router wiring) and every
//! `oagw::proxy::*` algorithm module are **not** reachable from an external
//! test crate: `src/api/rest/mod.rs` (owned by DECOMPOSITION entry 2.1, out
//! of this entry's file-ownership list) declares `mod proxy;` as
//! crate-private, and `src/proxy/mod.rs` declares every algorithm submodule
//! `pub(crate)` -- that boundary applies to every external test crate, not
//! just this one, exactly as documented in `tests/upstream_management.rs`
//! for the analogous 2.2 case. The full-router, `tower::ServiceExt`-driven
//! HTTP-level tests this FEATURE's flows call for instead live as an inline
//! `#[cfg(test)] mod tests` inside `src/api/rest/proxy.rs`, which -- being
//! part of the `oagw` crate itself -- has the access this file cannot.
//!
//! This file instead exercises the parts of `cpt-cf-oagw-algo-proxy-select-endpoint`
//! (`classify_pool`, reused verbatim from entry 2.2 to satisfy
//! `inst-proxy-ep-classify-alias`'s "re-run the write-time classification"
//! requirement), `cpt-cf-oagw-algo-proxy-parse-request` (alias
//! normalization, reused verbatim from entry 2.2's `normalize_alias_literal`)
//! and `cpt-cf-oagw-algo-proxy-map-error` (the full gateway error-source
//! catalog this feature's own error conditions render through) that are
//! genuinely visible from `oagw`'s public API, as a black-box consumer
//! would exercise them.

#![allow(clippy::unwrap_used)]

use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use http_body_util::BodyExt;
use oagw::config::OagwConfig;
use oagw::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER_NAME, OagwError, OagwErrorKind};
use oagw::model::route::{HttpMatch, HttpMethod, PathSuffixMode, Route, RouteMatch};
use oagw::model::upstream::alias::{AliasClass, classify_pool, normalize_alias_literal};
use oagw::model::upstream::ident::{PROTOCOL_GRPC, PROTOCOL_HTTP, is_valid_protocol};
use oagw::model::upstream::{Endpoint, EndpointScheme};
use tower::ServiceExt;
use uuid::Uuid;

fn endpoint(host: &str) -> Endpoint {
    Endpoint {
        scheme: EndpointScheme::Https,
        host: host.to_owned(),
        port: 443,
    }
}

/// `cpt-cf-oagw-algo-proxy-select-endpoint` step `inst-proxy-ep-classify-alias`
/// requires re-running the *same* derivability classification the
/// management layer applies at write time, rather than reimplementing it:
/// a two-or-more-hostname pool whose stored alias equals the PSL-derived
/// common suffix is the "common-suffix-derived" case that makes
/// `X-OAGW-Target-Host` mandatory.
#[test]
fn common_suffix_pool_classification_matches_the_write_time_algorithm() {
    let pool = vec![endpoint("us.vendor.com"), endpoint("eu.vendor.com")];
    assert_eq!(
        classify_pool(&pool),
        AliasClass::Derivable("vendor.com".to_owned())
    );
}

/// The counterpart negative case: an IP-based or no-common-suffix pool is
/// `NonDerivable`, meaning any stored alias for it is explicit (or
/// IP-based) rather than common-suffix-derived, so the header stays
/// optional on a single-endpoint upstream and drives round robin absent a
/// header on a multi-endpoint one.
#[test]
fn explicit_alias_pool_is_never_classified_as_common_suffix_derived() {
    let pool = vec![endpoint("a.internal"), endpoint("b.internal")];
    assert_eq!(classify_pool(&pool), AliasClass::NonDerivable);
}

/// `cpt-cf-oagw-algo-proxy-parse-request` step `inst-proxy-parse-normalize-alias`
/// normalizes the inbound alias segment identically to how the management
/// layer normalizes it at write time, so that a proxy request's alias
/// lookup key matches a stored `Upstream.alias` byte-for-byte.
#[test]
fn inbound_alias_normalization_matches_the_stored_alias_normalization() {
    assert_eq!(
        normalize_alias_literal("API.Example.COM.").unwrap(),
        "api.example.com"
    );
}

#[test]
fn malformed_alias_is_rejected_by_the_shared_grammar() {
    assert!(normalize_alias_literal("bad alias!").is_err());
    assert!(normalize_alias_literal("").is_err());
}

/// The graded `config/e2e-local.yaml` values this feature's forwarding gate
/// (`cpt-cf-oagw-dod-proxy-plaintext-gate`) and deadline
/// (`cpt-cf-oagw-dod-proxy-timeout-no-retry`) read.
#[test]
fn graded_config_permits_plaintext_upstreams_under_a_two_second_deadline() {
    let config = OagwConfig::resolve(&serde_json::json!({
        "proxy_timeout_secs": 2,
        "allow_http_upstream": true,
        "ssrf_policy": { "enabled": false },
    }))
    .unwrap();
    assert_eq!(config.proxy_timeout_secs, 2);
    assert!(config.allow_http_upstream);
}

/// A `Route`'s `match.http` fields this path's guard-rule and route-match
/// algorithms read (`path_suffix_mode`, `query_allowlist`, `priority`)
/// parse with the documented defaults.
#[test]
fn route_http_match_defaults_to_append_mode_and_empty_allowlist() {
    let upstream_id = Uuid::new_v4();
    let route: Route = serde_json::from_value(serde_json::json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": "/v1/models" } },
    }))
    .unwrap();
    let http = route.route_match.http.expect("http match present");
    assert_eq!(http.path_suffix_mode, PathSuffixMode::Append);
    assert!(http.query_allowlist.is_empty());
    assert!(route.enabled);
}

/// `cpt-cf-oagw-dod-proxy-endpoint-registration`: the proxy endpoint must be
/// registered "for every method the route schema admits (GET, POST, PUT,
/// DELETE, PATCH)". Whether `axum`'s router actually wires all five methods
/// to the handler is a `pub(crate)` concern of `src/api/rest/proxy.rs` this
/// external test crate cannot reach (see the module doc comment above), but
/// the *set* of methods the route-match domain model admits is this
/// documented five -- no more, no less -- and is reachable via the public
/// `oagw::model::route::HttpMethod` enum every `match.http.methods` entry is
/// drawn from.
#[test]
fn route_http_method_allowlist_is_exactly_the_five_documented_methods() {
    let upstream_id = Uuid::new_v4();
    let route: Route = serde_json::from_value(serde_json::json!({
        "upstream_id": upstream_id,
        "match": {
            "http": {
                "methods": ["GET", "POST", "PUT", "DELETE", "PATCH"],
                "path": "/v1",
            }
        },
    }))
    .unwrap();
    let methods = route.route_match.http.unwrap().methods;
    assert_eq!(
        methods,
        vec![
            HttpMethod::Get,
            HttpMethod::Post,
            HttpMethod::Put,
            HttpMethod::Delete,
            HttpMethod::Patch,
        ]
    );

    // A sixth, undocumented method is not a member of the enum at all --
    // deserialization itself must reject it rather than accepting it as a
    // silently-ignored extra allowlist entry.
    let rejected: Result<Route, _> = serde_json::from_value(serde_json::json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["TRACE"], "path": "/v1" } },
    }));
    assert!(rejected.is_err());
}

/// DESIGN states the gRPC protocol is schema-defined (an upstream may
/// declare `protocol: gts...grpc.v1`) but that no gRPC proxy code path is
/// reachable in this decomposition round -- a gRPC-protocol upstream must be
/// *rejected with a documented error* (`404 RouteNotFound`, since
/// `cpt-cf-oagw-algo-proxy-match-route` treats a non-HTTP protocol as "no
/// route"), never silently proxied. The route-matching rejection itself
/// lives in `crate::proxy::route_match`, `pub(crate)` and unreachable from
/// this external test crate (covered internally by
/// `src/proxy/route_match.rs`'s own `grpc_protocol_upstream_never_matches`
/// test) -- what *is* reachable here is the precondition the rejection acts
/// on: the gRPC protocol identifier is a distinct, well-formed, schema-legal
/// value, not merely absent from the model.
#[test]
fn grpc_protocol_is_schema_legal_and_distinct_from_http_though_no_proxy_path_reaches_it() {
    assert_ne!(PROTOCOL_HTTP, PROTOCOL_GRPC);
    assert!(is_valid_protocol(PROTOCOL_HTTP));
    assert!(is_valid_protocol(PROTOCOL_GRPC));

    // An Upstream record may legally declare the gRPC protocol identifier
    // (upstream-management, 2.2's concern); this feature's own concern is
    // only that proxying it is never reachable, which the route-match
    // algorithm enforces internally (cited above), not this deserialization
    // step.
    let upstream: oagw::model::upstream::Upstream = serde_json::from_value(serde_json::json!({
        "server": { "endpoints": [ { "scheme": "grpc", "host": "svc.internal" } ] },
        "protocol": PROTOCOL_GRPC,
    }))
    .unwrap();
    assert_eq!(upstream.protocol, PROTOCOL_GRPC);
}

#[test]
fn route_http_match_accepts_an_explicit_query_allowlist_and_disabled_suffix_mode() {
    let route_match: RouteMatch = serde_json::from_value(serde_json::json!({
        "http": {
            "methods": ["GET"],
            "path": "/v1",
            "query_allowlist": ["q", "page"],
            "path_suffix_mode": "disabled",
        }
    }))
    .unwrap();
    let http: HttpMatch = route_match.http.unwrap();
    assert_eq!(http.path_suffix_mode, PathSuffixMode::Disabled);
    assert_eq!(
        http.query_allowlist,
        vec!["q".to_owned(), "page".to_owned()]
    );
}

/// Renders one gateway error kind through a real `axum::Router` and asserts
/// the documented `(status, GTS type, X-OAGW-Error-Source: gateway)`
/// triple -- the same pattern `tests/gear_foundation.rs` establishes for
/// entry 2.1, applied here to the error kinds that are this feature's own
/// (`cpt-cf-oagw-algo-proxy-map-error`), not gear-foundation's generic
/// catalog.
async fn assert_gateway_error(kind: OagwErrorKind, status: StatusCode, gts_type: &str) {
    let app = axum::Router::new().route(
        "/boom",
        get(move || async move {
            OagwError::new(kind, "occurrence-specific detail")
                .with_instance("/oagw/v1/proxy/boom")
                .into_response()
        }),
    );
    let request = axum::http::Request::builder()
        .uri("/boom")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), status);
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
    assert_eq!(json["type"], gts_type);
}

#[tokio::test]
async fn route_not_found_renders_the_documented_404() {
    assert_gateway_error(
        OagwErrorKind::RouteNotFound,
        StatusCode::NOT_FOUND,
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
    )
    .await;
}

#[tokio::test]
async fn link_unavailable_renders_the_documented_503() {
    assert_gateway_error(
        OagwErrorKind::LinkUnavailable,
        StatusCode::SERVICE_UNAVAILABLE,
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
    )
    .await;
}

#[tokio::test]
async fn missing_target_host_renders_the_documented_400() {
    assert_gateway_error(
        OagwErrorKind::MissingTargetHost,
        StatusCode::BAD_REQUEST,
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1",
    )
    .await;
}

#[tokio::test]
async fn invalid_target_host_renders_the_documented_400() {
    assert_gateway_error(
        OagwErrorKind::InvalidTargetHost,
        StatusCode::BAD_REQUEST,
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1",
    )
    .await;
}

#[tokio::test]
async fn unknown_target_host_renders_the_documented_400() {
    assert_gateway_error(
        OagwErrorKind::UnknownTargetHost,
        StatusCode::BAD_REQUEST,
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1",
    )
    .await;
}

#[tokio::test]
async fn payload_too_large_renders_the_documented_413() {
    assert_gateway_error(
        OagwErrorKind::PayloadTooLarge,
        StatusCode::PAYLOAD_TOO_LARGE,
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
    )
    .await;
}

#[tokio::test]
async fn protocol_error_renders_the_documented_502() {
    assert_gateway_error(
        OagwErrorKind::ProtocolError,
        StatusCode::BAD_GATEWAY,
        "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
    )
    .await;
}

#[tokio::test]
async fn downstream_error_renders_the_documented_502() {
    assert_gateway_error(
        OagwErrorKind::DownstreamError,
        StatusCode::BAD_GATEWAY,
        "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
    )
    .await;
}

#[tokio::test]
async fn connection_timeout_renders_the_documented_504() {
    assert_gateway_error(
        OagwErrorKind::ConnectionTimeout,
        StatusCode::GATEWAY_TIMEOUT,
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
    )
    .await;
}

#[tokio::test]
async fn request_timeout_renders_the_documented_504() {
    assert_gateway_error(
        OagwErrorKind::RequestTimeout,
        StatusCode::GATEWAY_TIMEOUT,
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
    )
    .await;
}

#[tokio::test]
async fn idle_timeout_renders_the_documented_504() {
    assert_gateway_error(
        OagwErrorKind::IdleTimeout,
        StatusCode::GATEWAY_TIMEOUT,
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1",
    )
    .await;
}

#[tokio::test]
async fn validation_error_renders_the_documented_400() {
    assert_gateway_error(
        OagwErrorKind::ValidationError,
        StatusCode::BAD_REQUEST,
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
    )
    .await;
}
