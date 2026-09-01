//! Tests for [`crate::domain::validation`].

use uuid::Uuid;

use super::{
    MAX_HOSTNAME_LEN, MIN_PORT, MatchKey, UpstreamInput, Validator, classify_host, is_gts_type_id,
    is_scheme_allowed, is_standard_port, is_valid_alias, normalize_alias, render_plugin_source,
    route_match_key,
};
use crate::config::OagwConfig;
use crate::domain::model::{
    CorsConfig, Endpoint, GrpcMatch, HttpMatch, HttpMethod, PathSuffixMode, Protocol,
    RateLimitConfig, RateLimitWindow, Route, Scheme, SharingMode, SustainedRateConfig,
};

fn validator() -> Validator {
    Validator::new(OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    })
}

fn tls_validator() -> Validator {
    Validator::new(OagwConfig::default())
}

fn endpoint(host: &str) -> Endpoint {
    Endpoint {
        scheme: Scheme::Https,
        host: host.to_owned(),
        port: 443,
    }
}

fn endpoint_with(host: &str, scheme: Scheme, port: u16) -> Endpoint {
    Endpoint {
        scheme,
        host: host.to_owned(),
        port,
    }
}

fn input(endpoints: Vec<Endpoint>, alias: Option<&str>) -> UpstreamInput {
    UpstreamInput {
        alias: alias.map(ToOwned::to_owned),
        enabled: None,
        tags: Vec::new(),
        server: crate::domain::model::ServerConfig { endpoints },
        protocol: Protocol::Http,
        auth: None,
        headers: crate::domain::model::HeadersConfig::default(),
        plugins: crate::domain::model::PluginConfig::default(),
        rate_limit: None,
        cors: None,
    }
}

/// CORS configuration with only the fields under test set.
fn cors(allow_credentials: bool, origins: &[&str]) -> CorsConfig {
    CorsConfig {
        enabled: true,
        sharing: SharingMode::Private,
        allowed_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
        allowed_methods: Vec::new(),
        allow_headers: Vec::new(),
        expose_headers: Vec::new(),
        allow_credentials,
        max_age: None,
    }
}

/// Rate-limit budget with only the fields under test set.
fn rate_limit(rate: u64, burst_capacity: Option<u64>) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: crate::domain::model::RateLimitAlgorithm::TokenBucket,
        sustained: SustainedRateConfig {
            rate,
            window: RateLimitWindow::Second,
        },
        burst: burst_capacity.map(|capacity| crate::domain::model::BurstConfig {
            capacity: Some(capacity),
        }),
        scope: crate::domain::model::RateLimitScope::Tenant,
        strategy: crate::domain::model::RateLimitStrategy::Reject,
        response_headers: true,
        cost: 1,
    }
}

// -- alias derivation table (DESIGN section 3.1) -----------------------------

#[test]
fn single_hostname_on_a_standard_port_derives_the_hostname() {
    let derived = validator().derive_alias(&[endpoint("api.openai.com")]);
    assert_eq!(derived.as_deref(), Some("api.openai.com"));
}

#[test]
fn single_hostname_on_a_non_standard_port_derives_hostname_port() {
    let derived = validator().derive_alias(&[endpoint_with("api.openai.com", Scheme::Https, 8443)]);
    assert_eq!(derived.as_deref(), Some("api.openai.com:8443"));
}

#[test]
fn multi_host_pool_with_common_suffix_derives_the_suffix() {
    let derived = validator().derive_alias(&[endpoint("us.vendor.com"), endpoint("eu.vendor.com")]);
    assert_eq!(derived.as_deref(), Some("vendor.com"));
}

#[test]
fn multi_host_pool_with_suffix_and_non_standard_port_keeps_the_port() {
    let pool = vec![
        endpoint_with("us.vendor.com", Scheme::Https, 8443),
        endpoint_with("eu.vendor.com", Scheme::Https, 8443),
    ];
    let derived = validator().derive_alias(&pool);
    assert_eq!(derived.as_deref(), Some("vendor.com:8443"));
}

#[test]
fn plaintext_standard_port_is_80_not_443() {
    let pool = vec![endpoint_with("api.example.com", Scheme::Http, 80)];
    assert_eq!(
        validator().derive_alias(&pool).as_deref(),
        Some("api.example.com")
    );
    let non_standard = vec![endpoint_with("api.example.com", Scheme::Http, 8080)];
    assert_eq!(
        validator().derive_alias(&non_standard).as_deref(),
        Some("api.example.com:8080")
    );
}

#[test]
fn ip_endpoints_are_not_derivable() {
    let ipv4 = vec![endpoint("10.0.1.1"), endpoint("10.0.1.2")];
    assert!(validator().derive_alias(&ipv4).is_none());
    let ipv6 = vec![endpoint("2001:db8::1")];
    assert!(validator().derive_alias(&ipv6).is_none());
    let mixed = vec![endpoint("api.example.com"), endpoint("10.0.1.2")];
    assert!(validator().derive_alias(&mixed).is_none());
}

#[test]
fn bare_public_suffix_pools_are_not_derivable() {
    let pool = vec![endpoint("foo.co.uk"), endpoint("bar.co.uk")];
    assert!(
        validator().derive_alias(&pool).is_none(),
        "co.uk is a public suffix"
    );
}

#[test]
fn pools_without_a_common_suffix_are_not_derivable() {
    let pool = vec![endpoint("us.foo.com"), endpoint("eu.bar.com")];
    assert!(validator().derive_alias(&pool).is_none());
}

