//! Unit tests for the structural invariant helpers.
// @cpt-dod:cpt-cf-oagw-dod-upstream-management-endpoint-scheme-validation:p1
// @cpt-dod:cpt-cf-oagw-dod-upstream-management-header-transform-config:p1
// @cpt-dod:cpt-cf-oagw-dod-upstream-management-nested-subconfig:p1
// @cpt-dod:cpt-cf-oagw-dod-upstream-management-plugin-references:p1
// @cpt-dod:cpt-cf-oagw-dod-upstream-management-tags:p1

use super::*;
use crate::domain::dto::{
    ALIAS_PATTERN, CorsConfig, Endpoint, EndpointScheme, GrpcMatch, HttpMatch, HttpMethod,
    MatchConfig, PathSuffixMode, RateScope, RateStrategy, RateWindow, SharingMode, Upstream,
};
use crate::domain::gts_helpers::{PROTOCOL_GRPC, PROTOCOL_HTTP};

fn endpoint() -> Endpoint {
    Endpoint { scheme: EndpointScheme::Https, host: "api.vendor.com".to_owned(), port: 443 }
}

fn seed_upstream() -> Upstream {
    serde_json::from_str(
        r#"{
            "alias": "api.vendor.com",
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": { "endpoints": [ { "scheme": "https", "host": "api.vendor.com" } ] }
        }"#,
    )
    .expect("seed upstream parses")
}

#[test]
fn bound_invalid_value_is_truncated_and_control_free() {
    let long = "x".repeat(300);
    let bounded = bound_invalid_value(&long);
    assert_eq!(bounded.len(), MAX_INVALID_VALUE_LEN, "echo is <= 128 characters");
    assert_eq!(bound_invalid_value("a\r\nb").len(), 2, "CR/LF dropped");
    assert!(!bound_invalid_value("bad\u{0}host").contains('\u{0}'));
    assert!(bound_invalid_value("api.vendor.com").starts_with("api.vendor.com"));
}

#[test]
fn alias_pattern_is_validated_without_a_regex_engine() {
    for ok in ["a", "api", "api.vendor.com", "api-vendor", "8443", "api:8443", "api-v1.x"] {
        assert!(alias_is_valid(ok), "`{ok}` matches the alias pattern");
    }
    for bad in ["", "-api", "api-", "API", "api vendor", "api/", ".api", "api:"] {
        assert!(!alias_is_valid(bad), "`{bad}` does not match the alias pattern");
    }
    // The schema pattern itself, spelled out for the record.
    assert_eq!(ALIAS_PATTERN, "^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$");
}

#[test]
fn tag_pattern_is_validated() {
    assert!(tag_is_valid("openai"));
    assert!(tag_is_valid("llm_v2"));
    assert!(!tag_is_valid("OpenAI"));
    assert!(!tag_is_valid(""));
    assert!(!tag_is_valid("open ai"));
}

#[test]
fn host_validation_accepts_rfc1123_hostnames_and_ips_and_strips_a_trailing_dot() {
    assert_eq!(validate_host("host", "api.vendor.com").expect("ok"), "api.vendor.com");
    assert_eq!(validate_host("host", "API.VENDOR.COM.").expect("ok"), "api.vendor.com");
    assert_eq!(validate_host("host", "10.0.0.1").expect("ok"), "10.0.0.1");
    assert_eq!(validate_host("host", "::1").expect("ok"), "::1");
    assert!(host_is_ip("10.0.0.1"));
    assert!(!host_is_ip("api.vendor.com"));
    for bad in ["", "-api.vendor.com", "api-.vendor.com", "api..vendor.com"] {
        assert!(validate_host("host", bad).is_err(), "`{bad}` must be rejected");
    }
    let long_label = "a".repeat(64);
    assert!(validate_host("host", &long_label).is_err(), "label exceeds 63 characters");
    let long_host = format!("{}.com", "a".repeat(250));
    assert!(validate_host("host", &long_host).is_err(), "host exceeds 253 characters");
}

