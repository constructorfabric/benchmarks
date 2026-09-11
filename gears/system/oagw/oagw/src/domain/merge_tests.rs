//! Hierarchical merge: sharing modes and the upstream < route < tenant order.

use super::*;
use crate::domain::model::{
    AuthConfig, BurstCapacity, ConfigMap, CorsConfig, Endpoint, HttpMatch, MatchConfig,
    PathSuffixMode, PluginBinding, PluginsConfig, Protocol, RateLimitAlgorithm, RateLimitScope,
    RateLimitStrategy, RateWindow, Scheme, ServerConfig, SustainedRate,
};
use uuid::Uuid;

fn upstream(alias: &str) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        alias: alias.to_owned(),
        enabled: true,
        protocol: Protocol::Http,
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: Scheme::Https,
                host: "api.openai.com".to_owned(),
                port: 443,
            }],
        },
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: Vec::new(),
    }
}

fn route() -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        upstream_id: Uuid::new_v4(),
        enabled: true,
        priority: 0,
        r#match: MatchConfig {
            http: Some(HttpMatch {
                methods: vec!["GET".to_owned()],
                path: "/".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: Vec::new(),
    }
}

fn auth(secret: &str, sharing: SharingMode) -> AuthConfig {
    let mut config = ConfigMap::new();
    config.insert("secret_ref".to_owned(), secret.into());
    AuthConfig {
        plugin_type: Some(crate::domain::gts::APIKEY_AUTH_PLUGIN_ID.to_owned()),
        sharing,
        config,
    }
}