#[test]
fn identical_hosts_derive_the_host() {
    let pool = vec![endpoint("api.example.com"), endpoint("api.example.com")];
    assert_eq!(
        validator().derive_alias(&pool).as_deref(),
        Some("api.example.com")
    );
}

#[test]
fn nested_common_suffix_picks_the_longest_shared_tail() {
    let pool = vec![
        endpoint("a.b.vendor.com"),
        endpoint("c.b.vendor.com"),
        endpoint("d.b.vendor.com"),
    ];
    assert_eq!(
        validator().derive_alias(&pool).as_deref(),
        Some("b.vendor.com")
    );
}

#[test]
fn user_alias_must_equal_the_derived_value() {
    let pool = vec![endpoint("api.openai.com")];
    let error = validator()
        .resolve_create_alias(&pool, Some("openai"))
        .expect_err("differing alias is rejected");
    assert!(
        matches!(error, crate::domain::error::OagwError::Validation(_)),
        "{error:?}"
    );
    assert_eq!(error.context().alias.as_deref(), Some("api.openai.com"));
    assert_eq!(error.context().invalid_value.as_deref(), Some("openai"));
    assert!(error.detail().contains("api.openai.com"), "{error}");

    // The exact derived value is accepted silently (idempotent), including a
    // case/spelling variant that normalises to it.
    assert_eq!(
        validator()
            .resolve_create_alias(&pool, Some("api.openai.com"))
            .expect("exact"),
        "api.openai.com"
    );
    assert_eq!(
        validator()
            .resolve_create_alias(&pool, Some("Api.OpenAI.COM."))
            .expect("normalised"),
        "api.openai.com"
    );
}

#[test]
fn non_derivable_pools_require_an_explicit_alias() {
    let pool = vec![endpoint("10.0.1.1")];
    let missing = validator().resolve_create_alias(&pool, None);
    assert!(missing.is_err(), "alias is required");

    let supplied = validator().resolve_create_alias(&pool, Some("my-service"));
    assert_eq!(supplied.expect("alias accepted"), "my-service");

    let invalid = validator().resolve_create_alias(&pool, Some("Bad Alias!"));
    assert!(invalid.is_err(), "alias pattern is enforced");
}

// -- alias pattern + normalisation -------------------------------------------

#[test]
fn alias_pattern_is_enforced() {
    for accepted in [
        "a",
        "api",
        "api.openai.com",
        "vendor.com:8443",
        "my-service",
        "0abc",
    ] {
        assert!(is_valid_alias(accepted), "{accepted} must be accepted");
    }
    for rejected in [
        "", "-api", "api-", "Api", "api .com", "api com", "api/com", "api_com", ".api", "api.",
        "a b", "@", "*",
    ] {
        assert!(!is_valid_alias(rejected), "{rejected} must be rejected");
    }
}

#[test]
fn alias_normalisation_lowercases_and_strips_trailing_dots() {
    assert_eq!(normalize_alias("Api.OpenAI.COM."), "api.openai.com");
    assert_eq!(normalize_alias("  Vendor.COM  "), "vendor.com");
    assert_eq!(normalize_alias("already-normal"), "already-normal");
}

/// `unmatched` is the literal the metrics fold every unresolved request onto
/// ([`crate::domain::metrics::UNMATCHED_HOST`]), so no upstream may claim it:
/// its in-flight series would be indistinguishable from a request that resolved
/// to nothing.
#[test]
fn the_metrics_fold_literal_is_a_reserved_alias() {
    assert!(!is_valid_alias(crate::domain::metrics::UNMATCHED_HOST));

    let error = validator()
        .check_alias_shape(crate::domain::metrics::UNMATCHED_HOST)
        .expect_err("reserved alias is rejected");
    assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    assert!(
        error.detail().contains("reserved"),
        "the error must name the reservation: {}",
        error.detail()
    );
    assert_eq!(
        error.context().alias.as_deref(),
        Some(crate::domain::metrics::UNMATCHED_HOST)
    );

    // The reservation is an alias-level rule, so a hostname-based pool that
    // would derive it is rejected the same way through the create path.
    let derived = validator().resolve_create_alias(&[endpoint("unmatched")], None);
    assert!(derived.is_err(), "a derived reserved alias is rejected too");
}

// -- hostname / IP validation -------------------------------------------------

#[test]
fn valid_hosts_are_classified() {
    let cases = [
        (
            "api.openai.com",
            super::HostKind::Hostname("api.openai.com".to_owned()),
        ),
        (
            "API.OpenAI.COM",
            super::HostKind::Hostname("api.openai.com".to_owned()),
        ),
        (
            "api.openai.com.",
            super::HostKind::Hostname("api.openai.com".to_owned()),
        ),
        (
            "localhost",
            super::HostKind::Hostname("localhost".to_owned()),
        ),
        (
            "a-b.c-d.example",
            super::HostKind::Hostname("a-b.c-d.example".to_owned()),
        ),
        (
            "xn--80ak6aa92e.com",
            super::HostKind::Hostname("xn--80ak6aa92e.com".to_owned()),
        ),
        ("10.0.1.1", super::HostKind::Ipv4),
        ("255.255.255.255", super::HostKind::Ipv4),
        ("2001:db8::1", super::HostKind::Ipv6),
        ("[2001:db8::1]", super::HostKind::Ipv6),
        ("::1", super::HostKind::Ipv6),
    ];
    for (host, expected) in cases {
        assert_eq!(classify_host(host).expect(host), expected, "{host}");
    }
}

