//! Tests for [`crate::domain::model`].

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use uuid::Uuid;

use crate::domain::model::{
    AuthConfig, BurstConfig, CorsConfig, CorsMethod, Endpoint, GrpcMatch, HeaderRules,
    HeadersConfig, HttpMatch, HttpMethod, PLUGIN_TYPE_ID, PassthroughMode, PathSuffixMode, Plugin,
    PluginBinding, PluginConfig, Protocol, ROUTE_TYPE_ID, RateLimitAlgorithm, RateLimitConfig,
    RateLimitScope, RateLimitStrategy, RateLimitWindow, ResolvedProxyTarget, Route, RouteMatch,
    Scheme, ServerConfig, SharingMode, SustainedRateConfig, UPSTREAM_TYPE_ID, Upstream,
    format_plugin_id, format_route_id, format_upstream_id, gts_instance_id, parse_plugin_id,
    parse_route_id, parse_upstream_id,
};

/// Expected value after a wire round trip: fields excluded from the wire
/// (`tenant_id`, `created_at`, `updated_at`) reset to their serde defaults.
fn strip_non_wire(mut upstream: Upstream) -> Upstream {
    upstream.tenant_id = Uuid::nil();
    upstream.created_at = SystemTime::UNIX_EPOCH;
    upstream.updated_at = SystemTime::UNIX_EPOCH;
    upstream
}

fn sample_endpoint() -> Endpoint {
    Endpoint {
        scheme: Scheme::Https,
        host: "api.example.com".to_owned(),
        port: 8443,
    }
}

fn sample_upstream() -> Upstream {
    Upstream {
        id: Uuid::from_u128(0xA1),
        enabled: true,
        alias: "payments".to_owned(),
        tags: vec!["gold".to_owned()],
        server: ServerConfig {
            endpoints: vec![sample_endpoint()],
        },
        protocol: Protocol::Http,
        auth: Some(AuthConfig {
            auth_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".to_owned(),
            sharing: SharingMode::Inherit,
            config: serde_json::json!({ "header": "x-api-key" }),
        }),
        headers: HeadersConfig::default(),
        plugins: PluginConfig {
            sharing: SharingMode::Private,
            items: vec![PluginBinding::new(
                "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                serde_json::json!({ "headers": { "x-trace": "required" } }),
            )],
        },
        rate_limit: Some(RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRateConfig {
                rate: 100,
                window: RateLimitWindow::Minute,
            },
            burst: Some(BurstConfig {
                capacity: Some(150),
            }),
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            response_headers: true,
            cost: 1,
        }),
        cors: None,
        tenant_id: Uuid::from_u128(0x71),
        created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        updated_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_100),
    }
}

#[test]
fn gts_ids_format_and_parse_round_trip() {
    let id = Uuid::from_u128(0x1234);
    assert_eq!(format_upstream_id(id), format!("{UPSTREAM_TYPE_ID}~{id}"));
    assert_eq!(format_route_id(id), format!("{ROUTE_TYPE_ID}~{id}"));
    assert_eq!(format_plugin_id(id), format!("{PLUGIN_TYPE_ID}~{id}"));
    assert_eq!(parse_upstream_id(&format_upstream_id(id)), Some(id));
    assert_eq!(parse_route_id(&format_route_id(id)), Some(id));
    assert_eq!(parse_plugin_id(&format_plugin_id(id)), Some(id));
}

#[test]
fn gts_ids_reject_wrong_type_and_garbage() {
    let id = Uuid::from_u128(0x99);
    let upstream_id = format_upstream_id(id);
    assert_eq!(parse_route_id(&upstream_id), None);
    assert_eq!(parse_upstream_id("not-a-gts-id"), None);
    assert_eq!(parse_upstream_id(&format!("{UPSTREAM_TYPE_ID}~nope")), None);
    assert_eq!(parse_upstream_id(&format!("{UPSTREAM_TYPE_ID}~")), None);
    // `gts://` URI spelling is accepted for the instance-id helper.
    let uri = format!("gts://{upstream_id}");
    let instance = id.to_string();
    assert_eq!(gts_instance_id(&uri), Some(instance.as_str()));
    assert_eq!(gts_instance_id(UPSTREAM_TYPE_ID), None);
}

