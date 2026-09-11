//! Unit tests for the shared domain model types (DoD
//! `cpt-cf-oagw-dod-gear-foundation-domain-model`): serialization round-trips
//! against the two schemas and the `https` scheme default.
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-domain-model:p1
// @cpt-dod:cpt-cf-oagw-dod-upstream-management-schema-shapes:p1

use serde_json::json;
use uuid::Uuid;

use super::*;

const UPSTREAM_SCHEMA_PROPERTIES: [&str; 11] = [
    "id", "enabled", "alias", "tags", "server", "protocol", "auth", "headers", "plugins",
    "rate_limit", "cors",
];

const ROUTE_SCHEMA_PROPERTIES: [&str; 6] = ["id", "tags", "upstream_id", "match", "plugins", "rate_limit"];

/// The named closed set of schema-external API fields the route payload
/// contract admits.
const ROUTE_SCHEMA_EXTERNAL: [&str; 3] = ["priority", "enabled", "cors"];

fn endpoint(host: &str) -> Endpoint {
    Endpoint { scheme: EndpointScheme::Https, host: host.to_owned(), port: 443 }
}

fn upstream() -> Upstream {
    Upstream {
        id: Uuid::nil(),
        tenant_id: Uuid::nil(),
        alias: "api.vendor.com".to_owned(),
        protocol: crate::domain::gts_helpers::PROTOCOL_HTTP.to_owned(),
        enabled: true,
        server: ServerConfig { endpoints: vec![endpoint("api.vendor.com")] },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

/// An upstream carrying every optional sub-configuration, so the serialized
/// key set can be compared against the schema's property list.
fn full_upstream() -> Upstream {
    Upstream {
        auth: Some(AuthConfig {
            auth_type: Some(crate::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID.to_owned()),
            sharing: SharingMode::Inherit,
            config: Some(json!({ "api_key_ref": "cred://tenant-a/openai" })),
        }),
        headers: Some(HeadersConfig {
            request: Some(RequestHeaders {
                set: Some([("x-a".to_owned(), "1".to_owned())].into_iter().collect()),
                add: None,
                remove: None,
                passthrough: None,
                passthrough_allowlist: None,
            }),
            response: None,
        }),
        rate_limit: Some(RateLimitConfig {
            sharing: SharingMode::Private,
            ..serde_json::from_str::<RateLimitConfig>(r#"{"sustained":{"rate":10}}"#).expect("parses")
        }),
        cors: Some(CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            ..CorsConfig::default()
        }),
        plugins: Some(PluginsConfig { sharing: SharingMode::Inherit, items: vec![] }),
        ..upstream()
    }
}

fn route() -> Route {
    Route {
        id: Uuid::nil(),
        tenant_id: Uuid::nil(),
        upstream_id: Uuid::nil(),
        match_type: RouteMatchType::Http,
        priority: 0,
        enabled: true,
        match_: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "/v1/chat".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

#[test]
fn endpoint_scheme_defaults_to_https() {
    assert_eq!(EndpointScheme::default(), EndpointScheme::Https);
    // Absent on the wire -> `https`.
    let endpoint: Endpoint = serde_json::from_str(r#"{"host":"api.vendor.com"}"#).expect("parses");
    assert_eq!(endpoint.scheme, EndpointScheme::Https);
    assert_eq!(endpoint.port, 443, "port defaults to 443");
}

#[test]
fn standard_port_per_scheme_matches_the_alias_algorithm() {
    assert_eq!(EndpointScheme::Http.standard_port(), 80);
    for scheme in [EndpointScheme::Https, EndpointScheme::Wss, EndpointScheme::Wt, EndpointScheme::Grpc] {
        assert_eq!(scheme.standard_port(), 443, "{scheme:?} standard port is 443");
    }
}

#[test]
fn http_scheme_is_gated_on_allow_http_upstream() {
    assert!(!EndpointScheme::Http.is_allowed(false));
    assert!(EndpointScheme::Http.is_allowed(true), "graded deviation 2 admits `http`");
    for scheme in [EndpointScheme::Https, EndpointScheme::Wss, EndpointScheme::Wt, EndpointScheme::Grpc] {
        assert!(scheme.is_allowed(false));
    }
}

#[test]
fn upstream_serializes_exactly_the_schema_property_names() {
    let value = serde_json::to_value(full_upstream()).expect("serializes");
    let obj = value.as_object().expect("object");
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut expected = UPSTREAM_SCHEMA_PROPERTIES;
    expected.sort_unstable();
    assert_eq!(keys, expected, "`additionalProperties: false` forbids any other member");
    // `tenant_id` is server-assigned and never on the wire.
    assert!(!obj.contains_key("tenant_id"));
}

#[test]
fn upstream_round_trips_through_the_schema_shape() {
    let original = Upstream {
        auth: Some(AuthConfig {
            auth_type: Some(crate::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID.to_owned()),
            sharing: SharingMode::Inherit,
            config: Some(json!({"api_key_ref": "cred://tenant-a/openai"})),
        }),
        headers: Some(HeadersConfig {
            request: Some(RequestHeaders {
                set: Some([("x-a".to_owned(), "1".to_owned())].into_iter().collect()),
                add: None,
                remove: Some(vec!["x-b".to_owned()]),
                passthrough: Some(HeaderPassthrough::Allowlist),
                passthrough_allowlist: Some(vec!["x-c".to_owned()]),
            }),
            response: Some(ResponseHeaders::default()),
        }),
        rate_limit: Some(RateLimitConfig {
            sharing: SharingMode::Enforce,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate { rate: 100, window: RateWindow::Minute },
            burst: Some(BurstCapacity { capacity: 20 }),
            budget: Some(Budget { mode: BudgetMode::Allocated, total: Some(10_000), overcommit_ratio: Some(1.5) }),
            scope: RateScope::User,
            strategy: RateStrategy::Reject,
            cost: 2,
            response_headers: true,
        }),
        cors: Some(CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: Some(vec!["https://app.vendor.com".to_owned()]),
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            expose_headers: vec!["x-trace".to_owned()],
            allow_credentials: false,
        }),
        plugins: Some(PluginsConfig {
            sharing: SharingMode::Inherit,
            items: vec![crate::domain::gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID.to_owned()],
        }),
        tags: vec!["openai".to_owned(), "llm".to_owned()],
        ..upstream()
    };
    let json = serde_json::to_string(&original).expect("serializes");
    let back: Upstream = serde_json::from_str(&json).expect("round-trips");
    assert_eq!(back, original);
}

#[test]
fn upstream_omits_unspecified_fields_rather_than_defaulting_them() {
    // `inst-gf-merge-13`: a field no layer specifies stays absent.
    let value = serde_json::to_value(upstream()).expect("serializes");
    for absent in ["auth", "headers", "rate_limit", "cors", "plugins"] {
        assert!(!value.as_object().expect("object").contains_key(absent));
    }
}

#[test]
fn route_serializes_the_schema_properties_plus_the_named_external_set() {
    let value = serde_json::to_value(Route {
        rate_limit: Some(serde_json::from_str::<RateLimitConfig>(r#"{"sustained":{"rate":10}}"#).expect("parses")),
        cors: Some(CorsConfig { enabled: true, ..CorsConfig::default() }),
        plugins: Some(PluginsConfig { sharing: SharingMode::Inherit, items: vec![] }),
        ..route()
    })
    .expect("serializes");
    let obj = value.as_object().expect("object");
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut expected = ROUTE_SCHEMA_PROPERTIES.to_vec();
    expected.extend_from_slice(&ROUTE_SCHEMA_EXTERNAL);
    expected.sort_unstable();
    assert_eq!(keys, expected);
    assert!(!obj.contains_key("tenant_id"), "`tenant_id` is server-assigned");
    assert!(!obj.contains_key("match_type"), "`match_type` is derived, not accepted on write");
}

#[test]
fn route_match_is_serialized_under_the_schema_key() {
    let value = serde_json::to_value(route()).expect("serializes");
    assert!(value.get("match").is_some(), "the schema property is `match`");
    assert_eq!(
        value["match"]["http"]["methods"],
        json!(["GET"]),
        "methods serialize as the schema's wire verbs"
    );
    assert_eq!(value["match"]["http"]["path_suffix_mode"], json!("append"));
}

#[test]
fn route_match_type_is_derived_on_write_and_never_on_the_wire() {
    let grpc = Route {
        match_type: RouteMatchType::Http,
        match_: MatchConfig {
            http: None,
            grpc: Some(GrpcMatch { service: "foo.v1.UserService".to_owned(), method: "GetUser".to_owned() }),
        },
        ..route()
    };
    // The wire carries no `match_type`, so the round trip reads the default
    // back; the write path derives it from the selected block.
    let back: Route = serde_json::from_str(&serde_json::to_string(&grpc).expect("serializes")).expect("round-trips");
    assert_eq!(back.match_type, RouteMatchType::Http);
    let derived = crate::domain::validation::validate_route(&grpc).expect("validated");
    assert_eq!(derived.match_type, RouteMatchType::Grpc);
    // The http block derives the http match type.
    let derived = crate::domain::validation::validate_route(&route()).expect("validated");
    assert_eq!(derived.match_type, RouteMatchType::Http);
}

#[test]
fn route_defaults_materialize_priority_and_enabled() {
    let route: Route = serde_json::from_value(json!({
        "upstream_id": Uuid::nil(),
        "match": { "http": { "methods": ["GET"], "path": "/v1" } }
    }))
    .expect("parses");
    assert_eq!(route.priority, 0, "priority defaults to 0");
    assert!(route.enabled, "enabled defaults to true");
    assert_eq!(route.match_.http.as_ref().expect("http").path_suffix_mode, PathSuffixMode::Append);
    assert!(route.match_.http.as_ref().expect("http").query_allowlist.is_empty());
}

#[test]
fn http_method_serializes_as_the_schema_verb() {
    for (method, verb) in [
        (HttpMethod::Get, "GET"),
        (HttpMethod::Post, "POST"),
        (HttpMethod::Put, "PUT"),
        (HttpMethod::Delete, "DELETE"),
        (HttpMethod::Patch, "PATCH"),
    ] {
        assert_eq!(serde_json::to_value(method).expect("serializes"), json!(verb));
        assert_eq!(method.as_str(), verb);
    }
}

#[test]
fn sharing_mode_round_trips_all_three_values() {
    for (mode, text) in [
        (SharingMode::Private, "private"),
        (SharingMode::Inherit, "inherit"),
        (SharingMode::Enforce, "enforce"),
    ] {
        assert_eq!(serde_json::to_value(mode).expect("serializes"), json!(text));
        let back: SharingMode = serde_json::from_value(json!(text)).expect("parses");
        assert_eq!(back, mode);
    }
    assert_eq!(SharingMode::default(), SharingMode::Private);
}

#[test]
fn rate_limit_defaults_match_the_field_table() {
    let cfg: RateLimitConfig = serde_json::from_value(json!({
        "sustained": { "rate": 10 }
    }))
    .expect("parses");
    assert_eq!(cfg.sharing, SharingMode::Private);
    assert_eq!(cfg.algorithm, RateAlgorithm::TokenBucket);
    assert_eq!(cfg.sustained.window, RateWindow::Second);
    assert_eq!(cfg.scope, RateScope::Tenant);
    assert_eq!(cfg.strategy, RateStrategy::Reject);
    assert_eq!(cfg.cost, 1);
    assert!(cfg.response_headers);
    assert_eq!(
        cfg.effective_burst_capacity(),
        10,
        "burst defaults to the sustained rate"
    );
    assert!((cfg.refill_per_second() - 10.0).abs() < f64::EPSILON);
}

#[test]
fn rate_limit_rejects_an_unknown_key() {
    let err = serde_json::from_value::<RateLimitConfig>(json!({
        "sustained": { "rate": 10 },
        "no_such_key": 1
    }))
    .expect_err("unknown key rejected");
    assert!(err.to_string().contains("no_such_key"), "{err}");
}

#[test]
fn rate_limit_window_converts_to_one_per_second_refill_rate() {
    for (window, secs) in [
        (RateWindow::Second, 1_u64),
        (RateWindow::Minute, 60),
        (RateWindow::Hour, 3_600),
        (RateWindow::Day, 86_400),
    ] {
        assert_eq!(window.secs(), secs);
    }
    let cfg = RateLimitConfig {
        sustained: SustainedRate { rate: 120, window: RateWindow::Minute },
        ..serde_json::from_str(r#"{"sustained":{"rate":120,"window":"minute"}}"#).expect("parses")
    };
    assert!((cfg.refill_per_second() - 2.0).abs() < f64::EPSILON);
}

#[test]
fn cors_defaults_match_the_field_table() {
    let cfg: CorsConfig = serde_json::from_str(r#"{"enabled":true}"#).expect("parses");
    assert_eq!(cfg.sharing, SharingMode::Private);
    assert!(cfg.enabled);
    assert_eq!(cfg.allowed_origins, None, "absent origins stay absent");
    assert_eq!(cfg.allowed_methods, vec!["GET".to_owned(), "POST".to_owned()]);
    assert!(cfg.expose_headers.is_empty());
    assert!(!cfg.allow_credentials);
    assert!(!cfg.allows_wildcard().expect("no origins"));
}

#[test]
fn cors_rejects_credentials_with_wildcard_origin() {
    let cfg = CorsConfig {
        enabled: true,
        allowed_origins: Some(vec!["*".to_owned()]),
        allow_credentials: true,
        ..CorsConfig::default()
    };
    let err = cfg.allows_wildcard().expect_err("wildcard + credentials rejected");
    assert!(err.to_string().contains("allow_credentials with wildcard origin"));
}

#[test]
fn credential_reference_boundary_accepts_only_cred_uris() {
    assert!(is_cred_reference("cred://tenant-a/stripe/live"));
    assert!(is_cred_reference("cred://a1_B-c.d/key"));
    for bad in [
        "",
        "cred://",
        "cred:///",
        "https://tenant-a/stripe/live",
        "sk_live_51H8xYz",
        "cred:/tenant-a/key",
        "cred://tenant a/key",
    ] {
        assert!(!is_cred_reference(bad), "`{bad}` must be rejected");
    }
    // The typed reference round-trips and rejects non-references by name.
    assert_eq!(
        CredentialRef::parse("auth.config.api_key_ref", "cred://tenant-a/openai")
            .expect("accepts")
            .as_str(),
        "cred://tenant-a/openai"
    );
    let err = CredentialRef::parse("auth.config.api_key_ref", "sk_live_51H8xYz").expect_err("rejects");
    let text = err.to_string();
    assert!(text.contains("auth.config.api_key_ref"), "`{text}` names the field");
    assert!(!text.contains("sk_live"), "`{text}` never echoes the rejected value");
}

#[test]
fn plugin_record_round_trips_with_the_documented_fields() {
    let plugin = Plugin {
        id: Uuid::nil(),
        tenant_id: Uuid::nil(),
        plugin_type: crate::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
        name: "required-headers".to_owned(),
        config_schema: Some(json!({"type": "object"})),
        source_code: Some("registry:required-headers@1".to_owned()),
        last_used_at: None,
        gc_eligible_at: None,
    };
    let back: Plugin = serde_json::from_str(&serde_json::to_string(&plugin).expect("serializes")).expect("round-trips");
    assert_eq!(back, plugin);
    let value = serde_json::to_value(&plugin).expect("serializes");
    assert!(!value.as_object().expect("object").contains_key("tenant_id"));
}

#[test]
fn server_config_requires_at_least_the_documented_endpoint_keys() {
    let endpoint: Endpoint = serde_json::from_value(json!({
        "scheme": "https", "host": "api.vendor.com", "port": 8443
    }))
    .expect("parses");
    assert_eq!(endpoint.port, 8443);
    // `additionalProperties: false` on the endpoint object.
    let err = serde_json::from_value::<Endpoint>(json!({
        "scheme": "https", "host": "api.vendor.com", "weight": 1
    }))
    .expect_err("unknown endpoint key rejected");
    assert!(err.to_string().contains("weight"));
}

#[test]
fn headers_config_round_trips_the_documented_field_set() {
    let headers: HeadersConfig = serde_json::from_value(json!({
        "request": {
            "set": { "x-forwarded-by": "oagw" },
            "add": { "x-a": "1" },
            "remove": ["x-internal"],
            "passthrough": "allowlist",
            "passthrough_allowlist": ["x-tenant"]
        },
        "response": { "set": {}, "add": {}, "remove": ["server"] }
    }))
    .expect("parses");

    let request = headers.request.as_ref().expect("request block");
    assert_eq!(
        request.set.as_ref().expect("set").get("x-forwarded-by").map(String::as_str),
        Some("oagw")
    );
    assert_eq!(request.remove.as_ref().expect("remove"), &["x-internal"]);
    assert_eq!(request.passthrough, Some(HeaderPassthrough::Allowlist));
    assert_eq!(
        request.passthrough_allowlist.as_ref().expect("allowlist"),
        &["x-tenant"]
    );
    assert_eq!(
        serde_json::to_value(&headers).expect("serializes"),
        json!({
            "request": {
                "set": { "x-forwarded-by": "oagw" },
                "add": { "x-a": "1" },
                "remove": ["x-internal"],
                "passthrough": "allowlist",
                "passthrough_allowlist": ["x-tenant"]
            },
            "response": { "set": {}, "add": {}, "remove": ["server"] }
        })
    );
    assert!(serde_json::from_value::<HeadersConfig>(json!({"request": {"weight": 1}})).is_err());
}