#[test]
fn invalid_hosts_are_rejected() {
    let rejected = [
        "",
        " ",
        "api .com",
        "api..com",
        ".api.com",
        "api.com:443",
        "[2001:db8::1]:443",
        "api.com/v1",
        "api.com?x=1",
        "user@api.com",
        "api%20.com",
        "api.com\\x",
        "-api.com",
        "api-.com",
        "256.1.1.1",
        "1.2.3.4.5",
        "1.2.3",
        "2001:db8::1::2",
        "2001:db8::1:",
    ];
    for host in rejected {
        assert!(classify_host(host).is_err(), "{host} must be rejected");
    }
}

#[test]
fn hostname_length_limits_are_enforced() {
    let long_label = "a".repeat(64);
    assert!(
        classify_host(&format!("api.{long_label}.com")).is_err(),
        "label > 63"
    );
    let ok_label = "a".repeat(63);
    assert!(classify_host(&format!("api.{ok_label}.com")).is_ok());

    let mut long_host = String::new();
    while long_host.len() < MAX_HOSTNAME_LEN {
        long_host.push_str("ab.");
    }
    assert!(classify_host(&long_host).is_err(), "host > 253");
}

// -- scheme gating, ports and pool consistency --------------------------------

#[test]
fn plaintext_schemes_are_gated_by_the_configuration() {
    for scheme in [Scheme::Https, Scheme::Wss, Scheme::Wt, Scheme::Grpc] {
        assert!(
            is_scheme_allowed(scheme, false),
            "{scheme:?} is always allowed"
        );
        assert!(is_scheme_allowed(scheme, true));
    }
    assert!(!is_scheme_allowed(Scheme::Http, false));
    assert!(!is_scheme_allowed(Scheme::Ws, false));
    assert!(is_scheme_allowed(Scheme::Http, true));
    assert!(is_scheme_allowed(Scheme::Ws, true));
}

#[test]
fn http_scheme_is_rejected_when_plaintext_upstreams_are_not_allowed() {
    let strict = tls_validator();
    let error = strict
        .validate_endpoint(&endpoint_with("api.example.com", Scheme::Http, 443))
        .expect_err("http requires allow_http_upstream");
    assert!(error.detail().contains("allow_http_upstream"), "{error}");
    assert!(
        error.detail().contains("server.endpoints[].scheme"),
        "{error}"
    );

    // The same endpoint validates once the operator allows plaintext.
    assert!(
        validator()
            .validate_endpoint(&endpoint_with("api.example.com", Scheme::Http, 80))
            .is_ok()
    );
    assert!(
        tls_validator()
            .validate_endpoint(&endpoint_with("api.example.com", Scheme::Wss, 443))
            .is_ok()
    );
}

#[test]
fn port_zero_is_rejected() {
    let error = validator()
        .validate_endpoint(&endpoint_with("api.example.com", Scheme::Https, 0))
        .expect_err("port 0 is invalid");
    assert_eq!(MIN_PORT, 1);
    assert!(error.detail().contains("port"), "{error}");
}

#[test]
fn pool_must_share_scheme_and_port() {
    let mixed_scheme = vec![
        endpoint_with("us.vendor.com", Scheme::Https, 443),
        endpoint_with("eu.vendor.com", Scheme::Wss, 443),
    ];
    let error = validator()
        .validate_pool(&mixed_scheme)
        .expect_err("scheme mismatch");
    assert!(error.detail().contains("same scheme"), "{error}");

    let mixed_port = vec![
        endpoint_with("us.vendor.com", Scheme::Https, 443),
        endpoint_with("eu.vendor.com", Scheme::Https, 8443),
    ];
    let error = validator()
        .validate_pool(&mixed_port)
        .expect_err("port mismatch");
    assert!(error.detail().contains("same port"), "{error}");

    let empty: Vec<Endpoint> = Vec::new();
    let error = validator()
        .validate_pool(&empty)
        .expect_err("pool must not be empty");
    assert!(error.detail().contains("at least one endpoint"), "{error}");

    let consistent = vec![
        endpoint_with("us.vendor.com", Scheme::Https, 8443),
        endpoint_with("eu.vendor.com", Scheme::Https, 8443),
    ];
    assert!(validator().validate_pool(&consistent).is_ok());
}

// -- CORS and rate-limit budgets ----------------------------------------------

#[test]
fn allow_credentials_rejects_the_wildcard_origin() {
    assert!(
        validator()
            .validate_cors(&cors(true, &["https://app.example.com"]))
            .is_ok()
    );

    let error = validator()
        .validate_cors(&cors(true, &["*"]))
        .expect_err("wildcard origin with credentials");
    assert!(error.detail().contains("allow_credentials"), "{error}");

    assert!(
        validator().validate_cors(&cors(false, &["*"])).is_ok(),
        "wildcard without credentials is fine"
    );
}

#[test]
fn rate_limit_budget_requires_positive_rates() {
    assert!(
        validator()
            .validate_rate_limit(&rate_limit(1, None))
            .is_ok()
    );
    assert!(
        validator()
            .validate_rate_limit(&rate_limit(10, Some(20)))
            .is_ok()
    );

    let error = validator()
        .validate_rate_limit(&rate_limit(0, None))
        .expect_err("rate 0");
    assert!(error.detail().contains("sustained.rate"), "{error}");

    let error = validator()
        .validate_rate_limit(&rate_limit(10, Some(0)))
        .expect_err("capacity 0");
    assert!(error.detail().contains("burst.capacity"), "{error}");
}

// -- alias immutability on replace (DESIGN section 3.1 update matrix) ----------

fn stored(endpoints: Vec<Endpoint>, alias: &str) -> crate::domain::model::Upstream {
    crate::domain::model::Upstream {
        alias: alias.to_owned(),
        server: crate::domain::model::ServerConfig { endpoints },
        ..crate::domain::model::Upstream::default()
    }
}

