#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use super::*;
use crate::domain::model::{Endpoint, Protocol, ServerConfig, Upstream};
use std::collections::BTreeMap;

#[test]
fn gts_base_types_are_prefixed_and_typed() {
    for base in [
        UPSTREAM_TYPE,
        ROUTE_TYPE,
        AUTH_PLUGIN_TYPE,
        GUARD_PLUGIN_TYPE,
        TRANSFORM_PLUGIN_TYPE,
    ] {
        assert!(base.starts_with("gts."), "{base}");
        assert!(base.ends_with(".v1~"), "{base}");
    }
}

#[test]
fn resource_gts_id_composes_type_and_uuid() {
    let id = uuid::Uuid::from_u128(0x1234);
    assert_eq!(
        resource_gts_id(UPSTREAM_TYPE, id),
        format!("{UPSTREAM_TYPE}{id}")
    );
}

#[test]
fn protocol_serializes_to_the_schema_gts_ids() {
    assert_eq!(
        serde_json::to_value(Protocol::Http).unwrap(),
        serde_json::json!(PROTOCOL_HTTP)
    );
    assert_eq!(
        serde_json::to_value(Protocol::Grpc).unwrap(),
        serde_json::json!("gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")
    );
    assert_eq!(
        serde_json::from_value::<Protocol>(serde_json::json!(PROTOCOL_HTTP)).unwrap(),
        Protocol::Http
    );
}

#[test]
fn rfc3339_formatter_matches_known_instants() {
    assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(format_rfc3339(86_399), "1970-01-01T23:59:59Z");
    assert_eq!(format_rfc3339(86_400), "1970-01-02T00:00:00Z");
    assert_eq!(format_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
    assert_eq!(format_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(format_rfc3339(1_234_567_890), "2009-02-13T23:31:30Z");
}

#[test]
fn now_epoch_secs_is_sane() {
    let now = now_epoch_secs();
    assert!(now > 1_700_000_000, "clock far in the past: {now}");
}

#[test]
fn endpoint_defaults_apply() {
    let endpoint: Endpoint =
        serde_json::from_str(r#"{"scheme":"https","host":"api.example.com"}"#).expect("endpoint");
    assert_eq!(endpoint.port, 443);
    assert_eq!(endpoint.scheme, EndpointScheme::Https);

    let bare: Endpoint =
        serde_json::from_str(r#"{"host":"api.openai.com","scheme":"https"}"#).expect("endpoint");
    assert_eq!(bare.port, 443);
}

#[test]
fn endpoint_rejects_unknown_fields() {
    let err = serde_json::from_value::<Endpoint>(serde_json::json!({
        "scheme": "https",
        "host": "api.openai.com",
        "proto": "h2"
    }))
    .expect_err("unknown key must be refused");
    assert!(err.to_string().contains("unknown field"), "{err}");
}

#[test]
fn upstream_round_trips() {
    let upstream = Upstream {
        id: uuid::Uuid::new_v4(),
        tenant_id: uuid::Uuid::new_v4(),
        alias: "api.openai.com".to_owned(),
        protocol: Protocol::Http,
        enabled: true,
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Https,
                host: "api.openai.com".to_owned(),
                port: 443,
            }],
        },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec!["llm".to_owned()],
        created_at: "1970-01-01T00:00:00Z".to_owned(),
        updated_at: "1970-01-01T00:00:00Z".to_owned(),
    };
    let json = serde_json::to_value(&upstream).unwrap();
    let back: Upstream = serde_json::from_value(json).unwrap();
    assert_eq!(back, upstream);
}

#[test]
fn header_rules_deny_unknown_fields() {
    let err = serde_json::from_value::<HeadersConfig>(serde_json::json!({
        "request": { "nope": 1 }
    }))
    .expect_err("unknown key must be refused");
    assert!(err.to_string().contains("unknown field"), "{err}");

    let ok: HeadersConfig = serde_json::from_value(serde_json::json!({
        "request": { "set": { "x-trace": "1" }, "passthrough": "allowlist" }
    }))
    .expect("valid headers config");
    let rules = ok.request.expect("request rules");
    assert_eq!(
        rules.set,
        BTreeMap::from([("x-trace".to_owned(), "1".to_owned())])
    );
    assert_eq!(rules.passthrough, Some(Passthrough::Allowlist));
}

/// `DESIGN` §"Standard ports": HTTP 80, HTTPS/WSS/WebTransport/gRPC 443. The
/// cleartext spellings exist so the data plane can address a plaintext
/// upstream when the deployment allows it; the management API keeps
/// requiring `https` endpoints (`DESIGN` §2.2).
#[test]
fn scheme_ports_tls_and_http_capabilities_are_tabulated() {
    let expected = [
        (EndpointScheme::Https, "https", 443, true, true),
        (EndpointScheme::Wss, "wss", 443, true, true),
        (EndpointScheme::Wt, "wt", 443, true, false),
        (EndpointScheme::Grpc, "grpc", 443, true, false),
        (EndpointScheme::Http, "http", 80, false, true),
        (EndpointScheme::Ws, "ws", 80, false, true),
    ];
    for (scheme, spelling, port, tls, http) in expected {
        assert_eq!(scheme.as_str(), spelling, "{scheme:?}");
        assert_eq!(scheme.standard_port(), port, "{scheme:?}");
        assert_eq!(scheme.is_tls(), tls, "{scheme:?}");
        assert_eq!(scheme.is_http(), http, "{scheme:?}");
    }
}