fn limit(rate: u32, sharing: SharingMode) -> RateLimitConfig {
    RateLimitConfig {
        sharing,
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained: SustainedRate {
            rate,
            window: RateWindow::Minute,
        },
        burst: BurstCapacity { capacity: None },
        budget: None,
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

fn binding(name: &str) -> PluginBinding {
    PluginBinding {
        plugin_ref: name.to_owned(),
        plugin_uuid: None,
        config: ConfigMap::new(),
    }
}

fn plugins(sharing: SharingMode, refs: &[&str]) -> PluginsConfig {
    PluginsConfig {
        sharing,
        items: refs.iter().map(|name| binding(name)).collect(),
    }
}

fn cors(origins: &[&str], sharing: SharingMode) -> CorsConfig {
    CorsConfig {
        sharing,
        enabled: true,
        allowed_origins: origins.iter().map(|o| (*o).to_owned()).collect(),
        allowed_methods: vec!["GET".to_owned()],
        expose_headers: Vec::new(),
        allow_credentials: false,
    }
}

#[test]
fn a_descendants_own_auth_wins_over_an_inherited_one() {
    let mut child = upstream("api.openai.com");
    child.auth = Some(auth("cred://mine", SharingMode::Private));
    let mut parent = upstream("api.openai.com");
    parent.auth = Some(auth("cred://partner", SharingMode::Inherit));

    let effective = effective_config(&child, &[parent], None);
    let secret = effective.auth.unwrap().config.get("secret_ref").cloned();
    assert_eq!(secret, Some("cred://mine".into()));
}

#[test]
fn an_enforced_ancestor_auth_pins_the_credential() {
    let mut child = upstream("api.openai.com");
    child.auth = Some(auth("cred://mine", SharingMode::Private));
    let mut parent = upstream("api.openai.com");
    parent.auth = Some(auth("cred://partner", SharingMode::Enforce));

    let effective = effective_config(&child, &[parent], None);
    let secret = effective.auth.unwrap().config.get("secret_ref").cloned();
    assert_eq!(secret, Some("cred://partner".into()));
}

#[test]
fn a_private_ancestor_auth_is_invisible() {
    let child = upstream("api.openai.com");
    let mut parent = upstream("api.openai.com");
    parent.auth = Some(auth("cred://partner", SharingMode::Private));

    let effective = effective_config(&child, &[parent], None);
    assert!(effective.auth.is_none());
}

#[test]
fn an_inherited_ancestor_auth_fills_a_gap() {
    let child = upstream("api.openai.com");
    let mut parent = upstream("api.openai.com");
    parent.auth = Some(auth("cred://partner", SharingMode::Inherit));

    let effective = effective_config(&child, &[parent], None);
    let secret = effective.auth.unwrap().config.get("secret_ref").cloned();
    assert_eq!(secret, Some("cred://partner".into()));
}

#[test]
fn a_route_rate_limit_overrides_the_upstreams() {
    let mut up = upstream("api.openai.com");
    up.rate_limit = Some(limit(1_000, SharingMode::Private));
    let mut route = route();
    route.rate_limit = Some(limit(10, SharingMode::Private));

    let effective = effective_config(&up, &[], Some(&route));
    assert_eq!(effective.rate_limit.unwrap().sustained.rate, 10);
}

#[test]
fn an_enforced_ancestor_limit_clamps_to_the_stricter_value() {
    let mut child = upstream("api.openai.com");
    child.rate_limit = Some(limit(500, SharingMode::Private));
    let mut parent = upstream("api.openai.com");
    parent.rate_limit = Some(limit(100, SharingMode::Enforce));

    let effective = effective_config(&child, &[parent], None);
    assert_eq!(effective.rate_limit.unwrap().sustained.rate, 100);
}

#[test]
fn an_enforced_ancestor_limit_never_loosens_a_stricter_descendant() {
    let mut child = upstream("api.openai.com");
    child.rate_limit = Some(limit(10, SharingMode::Private));
    let mut parent = upstream("api.openai.com");
    parent.rate_limit = Some(limit(10_000, SharingMode::Enforce));

    let effective = effective_config(&child, &[parent], None);
    assert_eq!(effective.rate_limit.unwrap().sustained.rate, 10);
}

#[test]
fn an_inherited_ancestor_limit_only_applies_when_the_descendant_states_none() {
    let child = upstream("api.openai.com");
    let mut parent = upstream("api.openai.com");
    parent.rate_limit = Some(limit(250, SharingMode::Inherit));
    assert_eq!(
        effective_config(&child, &[parent.clone()], None)
            .rate_limit
            .unwrap()
            .sustained
            .rate,
        250
    );

    let mut child = upstream("api.openai.com");
    child.rate_limit = Some(limit(9_000, SharingMode::Private));
    assert_eq!(
        effective_config(&child, &[parent], None)
            .rate_limit
            .unwrap()
            .sustained
            .rate,
        9_000
    );
}

#[test]
fn a_private_ancestor_limit_does_not_reach_the_descendant() {
    let child = upstream("api.openai.com");
    let mut parent = upstream("api.openai.com");
    parent.rate_limit = Some(limit(5, SharingMode::Private));
    assert!(effective_config(&child, &[parent], None).rate_limit.is_none());
}

#[test]
fn plugin_chains_concatenate_root_first_then_upstream_then_route() {
    let mut child = upstream("api.openai.com");
    child.plugins = Some(plugins(SharingMode::Private, &["u1", "u2"]));
    let mut parent = upstream("api.openai.com");
    parent.plugins = Some(plugins(SharingMode::Inherit, &["p1"]));
    let mut root = upstream("api.openai.com");
    root.plugins = Some(plugins(SharingMode::Enforce, &["r1"]));
    let mut route = route();
    route.plugins = Some(plugins(SharingMode::Private, &["rt1"]));

    let effective = effective_config(&child, &[parent, root], Some(&route));
    let names: Vec<&str> = effective
        .plugins
        .iter()
        .map(|item| item.plugin_ref.as_str())
        .collect();
    assert_eq!(names, ["r1", "p1", "u1", "u2", "rt1"]);
}

#[test]
fn a_private_ancestor_chain_is_not_inherited() {
    let child = upstream("api.openai.com");
    let mut parent = upstream("api.openai.com");
    parent.plugins = Some(plugins(SharingMode::Private, &["p1"]));
    assert!(effective_config(&child, &[parent], None).plugins.is_empty());
}

#[test]
fn inherited_cors_origins_are_unioned() {
    let mut child = upstream("api.openai.com");
    child.cors = Some(cors(&["https://admin.example.com"], SharingMode::Private));
    let mut parent = upstream("api.openai.com");
    parent.cors = Some(cors(&["https://app.example.com"], SharingMode::Inherit));

    let effective = effective_config(&child, &[parent], None).cors.unwrap();
    assert!(effective.origin_allowed("https://admin.example.com"));
    assert!(effective.origin_allowed("https://app.example.com"));
}

#[test]
fn enforced_cors_pins_the_ancestor_policy() {
    let mut child = upstream("api.openai.com");
    child.cors = Some(cors(&["https://anything.example"], SharingMode::Private));
    let mut parent = upstream("api.openai.com");
    parent.cors = Some(cors(&["https://app.example.com"], SharingMode::Enforce));

    let effective = effective_config(&child, &[parent], None).cors.unwrap();
    assert!(effective.origin_allowed("https://app.example.com"));
    assert!(!effective.origin_allowed("https://anything.example"));
}

#[test]
fn tags_union_add_only_across_the_hierarchy() {
    let mut child = upstream("api.openai.com");
    child.tags = vec!["llm".to_owned()];
    let mut parent = upstream("api.openai.com");
    parent.tags = vec!["openai".to_owned(), "llm".to_owned()];
    let mut route = route();
    route.tags = vec!["chat".to_owned()];

    let effective = effective_config(&child, &[parent], Some(&route));
    assert_eq!(effective.tags, ["openai", "llm", "chat"]);
}