fn replace(endpoints: Vec<Endpoint>, alias: Option<&str>) -> UpstreamInput {
    input(endpoints, alias)
}

#[test]
fn derivable_pool_keeps_its_alias_while_endpoints_change() {
    let existing = stored(vec![endpoint("api.openai.com")], "api.openai.com");
    let draft = replace(
        vec![endpoint("eu.api.openai.com"), endpoint("us.api.openai.com")],
        None,
    );
    let replaced = validator()
        .validate_upstream_replace(&existing, &draft)
        .expect("same derived alias");
    assert_eq!(replaced.alias, "api.openai.com");

    // A pool whose recomputed alias differs is rejected.
    let draft = replace(vec![endpoint("api.vendor.com")], None);
    let error = validator()
        .validate_upstream_replace(&existing, &draft)
        .expect_err("alias would change");
    assert!(error.detail().contains("immutable"), "{error}");
    assert_eq!(error.context().alias.as_deref(), Some("api.openai.com"));
}

#[test]
fn derivable_pool_cannot_become_non_derivable() {
    let existing = stored(vec![endpoint("api.openai.com")], "api.openai.com");
    let draft = replace(vec![endpoint("10.0.0.1")], Some("api.openai.com"));
    let error = validator()
        .validate_upstream_replace(&existing, &draft)
        .expect_err("pool turns non-derivable");
    assert!(error.detail().contains("non-derivable"), "{error}");
}

#[test]
fn non_derivable_pool_keeps_an_explicit_alias() {
    let existing = stored(vec![endpoint("10.0.0.1")], "payments");
    let draft = replace(vec![endpoint("10.0.0.2")], None);
    let replaced = validator()
        .validate_upstream_replace(&existing, &draft)
        .expect("alias is kept");
    assert_eq!(replaced.alias, "payments");

    let draft = replace(vec![endpoint("10.0.0.2")], Some("other"));
    let error = validator()
        .validate_upstream_replace(&existing, &draft)
        .expect_err("alias is immutable");
    assert!(error.detail().contains("cannot be overridden"), "{error}");
}

#[test]
fn non_derivable_pool_cannot_become_derivable_with_a_new_alias() {
    let existing = stored(vec![endpoint("10.0.0.1")], "payments");
    let draft = replace(vec![endpoint("api.openai.com")], None);
    let error = validator()
        .validate_upstream_replace(&existing, &draft)
        .expect_err("derived alias differs from the stored one");
    assert!(error.detail().contains("immutable"), "{error}");
}

#[test]
fn validate_upstream_defaults_enabled_and_derives_the_alias() {
    let draft = input(vec![endpoint("api.openai.com")], None);
    let model = validator().validate_upstream(&draft).expect("valid");
    assert!(model.enabled, "enabled defaults to true");
    assert_eq!(model.alias, "api.openai.com");
    assert!(model.id.is_nil(), "id is server-generated");
    assert!(model.tenant_id.is_nil(), "tenant_id is server-generated");
}

#[test]
fn validate_upstream_rejects_an_invalid_pool_before_the_alias() {
    let draft = input(vec![endpoint("10.0.1.1")], None);
    let error = validator()
        .validate_upstream(&draft)
        .expect_err("alias required");
    assert!(error.detail().contains("explicit alias"), "{error}");

    let draft = input(Vec::new(), Some("payments"));
    let error = validator()
        .validate_upstream(&draft)
        .expect_err("empty pool");
    assert!(error.detail().contains("at least one endpoint"), "{error}");
}

// -- route validation -----------------------------------------------------------

fn http_match(path: &str, methods: &[HttpMethod]) -> HttpMatch {
    HttpMatch {
        methods: methods.to_vec(),
        path: path.to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Append,
    }
}

fn upstream(protocol: Protocol) -> crate::domain::model::Upstream {
    crate::domain::model::Upstream {
        alias: "api.example.com".to_owned(),
        protocol,
        ..crate::domain::model::Upstream::default()
    }
}

fn route_input(r#match: crate::domain::model::RouteMatch) -> super::RouteInput {
    super::RouteInput {
        r#match,
        headers: crate::domain::model::HeadersConfig::default(),
        plugins: crate::domain::model::PluginConfig::default(),
        rate_limit: None,
        cors: None,
        enabled: None,
        priority: Some(3),
        tags: Vec::new(),
    }
}

#[test]
fn validate_route_requires_exactly_one_match_variant() {
    let owner = upstream(Protocol::Http);
    let empty = route_input(crate::domain::model::RouteMatch::default());
    let error = validator()
        .validate_route(Uuid::from_u128(1), &owner, &empty)
        .expect_err("match required");
    assert!(
        error.detail().contains("one of `http` or `grpc`"),
        "{error}"
    );

    let both = crate::domain::model::RouteMatch {
        http: Some(http_match("/v1", &[HttpMethod::Get])),
        grpc: Some(GrpcMatch {
            service: "svc.v1.Svc".to_owned(),
            method: "Get".to_owned(),
        }),
    };
    let error = validator()
        .validate_route(Uuid::from_u128(1), &owner, &route_input(both))
        .expect_err("ambiguous match");
    assert!(error.detail().contains("exactly one"), "{error}");
}