#[test]
fn endpoint_validation_applies_the_scheme_set_and_the_http_gate() {
    assert!(validate_endpoint(&endpoint(), false).is_ok());
    // Graded deviation 2: `http` is a legal scheme while the flag is set.
    let http = Endpoint { scheme: EndpointScheme::Http, host: "api.vendor.com".to_owned(), port: 80 };
    assert!(validate_endpoint(&http, true).is_ok());
    let err = validate_endpoint(&http, false).expect_err("`http` rejected while the flag is off");
    assert!(err.to_string().contains("allow_http_upstream"), "`{err}` names the gate");
    assert!(validate_endpoint(&Endpoint { port: 0, ..endpoint() }, false).is_err());
    assert!(validate_endpoint(&Endpoint { host: String::new(), ..endpoint() }, false).is_err());
}

#[test]
fn server_validation_requires_a_uniform_pool() {
    let uniform = ServerConfig {
        endpoints: vec![endpoint(), Endpoint { host: "eu.vendor.com".to_owned(), ..endpoint() }],
    };
    assert!(validate_server(&uniform, false).is_ok());
    let mixed_scheme = ServerConfig {
        endpoints: vec![endpoint(), Endpoint { scheme: EndpointScheme::Wss, ..endpoint() }],
    };
    let err = validate_server(&mixed_scheme, false).expect_err("uniform in scheme");
    assert!(err.to_string().contains("scheme"));
    let mixed_port = ServerConfig {
        endpoints: vec![endpoint(), Endpoint { port: 8443, ..endpoint() }],
    };
    let err = validate_server(&mixed_port, false).expect_err("uniform in port");
    assert!(err.to_string().contains("port"));
    let err =
        validate_server(&ServerConfig { endpoints: vec![] }, false).expect_err("at least one endpoint");
    assert!(err.to_string().contains("at least one endpoint"));
}

#[test]
fn protocol_is_limited_to_the_two_gts_identifiers() {
    assert!(validate_protocol(PROTOCOL_HTTP).is_ok());
    // gRPC is accepted as configuration surface only (graded deviation 7).
    assert!(validate_protocol(PROTOCOL_GRPC).is_ok());
    assert!(validate_protocol("http").is_err());
    assert!(validate_protocol("gts.cf.core.oagw.protocol.v1~cf.core.oagw.ws.v1").is_err());
}

#[test]
fn match_validation_requires_exactly_one_block() {
    let http = MatchConfig {
        http: Some(HttpMatch {
            methods: vec![HttpMethod::Get, HttpMethod::Post],
            path: "/v1/chat".to_owned(),
            query_allowlist: vec![],
            path_suffix_mode: PathSuffixMode::Append,
        }),
        grpc: None,
    };
    assert_eq!(validate_match(&http).expect("ok"), RouteMatchKind::Http);
    let grpc = MatchConfig {
        http: None,
        grpc: Some(GrpcMatch { service: "foo.v1.UserService".to_owned(), method: "GetUser".to_owned() }),
    };
    assert_eq!(validate_match(&grpc).expect("ok"), RouteMatchKind::Grpc);
    assert!(validate_match(&MatchConfig::default()).is_err(), "neither block is a rejection");
    assert!(
        validate_match(&MatchConfig { http: http.http, grpc: grpc.grpc }).is_err(),
        "both blocks is a rejection"
    );
    let empty_methods = MatchConfig {
        http: Some(HttpMatch {
            methods: vec![],
            path: "/v1".to_owned(),
            query_allowlist: vec![],
            path_suffix_mode: PathSuffixMode::Append,
        }),
        grpc: None,
    };
    assert!(validate_match(&empty_methods).is_err(), "methods requires minItems 1");
    let crlf_allowlist = MatchConfig {
        http: Some(HttpMatch {
            methods: vec![HttpMethod::Get],
            path: "/v1".to_owned(),
            query_allowlist: vec!["api\rkey".to_owned()],
            path_suffix_mode: PathSuffixMode::Append,
        }),
        grpc: None,
    };
    assert!(validate_match(&crlf_allowlist).is_err(), "a query name must not carry CR/LF");
}

