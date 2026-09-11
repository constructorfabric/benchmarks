#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Unit tests for endpoint selection (DESIGN §Multi-Endpoint Load Balancing, ADR-0001).

use std::sync::Arc;

use serde_json::json;
use uuid::Uuid;

use super::*;

fn endpoint(scheme: crate::domain::model::Scheme, host: &str, port: u16) -> Endpoint {
    Endpoint { scheme, host: host.to_string(), port: Some(port) }
}

fn upstream(alias: &str, endpoints: Vec<Endpoint>) -> Upstream {
    serde_json::from_value::<Upstream>(serde_json::json!({
        "id": "00000000-0000-4000-8000-000000000001",
        "tenant_id": "00000000-0000-0000-0000-000000000000",
        "alias": alias,
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": endpoints },
    }))
    .expect("minimal upstream")
}

fn plane() -> DataPlane {
    DataPlane::new(
        Arc::new(crate::infra::storage::memory::MemoryStores::new()),
        AuthPluginRegistry::with_builtins(Arc::new(crate::infra::credentials::MissingResolver)),
        GuardPluginRegistry::with_builtins(),
        TransformPluginRegistry::with_builtins(),
        DataPlaneSettings { allow_http_upstream: true, ..DataPlaneSettings::default() },
        None,
    )
}

#[tokio::test]
async fn a_single_endpoint_is_used_regardless_of_the_header() {
    let plane = plane();
    let up = upstream("single.example.com", vec![endpoint(crate::domain::model::Scheme::Http, "127.0.0.1", 18099)]);
    let chosen = plane.select_endpoint(&up, Some("not-a-configured-host")).unwrap();
    assert_eq!(chosen.host, "127.0.0.1");
}

#[tokio::test]
async fn an_explicit_alias_balances_over_the_pool_without_a_header() {
    let plane = plane();
    let up = upstream(
        "pool.internal",
        vec![
            endpoint(crate::domain::model::Scheme::Http, "10.0.0.1", 8080),
            endpoint(crate::domain::model::Scheme::Http, "10.0.0.2", 8080),
        ],
    );
    assert_eq!(plane.select_endpoint(&up, None).unwrap().host, "10.0.0.1");
    assert_eq!(plane.select_endpoint(&up, None).unwrap().host, "10.0.0.2");
    assert_eq!(plane.select_endpoint(&up, None).unwrap().host, "10.0.0.1", "the counter wraps");
}

#[tokio::test]
async fn an_explicit_alias_honours_a_matching_target_host() {
    let plane = plane();
    let up = upstream(
        "pool.internal",
        vec![
            endpoint(crate::domain::model::Scheme::Http, "10.0.0.1", 8080),
            endpoint(crate::domain::model::Scheme::Http, "10.0.0.2", 8080),
        ],
    );
    let chosen = plane.select_endpoint(&up, Some("10.0.0.2")).unwrap();
    assert_eq!(chosen.host, "10.0.0.2");
}

#[tokio::test]
async fn an_unknown_target_host_is_rejected_for_an_explicit_alias() {
    let plane = plane();
    let up = upstream(
        "pool.internal",
        vec![
            endpoint(crate::domain::model::Scheme::Http, "10.0.0.1", 8080),
            endpoint(crate::domain::model::Scheme::Http, "10.0.0.2", 8080),
        ],
    );
    let error = plane.select_endpoint(&up, Some("10.0.0.9")).unwrap_err();
    assert!(matches!(error, crate::domain::error::DomainError::TargetHostUnknown(_)), "{error}");
}

#[tokio::test]
async fn a_common_suffix_alias_requires_the_header() {
    let plane = plane();
    let up = upstream(
        "vendor.com",
        vec![
            endpoint(crate::domain::model::Scheme::Http, "us.vendor.com", 443),
            endpoint(crate::domain::model::Scheme::Http, "eu.vendor.com", 443),
        ],
    );
    let error = plane.select_endpoint(&up, None).unwrap_err();
    assert!(matches!(error, crate::domain::error::DomainError::TargetHostRequired(_)), "{error}");
}

#[tokio::test]
async fn a_malformed_target_host_is_rejected() {
    let plane = plane();
    let up = upstream(
        "vendor.com",
        vec![
            endpoint(crate::domain::model::Scheme::Http, "us.vendor.com", 443),
            endpoint(crate::domain::model::Scheme::Http, "eu.vendor.com", 443),
        ],
    );
    let error = plane.select_endpoint(&up, Some("us.vendor.com:8080")).unwrap_err();
    assert!(matches!(error, crate::domain::error::DomainError::TargetHostInvalid(_)), "{error}");
}

#[tokio::test]
async fn a_target_host_outside_the_pool_is_rejected() {
    let plane = plane();
    let up = upstream(
        "vendor.com",
        vec![
            endpoint(crate::domain::model::Scheme::Http, "us.vendor.com", 443),
            endpoint(crate::domain::model::Scheme::Http, "eu.vendor.com", 443),
        ],
    );
    let error = plane.select_endpoint(&up, Some("apac.vendor.com")).unwrap_err();
    assert!(matches!(error, crate::domain::error::DomainError::TargetHostUnknown(_)), "{error}");
}