#[test]
fn validate_route_enforces_match_shape_and_protocol() {
    let owner = upstream(Protocol::Http);
    let no_methods = route_input(crate::domain::model::RouteMatch {
        http: Some(http_match("/v1", &[])),
        grpc: None,
    });
    let error = validator()
        .validate_route(Uuid::from_u128(1), &owner, &no_methods)
        .expect_err("methods required");
    assert!(error.detail().contains("at least one method"), "{error}");

    let empty_path = route_input(crate::domain::model::RouteMatch {
        http: Some(http_match("", &[HttpMethod::Get])),
        grpc: None,
    });
    let error = validator()
        .validate_route(Uuid::from_u128(1), &owner, &empty_path)
        .expect_err("path required");
    assert!(error.detail().contains("path"), "{error}");

    let relative_path = route_input(crate::domain::model::RouteMatch {
        http: Some(http_match("v1/orders", &[HttpMethod::Get])),
        grpc: None,
    });
    let error = validator()
        .validate_route(Uuid::from_u128(1), &owner, &relative_path)
        .expect_err("path must be rooted");
    assert!(error.detail().contains("must start with '/'"), "{error}");
    assert_eq!(
        error.context().path.as_deref(),
        Some("v1/orders"),
        "{error}"
    );

    let grpc_on_http_upstream = route_input(crate::domain::model::RouteMatch {
        http: None,
        grpc: Some(GrpcMatch {
            service: "svc.v1.Svc".to_owned(),
            method: "Get".to_owned(),
        }),
    });
    let error = validator()
        .validate_route(Uuid::from_u128(1), &owner, &grpc_on_http_upstream)
        .expect_err("protocol mismatch");
    assert!(error.detail().contains("gRPC"), "{error}");

    let grpc_owner = upstream(Protocol::Grpc);
    let ok = validator().validate_route(Uuid::from_u128(1), &grpc_owner, &grpc_on_http_upstream);
    assert!(ok.is_ok(), "{ok:?}");

    let http_on_grpc_upstream = route_input(crate::domain::model::RouteMatch {
        http: Some(http_match("/v1", &[HttpMethod::Get])),
        grpc: None,
    });
    let error = validator()
        .validate_route(Uuid::from_u128(1), &grpc_owner, &http_on_grpc_upstream)
        .expect_err("protocol mismatch");
    assert!(error.detail().contains("HTTP"), "{error}");
}

#[test]
fn route_match_key_ignores_method_order_and_optional_fields() {
    let mut route = Route {
        r#match: crate::domain::model::RouteMatch {
            http: Some(http_match("/v1/pay", &[HttpMethod::Get, HttpMethod::Post])),
            grpc: None,
        },
        priority: 4,
        ..Route::default()
    };
    let mut reordered = route.clone();
    reordered.r#match.http.as_mut().expect("http").methods =
        vec![HttpMethod::Post, HttpMethod::Get];
    assert_eq!(route_match_key(&route), route_match_key(&reordered));

    reordered
        .r#match
        .http
        .as_mut()
        .expect("http")
        .query_allowlist = vec!["q".to_owned()];
    assert_eq!(
        route_match_key(&route),
        route_match_key(&reordered),
        "query allowlist is not part of the key"
    );

    reordered.priority = 9;
    assert_eq!(
        route_match_key(&route),
        route_match_key(&reordered),
        "priority is compared separately from the match rule"
    );

    route.r#match.http.as_mut().expect("http").path = "/v2/pay".to_owned();
    assert_ne!(route_match_key(&route), route_match_key(&reordered));

    reordered.r#match.http.as_mut().expect("http").path = "/v2/pay".to_owned();
    reordered.r#match.http.as_mut().expect("http").methods = vec![HttpMethod::Get];
    assert_ne!(
        route_match_key(&route),
        route_match_key(&reordered),
        "a different method set is a different match rule"
    );
}

#[test]
fn route_match_key_covers_grpc_matches() {
    let route = Route {
        r#match: crate::domain::model::RouteMatch {
            http: None,
            grpc: Some(GrpcMatch {
                service: "svc.v1.Svc".to_owned(),
                method: "Get".to_owned(),
            }),
        },
        priority: 0,
        ..Route::default()
    };
    assert_eq!(
        route_match_key(&route),
        MatchKey::Grpc {
            service: "svc.v1.Svc".to_owned(),
            method: "Get".to_owned(),
        }
    );
}

// -- plugin validation ----------------------------------------------------------

#[test]
fn validate_plugin_defaults_enabled_and_requires_a_gts_type() {
    let draft = super::PluginInput {
        plugin_type: "gts.cf.core.oagw.guard_plugin.v1".to_owned(),
        config: serde_json::json!({"headers": ["x-request-id"]}),
        enabled: None,
        tags: vec!["guard".to_owned()],
    };
    let model = validator().validate_plugin(&draft).expect("valid");
    assert!(model.enabled, "enabled defaults to true");
    assert!(model.id.is_nil());
    assert_eq!(model.plugin_type, "gts.cf.core.oagw.guard_plugin.v1");

    for rejected in [
        "",
        "guard",
        "cf.core.oagw.guard_plugin.v1",
        "GTS.cf.core.x.v1",
    ] {
        let draft = super::PluginInput {
            plugin_type: rejected.to_owned(),
            config: serde_json::json!({}),
            enabled: None,
            tags: Vec::new(),
        };
        assert!(
            validator().validate_plugin(&draft).is_err(),
            "{rejected} must be rejected"
        );
    }
}

#[test]
fn validate_plugin_requires_an_object_config() {
    let draft = super::PluginInput {
        plugin_type: "gts.cf.core.oagw.guard_plugin.v1".to_owned(),
        config: serde_json::json!([1, 2, 3]),
        enabled: None,
        tags: Vec::new(),
    };
    let error = validator()
        .validate_plugin(&draft)
        .expect_err("array config");
    assert!(error.detail().contains("`config`"), "{error}");
}

