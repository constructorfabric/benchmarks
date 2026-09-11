//! Unit tests for the domain model's wire shapes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn scheme_accepts_all_documented_values() {
    for name in ["http", "https", "wss", "wt", "grpc"] {
        let scheme: EndpointScheme = serde_json::from_value(serde_json::json!(name))
            .unwrap_or_else(|e| panic!("{name} must parse: {e}"));
        assert_eq!(
            serde_json::to_value(scheme).unwrap(),
            serde_json::json!(name)
        );
    }
}

#[test]
fn scheme_rejects_unknown_values() {
    assert!(serde_json::from_value::<EndpointScheme>(serde_json::json!("ftp")).is_err());
    assert!(serde_json::from_value::<EndpointScheme>(serde_json::json!("HTTPS")).is_err());
}

#[test]
fn endpoint_defaults_scheme_to_https_and_port_to_443() {
    let ep: Endpoint =
        serde_json::from_value(serde_json::json!({"host": "api.openai.com"})).unwrap();
    assert_eq!(ep.scheme, EndpointScheme::Https);
    assert_eq!(ep.port, 443);
}

#[test]
fn endpoint_normalises_host_case_and_trailing_dot() {
    let ep = Endpoint {
        scheme: EndpointScheme::Https,
        host: "API.OpenAI.COM.".to_owned(),
        port: 443,
    };
    assert_eq!(ep.normalised_host(), "api.openai.com");
}

#[test]
fn standard_ports_are_recognised_per_scheme() {
    assert!(EndpointScheme::Http.is_standard_port(80));
    assert!(EndpointScheme::Https.is_standard_port(443));
    assert!(EndpointScheme::Wss.is_standard_port(443));
    assert!(EndpointScheme::Wt.is_standard_port(443));
    assert!(EndpointScheme::Grpc.is_standard_port(443));
    assert!(!EndpointScheme::Https.is_standard_port(8443));
    assert_eq!(EndpointScheme::Http.standard_port(), 80);
}

#[test]
fn upstream_rejects_unknown_fields() {
    let body = serde_json::json!({
        "protocol": crate::gts_helpers::PROTOCOL_HTTP,
        "server": {"endpoints": [{"host": "api.openai.com"}]},
        "unexpected": true
    });
    assert!(serde_json::from_value::<Upstream>(body).is_err());
}

#[test]
fn upstream_requires_server_and_protocol() {
    assert!(
        serde_json::from_value::<Upstream>(serde_json::json!({"server": {"endpoints": []}}))
            .is_err()
    );
    let body = serde_json::json!({
        "protocol": crate::gts_helpers::PROTOCOL_HTTP,
        "server": {"endpoints": [{"host": "api.openai.com"}]}
    });
    let upstream: Upstream = serde_json::from_value(body).unwrap();
    assert!(upstream.enabled);
    assert!(upstream.created_at.is_none());
}

#[test]
fn route_requires_upstream_and_match() {
    assert!(
        serde_json::from_value::<Route>(serde_json::json!({"id": "x", "tenant_id": Uuid::nil()}))
            .is_err()
    );
    let body = serde_json::json!({
        "id": "r",
        "tenant_id": Uuid::nil(),
        "upstream_id": "u",
        "match": {"http": {"methods": ["GET"], "path": "/v1/chat"}}
    });
    let route: Route = serde_json::from_value(body).unwrap();
    assert_eq!(
        route.http_match().map(|m| m.path.as_str()),
        Some("/v1/chat")
    );
    assert!(route.grpc_match().is_none());
    assert_eq!(route.priority, 0);
    assert!(route.enabled);
}

#[test]
fn route_match_accepts_the_grpc_shape() {
    let body = serde_json::json!({
        "id": "r", "tenant_id": Uuid::nil(), "upstream_id": "u",
        "match": {"grpc": {"service": "foo.v1.UserService", "method": "GetUser"}}
    });
    let route: Route = serde_json::from_value(body).unwrap();
    assert!(route.http_match().is_none());
    assert_eq!(
        route.grpc_match().map(|m| m.method.as_str()),
        Some("GetUser")
    );
}

#[test]
fn http_match_defaults_suffix_mode_to_append() {
    let m: HttpMatch =
        serde_json::from_value(serde_json::json!({"methods": ["GET"], "path": "/x"})).unwrap();
    assert_eq!(m.path_suffix_mode, PathSuffixMode::Append);
    assert!(m.query_allowlist.is_empty());
}

