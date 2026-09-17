//! Tests of the upstream aggregate and its configuration objects.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

use uuid::uuid;

use super::*;
use crate::domain::plugin::{PluginKind, PluginRef};

const TENANT: uuid::Uuid = uuid!("00000000-0000-0000-0000-0000000000d1");
const UPSTREAM_ID: uuid::Uuid = uuid!("00000000-0000-0000-0000-0000000000d2");

fn endpoint(scheme: EndpointScheme, host: &str) -> Endpoint {
    Endpoint::new(scheme, host, None).unwrap()
}

fn spec(server: ServerConfig) -> UpstreamSpec {
    UpstreamSpec {
        tenant_id: TENANT,
        alias: None,
        protocol: Protocol::Http,
        enabled: true,
        server,
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: Vec::new(),
    }
}

fn hostname_upstream() -> Upstream {
    let server =
        ServerConfig::new(vec![endpoint(EndpointScheme::Https, "api.example.com")]).unwrap();
    Upstream::new(UPSTREAM_ID, &spec(server)).unwrap()
}

#[test]
fn every_scheme_is_a_legal_endpoint_scheme() {
    for (token, expected) in [
        ("https", EndpointScheme::Https),
        ("http", EndpointScheme::Http),
        ("wss", EndpointScheme::Wss),
        ("wt", EndpointScheme::Wt),
        ("grpc", EndpointScheme::Grpc),
        ("HTTP", EndpointScheme::Http),
        ("HTTPS", EndpointScheme::Https),
    ] {
        assert_eq!(EndpointScheme::parse(token).unwrap(), expected, "{token}");
        assert_eq!(expected.as_str(), token.to_ascii_lowercase());
    }
    assert!(EndpointScheme::parse("ftp").is_err());
    assert!(EndpointScheme::parse("htt").is_err());
}

#[test]
fn the_scheme_enum_is_independent_of_the_egress_gate() {
    // `http` is a legal scheme value; the egress gate lives in the data plane
    // and is never consulted here.
    for scheme in EndpointScheme::ALL {
        assert!(
            Endpoint::new(scheme, "api.example.com", None).is_ok(),
            "{scheme}"
        );
    }
    assert!(EndpointScheme::Http.is_plaintext());
    assert!(!EndpointScheme::Https.is_plaintext());
    assert!(EndpointScheme::Wss.requires_tls());
    assert!(EndpointScheme::Wt.requires_tls());
    assert!(EndpointScheme::Grpc.requires_tls());
}

#[test]
fn scheme_defaults_follow_the_documented_ports() {
    assert_eq!(EndpointScheme::Http.default_port(), 80);
    assert_eq!(EndpointScheme::Https.default_port(), 443);
    assert_eq!(EndpointScheme::Wss.default_port(), 443);
    assert_eq!(EndpointScheme::Wt.default_port(), 443);
    assert_eq!(EndpointScheme::Grpc.default_port(), 443);
    assert!(EndpointScheme::Http.is_standard_port(80));
    assert!(EndpointScheme::Https.is_standard_port(443));
    assert!(!EndpointScheme::Https.is_standard_port(8443));
}

#[test]
fn endpoints_normalize_their_host() {
    let endpoint = Endpoint::new(EndpointScheme::Https, "API.Example.COM.", None).unwrap();
    assert_eq!(endpoint.host(), "api.example.com");
    let bracketed = Endpoint::parse_url("https://[2001:db8::1]:8443/x").unwrap();
    assert_eq!(bracketed.host(), "2001:db8::1");
    assert_eq!(bracketed.port(), Some(8443));
    assert_eq!(bracketed.effective_port(), 8443);
    assert_eq!(bracketed.authority(), "[2001:db8::1]:8443");
    assert!(
        bracketed
            .base_url()
            .starts_with("https://[2001:db8::1]:8443")
    );
}