#[test]
fn gts_type_ids_are_recognised() {
    assert!(is_gts_type_id("gts.cf.core.oagw.guard_plugin.v1"));
    assert!(is_gts_type_id("gts.cf.core.oagw.auth_plugin.v1"));
    assert!(!is_gts_type_id("gts.cf.v1"));
    assert!(!is_gts_type_id("cf.core.oagw.guard_plugin.v1"));
    assert!(!is_gts_type_id("gts.CF.Core.v1"));
    assert!(!is_gts_type_id(
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.apikey.v1"
    ));
}

// -- standard ports and plugin source rendering ---------------------------------

#[test]
fn standard_ports_follow_the_scheme() {
    assert!(is_standard_port(Scheme::Https, 443));
    assert!(is_standard_port(Scheme::Wss, 443));
    assert!(is_standard_port(Scheme::Wt, 443));
    assert!(is_standard_port(Scheme::Grpc, 443));
    assert!(is_standard_port(Scheme::Http, 80));
    assert!(is_standard_port(Scheme::Ws, 80));
    assert!(!is_standard_port(Scheme::Https, 8443));
    assert!(!is_standard_port(Scheme::Http, 8080));
}

#[test]
fn plugin_source_is_deterministic_and_sorted() {
    let tenant = Uuid::from_u128(0x77);
    let first = render_plugin_source(
        "gts.cf.core.oagw.guard_plugin.v1~00000000-0000-0000-0000-000000000004",
        "gts.cf.core.oagw.guard_plugin.v1",
        &tenant,
        true,
        &["guard".to_owned()],
        &serde_json::json!({"z": 1, "a": {"b": 2}}),
    );
    let second = render_plugin_source(
        "gts.cf.core.oagw.guard_plugin.v1~00000000-0000-0000-0000-000000000004",
        "gts.cf.core.oagw.guard_plugin.v1",
        &tenant,
        true,
        &["guard".to_owned()],
        &serde_json::json!({"a": {"b": 2}, "z": 1}),
    );
    assert_eq!(first, second, "key order must not matter");
    assert!(
        first.contains("PLUGIN_TYPE = \"gts.cf.core.oagw.guard_plugin.v1\""),
        "{first}"
    );
    assert!(first.contains("ENABLED = True"), "{first}");
    assert!(first.contains("TAGS = [\"guard\"]"), "{first}");
    assert!(
        first.starts_with("# plugin_id: gts.cf.core.oagw.guard_plugin.v1~"),
        "{first}"
    );
}

#[test]
fn standard_errors_carry_oagw_context() {
    // Spot check that the errors raised by the validator keep the DESIGN
    // section 3.3 shape (400 + the GTS validation type).
    let error = validator()
        .resolve_create_alias(&[endpoint("10.0.0.1")], None)
        .expect_err("alias required");
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
    assert_eq!(
        error.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert_eq!(error.context().alias, None, "no alias was derived");
}

// -- tag validation (schema pattern `^[a-z0-9_-]+$`) ----------------------------

#[test]
fn tags_must_match_the_schema_pattern_and_bounded_counts() {
    let v = validator();
    let error = v
        .validate_tags(&["Bad Tag".to_owned()], "tags")
        .expect_err("uppercase and space");
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
    assert!(error.detail().contains("^[a-z0-9_-]+$"), "{error}");
    assert_eq!(error.context().invalid_value.as_deref(), Some("Bad Tag"));

    let long = "a".repeat(65);
    let error = v
        .validate_tags(std::slice::from_ref(&long), "tags")
        .expect_err("too long");
    assert!(error.detail().contains("64"), "{error}");

    let too_many: Vec<String> = (0..33).map(|index| format!("tag-{index}")).collect();
    let error = v.validate_tags(&too_many, "tags").expect_err("too many");
    assert!(error.detail().contains("at most 32 tags"), "{error}");

    v.validate_tags(&["payments_eu-1".to_owned()], "tags")
        .expect("pattern accepted");
    v.validate_tags(&[], "tags").expect("empty is fine");
}

#[test]
fn tags_are_validated_on_every_resource_kind() {
    let v = validator();
    let mut draft = input(vec![endpoint("api.openai.com")], Some("api.openai.com"));
    draft.tags = vec!["NOT VALID".to_owned()];
    let error = v.validate_upstream(&draft).expect_err("upstream tags");
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);

    let mut plugin = super::PluginInput {
        plugin_type: "gts.cf.core.oagw.guard_plugin.v1".to_owned(),
        config: serde_json::json!({}),
        enabled: None,
        tags: vec!["NOT VALID".to_owned()],
    };
    let error = v.validate_plugin(&plugin).expect_err("plugin tags");
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);

    let mut route = route_input(crate::domain::model::RouteMatch {
        http: Some(http_match("/v1", &[HttpMethod::Get])),
        grpc: None,
    });
    route.tags = vec!["NOT VALID".to_owned()];
    let error = v
        .validate_route(Uuid::from_u128(1), &upstream(Protocol::Http), &route)
        .expect_err("route tags");
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);

    plugin.tags = vec!["ok_tag".to_owned()];
    v.validate_plugin(&plugin).expect("valid plugin tags");
}

// -- plugin binding resolution ---------------------------------------------------

fn catalog(ids: &[u128]) -> super::PluginCatalog {
    super::PluginCatalog::of(ids.iter().map(|id| Uuid::from_u128(*id)))
}

