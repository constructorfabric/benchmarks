//! Tests for the `Upstream` entity.

use uuid::Uuid;

use crate::domain::upstream::{
    AuthConfig, CorsConfig, Endpoint, HeadersConfig, Server, Upstream,
};

fn endpoint(scheme: &str, host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: scheme.to_owned(),
        host: host.to_owned(),
        port,
    }
}

#[test]
fn the_endpoint_triples_are_the_pool_in_order() {
    let mut upstream = Upstream::default();
    upstream.server = Server {
        endpoints: vec![
            endpoint("https", "a.example.com", 443),
            endpoint("https", "b.example.com", 8443),
        ],
    };
    assert_eq!(
        upstream.endpoint_triples(),
        vec![
            ("a.example.com".to_owned(), 443u16, "https"),
            ("b.example.com".to_owned(), 8443u16, "https"),
        ]
    );
}

#[test]
fn a_single_host_upstream_derives_its_alias() {
    let upstream = Upstream {
        server: Server {
            endpoints: vec![endpoint("https", "api.example.com", 443)],
        },
        ..Upstream::default()
    };
    assert_eq!(upstream.derived_alias().as_deref(), Some("api.example.com"));
}

#[test]
fn an_ip_literal_pool_derives_nothing() {
    let upstream = Upstream {
        server: Server {
            endpoints: vec![endpoint("https", "10.0.0.7", 443)],
        },
        ..Upstream::default()
    };
    assert_eq!(upstream.derived_alias(), None);
}

#[test]
fn an_upstream_is_enabled_by_default() {
    let upstream = Upstream::default();
    assert!(upstream.enabled);
    assert!(upstream.cors.is_none());
    assert!(upstream.auth.is_none());
    assert!(upstream.rate_limit.is_none());
    assert!(upstream.protocol.is_empty());
}

#[test]
fn the_host_of_an_endpoint_normalizes_to_lower_case() {
    let endpoint = endpoint("https", "API.Example.COM.", 443);
    assert_eq!(endpoint.normalized_host(), "api.example.com");
}

#[test]
fn the_alias_is_immutable_across_serialization() {
    let upstream = Upstream {
        tenant_id: Uuid::nil(),
        alias: "api.example.com".to_owned(),
        ..Upstream::default()
    };
    let json = serde_json::to_value(&upstream).expect("serializes");
    let round: Upstream = serde_json::from_value(json).expect("deserializes");
    assert_eq!(round.alias, "api.example.com");
}

#[test]
fn an_auth_configuration_carries_only_a_reference() {
    let auth = AuthConfig {
        auth_type: "apikey".to_owned(),
        sharing: "tenant".to_owned(),
        config: serde_json::json!({ "credential": "cred://stripe-key" })
            .as_object()
            .cloned()
            .expect("an object"),
    };
    let rendered = serde_json::to_value(&auth).expect("serializes");
    // The config is redacted on the wire: a credential reference is configuration, not
    // material, but the whole block is withheld rather than selectively echoed.
    assert_eq!(rendered["type"], "apikey");
    assert_eq!(rendered["config"], "<redacted>");
    // Debug never carries it either way.
    let debug = format!("{auth:?}");
    assert!(
        !debug.contains("cred://"),
        "the debug output names the credential reference: {debug}"
    );
    assert!(debug.contains("<redacted>"), "{debug}");
}

#[test]
fn a_default_cors_policy_is_disabled() {
    let cors = CorsConfig::default();
    assert!(!cors.enabled);
    assert!(cors.allowed_origins.is_empty());
    assert_eq!(cors.allowed_methods, vec!["GET".to_owned(), "POST".to_owned()]);
}

#[test]
fn the_header_rules_default_to_full_passthrough() {
    let headers = HeadersConfig::default();
    assert!(matches!(
        headers.request.passthrough,
        crate::domain::upstream::PassthroughMode::All
    ));
    assert!(headers.request.set.is_empty());
    assert!(headers.response.set.is_empty());
}

#[test]
fn an_upstream_accepts_a_json_body_with_only_endpoints() {
    let parsed: Upstream = serde_json::from_str(
        r#"{ "server": { "endpoints": [ { "scheme": "https", "host": "api.example.com", "port": 443 } ] } }"#,
    )
    .expect("parses");
    assert_eq!(parsed.server.endpoints.len(), 1);
    assert!(parsed.enabled);
    assert_eq!(
        parsed.headers.request.passthrough,
        crate::domain::upstream::PassthroughMode::All
    );
}

#[test]
fn an_unknown_field_in_the_upstream_body_is_rejected() {
    let err = serde_json::from_str::<Upstream>(
        r#"{ "nonesense": true, "server": { "endpoints": [] } }"#,
    )
    .err()
    .expect("unknown fields are rejected");
    assert!(err.to_string().contains("unknown field"), "{err}");
}

#[test]
fn the_tenant_is_taken_from_the_context_not_the_body() {
    // `tenant_id` defaults to the nil UUID when the caller does not supply it, which is
    // what lets the store stamp the caller's tenant over it.
    let parsed: Upstream = serde_json::from_str(r#"{ "server": { "endpoints": [] } }"#)
        .expect("parses");
    assert_eq!(parsed.tenant_id, Uuid::nil());
}
