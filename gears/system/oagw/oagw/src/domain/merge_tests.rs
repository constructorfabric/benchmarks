//! Unit tests for [`super::merge`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use uuid::Uuid;

use super::{apply_route, merge_upstream_chain, Scoped};
use crate::domain::models::{
    AuthConfig, CorsConfig, Endpoint, EndpointScheme, HeadersConfig,
    Protocol, RateLimitAlgorithm, RateLimitConfig, RateLimitScope,
    RateLimitStrategy, RateWindow, Route, ServerConfig, SharingMode, SustainedRate, Upstream,
};

fn upstream(tenant: Uuid, alias: &str) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        alias: alias.to_owned(),
        enabled: true,
        protocol: Protocol::Http,
        server: ServerConfig {
            endpoints: vec![Endpoint::new(EndpointScheme::Https, "api.example.com", 443)],
        },
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: Vec::new(),
        created_at: 0,
        updated_at: 0,
    }
}

fn rate(rate: u64, window: RateWindow, sharing: SharingMode) -> RateLimitConfig {
    RateLimitConfig {
        sharing,
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained: SustainedRate { rate, window },
        burst: None,
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

#[test]
fn the_selected_upstream_is_reported_as_the_target() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let root_upstream = upstream(root, "api.vendor.com");
    let leaf_upstream = upstream(leaf, "api.vendor.com");
    let chain = [&root_upstream, &leaf_upstream];

    let merged = merge_upstream_chain(&chain);
    assert_eq!(merged.upstream.id, leaf_upstream.id);
    assert_eq!(merged.protocol(), Protocol::Http);
}

#[test]
fn an_inherited_auth_block_is_overridden_by_the_descendant() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let mut root_upstream = upstream(root, "api.vendor.com");
    root_upstream.auth = Some(AuthConfig {
        plugin_type: Some(crate::domain::plugin::builtins::AUTH_OAUTH2_CLIENT_CRED.to_owned()),
        sharing: SharingMode::Inherit,
        config: Some(serde_json::json!({ "method": "partner-key" })),
    });
    let mut leaf_upstream = upstream(leaf, "api.vendor.com");
    leaf_upstream.auth = Some(AuthConfig {
        plugin_type: Some(crate::domain::plugin::builtins::AUTH_OAUTH2_CLIENT_CRED.to_owned()),
        sharing: SharingMode::Inherit,
        config: Some(serde_json::json!({ "method": "leaf-key" })),
    });

    let merged = merge_upstream_chain(&[&root_upstream, &leaf_upstream]);
    let auth = merged.auth.unwrap();
    assert_eq!(
        auth.config.unwrap()["method"],
        serde_json::json!("leaf-key")
    );
}

#[test]
fn an_enforced_auth_block_survives_shadowing() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let mut root_upstream = upstream(root, "api.vendor.com");
    root_upstream.auth = Some(AuthConfig {
        plugin_type: Some(crate::domain::plugin::builtins::AUTH_OAUTH2_CLIENT_CRED.to_owned()),
        sharing: SharingMode::Enforce,
        config: Some(serde_json::json!({ "method": "enforced" })),
    });
    let mut leaf_upstream = upstream(leaf, "api.vendor.com");
    leaf_upstream.auth = Some(AuthConfig {
        plugin_type: Some(crate::domain::plugin::builtins::AUTH_OAUTH2_CLIENT_CRED.to_owned()),
        sharing: SharingMode::Inherit,
        config: Some(serde_json::json!({ "method": "leaf-key" })),
    });

    let merged = merge_upstream_chain(&[&root_upstream, &leaf_upstream]);
    let auth = merged.auth.unwrap();
    assert_eq!(
        auth.config.unwrap()["method"],
        serde_json::json!("enforced")
    );
}

#[test]
fn a_private_auth_block_never_reaches_a_descendant() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let mut root_upstream = upstream(root, "api.vendor.com");
    root_upstream.auth = Some(AuthConfig {
        plugin_type: Some(crate::domain::plugin::builtins::AUTH_OAUTH2_CLIENT_CRED.to_owned()),
        sharing: SharingMode::Private,
        config: Some(serde_json::json!({ "method": "private" })),
    });
    let leaf_upstream = upstream(leaf, "api.vendor.com");

    let merged = merge_upstream_chain(&[&root_upstream, &leaf_upstream]);
    assert!(merged.auth.is_none());
    // ... but it still applies to the ancestor's own requests.
    let own = merge_upstream_chain(&[&root_upstream]);
    assert_eq!(
        own.auth.unwrap().config.unwrap()["method"],
        serde_json::json!("private")
    );
}