#[test]
fn upstream_validation_rejects_an_illegal_alias_and_protocol() {
    let upstream = seed_upstream();
    assert!(validate_upstream(&upstream, false).is_ok());

    let bad_alias = Upstream { alias: "API".to_owned(), ..upstream.clone() };
    assert!(validate_upstream(&bad_alias, false).is_err(), "alias pattern enforced");

    let bad_protocol = Upstream { protocol: "http".to_owned(), ..upstream };
    assert!(validate_upstream(&bad_protocol, false).is_err(), "protocol restricted to the two GTS ids");
}

#[test]
fn upstream_validation_propagates_the_cors_wildcard_rule() {
    let upstream = Upstream {
        cors: Some(CorsConfig {
            enabled: true,
            allowed_origins: Some(vec!["*".to_owned()]),
            allow_credentials: true,
            ..CorsConfig::default()
        }),
        ..seed_upstream()
    };
    let err = validate_upstream(&upstream, false).expect_err("wildcard + credentials rejected");
    assert!(err.to_string().contains("allow_credentials with wildcard origin"));
}

#[test]
fn upstream_validation_rejects_an_illegal_tag() {
    let upstream = Upstream { tags: vec!["OpenAI".to_owned()], ..seed_upstream() };
    assert!(validate_upstream(&upstream, false).is_err());
}

#[test]
fn upstream_validation_normalizes_endpoint_hosts_to_lowercase() {
    let upstream = Upstream {
        server: ServerConfig {
            endpoints: vec![Endpoint { host: "API.VENDOR.COM.".to_owned(), ..endpoint() }],
        },
        ..seed_upstream()
    };
    let validated = validate_upstream(&upstream, false).expect("ok");
    assert_eq!(validated.server.endpoints[0].host, "api.vendor.com");
}

#[test]
fn cors_validation_accepts_the_documented_method_set_and_an_origin_uri() {
    let cors = CorsConfig {
        enabled: true,
        allowed_origins: Some(vec!["https://app.vendor.com".to_owned(), "*".to_owned()]),
        allowed_methods: ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"]
            .iter()
            .map(|method| (*method).to_owned())
            .collect(),
        ..CorsConfig::default()
    };
    assert!(validate_cors(&cors).is_ok(), "the documented method set is accepted");
}

#[test]
fn cors_validation_rejects_a_method_outside_the_documented_set() {
    let cors = CorsConfig {
        enabled: true,
        allowed_methods: vec!["TRACE".to_owned()],
        ..CorsConfig::default()
    };
    let err = validate_cors(&cors).expect_err("TRACE is not in the schema method set");
    assert!(err.to_string().contains("cors.allowed_methods"), "{err}");
}

#[test]
fn cors_validation_rejects_an_origin_that_is_not_a_uri_nor_the_wildcard() {
    let cors = CorsConfig {
        enabled: true,
        allowed_origins: Some(vec!["app.vendor.com".to_owned()]),
        ..CorsConfig::default()
    };
    let err = validate_cors(&cors).expect_err("a bare host is not a well-formed origin");
    assert!(err.to_string().contains("cors.allowed_origins"), "{err}");
}

#[test]
fn cors_validation_rejects_a_path_or_query_carrying_origin() {
    assert!(validate_cors(&CorsConfig {
        enabled: true,
        allowed_origins: Some(vec!["https://app.vendor.com/path".to_owned()]),
        ..CorsConfig::default()
    })
    .is_err());
    assert!(validate_cors(&CorsConfig {
        enabled: true,
        allowed_origins: Some(vec!["https://app.vendor.com/?x=1".to_owned()]),
        ..CorsConfig::default()
    })
    .is_err());
}

#[test]
fn cors_validation_rejects_credentials_with_a_wildcard_origin() {
    let cors = CorsConfig {
        enabled: true,
        allow_credentials: true,
        allowed_origins: Some(vec!["*".to_owned()]),
        ..CorsConfig::default()
    };
    let err = validate_cors(&cors).expect_err("credentials with a wildcard origin is rejected");
    assert!(
        err.to_string().contains("allow_credentials") && err.to_string().contains("wildcard"),
        "{err}"
    );
}