#[test]
fn upstream_schema_block_round_trips() {
    let upstream = sample_upstream();
    let encoded = serde_json::to_value(&upstream).expect("serialise");
    assert_eq!(encoded["alias"], "payments");
    assert_eq!(encoded["protocol"], Protocol::Http.gts_id());
    assert_eq!(
        encoded["server"]["endpoints"][0]["port"],
        serde_json::json!(8443)
    );
    assert_eq!(encoded["enabled"], true);
    // Schema-shaped document: `tenant_id`, `created_at`, `updated_at` stay off
    // the wire.
    assert!(encoded.get("tenant_id").is_none());
    assert!(encoded.get("created_at").is_none());
    assert!(encoded.get("updated_at").is_none());

    let decoded: Upstream = serde_json::from_value(encoded).expect("deserialise");
    assert_eq!(decoded, strip_non_wire(upstream.clone()));
    assert_eq!(decoded.endpoints(), upstream.endpoints());
    assert_eq!(
        decoded.tenant_id,
        Uuid::nil(),
        "wire documents carry no tenant"
    );
}

#[test]
fn upstream_deserialises_the_documented_schema_shape() {
    let raw = serde_json::json!({
        "enabled": true,
        "alias": "invoices",
        "tags": ["beta"],
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": 9090 } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "auth": { "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                  "sharing": "inherit", "config": { "header": "x-api-key" } },
        "headers": { "request": { "set": { "x-forwarded-by": "oagw" }, "remove": ["x-secret"] },
                     "response": { "add": { "x-served-by": "oagw" } } },
        "plugins": { "sharing": "private", "items": [ "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.noop.v1" ] },
        "rate_limit": { "algorithm": "sliding_window", "sustained": { "rate": 10 },
                        "scope": "user", "strategy": "queue", "response_headers": false, "cost": 2 },
        "cors": { "enabled": true, "allowed_origins": ["https://app.example.com"],
                  "allowed_methods": ["GET", "POST"], "allow_headers": ["x-request-id"],
                  "expose_headers": ["x-request-id"], "allow_credentials": true }
    });
    let upstream: Upstream = serde_json::from_value(raw).expect("schema shape must parse");
    assert_eq!(upstream.alias, "invoices");
    assert_eq!(upstream.protocol, Protocol::Http);
    assert_eq!(upstream.endpoints()[0].scheme, Scheme::Http);
    assert_eq!(
        upstream.auth.as_ref().expect("auth").sharing,
        SharingMode::Inherit
    );
    assert_eq!(upstream.plugins.items.len(), 1);
    assert_eq!(
        upstream.plugins.items[0].plugin_ref,
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.noop.v1"
    );
    assert_eq!(upstream.plugins.items[0].config, serde_json::json!({}));

    let rate_limit = upstream.rate_limit.as_ref().expect("rate limit");
    assert_eq!(rate_limit.algorithm, RateLimitAlgorithm::SlidingWindow);
    assert_eq!(rate_limit.sustained.window, RateLimitWindow::Second);
    assert!(!rate_limit.response_headers);
    assert_eq!(rate_limit.cost, 2);

    let cors = upstream.cors.as_ref().expect("cors");
    assert!(cors.allow_credentials);
    assert_eq!(
        cors.allowed_methods,
        vec![CorsMethod::Get, CorsMethod::Post]
    );

    // Round trip back to the wire keeps the same fields.
    let encoded = serde_json::to_value(&upstream).expect("serialise");
    assert_eq!(
        encoded["plugins"]["items"][0],
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.noop.v1"
    );
    assert!(encoded["rate_limit"].get("window").is_none());
    assert!(encoded["rate_limit"].get("burst").is_none());
    assert!(encoded["cors"].get("max_age").is_none());
}