#[test]
fn endpoints_reject_invalid_hosts() {
    for raw in [
        ("https://", "empty host"),
        ("https://-bad.example.com", "leading hyphen"),
        ("https://bad..example.com", "empty label"),
        ("https://bad_example.com", "underscore"),
        ("https://api.example.com:0", "zero port"),
        ("https://api.example.com:99999", "port out of range"),
        ("not-a-url", "no scheme"),
    ] {
        assert!(
            Endpoint::parse_url(raw.0).is_err(),
            "'{}' must be rejected",
            raw.0
        );
    }
    assert!(Endpoint::new(EndpointScheme::Https, "", None).is_err());
    assert!(Endpoint::new(EndpointScheme::Https, "a".repeat(260).as_str(), None).is_err());
}

#[test]
fn endpoints_parse_urls_with_any_documented_scheme() {
    for (raw, scheme) in [
        ("https://api.example.com", EndpointScheme::Https),
        ("http://api.example.com", EndpointScheme::Http),
        ("wss://api.example.com", EndpointScheme::Wss),
        ("grpc://api.example.com", EndpointScheme::Grpc),
    ] {
        let endpoint = Endpoint::parse_url(raw).unwrap();
        assert_eq!(endpoint.scheme(), scheme, "{raw}");
        assert_eq!(endpoint.effective_port(), scheme.default_port());
    }
}

#[test]
fn ip_endpoints_are_recognized() {
    let v4 = Endpoint::parse_url("https://10.0.1.1").unwrap();
    let v6 = Endpoint::parse_url("https://[2001:db8::1]").unwrap();
    let host = Endpoint::parse_url("https://api.example.com").unwrap();
    assert!(v4.is_ip_endpoint());
    assert!(v6.is_ip_endpoint());
    assert!(!host.is_ip_endpoint());
    assert_eq!(v4.authority(), "10.0.1.1:443");
}

#[test]
fn an_endpoint_pool_requires_at_least_one_endpoint() {
    assert!(ServerConfig::new(Vec::new()).is_err());
}

#[test]
fn an_endpoint_pool_must_share_scheme_and_port() {
    let mixed_scheme = ServerConfig::new(vec![
        endpoint(EndpointScheme::Https, "a.example.com"),
        endpoint(EndpointScheme::Http, "b.example.com"),
    ])
    .unwrap_err();
    assert_eq!(mixed_scheme.http_status(), 400);
    let mixed_port = ServerConfig::new(vec![
        Endpoint::new(EndpointScheme::Https, "a.example.com", None).unwrap(),
        Endpoint::new(EndpointScheme::Https, "b.example.com", Some(8443)).unwrap(),
    ])
    .unwrap_err();
    assert_eq!(mixed_port.http_status(), 400);
    assert!(mixed_port.to_string().contains("port"));
}

#[test]
fn a_homogeneous_pool_is_accepted() {
    let server = ServerConfig::new(vec![
        endpoint(EndpointScheme::Https, "a.example.com"),
        endpoint(EndpointScheme::Https, "b.example.com"),
    ])
    .unwrap();
    assert_eq!(server.endpoints().len(), 2);
    assert_eq!(server.primary().host(), "a.example.com");
}

#[test]
fn a_hostname_upstream_auto_derives_its_alias() {
    let upstream = hostname_upstream();
    assert_eq!(upstream.alias.as_str(), "api.example.com");
    assert_eq!(upstream.id, UPSTREAM_ID);
    assert_eq!(upstream.tenant_id, TENANT);
    assert!(upstream.is_enabled());
    assert_eq!(upstream.protocol, Protocol::Http);
    assert_eq!(
        upstream.protocol.gts_id(),
        crate::domain::upstream::PROTOCOL_HTTP
    );
}

#[test]
fn an_ip_upstream_requires_the_explicit_alias() {
    let server = ServerConfig::new(vec![endpoint(EndpointScheme::Https, "10.0.1.1")]).unwrap();
    assert!(Upstream::new(UPSTREAM_ID, &spec(server)).is_err());
    let mut with_alias =
        spec(ServerConfig::new(vec![endpoint(EndpointScheme::Https, "10.0.1.1")]).unwrap());
    with_alias.alias = Some(Alias::parse("my-service").unwrap());
    let upstream = Upstream::new(UPSTREAM_ID, &with_alias).unwrap();
    assert_eq!(upstream.alias.as_str(), "my-service");
}

