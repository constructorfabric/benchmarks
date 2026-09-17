use super::*;

fn endpoint(scheme: Scheme, host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme,
        host: host.to_owned(),
        port,
    }
}

fn http_upstream(endpoints: Vec<Endpoint>) -> UpstreamSpec {
    UpstreamSpec {
        server: ServerConfig { endpoints },
        ..UpstreamSpec::default()
    }
}

#[test]
fn scheme_standard_ports_follow_the_contract() {
    assert_eq!(Scheme::Http.standard_port(), 80);
    assert_eq!(Scheme::Https.standard_port(), 443);
    assert_eq!(Scheme::Wss.standard_port(), 443);
    assert_eq!(Scheme::Wt.standard_port(), 443);
    assert_eq!(Scheme::Grpc.standard_port(), 443);
    assert_eq!(Scheme::default(), Scheme::Https);
}

#[test]
fn endpoint_defaults_port_to_443() {
    let endpoint: Endpoint = serde_json::from_value(serde_json::json!({
        "scheme": "https", "host": "api.openai.com"
    }))
    .expect("valid endpoint");
    assert_eq!(endpoint.port, 443);
    assert_eq!(endpoint.authority(), "api.openai.com");
}

#[test]
fn http_is_a_legal_scheme_at_create_time() {
    // Wire-contract note W2: `{"scheme": "http", "port": 80}` must deserialize.
    let endpoint: Endpoint = serde_json::from_value(serde_json::json!({
        "scheme": "http", "host": "127.0.0.1", "port": 80
    }))
    .expect("http scheme must deserialize");
    assert_eq!(endpoint.scheme, Scheme::Http);
    assert_eq!(endpoint.port, 80);
    assert_eq!(endpoint.authority(), "127.0.0.1");
}

#[test]
fn protocol_deserializes_from_gts_ids() {
    let http: Protocol =
        serde_json::from_value(serde_json::json!(ids::PROTOCOL_HTTP)).expect("http protocol");
    let grpc: Protocol =
        serde_json::from_value(serde_json::json!(ids::PROTOCOL_GRPC)).expect("grpc protocol");
    assert_eq!(http, Protocol::Http);
    assert_eq!(grpc, Protocol::Grpc);
    assert_eq!(http.as_gts_id(), ids::PROTOCOL_HTTP);
}

#[test]
fn plugin_binding_accepts_bare_and_detailed_forms() {
    let bare: PluginBindingSpec =
        serde_json::from_value(serde_json::json!("abc-uuid")).expect("bare binding");
    let detailed: PluginBindingSpec = serde_json::from_value(serde_json::json!({
        "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
        "config": { "required_request_headers": "x-trace" }
    }))
    .expect("detailed binding");

    let normalized_bare = PluginBinding::from_spec(bare);
    assert_eq!(normalized_bare.plugin_ref, "abc-uuid");
    assert!(normalized_bare.config.is_null());

    let normalized_detailed = PluginBinding::from_spec(detailed);
    assert!(
        normalized_detailed
            .plugin_ref
            .ends_with("required_headers.v1")
    );
    assert_eq!(
        normalized_detailed.config["required_request_headers"],
        "x-trace"
    );
}

#[test]
fn rate_limit_defaults_match_the_schema() {
    let config: RateLimitConfig = serde_json::from_value(serde_json::json!({
        "sustained": { "rate": 10 }
    }))
    .expect("rate limit");
    assert_eq!(config.sharing, Sharing::Private);
    assert_eq!(config.algorithm, RateLimitAlgorithm::TokenBucket);
    assert_eq!(config.sustained.window, RateWindow::Second);
    assert_eq!(config.scope, RateLimitScope::Tenant);
    assert_eq!(config.strategy, RateLimitStrategy::Reject);
    assert_eq!(config.cost, 1);
    assert!(config.response_headers);
    assert_eq!(config.capacity(), 10);
    assert!((config.tokens_per_second() - 10.0).abs() < f64::EPSILON);
}

#[test]
fn burst_defaults_to_sustained_rate_when_absent() {
    let config: RateLimitConfig = serde_json::from_value(serde_json::json!({
        "sustained": { "rate": 5 }, "burst": { "capacity": 25 }
    }))
    .expect("rate limit");
    assert_eq!(config.capacity(), 25);
}

#[test]
fn cors_defaults_match_the_schema() {
    let config: CorsConfig =
        serde_json::from_value(serde_json::json!({ "enabled": true })).expect("cors config");
    assert_eq!(config.sharing, Sharing::Private);
    assert_eq!(config.allowed_methods, vec!["GET", "POST"]);
    assert!(!config.allow_credentials);
    assert!(!config.origin_allowed("https://example.com"));
    assert!(config.method_allowed("get"));
}

