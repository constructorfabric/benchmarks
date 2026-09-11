//! Domain model tests.
//!
//! Covers `cpt-cf-oagw-dod-domain-model-types`: `Upstream` and `Route` mirror
//! their shipped JSON Schemas property for property, `Route` additionally
//! carries the §1.5-added `cors`, `priority`, and `enabled`, the
//! sub-configurations round-trip, and the domain layer stays free of
//! transport and persistence types.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use std::collections::BTreeMap;

use serde_json::{Value, json};

use oagw::{
    Algorithm, Alias, AuthConfig, CorsConfig, Endpoint, EndpointHost, GrpcMatch, HeadersConfig,
    HttpMatch, MatchConfig, ModelError, Passthrough, Plugin, PluginsConfig, RateLimitConfig,
    RequestHeaderRules, ResponseHeaderRules, Route, Scheme, ServerConfig, SharingMode, Sustained,
    Upstream, Window,
};

const UPSTREAM_SCHEMA: &str = include_str!("../../docs/schemas/upstream.v1.schema.json");
const ROUTE_SCHEMA: &str = include_str!("../../docs/schemas/route.v1.schema.json");

/// The `properties` key set of a shipped schema, read from the frozen file.
fn schema_properties(schema: &str) -> Vec<String> {
    let parsed: Value = serde_json::from_str(schema).expect("shipped schema parses");
    parsed["properties"]
        .as_object()
        .expect("schema declares properties")
        .keys()
        .cloned()
        .collect()
}

fn endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: Scheme::Https,
        host: EndpointHost::parse(host).expect("valid endpoint host"),
        port: Some(port),
    }
}

fn upstream() -> Upstream {
    Upstream {
        id: uuid::Uuid::nil(),
        enabled: true,
        alias: Some(String::from("api.openai.com")),
        tags: vec![String::from("llm")],
        server: ServerConfig {
            endpoints: vec![endpoint("api.openai.com", 443)],
        },
        protocol: String::from("gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"),
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
    }
}

fn route() -> Route {
    Route {
        id: uuid::Uuid::nil(),
        upstream_id: uuid::Uuid::nil(),
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![String::from("GET")],
                path: String::from("/v1/chat"),
                query_allowlist: vec![],
                path_suffix_mode: None,
            }),
            grpc: None,
        },
        plugins: None,
        rate_limit: None,
        tags: vec![],
        cors: None,
        priority: None,
        enabled: None,
    }
}

#[test]
fn upstream_carries_exactly_the_schema_properties() {
    let serialized = serde_json::to_value(upstream()).expect("upstream serializes");
    let mut keys: Vec<String> = serialized
        .as_object()
        .expect("upstream is an object")
        .keys()
        .cloned()
        .collect();
    keys.sort();

    let mut expected = schema_properties(UPSTREAM_SCHEMA);
    expected.sort();

    assert_eq!(
        keys, expected,
        "Upstream must mirror the schema property set"
    );
}

#[test]
fn route_carries_exactly_the_schema_properties_plus_the_added_ones() {
    let serialized = serde_json::to_value(route()).expect("route serializes");
    let mut keys: Vec<String> = serialized
        .as_object()
        .expect("route is an object")
        .keys()
        .cloned()
        .collect();
    keys.sort();

    let mut expected = schema_properties(ROUTE_SCHEMA);
    // §1.5 additions: route-level `cors`, and the DESIGN §3.1 `priority` and
    // `enabled` attributes the shipped schema omits.
    expected.extend([
        String::from("cors"),
        String::from("priority"),
        String::from("enabled"),
    ]);
    expected.sort();
    expected.dedup();

    assert_eq!(keys, expected, "Route must mirror the schema property set");
}

#[test]
fn upstream_requires_server_and_protocol() {
    let no_protocol = json!({
        "id": uuid::Uuid::nil(),
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com" } ] }
    });
    assert!(
        serde_json::from_value::<Upstream>(no_protocol).is_err(),
        "protocol is a required field of the shipped schema"
    );

    let no_server = json!({
        "id": uuid::Uuid::nil(),
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    });
    assert!(
        serde_json::from_value::<Upstream>(no_server).is_err(),
        "server is a required field of the shipped schema"
    );
}

#[test]
fn upstream_rejects_unknown_properties() {
    let raw = json!({
        "id": uuid::Uuid::nil(),
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "unexpected": true
    });
    assert!(
        serde_json::from_value::<Upstream>(raw).is_err(),
        "additionalProperties: false"
    );
}