#[test]
fn a_builtin_reference_resolves_in_both_spellings() {
    let catalog = catalog(&[]);
    assert!(catalog.resolves(crate::domain::plugin::builtin::APIKEY_AUTH));
    let fragment = crate::domain::plugin::builtin::APIKEY_AUTH
        .rsplit('~')
        .next()
        .expect("fragment");
    assert!(catalog.resolves(fragment), "{fragment}");
    assert!(
        !catalog.resolves("cf.core.oagw.logging.v1"),
        "catalog-only id"
    );
}

#[test]
fn a_custom_reference_resolves_as_a_bare_uuid_and_as_a_gts_id() {
    let id = Uuid::from_u128(0xC1);
    let catalog = catalog(&[0xC1]);
    assert!(catalog.resolves(&id.to_string()));
    assert!(catalog.resolves(&format!("gts.cf.core.oagw.plugin.v1~{id}")));
    assert!(!catalog.resolves(&Uuid::from_u128(0xC2).to_string()));
    assert!(!catalog.resolves("gts.cf.core.oagw.plugin.v1~not-a-uuid"));
}

#[test]
fn an_unknown_upstream_plugin_reference_is_a_400() {
    let v = validator();

    // A catalogue-only auth id: no implementation backs it, so it can never
    // resolve (503 `plugin.not_found` at proxy time, 400 here).
    let mut draft = input(vec![endpoint("api.openai.com")], Some("api.openai.com"));
    draft.auth = Some(crate::domain::model::AuthConfig {
        auth_type: "cf.core.oagw.logging.v1".to_owned(),
        sharing: SharingMode::Private,
        config: serde_json::json!({}),
    });
    let validated = v.validate_upstream(&draft).expect("valid draft");
    let error = v
        .validate_bindings(&validated, &catalog(&[]))
        .expect_err("unknown auth plugin");
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
    assert_eq!(
        error.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(
        error.detail().contains("cf.core.oagw.logging.v1"),
        "{error}"
    );
    assert_eq!(
        error.context().plugin_id.as_deref(),
        Some("cf.core.oagw.logging.v1")
    );

    // An unknown custom-plugin UUID in the chain is rejected the same way.
    let mut draft = input(vec![endpoint("api.openai.com")], Some("api.openai.com"));
    draft
        .plugins
        .items
        .push(crate::domain::model::PluginBinding::new(
            "00000000-0000-0000-0000-0000000000c2",
            serde_json::json!({}),
        ));
    let validated = v.validate_upstream(&draft).expect("valid draft");
    let error = v
        .validate_bindings(&validated, &catalog(&[0xC1]))
        .expect_err("unknown plugin in the chain");
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
    assert!(
        error
            .detail()
            .contains("00000000-0000-0000-0000-0000000000c2"),
        "{error}"
    );

    // A reference that resolves, in either spelling, is accepted.
    let mut draft = input(vec![endpoint("api.openai.com")], Some("api.openai.com"));
    draft.auth = Some(crate::domain::model::AuthConfig {
        auth_type: crate::domain::plugin::builtin::APIKEY_AUTH.to_owned(),
        sharing: SharingMode::Private,
        config: serde_json::json!({}),
    });
    draft
        .plugins
        .items
        .push(crate::domain::model::PluginBinding::new(
            "00000000-0000-0000-0000-0000000000c1",
            serde_json::json!({}),
        ));
    let validated = v.validate_upstream(&draft).expect("valid draft");
    v.validate_bindings(&validated, &catalog(&[0xC1]))
        .expect("resolves");
}

#[test]
fn a_plugin_bound_in_the_wrong_slot_is_a_400() {
    let v = validator();
    // A transform reference in the `auth` slot: the kind does not fit the slot,
    // so the binding is rejected before it can produce a per-request 503.
    let mut draft = input(vec![endpoint("api.openai.com")], Some("api.openai.com"));
    draft.auth = Some(crate::domain::model::AuthConfig {
        auth_type: crate::domain::plugin::builtin::REQUEST_ID_TRANSFORM.to_owned(),
        sharing: SharingMode::Private,
        config: serde_json::json!({}),
    });
    let validated = v.validate_upstream(&draft).expect("valid draft");
    let error = v
        .validate_bindings(&validated, &catalog(&[]))
        .expect_err("transform in the auth slot");
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
    assert_eq!(
        error.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert!(
        error
            .detail()
            .contains(crate::domain::plugin::builtin::REQUEST_ID_TRANSFORM),
        "the detail names the offending reference: {error}"
    );
    assert!(
        error
            .detail()
            .contains(crate::domain::plugin::AUTH_PLUGIN_TYPE_ID),
        "the detail names the expected kind: {error}"
    );
    assert!(
        error.detail().contains("auth"),
        "the detail names the slot: {error}"
    );
    assert_eq!(
        error.context().plugin_id.as_deref(),
        Some(crate::domain::plugin::builtin::REQUEST_ID_TRANSFORM)
    );

    // The same reference in a chain slot, which is what it was created for.
    let mut draft = input(vec![endpoint("api.openai.com")], Some("api.openai.com"));
    draft
        .plugins
        .items
        .push(crate::domain::model::PluginBinding::new(
            crate::domain::plugin::builtin::REQUEST_ID_TRANSFORM,
            serde_json::json!({}),
        ));
    let validated = v.validate_upstream(&draft).expect("valid draft");
    v.validate_bindings(&validated, &catalog(&[]))
        .expect("a transform belongs in the chain");
}

#[test]
fn a_guard_bound_in_the_auth_slot_is_rejected_and_in_the_chain_is_accepted() {
    let v = validator();
    let guard = crate::domain::plugin::builtin::REQUIRED_HEADERS_GUARD;

    let mut draft = input(vec![endpoint("api.openai.com")], Some("api.openai.com"));
    draft.auth = Some(crate::domain::model::AuthConfig {
        auth_type: guard.to_owned(),
        sharing: SharingMode::Private,
        config: serde_json::json!({}),
    });
    let validated = v.validate_upstream(&draft).expect("valid draft");
    let error = v
        .validate_bindings(&validated, &catalog(&[]))
        .expect_err("guard in the auth slot");
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);

    let mut draft = input(vec![endpoint("api.openai.com")], Some("api.openai.com"));
    draft
        .plugins
        .items
        .push(crate::domain::model::PluginBinding::new(
            guard,
            serde_json::json!({}),
        ));
    let validated = v.validate_upstream(&draft).expect("valid draft");
    v.validate_bindings(&validated, &catalog(&[]))
        .expect("a guard belongs in the chain");
}

#[test]
fn an_auth_plugin_bound_in_a_chain_is_rejected_and_in_the_auth_slot_is_accepted() {
    let v = validator();
    let auth = crate::domain::plugin::builtin::APIKEY_AUTH;

    let mut draft = input(vec![endpoint("api.openai.com")], Some("api.openai.com"));
    draft
        .plugins
        .items
        .push(crate::domain::model::PluginBinding::new(
            auth,
            serde_json::json!({}),
        ));
    let validated = v.validate_upstream(&draft).expect("valid draft");
    let error = v
        .validate_bindings(&validated, &catalog(&[]))
        .expect_err("auth plugin in the chain");
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
    assert!(
        error
            .detail()
            .contains(crate::domain::plugin::TRANSFORM_PLUGIN_TYPE_ID),
        "the detail names what a chain slot takes: {error}"
    );

    let mut draft = input(vec![endpoint("api.openai.com")], Some("api.openai.com"));
    draft.auth = Some(crate::domain::model::AuthConfig {
        auth_type: auth.to_owned(),
        sharing: SharingMode::Private,
        config: serde_json::json!({}),
    });
    let validated = v.validate_upstream(&draft).expect("valid draft");
    v.validate_bindings(&validated, &catalog(&[]))
        .expect("an auth plugin belongs in the auth slot");
}

#[test]
fn a_mismatched_custom_plugin_is_caught_when_the_catalogue_carries_its_kind() {
    let v = validator();
    // The tenant owns a guard plugin and bound it into the `auth` slot: the
    // catalogue knows the plugin's base type, so the binding is rejected.
    let plugin = crate::domain::model::Plugin {
        id: Uuid::from_u128(0xC1),
        plugin_type: crate::domain::plugin::GUARD_PLUGIN_TYPE_ID.to_owned(),
        ..crate::domain::model::Plugin::default()
    };
    let reference = plugin.id.to_string();
    let typed = super::PluginCatalog::of_plugins(&[std::sync::Arc::new(plugin)]);

    let mut draft = input(vec![endpoint("api.openai.com")], Some("api.openai.com"));
    draft.auth = Some(crate::domain::model::AuthConfig {
        auth_type: reference.clone(),
        sharing: SharingMode::Private,
        config: serde_json::json!({}),
    });
    let validated = v.validate_upstream(&draft).expect("valid draft");
    let error = v
        .validate_bindings(&validated, &typed)
        .expect_err("guard in the auth slot");
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);

    // In its own slot the same reference is accepted.
    let mut draft = input(vec![endpoint("api.openai.com")], Some("api.openai.com"));
    draft
        .plugins
        .items
        .push(crate::domain::model::PluginBinding::new(
            reference,
            serde_json::json!({}),
        ));
    let validated = v.validate_upstream(&draft).expect("valid draft");
    v.validate_bindings(&validated, &typed)
        .expect("a custom guard belongs in the chain");
}