#[test]
fn a_supplied_alias_must_equal_the_derived_one() {
    let server =
        ServerConfig::new(vec![endpoint(EndpointScheme::Https, "api.example.com")]).unwrap();
    let mut wrong = spec(server.clone());
    wrong.alias = Some(Alias::parse("other.example.com").unwrap());
    assert!(Upstream::new(UPSTREAM_ID, &wrong).is_err());
    let mut right = spec(server);
    right.alias = Some(Alias::parse("api.example.com").unwrap());
    assert!(Upstream::new(UPSTREAM_ID, &right).is_ok());
}

#[test]
fn invalid_tags_are_rejected() {
    let server =
        ServerConfig::new(vec![endpoint(EndpointScheme::Https, "api.example.com")]).unwrap();
    let mut with_bad_tag = spec(server);
    with_bad_tag.tags = vec![String::from("Bad Tag")];
    assert!(Upstream::new(UPSTREAM_ID, &with_bad_tag).is_err());
    let mut with_tags =
        spec(ServerConfig::new(vec![endpoint(EndpointScheme::Https, "api.example.com")]).unwrap());
    with_tags.tags = vec![String::from("team-a"), String::from("prod")];
    assert!(Upstream::new(UPSTREAM_ID, &with_tags).is_ok());
    assert!(
        validate_tags(&[
            String::from("ok_tag"),
            String::from("x-1"),
            String::from("y_2")
        ])
        .is_ok()
    );
    assert!(validate_tags(&[String::from("")]).is_err());
    assert!(validate_tags(&[String::from("UPPER")]).is_err());
}

#[test]
fn cors_forbids_credentials_with_the_wildcard_origin() {
    let mut cors = CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: vec![AllowedOrigin::Any],
        allowed_methods: vec![HttpMethod::Get],
        expose_headers: Vec::new(),
        allow_credentials: true,
    };
    assert!(cors.validate().is_err());
    cors.allowed_origins = vec![AllowedOrigin::parse("https://app.example.com").unwrap()];
    assert!(cors.validate().is_ok());
    cors.allow_credentials = false;
    cors.allowed_origins = vec![AllowedOrigin::Any];
    assert!(cors.validate().is_ok());
    // An upstream carrying this configuration is rejected.
    let server =
        ServerConfig::new(vec![endpoint(EndpointScheme::Https, "api.example.com")]).unwrap();
    let mut bad = spec(server);
    bad.cors = Some(CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: vec![AllowedOrigin::Any],
        allowed_methods: vec![HttpMethod::Get],
        expose_headers: Vec::new(),
        allow_credentials: true,
    });
    assert!(Upstream::new(UPSTREAM_ID, &bad).is_err());
}

#[test]
fn cors_origins_are_parsed_and_normalized() {
    assert_eq!(AllowedOrigin::parse("*").unwrap(), AllowedOrigin::Any);
    let origin = AllowedOrigin::parse("https://App.Example.COM").unwrap();
    assert_eq!(
        origin,
        AllowedOrigin::Exact("https://app.example.com".to_owned())
    );
    assert!(AllowedOrigin::parse("https://app.example.com/v1").is_err());
    assert!(AllowedOrigin::parse("https://app.example.com?x=1").is_err());
    assert!(AllowedOrigin::parse("not a url").is_err());
}

#[test]
fn header_rules_are_validated() {
    let mut headers = HeadersConfig {
        request: Some(RequestHeaderRules {
            set: BTreeMap::from([(String::from("X-OAGW-Target-Host"), String::from("h"))]),
            add: BTreeMap::new(),
            remove: vec![String::from("Connection")],
            passthrough: HeaderPassthrough::Allowlist,
            passthrough_allowlist: vec![String::from("X-Trace-Id")],
        }),
        response: Some(ResponseHeaderRules {
            set: BTreeMap::new(),
            add: BTreeMap::new(),
            remove: vec![String::from("Server")],
        }),
    };
    assert!(headers.validate().is_ok());
    if let Some(request) = &mut headers.request {
        request.remove.push(String::from("bad header"));
    }
    assert!(headers.validate().is_err());
}