#[test]
fn upstream_defaults_enabled_to_true_and_tags_to_empty() {
    let raw = json!({
        "id": uuid::Uuid::nil(),
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    });
    let parsed: Upstream = serde_json::from_value(raw).expect("upstream parses");
    assert!(parsed.enabled);
    assert!(parsed.tags.is_empty());
    assert_eq!(parsed.alias, None);
}

#[test]
fn endpoint_rejects_unknown_properties_and_validates_its_value_objects() {
    let bad = json!({ "scheme": "https", "host": "api.openai.com", "weight": 1 });
    assert!(serde_json::from_value::<Endpoint>(bad).is_err());

    let bad_host = json!({ "scheme": "https", "host": "-nope-", "port": 443 });
    assert!(serde_json::from_value::<Endpoint>(bad_host).is_err());

    let bad_scheme = json!({ "scheme": "ftp", "host": "api.openai.com", "port": 443 });
    assert!(serde_json::from_value::<Endpoint>(bad_scheme).is_err());
}

#[test]
fn endpoint_admits_an_ip_literal_host() {
    let raw = json!({ "scheme": "https", "host": "2001:db8::1", "port": 443 });
    let parsed: Endpoint = serde_json::from_value(raw).expect("ip literal host parses");
    assert_eq!(parsed.host.as_str(), "2001:db8::1");
}

#[test]
fn server_config_requires_at_least_one_endpoint() {
    let empty = ServerConfig { endpoints: vec![] };
    assert_eq!(empty.validate(), Err(ModelError::NoEndpoints));
    let one = ServerConfig {
        endpoints: vec![endpoint("api.openai.com", 443)],
    };
    assert_eq!(one.validate(), Ok(()));
}

#[test]
fn route_match_requires_exactly_one_of_http_or_grpc() {
    let neither = MatchConfig {
        http: None,
        grpc: None,
    };
    let both = MatchConfig {
        http: Some(HttpMatch {
            methods: vec![String::from("GET")],
            path: String::from("/v1"),
            query_allowlist: vec![],
            path_suffix_mode: None,
        }),
        grpc: Some(GrpcMatch {
            service: String::from("foo.v1.UserService"),
            method: String::from("GetUser"),
        }),
    };
    assert_eq!(neither.validate(), Err(ModelError::AmbiguousMatch));
    assert_eq!(both.validate(), Err(ModelError::AmbiguousMatch));
    assert_eq!(route().match_config.validate(), Ok(()));
}

#[test]
fn route_serializes_the_match_key_verbatim() {
    let serialized = serde_json::to_value(route()).expect("route serializes");
    assert!(
        serialized.get("match").is_some(),
        "the wire key must stay 'match'"
    );
    assert!(serialized.get("match_config").is_none());
    let back: Route = serde_json::from_value(serialized).expect("route round-trips");
    assert_eq!(back, route());
}

#[test]
fn route_adds_the_section_1_5_properties() {
    let mut with_additions = route();
    with_additions.cors = Some(CorsConfig {
        sharing: Some(SharingMode::Private),
        enabled: true,
        allowed_origins: vec![String::from("https://console.example.com")],
        allowed_methods: vec![String::from("GET"), String::from("POST")],
        expose_headers: vec![],
        allow_credentials: false,
    });
    with_additions.priority = Some(10);
    with_additions.enabled = Some(false);

    let serialized = serde_json::to_value(&with_additions).expect("route serializes");
    assert_eq!(serialized["priority"], 10);
    assert_eq!(serialized["enabled"], false);
    assert_eq!(serialized["cors"]["enabled"], true);
}

#[test]
fn header_rules_round_trip_their_schema_keys() {
    let headers = HeadersConfig {
        request: Some(RequestHeaderRules {
            set: BTreeMap::from([(String::from("x-tenant"), String::from("acme"))]),
            add: BTreeMap::new(),
            remove: vec![String::from("x-inbound")],
            passthrough: Some(Passthrough::Allowlist),
            passthrough_allowlist: vec![String::from("authorization")],
        }),
        response: Some(ResponseHeaderRules {
            set: BTreeMap::new(),
            add: BTreeMap::from([(String::from("x-served-by"), String::from("oagw"))]),
            remove: vec![String::from("server")],
        }),
    };

    let serialized = serde_json::to_value(&headers).expect("headers serialize");
    let request = &serialized["request"];
    for key in [
        "set",
        "add",
        "remove",
        "passthrough",
        "passthrough_allowlist",
    ] {
        assert!(request.get(key).is_some(), "request.{key} must be present");
    }
    let response = &serialized["response"];
    for key in ["set", "add", "remove"] {
        assert!(
            response.get(key).is_some(),
            "response.{key} must be present"
        );
    }
    assert!(
        response.get("passthrough").is_none(),
        "response rules have no passthrough fields"
    );
    assert_eq!(serialized["request"]["passthrough"], "allowlist");
}