#[test]
fn a_mismatched_route_binding_is_rejected() {
    let owner = upstream(Protocol::Http);
    let mut route = route_input(crate::domain::model::RouteMatch {
        http: Some(http_match("/v1", &[HttpMethod::Get])),
        grpc: None,
    });
    route
        .plugins
        .items
        .push(crate::domain::model::PluginBinding::new(
            crate::domain::plugin::builtin::APIKEY_AUTH,
            serde_json::json!({}),
        ));
    let validated = validator()
        .validate_route(Uuid::from_u128(1), &owner, &route)
        .expect("route shape is valid");
    let error = validator()
        .validate_route_bindings(&validated, &catalog(&[]))
        .expect_err("auth plugin in a route chain");
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
    assert!(
        error
            .detail()
            .contains(crate::domain::plugin::builtin::APIKEY_AUTH),
        "{error}"
    );
}

#[test]
fn an_unknown_route_plugin_reference_is_a_400() {
    let owner = upstream(Protocol::Http);
    let mut route = route_input(crate::domain::model::RouteMatch {
        http: Some(http_match("/v1", &[HttpMethod::Get])),
        grpc: None,
    });
    route
        .plugins
        .items
        .push(crate::domain::model::PluginBinding::new(
            "gts.cf.core.oagw.plugin.v1~00000000-0000-0000-0000-0000000000c1",
            serde_json::json!({}),
        ));
    let validated = validator()
        .validate_route(Uuid::from_u128(1), &owner, &route)
        .expect("route shape is valid");
    let error = validator()
        .validate_route_bindings(&validated, &catalog(&[]))
        .expect_err("unknown plugin");
    assert_eq!(error.status(), http::StatusCode::BAD_REQUEST);
    assert!(
        error
            .detail()
            .contains("00000000-0000-0000-0000-0000000000c1"),
        "{error}"
    );
}