#[test]
fn cors_validation_rejects_a_pattern_form_of_origin() {
    for origin in ["https://*.app.vendor.com", "https://app.vendor.com:443?x=1", "https://a*b.vendor.com"] {
        let cors = CorsConfig {
            enabled: true,
            allowed_origins: Some(vec![origin.to_owned()]),
            ..CorsConfig::default()
        };
        let err = validate_cors(&cors).expect_err("no pattern form of origin is accepted");
        assert!(err.to_string().contains("cors.allowed_origins"), "{origin}: {err}");
    }
}

#[test]
fn a_validated_upstream_stores_its_origins_in_the_canonical_form() {
    let upstream = crate::test_support::upstream(uuid::Uuid::new_v4(), "api.vendor.com");
    let mut upstream = upstream;
    upstream.cors = Some(CorsConfig {
        enabled: true,
        allowed_origins: Some(vec![
            "https://APP.Vendor.com:443".to_owned(),
            "https://other.vendor.com:8443".to_owned(),
        ]),
        ..CorsConfig::default()
    });
    let validated = validate_upstream(&upstream, false).expect("ok");
    assert_eq!(
        validated.cors.expect("cors").allowed_origins,
        Some(vec![
            "https://app.vendor.com".to_owned(),
            "https://other.vendor.com:8443".to_owned(),
        ])
    );
}

#[test]
fn rate_limit_validation_rejects_a_zero_rate_capacity_and_cost() {
    let zero_rate = serde_json::from_str::<crate::domain::dto::RateLimitConfig>(
        r#"{ "sustained": { "rate": 0 } }"#,
    )
    .expect("block");
    let err = validate_rate_limit(&zero_rate).expect_err("rate 0 is below the minimum");
    assert!(err.to_string().contains("rate_limit.sustained.rate"), "{err}");

    let zero_capacity = serde_json::from_str::<crate::domain::dto::RateLimitConfig>(
        r#"{ "sustained": { "rate": 5 }, "burst": { "capacity": 0 } }"#,
    )
    .expect("block");
    let err = validate_rate_limit(&zero_capacity).expect_err("capacity 0 is below the minimum");
    assert!(err.to_string().contains("rate_limit.burst.capacity"), "{err}");

    let zero_cost = serde_json::from_str::<crate::domain::dto::RateLimitConfig>(
        r#"{ "sustained": { "rate": 5 }, "cost": 0 }"#,
    )
    .expect("block");
    let err = validate_rate_limit(&zero_cost).expect_err("cost 0 is below the minimum");
    assert!(err.to_string().contains("rate_limit.cost"), "{err}");
}