#[test]
fn rate_limit_round_trips_its_schema_keys() {
    let rate_limit = RateLimitConfig {
        sharing: Some(SharingMode::Enforce),
        algorithm: Some(Algorithm::TokenBucket),
        sustained: Some(Sustained {
            rate: 100,
            window: Some(Window::Minute),
        }),
        burst: Some(oagw::Burst { capacity: 200 }),
        scope: Some(oagw::RateLimitScope::Tenant),
        strategy: Some(oagw::Strategy::Reject),
        cost: Some(2),
    };

    let serialized = serde_json::to_value(&rate_limit).expect("rate limit serializes");
    for key in [
        "sharing",
        "algorithm",
        "sustained",
        "burst",
        "scope",
        "strategy",
        "cost",
    ] {
        assert!(
            serialized.get(key).is_some(),
            "rate_limit.{key} must be present"
        );
    }
    assert_eq!(serialized["algorithm"], "token_bucket");
    assert_eq!(serialized["sustained"]["window"], "minute");

    let back: RateLimitConfig = serde_json::from_value(serialized).expect("rate limit round-trips");
    assert_eq!(back, rate_limit);
}

#[test]
fn plugins_config_carries_sharing_and_items() {
    let plugins = PluginsConfig {
        sharing: Some(SharingMode::Inherit),
        items: vec![
            String::from("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"),
            String::from("018f0000-0000-7000-8000-000000000000"),
        ],
    };
    let serialized = serde_json::to_value(&plugins).expect("plugins serialize");
    assert_eq!(serialized["sharing"], "inherit");
    assert_eq!(serialized["items"].as_array().map(Vec::len), Some(2));
}

#[test]
fn auth_config_carries_type_sharing_and_config() {
    let auth = AuthConfig {
        r#type: Some(String::from(
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
        )),
        sharing: Some(SharingMode::Private),
        config: Some(json!({ "header": "x-api-key" })),
    };
    let serialized = serde_json::to_value(&auth).expect("auth serializes");
    assert!(
        serialized.get("type").is_some(),
        "the wire key must stay 'type'"
    );
    assert!(serialized.get("r#type").is_none());
    let back: AuthConfig = serde_json::from_value(serialized).expect("auth round-trips");
    assert_eq!(back, auth);
}

#[test]
fn plugin_carries_the_design_3_1_attributes() {
    let plugin = Plugin {
        id: uuid::Uuid::nil(),
        tenant_id: uuid::Uuid::nil(),
        plugin_type: String::from("transform"),
        name: String::from("redact-headers"),
        description: Some(String::from("redacts response headers")),
        config_schema: Some(json!({ "type": "object" })),
        phases: vec![String::from("on_response")],
        source_code: String::from("def on_response(ctx): pass"),
        last_used_at: Some(1_700_000_000),
        gc_eligible_at: None,
    };
    let serialized = serde_json::to_value(&plugin).expect("plugin serializes");
    for key in [
        "id",
        "tenant_id",
        "plugin_type",
        "name",
        "description",
        "config_schema",
        "phases",
        "source_code",
        "last_used_at",
        "gc_eligible_at",
    ] {
        assert!(
            serialized.get(key).is_some(),
            "plugin.{key} must be present"
        );
    }
    assert_eq!(serialized["last_used_at"], 1_700_000_000);
    assert!(serialized["gc_eligible_at"].is_null());
}

#[test]
fn alias_value_object_normalizes_inside_the_upstream_model() {
    let alias = Alias::parse("API.OpenAI.COM.").expect("valid alias");
    assert_eq!(alias.to_string(), "api.openai.com");
}

#[test]
fn domain_layer_stays_free_of_transport_and_persistence_types() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let domain_dir = std::path::Path::new(manifest_dir).join("src/domain");
    let forbidden = [
        "axum", "http", "hyper", "sqlx", "sea_orm", "reqwest", "tonic",
    ];

    let mut checked = 0;
    let entries = std::fs::read_dir(&domain_dir).expect("domain directory is readable");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("domain source is readable");
        for crate_name in forbidden {
            for prefix in [format!("{crate_name}::"), format!("{crate_name} (")] {
                assert!(
                    !source.contains(&prefix),
                    "{} references the infrastructure crate `{crate_name}`",
                    path.display()
                );
            }
        }
        checked += 1;
    }
    assert!(
        checked >= 4,
        "expected the domain modules to be scanned, got {checked}"
    );
}
