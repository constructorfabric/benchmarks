//! Unit tests for configuration merging.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::domain::model::{
    Burst, Cors, Endpoint, EndpointScheme, HeadersConfig, HttpMatch, HttpMethod, MatchConfig,
    PassthroughMode, PathSuffixMode, PluginRef, PluginsConfig, Protocol, RateAlgorithm, RateScope,
    RateStrategy, RateWindow, RequestHeaderRules, ResponseHeaderRules, Route, ServerConfig,
    SharingMode, SustainedRate,
};
use std::time::Duration;

fn rate(per_second: u32, capacity: u32) -> RateLimit {
    RateLimit {
        sharing: SharingMode::Private,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate: per_second,
            window: RateWindow::Second,
        },
        burst: Burst {
            capacity: Some(capacity),
        },
        scope: RateScope::Tenant,
        strategy: RateStrategy::Reject,
        cost: 1,
    }
}

fn route(rate_limit: Option<RateLimit>) -> Route {
    Route {
        id: "r".to_owned(),
        tenant_id: uuid::Uuid::nil(),
        upstream_id: "u".to_owned(),
        enabled: true,
        priority: 0,
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "/v1/chat".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::default(),
            }),
            grpc: None,
        },
        rate_limit,
        cors: None,
        plugins: PluginsConfig::default(),
        tags: Vec::new(),
        created_at: None,
        updated_at: None,
    }
}

#[test]
fn rate_limit_min_takes_the_stricter_sustained_rate() {
    let merged = merge_rate_limit(Some(&rate(10, 20)), Some(&rate(5, 30))).unwrap();
    assert_eq!(merged.sustained.rate, 5);
    assert_eq!(merged.capacity(), 20);
}

#[test]
fn rate_limit_min_compares_across_windows() {
    let per_minute = RateLimit {
        sustained: SustainedRate {
            rate: 600,
            window: RateWindow::Minute,
        },
        burst: Burst {
            capacity: Some(600),
        },
        ..rate(600, 600)
    };
    let per_second = rate(20, 20);
    // 600/minute == 10/second is stricter than 20/second.
    let merged = merge_rate_limit(Some(&per_minute), Some(&per_second)).unwrap();
    assert_eq!(merged.sustained.rate, 600);
    assert_eq!(merged.sustained.window, RateWindow::Minute);
}

#[test]
fn rate_limit_capacity_defaults_to_the_sustained_rate() {
    let mut limit = rate(7, 7);
    limit.burst = Burst { capacity: None };
    let merged = merge_rate_limit(Some(&limit), None).unwrap();
    assert_eq!(merged.capacity(), 7);
}

#[test]
fn rate_limit_single_layer_is_kept() {
    assert!(merge_rate_limit(None, None).is_none());
    let merged = merge_rate_limit(None, Some(&rate(3, 3))).unwrap();
    assert_eq!(merged.sustained.rate, 3);
}

#[test]
fn plugins_are_concatenated_upstream_then_route() {
    let upstream = PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![
            PluginRef::GtsId(crate::gts_helpers::GUARD_REQUIRED_HEADERS.to_owned()),
            PluginRef::GtsId(crate::gts_helpers::TRANSFORM_REQUEST_ID.to_owned()),
        ],
    };
    let route = PluginsConfig {
        sharing: SharingMode::Inherit,
        items: vec![PluginRef::GtsId(
            crate::gts_helpers::TRANSFORM_REQUEST_ID.to_owned(),
        )],
    };
    let merged = merge_plugins(&upstream, Some(&route));
    assert_eq!(
        merged,
        vec![
            crate::domain::merge::PluginBinding::bare(crate::gts_helpers::GUARD_REQUIRED_HEADERS),
            crate::domain::merge::PluginBinding::bare(crate::gts_helpers::TRANSFORM_REQUEST_ID),
            crate::domain::merge::PluginBinding::bare(crate::gts_helpers::TRANSFORM_REQUEST_ID),
        ]
    );
    assert!(
        merged.iter().all(|binding| binding.config.is_null()),
        "a bare reference carries no configuration"
    );
}

#[test]
fn cors_origins_are_unioned() {
    let upstream = Cors {
        sharing: SharingMode::Inherit,
        enabled: true,
        allowed_origins: vec!["https://a.example".to_owned()],
        allowed_methods: vec!["GET".to_owned()],
        expose_headers: Vec::new(),
        allow_credentials: false,
    };
    let route = Cors {
        sharing: SharingMode::Inherit,
        enabled: true,
        allowed_origins: vec![
            "https://a.example".to_owned(),
            "https://b.example".to_owned(),
        ],
        allowed_methods: vec!["POST".to_owned()],
        expose_headers: Vec::new(),
        allow_credentials: false,
    };
    let merged = merge_cors(Some(&upstream), Some(&route)).unwrap();
    assert_eq!(merged.allowed_origins.len(), 2);
    assert!(merged.allowed_methods.contains(&"POST".to_owned()));
}

#[test]
fn cors_enabling_is_unioned() {
    let disabled = Cors {
        enabled: false,
        allowed_origins: vec![],
        ..merge_cors(None, None).unwrap_or_else(Cors::unset)
    };
    let enabled = Cors {
        enabled: true,
        allowed_origins: vec!["*".to_owned()],
        ..disabled.clone()
    };
    let merged = merge_cors(Some(&disabled), Some(&enabled)).unwrap();
    assert!(merged.enabled);
}

#[test]
fn tags_are_unioned_additively() {
    let upstream = vec!["llm".to_owned(), "openai".to_owned()];
    let route = vec!["openai".to_owned(), "chat".to_owned()];
    assert_eq!(merge_tags(&upstream, &route).len(), 3);
}