#[test]
fn cors_origin_and_method_checks() {
    let config: CorsConfig = serde_json::from_value(serde_json::json!({
        "enabled": true,
        "allowed_origins": ["https://app.example.com"],
        "allowed_methods": ["GET", "POST"],
        "allow_credentials": false
    }))
    .expect("cors config");
    assert!(config.origin_allowed("https://app.example.com"));
    assert!(!config.origin_allowed("http://app.example.com"));
    assert!(!config.method_allowed("DELETE"));

    let wildcard: CorsConfig = serde_json::from_value(serde_json::json!({
        "enabled": true, "allowed_origins": ["*"]
    }))
    .expect("wildcard cors");
    assert!(wildcard.origin_allowed("https://anything.example"));
    assert!(!wildcard.method_allowed("DELETE"));
}

#[test]
fn upstream_spec_defaults_enabled() {
    let spec: UpstreamSpec = serde_json::from_value(serde_json::json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
        "protocol": ids::PROTOCOL_HTTP
    }))
    .expect("upstream spec");
    assert!(spec.enabled);
    assert!(spec.auth.is_none());
    assert!(spec.rate_limit.is_none());
    assert_eq!(spec.headers, HeadersConfig::default());
}

#[test]
fn route_match_requires_exactly_one_discriminant() {
    let http: RouteMatch = serde_json::from_value(serde_json::json!({
        "http": { "methods": ["GET", "POST"], "path": "/v1/chat" }
    }))
    .expect("http match");
    match &http {
        RouteMatch::Http(match_rules) => {
            assert_eq!(match_rules.path, "/v1/chat");
            assert_eq!(match_rules.methods, vec!["GET", "POST"]);
            assert_eq!(match_rules.path_suffix_mode, PathSuffixMode::Append);
            assert!(match_rules.query_allowlist.is_empty());
        }
        RouteMatch::Grpc(_) => panic!("expected http match"),
    }
}

#[test]
fn route_match_deserializes_grpc() {
    let grpc: RouteMatch = serde_json::from_value(serde_json::json!({
        "grpc": { "service": "foo.v1.UserService", "method": "GetUser" }
    }))
    .expect("grpc match");
    match &grpc {
        RouteMatch::Grpc(match_rules) => {
            assert_eq!(match_rules.service, "foo.v1.UserService");
            assert_eq!(match_rules.method, "GetUser");
        }
        RouteMatch::Http(_) => panic!("expected grpc match"),
    }
}

#[test]
fn unknown_route_match_key_is_rejected() {
    let result: Result<RouteMatch, _> =
        serde_json::from_value(serde_json::json!({ "ws": { "path": "/x" } }));
    assert!(result.is_err());
}

#[test]
fn validate_host_accepts_and_normalizes() {
    assert_eq!(
        validate_host("API.OpenAI.com.").expect("valid").as_str(),
        "api.openai.com"
    );
    assert_eq!(
        validate_host("127.0.0.1").expect("valid").as_str(),
        "127.0.0.1"
    );
    assert_eq!(
        validate_host("localhost").expect("valid").as_str(),
        "localhost"
    );
    assert!(validate_host("").is_err());
    assert!(validate_host("-bad.example.com").is_err());
    assert!(validate_host("bad-.example.com").is_err());
    assert!(validate_host(&"a".repeat(64)).is_err());
}

#[test]
fn is_ip_literal_detects_v4_and_v6() {
    assert!(is_ip_literal("127.0.0.1"));
    assert!(is_ip_literal("::1"));
    assert!(is_ip_literal("[::1]"));
    assert!(!is_ip_literal("api.openai.com"));
}

#[test]
fn tag_and_alias_validation_follow_the_patterns() {
    assert!(validate_tag("llm").is_ok());
    assert!(validate_tag("llm_2-x").is_ok());
    assert!(validate_tag("LLM").is_err());
    assert!(validate_tag("").is_err());
    assert!(validate_tag("a b").is_err());

    assert!(validate_alias("api.openai.com").is_ok());
    assert!(validate_alias("vendor.com:8443").is_ok());
    assert!(validate_alias("my-service").is_ok());
    assert!(validate_alias("-leading").is_err());
    assert!(validate_alias("trailing-").is_err());
    assert!(validate_alias("").is_err());
}

#[test]
fn normalize_alias_lowercases_and_strips_trailing_dots() {
    assert_eq!(normalize_alias("  Api.OpenAI.COM. "), "api.openai.com");
    assert_eq!(normalize_alias("My-Service"), "my-service");
}

#[test]
fn upstream_roundtrips_through_json() {
    let record = Upstream {
        id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        alias: "api.openai.com".to_owned(),
        alias_explicit: false,
        spec: http_upstream(vec![endpoint(Scheme::Https, "api.openai.com", 443)]),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_100,
    };
    let json = serde_json::to_value(&record).expect("serializable");
    assert!(json["server"]["endpoints"].is_array());
    assert_eq!(json["enabled"], serde_json::Value::Bool(true));

    let back: Upstream = serde_json::from_value(json).expect("deserializable");
    assert_eq!(back, record);
}

#[test]
fn plugin_type_base_types_follow_the_contract() {
    assert_eq!(PluginType::Auth.base_type(), ids::AUTH_PLUGIN_TYPE);
    assert_eq!(PluginType::Guard.base_type(), ids::GUARD_PLUGIN_TYPE);
    assert_eq!(
        PluginType::Transform.base_type(),
        ids::TRANSFORM_PLUGIN_TYPE
    );
}