#[test]
fn credential_references_are_redacted() {
    let secret = SecretRef::parse("cred://openai/key").unwrap();
    assert_eq!(secret.as_str(), "cred://openai/key");
    assert_eq!(secret.redacted(), "cred://***");
    assert_eq!(secret.to_string(), "cred://***");
    assert_eq!(format!("{secret:?}"), "SecretRef(***)");
    assert!(SecretRef::parse("openai/key").is_err());
    assert!(SecretRef::parse("cred://").is_err());
}

#[test]
fn auth_config_extracts_credential_references() {
    let auth = AuthConfig {
        sharing: SharingMode::Inherit,
        plugin: Some(
            PluginRef::builtin(PluginKind::Auth, crate::domain::plugin::AUTH_APIKEY).unwrap(),
        ),
        config: serde_json::json!({ "credential": "cred://openai/key", "header": "X-Api-Key" }),
    };
    let secret = auth
        .secret_ref("credential")
        .unwrap()
        .expect("credential reference");
    assert_eq!(secret.as_str(), "cred://openai/key");
    assert!(auth.secret_ref("header").unwrap().is_none());
    assert!(auth.secret_ref("absent").unwrap().is_none());
    // A plain value is not a credential reference at all.
    let plain = AuthConfig {
        sharing: SharingMode::Private,
        plugin: None,
        config: serde_json::json!({ "credential": "openai/key" }),
    };
    assert_eq!(plain.secret_ref("credential").unwrap(), None);
    // A malformed `cred://` value is rejected.
    let malformed = AuthConfig {
        sharing: SharingMode::Private,
        plugin: None,
        config: serde_json::json!({ "credential": "cred://" }),
    };
    assert!(malformed.secret_ref("credential").is_err());
}

#[test]
fn sharing_modes_drive_hierarchical_visibility() {
    assert!(!SharingMode::Private.is_visible_to_descendants());
    assert!(SharingMode::Inherit.is_visible_to_descendants());
    assert!(SharingMode::Enforce.is_visible_to_descendants());
    assert!(SharingMode::Inherit.allows_override());
    assert!(!SharingMode::Enforce.allows_override());
    assert!(SharingMode::Enforce.is_enforced());
    assert_eq!(SharingMode::parse("inherit").unwrap(), SharingMode::Inherit);
    assert_eq!(SharingMode::parse("ENFORCE").unwrap(), SharingMode::Enforce);
    assert!(SharingMode::parse("shared").is_err());
}

#[test]
fn sustained_rates_are_compared_per_second() {
    let per_minute = SustainedRate {
        rate: std::num::NonZeroU32::new(10).unwrap(),
        window: RateLimitWindow::Minute,
    };
    let per_second = SustainedRate {
        rate: std::num::NonZeroU32::new(5).unwrap(),
        window: RateLimitWindow::Second,
    };
    let per_day = SustainedRate {
        rate: std::num::NonZeroU32::new(100).unwrap(),
        window: RateLimitWindow::Day,
    };
    assert_eq!(per_second.per_second(), 5);
    assert_eq!(per_minute.per_second(), 600);
    assert_eq!(per_day.per_second(), 100 * 86_400);
    assert_eq!(per_second.stricter_of(per_minute), per_second);
    assert_eq!(per_minute.stricter_of(per_second), per_second);
    assert_eq!(per_day.stricter_of(per_second), per_second);
}