#[test]
fn http_match_rejects_unknown_method_names() {
    assert!(serde_json::from_value::<HttpMethod>(serde_json::json!("HEAD")).is_err());
    assert_eq!(HttpMethod::parse("PATCH"), Some(HttpMethod::Patch));
    assert_eq!(HttpMethod::parse("OPTIONS"), None);
}

#[test]
fn rate_limit_defaults_and_capacity() {
    let rl: RateLimit =
        serde_json::from_value(serde_json::json!({"sustained": {"rate": 10}})).unwrap();
    assert_eq!(rl.sustained.window, RateWindow::Second);
    assert_eq!(rl.capacity(), 10);
    assert_eq!(rl.cost, 1);
    assert_eq!(rl.scope, RateScope::Tenant);
    assert_eq!(rl.strategy, RateStrategy::Reject);
    assert_eq!(rl.refill_interval(), std::time::Duration::from_millis(100));
}

#[test]
fn rate_limit_capacity_defaults_to_sustained_rate() {
    let rl: RateLimit = serde_json::from_value(serde_json::json!(
        {"sustained": {"rate": 5, "window": "minute"}, "burst": {"capacity": 20}}
    ))
    .unwrap();
    assert_eq!(rl.capacity(), 20);
    assert_eq!(rl.refill_interval(), std::time::Duration::from_secs(12));
}

#[test]
fn cors_origin_and_method_checks() {
    let cors: Cors = serde_json::from_value(serde_json::json!({
        "enabled": true,
        "allowed_origins": ["https://app.example"],
        "allowed_methods": ["GET", "POST"]
    }))
    .unwrap();
    assert!(cors.allows_origin("https://app.example"));
    assert!(!cors.allows_origin("https://other.example"));
    assert!(cors.allows_method("get"));
    assert!(!cors.allows_method("DELETE"));
    assert!(!cors.allow_credentials);
}

#[test]
fn cors_allows_wildcard_origin() {
    let cors: Cors = serde_json::from_value(serde_json::json!({
        "enabled": true, "allowed_origins": ["*"]
    }))
    .unwrap();
    assert!(cors.allows_origin("https://anything.example"));
}

#[test]
fn plugin_refs_accept_all_three_shapes() {
    let refs: Vec<PluginRef> = serde_json::from_value(serde_json::json!([
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
        Uuid::nil(),
        {"plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
         "config": {"require": ["x-trace-id"]}}
    ]))
    .unwrap();
    assert_eq!(refs.len(), 3);
    assert!(matches!(refs[0], PluginRef::GtsId(_)));
    assert!(matches!(refs[1], PluginRef::Uuid(_)));
    assert!(matches!(refs[2], PluginRef::Configured { .. }));

    let configured = match &refs[2] {
        PluginRef::Configured { plugin_ref, config } => (plugin_ref.clone(), config.clone()),
        _ => unreachable!(),
    };
    assert_eq!(
        configured.0,
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
    );
    assert_eq!(configured.1["require"], serde_json::json!(["x-trace-id"]));
    assert_eq!(refs[2].id(), configured.0);
    assert_eq!(refs[2].config(), configured.1);
    assert!(refs[0].config().is_null());
}

#[test]
fn plugin_kind_maps_to_its_type_prefix() {
    assert_eq!(
        PluginKind::Auth.type_prefix(),
        crate::gts_helpers::AUTH_PLUGIN_TYPE
    );
    assert_eq!(
        PluginKind::Guard.type_prefix(),
        crate::gts_helpers::GUARD_PLUGIN_TYPE
    );
    assert_eq!(
        PluginKind::Transform.type_prefix(),
        crate::gts_helpers::TRANSFORM_PLUGIN_TYPE
    );
}

#[test]
fn domain_error_displays_its_variant() {
    assert_eq!(
        DomainError::NotFound {
            kind: "upstream".to_owned(),
            target: "x".to_owned()
        }
        .to_string(),
        "no upstream for x"
    );
    assert!(
        DomainError::Invalid("bad".to_owned())
            .to_string()
            .contains("bad")
    );
}

#[test]
fn sharing_mode_and_passthrough_default_correctly() {
    assert_eq!(SharingMode::default(), SharingMode::Private);
    assert_eq!(PassthroughMode::default(), PassthroughMode::None);
}