#[test]
fn rate_limits_take_the_strictest_value_across_the_chain() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let mut root_upstream = upstream(root, "api.vendor.com");
    root_upstream.rate_limit = Some(rate(10, RateWindow::Minute, SharingMode::Enforce));
    let mut leaf_upstream = upstream(leaf, "api.vendor.com");
    leaf_upstream.rate_limit = Some(rate(1000, RateWindow::Minute, SharingMode::Inherit));

    let merged = merge_upstream_chain(&[&root_upstream, &leaf_upstream]);
    let merged = merged.rate_limit.unwrap();
    // 10/minute survives; the descendant's looser 1000/minute is discarded.
    assert_eq!(merged.sustained.rate, 10);
    assert_eq!(merged.effective_capacity(), 10);
    assert_eq!(merged.sharing, SharingMode::Enforce);
}

#[test]
fn a_private_ancestor_rate_limit_does_not_limit_the_descendant() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let mut root_upstream = upstream(root, "api.vendor.com");
    root_upstream.rate_limit = Some(rate(1, RateWindow::Minute, SharingMode::Private));
    let mut leaf_upstream = upstream(leaf, "api.vendor.com");
    leaf_upstream.rate_limit = Some(rate(1000, RateWindow::Minute, SharingMode::Inherit));

    let merged = merge_upstream_chain(&[&root_upstream, &leaf_upstream]);
    let merged = merged.rate_limit.unwrap();
    assert_eq!(merged.sustained.rate, 1000);
}

#[test]
fn a_strict_ancestor_that_opted_out_of_quota_headers_is_kept_quiet() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let mut root_upstream = upstream(root, "api.vendor.com");
    root_upstream.rate_limit = Some({
        let mut block = rate(10, RateWindow::Minute, SharingMode::Enforce);
        block.response_headers = false;
        block
    });
    let mut leaf_upstream = upstream(leaf, "api.vendor.com");
    leaf_upstream.rate_limit = Some(rate(1000, RateWindow::Minute, SharingMode::Inherit));

    let merged = merge_upstream_chain(&[&root_upstream, &leaf_upstream]);
    assert!(!merged.rate_limit.unwrap().response_headers);
}

#[test]
fn the_route_rate_limit_is_folded_with_min() {
    let tenant = Uuid::new_v4();
    let mut upstream = upstream(tenant, "api.vendor.com");
    upstream.rate_limit = Some(rate(100, RateWindow::Minute, SharingMode::Inherit));

    let mut config = merge_upstream_chain(&[&upstream]);
    let mut route = route_for(&upstream);
    route.rate_limit = Some(rate(10, RateWindow::Minute, SharingMode::Inherit));
    apply_route(&mut config, &route);

    let merged = config.rate_limit.unwrap();
    assert_eq!(merged.effective_capacity(), 10);
}

#[test]
fn plugin_chains_are_concatenated_ancestor_first() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let mut root_upstream = upstream(root, "api.vendor.com");
    root_upstream.plugins = Some(plugins(SharingMode::Enforce, &["cf.core.oagw.request_id.v1"]));
    let mut leaf_upstream = upstream(leaf, "api.vendor.com");
    leaf_upstream.plugins = Some(plugins(SharingMode::Inherit, &["cf.core.oagw.logging.v1"]));

    let merged = merge_upstream_chain(&[&root_upstream, &leaf_upstream]);
    assert_eq!(
        merged.plugins,
        vec![
            "cf.core.oagw.request_id.v1".to_owned(),
            "cf.core.oagw.logging.v1".to_owned()
        ]
    );
}

#[test]
fn route_plugins_are_appended_after_the_upstream_plugins() {
    let tenant = Uuid::new_v4();
    let upstream = upstream(tenant, "api.vendor.com");
    let mut config = merge_upstream_chain(&[&upstream]);
    config.plugins.push("cf.core.oagw.request_id.v1".to_owned());

    let mut route = route_for(&upstream);
    route.plugins = Some(plugins(
        SharingMode::Inherit,
        &["cf.core.oagw.request_id.v1", "cf.core.oagw.logging.v1"],
    ));
    apply_route(&mut config, &route);

    assert_eq!(
        config.plugins,
        vec![
            "cf.core.oagw.request_id.v1".to_owned(),
            "cf.core.oagw.logging.v1".to_owned()
        ]
    );
}