#[test]
fn an_inherited_rate_limit_is_taken_from_the_ancestor() {
    let ancestor = RateLimitConfig {
        sharing: SharingMode::Inherit,
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate: std::num::NonZeroU32::new(10).unwrap(),
            window: RateLimitWindow::Second,
        },
        burst: None,
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Reject,
        cost: std::num::NonZeroU32::new(1).unwrap(),
    };
    // No child configuration: the ancestor limit applies.
    assert_eq!(
        RateLimitConfig::effective(Some(&ancestor), None),
        Some(ancestor.clone())
    );
    // A child override replaces the ancestor limit.
    let mut child = ancestor.clone();
    child.sustained = SustainedRate {
        rate: std::num::NonZeroU32::new(20).unwrap(),
        window: RateLimitWindow::Second,
    };
    assert_eq!(
        RateLimitConfig::effective(Some(&ancestor), Some(&child))
            .unwrap()
            .sustained,
        child.sustained
    );
}

#[test]
fn an_enforced_rate_limit_can_only_be_tightened() {
    let ancestor = RateLimitConfig {
        sharing: SharingMode::Enforce,
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate: std::num::NonZeroU32::new(100).unwrap(),
            window: RateLimitWindow::Second,
        },
        burst: Some(BurstCapacity {
            capacity: std::num::NonZeroU32::new(500).unwrap(),
        }),
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Reject,
        cost: std::num::NonZeroU32::new(1).unwrap(),
    };
    let mut looser = ancestor.clone();
    looser_sustained(&mut looser);
    let effective = RateLimitConfig::effective(Some(&ancestor), Some(&looser)).unwrap();
    assert_eq!(
        effective.sustained, ancestor.sustained,
        "a descendant cannot loosen an enforced limit"
    );
    assert_eq!(effective.burst.unwrap().capacity.get(), 500);
    // A stricter descendant limit wins.
    let mut stricter = ancestor.clone();
    stricter.sustained = SustainedRate {
        rate: std::num::NonZeroU32::new(5).unwrap(),
        window: RateLimitWindow::Second,
    };
    stricter.burst = Some(BurstCapacity {
        capacity: std::num::NonZeroU32::new(10).unwrap(),
    });
    let effective = RateLimitConfig::effective(Some(&ancestor), Some(&stricter)).unwrap();
    assert_eq!(effective.sustained, stricter.sustained);
    assert_eq!(effective.burst.unwrap().capacity.get(), 10);
}

fn looser_sustained(config: &mut RateLimitConfig) {
    config.sustained = SustainedRate {
        rate: std::num::NonZeroU32::new(1_000).unwrap(),
        window: RateLimitWindow::Second,
    };
    config.burst = Some(BurstCapacity {
        capacity: std::num::NonZeroU32::new(2_000).unwrap(),
    });
}

#[test]
fn a_private_rate_limit_is_not_inherited() {
    let ancestor = RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: RateLimitAlgorithm::SlidingWindow,
        sustained: SustainedRate {
            rate: std::num::NonZeroU32::new(10).unwrap(),
            window: RateLimitWindow::Second,
        },
        burst: None,
        scope: RateLimitScope::User,
        strategy: RateLimitStrategy::Queue,
        cost: std::num::NonZeroU32::new(2).unwrap(),
    };
    assert_eq!(RateLimitConfig::effective(Some(&ancestor), None), None);
    let mut child = ancestor.clone();
    child.sharing = SharingMode::Inherit;
    assert_eq!(
        RateLimitConfig::effective(Some(&ancestor), Some(&child)).unwrap(),
        child
    );
}

#[test]
fn an_empty_ancestor_leaves_the_descendant_configuration_in_place() {
    let ancestor = RateLimitConfig {
        sharing: SharingMode::Enforce,
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate: std::num::NonZeroU32::new(10).unwrap(),
            window: RateLimitWindow::Second,
        },
        burst: None,
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Reject,
        cost: std::num::NonZeroU32::new(1).unwrap(),
    };
    assert_eq!(RateLimitConfig::effective(None, None), None);
    let mut child = ancestor.clone();
    child.sharing = SharingMode::Private;
    assert_eq!(
        RateLimitConfig::effective(None, Some(&child)).unwrap(),
        child
    );
}