#[test]
fn a_rate_limit_block_without_sustained_rate_is_rejected_at_the_boundary() {
    assert!(
        serde_json::from_str::<crate::domain::dto::RateLimitConfig>(r#"{ "cost": 1 }"#).is_err(),
        "`sustained` is required, so a rate cannot be defaulted in"
    );
}

fn parsed_rate_limit(json: &str) -> crate::domain::dto::RateLimitConfig {
    serde_json::from_str::<crate::domain::dto::RateLimitConfig>(json).expect("block parses")
}

#[test]
fn a_budget_total_below_one_is_rejected() {
    let block = parsed_rate_limit(
        r#"{ "sustained": { "rate": 5 }, "budget": { "mode": "allocated", "total": 0 } }"#,
    );
    let err = validate_rate_limit(&block).expect_err("a budget total of 0 is below the minimum");
    assert!(err.to_string().contains("rate_limit.budget.total"), "{err}");
}

#[test]
fn an_omitted_budget_total_is_rejected_when_the_budget_allocates_or_shares() {
    for mode in ["allocated", "shared"] {
        let json = format!(r#"{{ "sustained": {{ "rate": 5 }}, "budget": {{ "mode": "{mode}" }} }}"#);
        let block = parsed_rate_limit(&json);
        let err = validate_rate_limit(&block)
            .expect_err("an allocated or shared budget needs a total");
        assert!(err.to_string().contains("rate_limit.budget.total"), "{mode}: {err}");
    }
}

#[test]
fn an_overcommit_ratio_outside_its_range_is_rejected() {
    for ratio in [0.5, 0.99, 2.0001, 3.0] {
        let json = format!(
            r#"{{ "sustained": {{ "rate": 5 }}, "budget": {{ "mode": "allocated", "total": 10, "overcommit_ratio": {ratio} }} }}"#
        );
        let block = parsed_rate_limit(&json);
        let err = validate_rate_limit(&block)
            .expect_err("an overcommit ratio outside 1.0 to 2.0 is rejected");
        assert!(
            err.to_string().contains("rate_limit.budget.overcommit_ratio"),
            "{ratio}: {err}"
        );
    }
}

#[test]
fn the_budget_surface_is_accepted_at_both_range_ends_and_with_its_default() {
    for ratio in ["1.0", "2.0", "1.5"] {
        let json = format!(
            r#"{{ "sustained": {{ "rate": 5 }}, "budget": {{ "mode": "allocated", "total": 10, "overcommit_ratio": {ratio} }} }}"#
        );
        assert!(validate_rate_limit(&parsed_rate_limit(&json)).is_ok(), "{ratio}");
    }
    // The ratio defaults to 1.0 and the mode defaults to `unlimited`, which
    // needs no total.
    assert!(validate_rate_limit(&parsed_rate_limit(
        r#"{ "sustained": { "rate": 5 }, "budget": { "mode": "unlimited" } }"#
    ))
    .is_ok());
    assert!(validate_rate_limit(&parsed_rate_limit(
        r#"{ "sustained": { "rate": 5 }, "budget": { "mode": "allocated", "total": 1 } }"#
    ))
    .is_ok());
}

#[test]
fn an_unknown_key_in_the_rate_limit_block_is_rejected() {
    assert!(serde_json::from_str::<crate::domain::dto::RateLimitConfig>(
        r#"{ "sustained": { "rate": 5 }, "burstCapacity": 5 }"#
    )
    .is_err());
}

#[test]
fn the_documented_field_set_delta_is_accepted() {
    let block = parsed_rate_limit(
        r#"{
            "sustained": { "rate": 5, "window": "minute" },
            "burst": { "capacity": 9 },
            "sharing": "enforce",
            "algorithm": "sliding_window",
            "scope": "route",
            "strategy": "degrade",
            "cost": 3,
            "response_headers": false,
            "budget": { "mode": "allocated", "total": 100, "overcommit_ratio": 1.25 }
        }"#,
    );
    assert!(validate_rate_limit(&block).is_ok());
    assert_eq!(block.sustained.window, RateWindow::Minute);
    assert_eq!(block.algorithm, crate::domain::dto::RateAlgorithm::SlidingWindow);
    assert_eq!(block.scope, RateScope::Route);
    assert_eq!(block.strategy, RateStrategy::Degrade);
    assert_eq!(block.cost, 3);
    assert!(!block.response_headers);
}

#[test]
fn an_omitted_rate_limit_field_receives_its_declared_default() {
    let block = parsed_rate_limit(r#"{ "sustained": { "rate": 5 } }"#);
    assert_eq!(block.sharing, SharingMode::Private);
    assert_eq!(block.algorithm, crate::domain::dto::RateAlgorithm::TokenBucket);
    assert_eq!(block.sustained.window, RateWindow::Second);
    assert_eq!(block.scope, RateScope::Tenant);
    assert_eq!(block.strategy, crate::domain::dto::RateStrategy::Reject);
    assert_eq!(block.cost, 1);
    assert!(block.response_headers);
    assert_eq!(block.effective_burst_capacity(), 5, "burst defaults to the rate");
    assert!(block.budget.is_none());
}

#[test]
fn upstream_validation_runs_the_nested_rate_limit_and_cors_checks() {
    let upstream = Upstream {
        rate_limit: Some(
            serde_json::from_str::<crate::domain::dto::RateLimitConfig>(
                r#"{ "sustained": { "rate": 0 } }"#,
            )
            .expect("block"),
        ),
        ..seed_upstream()
    };
    let err = validate_upstream(&upstream, false).expect_err("rate 0 rejected");
    assert!(err.to_string().contains("rate_limit.sustained.rate"), "{err}");
}