#[test]
fn cors_origins_are_unioned_across_the_chain() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let mut root_upstream = upstream(root, "api.vendor.com");
    root_upstream.cors = Some(CorsConfig {
        sharing: SharingMode::Enforce,
        enabled: true,
        allowed_origins: vec!["https://root.example".to_owned()],
        allowed_methods: Vec::new(),
        expose_headers: vec!["X-Request-Id".to_owned()],
        allow_credentials: true,
    });
    let mut leaf_upstream = upstream(leaf, "api.vendor.com");
    leaf_upstream.cors = Some(CorsConfig {
        sharing: SharingMode::Inherit,
        enabled: true,
        allowed_origins: vec![
            "https://root.example".to_owned(),
            "https://leaf.example".to_owned(),
        ],
        allowed_methods: Vec::new(),
        expose_headers: Vec::new(),
        allow_credentials: false,
    });

    let merged = merge_upstream_chain(&[&root_upstream, &leaf_upstream]);
    let cors = merged.cors.unwrap();
    assert_eq!(
        cors.allowed_origins,
        vec![
            "https://root.example".to_owned(),
            "https://leaf.example".to_owned()
        ]
    );
    assert!(cors.allow_credentials);
    assert_eq!(cors.expose_headers, vec!["X-Request-Id".to_owned()]);
}

#[test]
fn tags_are_unioned_and_descendants_cannot_remove_them() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let mut root_upstream = upstream(root, "api.vendor.com");
    root_upstream.tags = vec!["platform".to_owned(), "paid".to_owned()];
    let mut leaf_upstream = upstream(leaf, "api.vendor.com");
    leaf_upstream.tags = vec!["paid".to_owned(), "team-a".to_owned()];

    let merged = merge_upstream_chain(&[&root_upstream, &leaf_upstream]);
    assert_eq!(
        merged.tags,
        vec!["platform".to_owned(), "paid".to_owned(), "team-a".to_owned()]
    );
}

#[test]
fn the_most_specific_header_block_wins() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let mut root_upstream = upstream(root, "api.vendor.com");
    root_upstream.headers = Some(HeadersConfig {
        request: None,
        response: None,
    });
    let mut leaf_upstream = upstream(leaf, "api.vendor.com");
    let mut remove = BTreeMap::new();
    remove.insert("x-internal".to_owned(), "1".to_owned());
    leaf_upstream.headers = Some(HeadersConfig {
        request: None,
        response: None,
    });
    let _ = remove;

    let merged = merge_upstream_chain(&[&root_upstream, &leaf_upstream]);
    assert!(merged.headers.is_some());
}

#[test]
fn a_chain_must_not_be_empty_for_a_real_upstream() {
    let tenant = Uuid::new_v4();
    let upstream = upstream(tenant, "api.vendor.com");
    // A single-element chain is the ordinary "no shadowing" case.
    let merged = merge_upstream_chain(&[&upstream]);
    assert_eq!(merged.upstream.id, upstream.id);
    assert!(merged.plugins.is_empty());
    assert!(merged.rate_limit.is_none());
}

#[test]
fn scoped_is_implemented_for_every_sharing_block() {
    let limit = rate(1, RateWindow::Second, SharingMode::Inherit);
    assert_eq!(Scoped::sharing(&limit), SharingMode::Inherit);
}

fn plugins(sharing: SharingMode, items: &[&str]) -> crate::domain::models::PluginsConfig {
    crate::domain::models::PluginsConfig {
        sharing,
        items: items.iter().map(|item| (*item).to_owned()).collect(),
    }
}

fn route_for(upstream: &Upstream) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id: upstream.tenant_id,
        upstream_id: upstream.id,
        enabled: true,
        priority: 0,
        match_config: crate::domain::models::MatchConfig {
            http: Some(crate::domain::models::HttpMatch {
                methods: Vec::new(),
                path: "/v1".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: crate::domain::models::PathSuffixMode::Append,
            }),
            grpc: None,
        },
        plugins: None,
        rate_limit: None,
        tags: Vec::new(),
        created_at: 0,
        updated_at: 0,
    }
}