#[test]
fn sharing_modes_are_classified() {
    assert!(may_override(SharingMode::Inherit));
    assert!(!may_override(SharingMode::Private));
    assert!(!may_override(SharingMode::Enforce));
    assert!(is_enforced(SharingMode::Enforce));
    assert!(!is_enforced(SharingMode::Inherit));
}

#[test]
fn headers_merge_route_rules_on_top_of_upstream_rules() {
    let mut upstream = HeadersConfig::default();
    upstream
        .request
        .set
        .insert("x-a".to_owned(), "1".to_owned());
    upstream.request.remove.push("x-drop".to_owned());
    let mut route = HeadersConfig::default();
    route.request.set.insert("x-a".to_owned(), "2".to_owned());
    route.request.add.insert("x-b".to_owned(), "3".to_owned());
    route.response.remove.push("server".to_owned());

    let merged = merge_headers(&upstream, Some(&route));
    assert_eq!(merged.request.set.get("x-a").map(String::as_str), Some("2"));
    assert_eq!(merged.request.add.get("x-b").map(String::as_str), Some("3"));
    assert!(merged.request.remove.contains(&"x-drop".to_owned()));
    assert!(merged.response.remove.contains(&"server".to_owned()));
}

#[test]
fn headers_passthrough_is_taken_from_the_route_when_set() {
    let upstream = HeadersConfig::default();
    let mut route = HeadersConfig::default();
    route.request.passthrough = PassthroughMode::Allowlist;
    route
        .request
        .passthrough_allowlist
        .push("x-forwarded-for".to_owned());
    let merged = merge_headers(&upstream, Some(&route));
    assert_eq!(merged.request.passthrough, PassthroughMode::Allowlist);
    assert_eq!(merged.request.passthrough_allowlist.len(), 1);
}

#[test]
fn effective_config_collects_every_aspect() {
    let mut upstream = crate::domain::model::Upstream {
        id: "u".to_owned(),
        tenant_id: uuid::Uuid::nil(),
        alias: "api.example.com".to_owned(),
        enabled: true,
        protocol: Protocol::Http,
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Http,
                host: "api.example.com".to_owned(),
                port: 80,
            }],
        },
        auth: None,
        headers: HeadersConfig::default(),
        rate_limit: Some(rate(10, 10)),
        cors: Some(Cors {
            sharing: SharingMode::Inherit,
            enabled: true,
            allowed_origins: vec!["https://app.example".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: vec!["x-trace".to_owned()],
            allow_credentials: false,
        }),
        plugins: PluginsConfig::default(),
        tags: vec!["llm".to_owned()],
        created_at: None,
        updated_at: None,
    };
    upstream.plugins.items.push(PluginRef::GtsId(
        crate::gts_helpers::TRANSFORM_REQUEST_ID.to_owned(),
    ));
    let effective = effective(&upstream, Some(&route(Some(rate(4, 4)))));
    assert_eq!(effective.sustained_rate, Some(4));
    assert_eq!(effective.burst_capacity, Some(4));
    assert_eq!(effective.plugins.len(), 1);
    assert!(effective.cors_enabled);
    assert_eq!(
        effective.cors_origins,
        vec!["https://app.example".to_owned()]
    );
    assert_eq!(effective.tags, vec!["llm".to_owned()]);
    assert_eq!(effective.rate_cost, 1);
    assert_eq!(effective.sustained_window, Some(RateWindow::Second));
}

#[test]
fn header_map_helper_builds_a_sorted_map() {
    let map = header_map(&[("b", "2"), ("a", "1")]);
    assert_eq!(map.keys().next().map(String::as_str), Some("a"));
}

#[test]
fn rate_limit_validation_rejects_zero() {
    let mut limit = rate(0, 1);
    assert!(validate_rate_limit(&limit).is_err());
    limit.sustained.rate = 1;
    limit.cost = 0;
    assert!(validate_rate_limit(&limit).is_err());
    limit.cost = 1;
    assert!(validate_rate_limit(&limit).is_ok());
}

#[test]
fn cors_validation_rejects_credentials_with_wildcard() {
    let cors = Cors {
        enabled: true,
        allowed_origins: vec!["*".to_owned()],
        allow_credentials: true,
        ..Cors::unset()
    };
    assert!(validate_cors(&cors).is_err());

    let absolute = Cors {
        allowed_origins: vec!["https://app.example".to_owned()],
        allow_credentials: true,
        ..cors
    };
    assert!(validate_cors(&absolute).is_ok());

    let relative = Cors {
        allowed_origins: vec!["app.example".to_owned()],
        ..absolute
    };
    assert!(validate_cors(&relative).is_err());
}

#[test]
fn rate_limit_strategy_and_algorithm_are_accepted() {
    let queued = RateLimit {
        algorithm: RateAlgorithm::SlidingWindow,
        strategy: RateStrategy::Queue,
        ..rate(1, 1)
    };
    assert!(validate_rate_limit(&queued).is_ok());
    assert_eq!(queued.strategy, RateStrategy::Queue);
}

#[test]
fn request_and_response_rules_default_empty() {
    assert!(RequestHeaderRules::default().is_empty());
    assert!(ResponseHeaderRules::default().is_empty());
    assert!(HeadersConfig::default().is_empty());
    let mut rules = RequestHeaderRules::default();
    rules.remove.push("x".to_owned());
    assert!(!rules.is_empty());
}

#[test]
fn window_durations_match_the_contract() {
    assert_eq!(RateWindow::Second.duration(), Duration::from_secs(1));
    assert_eq!(RateWindow::Minute.duration(), Duration::from_mins(1));
    assert_eq!(RateWindow::Hour.duration(), Duration::from_hours(1));
    assert_eq!(RateWindow::Day.duration(), Duration::from_hours(24));
}
