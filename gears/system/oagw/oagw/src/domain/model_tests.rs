//! Wire shapes of the domain model.

use super::*;

#[test]
fn schemes_know_their_standard_port_and_tls_posture() {
    assert!(Scheme::Https.is_tls());
    assert!(Scheme::Wss.is_tls());
    assert!(Scheme::Grpc.is_tls());
    assert!(Scheme::Wt.is_tls());
    assert!(!Scheme::Http.is_tls());
    assert!(!Scheme::Ws.is_tls());

    assert_eq!(Scheme::Https.standard_port(), 443);
    assert_eq!(Scheme::Http.standard_port(), 80);
    assert_eq!(Scheme::Ws.standard_port(), 80);
}

#[test]
fn plaintext_schemes_are_accepted_by_the_endpoint_shape() {
    // `allow_http_upstream` governs whether a plaintext connection is dialled;
    // which schemes deserialize is a separate question, and `http` is legal.
    let endpoint: Endpoint =
        serde_json::from_str(r#"{"scheme":"http","host":"127.0.0.1","port":80}"#).unwrap();
    assert_eq!(endpoint.scheme, Scheme::Http);
    assert_eq!(endpoint.port, 80);
}

#[test]
fn an_endpoint_defaults_to_https_on_443() {
    let endpoint: Endpoint = serde_json::from_str(r#"{"host":"api.openai.com"}"#).unwrap();
    assert_eq!(endpoint.scheme, Scheme::Https);
    assert_eq!(endpoint.port, 443);
}

#[test]
fn an_endpoint_rejects_unknown_members() {
    assert!(serde_json::from_str::<Endpoint>(r#"{"host":"a.example.com","tls":true}"#).is_err());
}

#[test]
fn the_authority_omits_a_standard_port() {
    let endpoint = Endpoint {
        scheme: Scheme::Https,
        host: "api.openai.com".to_owned(),
        port: 443,
    };
    assert_eq!(endpoint.authority(), "api.openai.com");
    let endpoint = Endpoint {
        port: 8443,
        ..endpoint
    };
    assert_eq!(endpoint.authority(), "api.openai.com:8443");
}

#[test]
fn the_protocol_serializes_as_its_gts_identifier() {
    let json = serde_json::to_string(&Protocol::Http).unwrap();
    assert_eq!(
        json,
        r#""gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1""#
    );
    let parsed: Protocol =
        serde_json::from_str(r#""gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1""#).unwrap();
    assert_eq!(parsed, Protocol::Grpc);
}

#[test]
fn a_plugin_binding_accepts_the_bare_string_form() {
    let binding: PluginBinding =
        serde_json::from_str(&format!("\"{}\"", gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID)).unwrap();
    assert_eq!(binding.plugin_ref, gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID);
    assert!(binding.config.is_empty());
    assert_eq!(binding.plugin_uuid, None);
}

#[test]
fn a_plugin_binding_accepts_the_object_form_with_config() {
    let binding: PluginBinding = serde_json::from_str(
        r#"{"plugin_ref":"gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
            "config":{"required_request_headers":"x-correlation-id,accept"}}"#,
    )
    .unwrap();
    assert_eq!(
        binding.config.get("required_request_headers").unwrap(),
        "x-correlation-id,accept"
    );
}

#[test]
fn a_uuid_backed_binding_extracts_its_plugin_uuid() {
    let uuid = uuid::Uuid::new_v4();
    let binding: PluginBinding =
        serde_json::from_str(&format!("\"{}{uuid}\"", gts::TRANSFORM_PLUGIN_BASE)).unwrap();
    assert_eq!(binding.plugin_uuid, Some(uuid));
}

#[test]
fn a_plugin_binding_always_serializes_as_the_object_form() {
    let binding: PluginBinding =
        serde_json::from_str(&format!("\"{}\"", gts::REQUEST_ID_TRANSFORM_PLUGIN_ID)).unwrap();
    let json = serde_json::to_value(&binding).unwrap();
    assert_eq!(json["plugin_ref"], gts::REQUEST_ID_TRANSFORM_PLUGIN_ID);
    assert!(json.get("config").is_some());
}

fn limit(rate: u32, window: RateWindow, capacity: Option<u32>) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained: SustainedRate { rate, window },
        burst: BurstCapacity { capacity },
        budget: None,
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

#[test]
fn the_refill_rate_normalizes_the_window() {
    assert!((limit(60, RateWindow::Minute, None).refill_per_second() - 1.0).abs() < f64::EPSILON);
    assert!((limit(1, RateWindow::Second, None).refill_per_second() - 1.0).abs() < f64::EPSILON);
}

#[test]
fn burst_capacity_defaults_to_the_sustained_rate() {
    assert_eq!(limit(100, RateWindow::Minute, None).capacity(), 100);
    assert_eq!(limit(100, RateWindow::Minute, Some(500)).capacity(), 500);
}

#[test]
fn the_stricter_of_two_limits_wins_field_by_field() {
    let ancestor = limit(10_000, RateWindow::Minute, Some(1_000));
    let descendant = limit(100, RateWindow::Minute, Some(50));
    let effective = RateLimitConfig::stricter_of(ancestor, descendant);
    assert_eq!(effective.sustained.rate, 100);
    assert_eq!(effective.capacity(), 50);
}

#[test]
fn cors_origin_matching_is_exact_and_port_sensitive() {
    let cors = CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: vec!["https://app.example.com".to_owned()],
        allowed_methods: vec!["GET".to_owned()],
        expose_headers: Vec::new(),
        allow_credentials: false,
    };
    assert!(cors.origin_allowed("https://app.example.com"));
    assert!(!cors.origin_allowed("https://app.example.com:8080"));
    assert!(!cors.origin_allowed("http://app.example.com"));
    assert!(!cors.origin_allowed("https://evil.com"));
    assert!(cors.method_allowed("get"));
    assert!(!cors.method_allowed("DELETE"));
}

#[test]
fn a_wildcard_origin_matches_anything() {
    let cors = CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: vec!["*".to_owned()],
        allowed_methods: vec!["GET".to_owned()],
        expose_headers: Vec::new(),
        allow_credentials: false,
    };
    assert!(cors.has_wildcard_origin());
    assert!(cors.origin_allowed("https://anything.example"));
}

#[test]
fn sharing_modes_know_their_visibility() {
    assert!(!SharingMode::Private.is_visible_to_descendants());
    assert!(SharingMode::Inherit.is_visible_to_descendants());
    assert!(SharingMode::Enforce.is_visible_to_descendants());
    assert!(SharingMode::Enforce.is_enforced());
    assert!(!SharingMode::Inherit.is_enforced());
}

#[test]
fn plugin_kinds_round_trip_through_their_base_type() {
    for kind in [PluginKind::Auth, PluginKind::Guard, PluginKind::Transform] {
        assert_eq!(PluginKind::from_base_type(kind.base_type()), Some(kind));
    }
    assert_eq!(PluginKind::from_base_type(gts::UPSTREAM_BASE), None);
}

#[test]
fn an_http_match_is_method_and_query_aware() {
    let http = HttpMatch {
        methods: vec!["GET".to_owned(), "POST".to_owned()],
        path: "/v1/chat".to_owned(),
        query_allowlist: vec!["model".to_owned()],
        path_suffix_mode: PathSuffixMode::Append,
    };
    assert!(http.allows_method("get"));
    assert!(!http.allows_method("DELETE"));
    assert!(http.allows_query_param("model"));
    assert!(!http.allows_query_param("secret"));
}