#[test]
fn upstream_rejects_unknown_fields() {
    let raw = serde_json::json!({
        "alias": "x",
        "server": { "endpoints": [] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "not_a_field": 1
    });
    let error =
        serde_json::from_value::<Upstream>(raw).expect_err("unknown field must be rejected");
    assert!(error.to_string().contains("unknown field"), "{error}");
}

#[test]
fn missing_required_upstream_fields_are_rejected() {
    let raw = serde_json::json!({ "alias": "x", "server": { "endpoints": [] } });
    let error = serde_json::from_value::<Upstream>(raw).expect_err("protocol is required");
    assert!(error.to_string().contains("protocol"), "{error}");
}

#[test]
fn plugin_bindings_accept_both_wire_forms() {
    let object_form: Vec<PluginBinding> = serde_json::from_value(serde_json::json!([
        { "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.apikey.v1",
          "config": { "keys": ["k1"] } }
    ]))
    .expect("object form");
    assert_eq!(object_form.len(), 1);
    assert_eq!(
        object_form[0].plugin_ref,
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.apikey.v1"
    );
    assert_eq!(object_form[0].config["keys"], serde_json::json!(["k1"]));

    let mixed: Vec<PluginBinding> = serde_json::from_value(serde_json::json!([
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.noop.v1",
        { "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.request_id.v1" }
    ]))
    .expect("mixed forms");
    assert_eq!(mixed.len(), 2);
    assert_eq!(mixed[0].config, serde_json::json!({}));
    assert_eq!(
        mixed[1].config,
        serde_json::json!({}),
        "config defaults to empty object"
    );

    // Bare string in, bare string out when the config is empty; object out
    // when it is not (ADR-0009).
    let bare = serde_json::to_value(&mixed[0]).expect("serialise");
    assert_eq!(
        bare,
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.noop.v1"
    );
    let enriched = serde_json::to_value(&object_form[0]).expect("serialise");
    assert_eq!(
        enriched["plugin_ref"],
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.apikey.v1"
    );
    assert_eq!(enriched["config"]["keys"], serde_json::json!(["k1"]));
}

#[test]
fn route_schema_block_round_trips() {
    let route = Route {
        id: Uuid::from_u128(0xB2),
        upstream_id: Uuid::from_u128(0xA1),
        r#match: RouteMatch {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get, HttpMethod::Post],
                path: "/v1/payments".to_owned(),
                query_allowlist: vec!["cursor".to_owned()],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        headers: HeadersConfig {
            request: HeaderRules {
                passthrough: PassthroughMode::Allowlist,
                passthrough_allowlist: vec!["x-trace".to_owned()],
                ..HeaderRules::default()
            },
            response: HeaderRules::default(),
        },
        plugins: PluginConfig::default(),
        rate_limit: None,
        cors: Some(CorsConfig {
            enabled: true,
            sharing: SharingMode::Enforce,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: vec![CorsMethod::Get, CorsMethod::Options],
            allow_headers: Vec::new(),
            expose_headers: Vec::new(),
            allow_credentials: false,
            max_age: Some(3_600),
        }),
        enabled: true,
        priority: 25,
        tags: vec!["edge".to_owned()],
        tenant_id: Uuid::from_u128(0x71),
        created_at: SystemTime::UNIX_EPOCH,
        updated_at: SystemTime::UNIX_EPOCH,
    };

    let encoded = serde_json::to_value(&route).expect("serialise");
    assert_eq!(encoded["match"]["http"]["path"], "/v1/payments");
    assert_eq!(
        encoded["match"]["http"]["methods"],
        serde_json::json!(["GET", "POST"])
    );
    assert_eq!(encoded["priority"], serde_json::json!(25));
    assert_eq!(encoded["cors"]["max_age"], serde_json::json!(3_600));
    assert!(encoded.get("tenant_id").is_none());
    assert!(encoded["headers"]["request"].get("set").is_none());

    let decoded: Route = serde_json::from_value(encoded).expect("deserialise");
    let mut wire_only = route.clone();
    wire_only.tenant_id = Uuid::nil();
    wire_only.created_at = SystemTime::UNIX_EPOCH;
    wire_only.updated_at = SystemTime::UNIX_EPOCH;
    assert_eq!(decoded, wire_only);
    assert_eq!(route.r#match.protocol(), Some(Protocol::Http));
}

#[test]
fn route_deserialises_the_documented_schema_shape() {
    let raw = serde_json::json!({
        "upstream_id": "00000000-0000-0000-0000-00000000000a",
        "match": { "grpc": { "service": "foo.v1.UserService", "method": "GetUser" } }
    });
    let route: Route = serde_json::from_value(raw).expect("schema shape must parse");
    assert_eq!(route.r#match.protocol(), Some(Protocol::Grpc));
    assert!(route.enabled, "routes default to enabled");
    assert_eq!(route.priority, 0);
    assert!(route.headers.is_empty());
}

#[test]
fn empty_route_match_has_no_protocol() {
    let empty = RouteMatch::default();
    assert_eq!(empty.protocol(), None);
    let both = RouteMatch {
        http: Some(HttpMatch {
            methods: vec![HttpMethod::Get],
            path: "/".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Disabled,
        }),
        grpc: Some(GrpcMatch {
            service: "s".to_owned(),
            method: "m".to_owned(),
        }),
    };
    assert_eq!(both.protocol(), None);
    let encoded = serde_json::to_value(&both).expect("serialise");
    assert!(encoded.get("http").is_some());
    assert!(encoded.get("grpc").is_some());
}

#[test]
fn route_rejects_unknown_fields() {
    let raw = serde_json::json!({
        "upstream_id": "00000000-0000-0000-0000-00000000000a",
        "match": {},
        "bogus": true
    });
    let error = serde_json::from_value::<Route>(raw).expect_err("unknown field must be rejected");
    assert!(error.to_string().contains("unknown field"), "{error}");
}

#[test]
fn plugin_resource_round_trips() {
    let plugin = Plugin {
        id: Uuid::from_u128(0xC3),
        plugin_type: "gts.cf.core.oagw.guard_plugin.v1".to_owned(),
        config: serde_json::json!({ "headers": ["x-trace"] }),
        enabled: true,
        tags: vec!["shared".to_owned()],
        tenant_id: Uuid::from_u128(0x71),
        created_at: SystemTime::UNIX_EPOCH,
        updated_at: SystemTime::UNIX_EPOCH,
    };
    let encoded = serde_json::to_value(&plugin).expect("serialise");
    assert_eq!(encoded["plugin_type"], "gts.cf.core.oagw.guard_plugin.v1");
    assert!(encoded.get("tenant_id").is_none());
    let decoded: Plugin = serde_json::from_value(encoded).expect("deserialise");
    let mut wire_only = plugin;
    wire_only.tenant_id = Uuid::nil();
    wire_only.created_at = SystemTime::UNIX_EPOCH;
    wire_only.updated_at = SystemTime::UNIX_EPOCH;
    assert_eq!(decoded, wire_only);
}

#[test]
fn resolved_proxy_target_is_shareable() {
    let upstream = Arc::new(sample_upstream());
    let route = Arc::new(Route {
        id: Uuid::from_u128(0xB2),
        upstream_id: upstream.id,
        ..Route::default()
    });
    let target = ResolvedProxyTarget {
        upstream: Arc::clone(&upstream),
        route: Some(Arc::clone(&route)),
    };
    let first = Arc::new(target);
    let second = Arc::clone(&first);
    assert!(Arc::ptr_eq(&first.upstream, &second.upstream));
    assert!(first.route.is_some());
}

#[test]
fn defaults_are_sensible() {
    assert_eq!(Scheme::default(), Scheme::Https);
    assert_eq!(Protocol::default(), Protocol::Http);
    assert_eq!(SharingMode::default(), SharingMode::Private);
    assert_eq!(PathSuffixMode::default(), PathSuffixMode::Append);
    assert_eq!(PassthroughMode::default(), PassthroughMode::None);
    assert_eq!(RateLimitWindow::default(), RateLimitWindow::Second);
    assert_eq!(
        RateLimitAlgorithm::default(),
        RateLimitAlgorithm::TokenBucket
    );
    assert_eq!(RateLimitScope::default(), RateLimitScope::Tenant);
    assert_eq!(RateLimitStrategy::default(), RateLimitStrategy::Reject);
    let upstream = Upstream::default();
    assert!(upstream.enabled);
    assert!(upstream.endpoints().is_empty());
    assert!(upstream.alias.is_empty());
    let route = Route::default();
    assert!(route.enabled);
    assert_eq!(route.priority, 0);
    let plugin = Plugin::default();
    assert!(plugin.enabled);
    assert_eq!(plugin.config, serde_json::json!({}));
}