#[tokio::test]
async fn a_common_suffix_alias_honours_a_configured_target_host() {
    let plane = plane();
    let up = upstream(
        "vendor.com",
        vec![
            endpoint(crate::domain::model::Scheme::Http, "us.vendor.com", 443),
            endpoint(crate::domain::model::Scheme::Http, "eu.vendor.com", 443),
        ],
    );
    assert_eq!(plane.select_endpoint(&up, Some("eu.vendor.com")).unwrap().host, "eu.vendor.com");
}

const ANCESTOR: Uuid = Uuid::from_u128(0x1000);
const DESCENDANT: Uuid = Uuid::from_u128(0x2000);

fn limit(rate: u32, capacity: u32, sharing: Option<crate::domain::model::SharingMode>) -> serde_json::Value {
    let mut body = json!({
        "sustained": {"rate": rate, "window": "minute"},
        "burst": {"capacity": capacity},
    });
    if let Some(sharing) = sharing {
        body["sharing"] = json!(sharing);
    }
    body
}

fn store_alias(plane: &DataPlane, tenant: Uuid, index: u64, alias: &str, rate_limit: serde_json::Value) {
    let mut upstream = upstream(alias, vec![endpoint(crate::domain::model::Scheme::Http, "127.0.0.1", 18100)]);
    upstream.id = Uuid::from_u128(
        0x0000_0000_0000_4000_8000_0000_0000_0000 | ((tenant.as_u128() & 0xFFFF) << 8) | u128::from(index),
    );
    upstream.tenant_id = tenant;
    upstream.rate_limit =
        Some(serde_json::from_value::<crate::domain::model::RateLimitConfig>(rate_limit).unwrap());
    plane.stores().upstreams.insert(upstream).unwrap();
}

#[tokio::test]
async fn an_ancestor_that_enforces_its_limit_joins_the_merge() {
    let plane = plane();
    store_alias(
        &plane,
        ANCESTOR,
        1,
        "shadowed.example.com",
        limit(5, 5, Some(crate::domain::model::SharingMode::Enforce)),
    );
    store_alias(
        &plane,
        DESCENDANT,
        1,
        "shadowed.example.com",
        limit(1000, 1000, Some(crate::domain::model::SharingMode::Private)),
    );

    let enforced = plane.enforced_ancestor_limits(DESCENDANT, &[ANCESTOR], "shadowed.example.com");
    assert_eq!(enforced.len(), 1, "the enforcing ancestor is collected");
    assert_eq!(enforced[0].rate, 5);

    // The descendant's own upstream shadows the ancestor's, and its stricter ancestor limit is
    // what the merge keeps.
    let selected = plane.resolve_alias(DESCENDANT, &[ANCESTOR], "shadowed.example.com").unwrap();
    let merged = crate::infra::ratelimit::EffectiveLimit::merge(
        selected.rate_limit.as_ref().map(crate::infra::ratelimit::effective_limit).as_ref(),
        enforced.first(),
    )
    .unwrap();
    assert_eq!(merged.rate, 5, "the ancestor's stricter limit wins");
}

#[tokio::test]
async fn an_ancestor_with_a_private_limit_is_not_enforced_across_shadowing() {
    let plane = plane();
    store_alias(&plane, ANCESTOR, 1, "open.example.com", limit(5, 5, Some(crate::domain::model::SharingMode::Private)));
    store_alias(&plane, DESCENDANT, 1, "open.example.com", limit(1000, 1000, None));

    let enforced = plane.enforced_ancestor_limits(DESCENDANT, &[ANCESTOR], "open.example.com");
    assert!(enforced.is_empty(), "a private ancestor limit stays with its own tenant");
}

fn framing(headers: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        map.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            axum::http::HeaderValue::from_str(value).unwrap(),
        );
    }
    map
}

#[test]
fn a_body_that_agrees_with_its_declared_length_is_accepted() {
    let headers = framing(&[("content-length", "11")]);
    assert!(validate_body(&headers, 11, 1024).is_ok());
}

#[test]
fn a_transfer_encoding_other_than_chunked_is_rejected() {
    let headers = framing(&[("transfer-encoding", "gzip")]);
    let error = validate_body(&headers, 0, 1024).unwrap_err();
    assert!(matches!(error, crate::domain::error::DomainError::Validation(_)), "{error}");
}

#[test]
fn a_malformed_content_length_is_rejected() {
    let headers = framing(&[("content-length", "five")]);
    let error = validate_body(&headers, 0, 1024).unwrap_err();
    assert!(matches!(error, crate::domain::error::DomainError::Validation(_)), "{error}");
}

#[test]
fn a_content_length_that_disagrees_with_the_body_is_rejected() {
    let headers = framing(&[("content-length", "5")]);
    let error = validate_body(&headers, 20, 1024).unwrap_err();
    assert!(matches!(error, crate::domain::error::DomainError::Validation(_)), "{error}");
}

#[test]
fn a_body_over_the_ceiling_is_payload_too_large() {
    let error = validate_body(&framing(&[]), 4096, 1024).unwrap_err();
    assert!(matches!(error, crate::domain::error::DomainError::PayloadTooLarge(_)), "{error}");
}