#[test]
fn cors_and_plugin_chains_resolve_across_the_hierarchy() {
    let mut ancestor_cors = CorsConfig {
        sharing: SharingMode::Inherit,
        enabled: true,
        allowed_origins: vec![AllowedOrigin::parse("https://app.example.com").unwrap()],
        allowed_methods: vec![HttpMethod::Get, HttpMethod::Post],
        expose_headers: Vec::new(),
        allow_credentials: false,
    };
    // No descendant configuration: the ancestor value is inherited.
    assert_eq!(
        CorsConfig::effective(Some(&ancestor_cors), None),
        Some(ancestor_cors.clone())
    );
    // A descendant override replaces it.
    let child_cors = CorsConfig {
        sharing: SharingMode::Private,
        enabled: false,
        allowed_origins: Vec::new(),
        allowed_methods: Vec::new(),
        expose_headers: Vec::new(),
        allow_credentials: false,
    };
    assert_eq!(
        CorsConfig::effective(Some(&ancestor_cors), Some(&child_cors)),
        Some(child_cors.clone())
    );
    // A private ancestor is invisible.
    ancestor_cors.sharing = SharingMode::Private;
    assert_eq!(CorsConfig::effective(Some(&ancestor_cors), None), None);

    let ancestor_chain = PluginChain {
        sharing: SharingMode::Inherit,
        items: vec![
            PluginRef::builtin(PluginKind::Auth, crate::domain::plugin::AUTH_NOOP).unwrap(),
        ],
    };
    let child_chain = PluginChain {
        sharing: SharingMode::Private,
        items: vec![
            PluginRef::builtin(PluginKind::Auth, crate::domain::plugin::AUTH_NOOP).unwrap(),
            PluginRef::builtin(
                PluginKind::Transform,
                crate::domain::plugin::TRANSFORM_REQUEST_ID,
            )
            .unwrap(),
        ],
    };
    let effective = PluginChain::effective(Some(&ancestor_chain), Some(&child_chain)).unwrap();
    assert_eq!(
        effective.items.len(),
        2,
        "inherited plugins are not duplicated"
    );
    assert_eq!(effective.items[0], ancestor_chain.items[0]);
    assert_eq!(effective.items[1], child_chain.items[1]);
    assert_eq!(
        PluginChain::effective(Some(&ancestor_chain), None),
        Some(ancestor_chain)
    );
    assert_eq!(
        PluginChain::effective(None, Some(&child_chain)),
        Some(child_chain)
    );
}

#[test]
fn protocol_identifiers_are_gts_validated() {
    assert_eq!(
        Protocol::Http.gts_id(),
        crate::domain::upstream::PROTOCOL_HTTP
    );
    assert_eq!(
        Protocol::Grpc.gts_id(),
        crate::domain::upstream::PROTOCOL_GRPC
    );
    assert_eq!(Protocol::parse("grpc").unwrap(), Protocol::Grpc);
    assert_eq!(
        Protocol::parse(crate::domain::upstream::PROTOCOL_HTTP).unwrap(),
        Protocol::Http
    );
    assert!(Protocol::parse("websocket").is_err());
    assert_eq!(Protocol::Http.to_string(), "http");
}

#[test]
fn http_methods_parse_from_their_tokens() {
    for (token, method) in [
        ("GET", HttpMethod::Get),
        ("post", HttpMethod::Post),
        ("put", HttpMethod::Put),
        ("patch", HttpMethod::Patch),
        ("delete", HttpMethod::Delete),
        ("head", HttpMethod::Head),
        ("options", HttpMethod::Options),
    ] {
        assert_eq!(HttpMethod::parse(token).unwrap(), method, "{token}");
    }
    assert!(HttpMethod::parse("TRACE").is_err());
    assert!(HttpMethod::Get.is_route_match_method());
    assert!(!HttpMethod::Head.is_route_match_method());
    assert!(!HttpMethod::Options.is_route_match_method());
}

#[test]
fn the_upstream_gts_type_is_valid() {
    assert_eq!(
        crate::domain::upstream::UPSTREAM_GTS_TYPE,
        "gts.cf.core.oagw.upstream.v1~"
    );
    assert!(
        crate::domain::upstream::UPSTREAM_GTS_TYPE.starts_with("gts.cf.core.oagw.upstream.v1~")
    );
}
