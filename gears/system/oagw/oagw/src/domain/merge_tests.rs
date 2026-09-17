use super::{
    EffectiveConfig, fold_cors, fold_rate_limit, merge, merge_auth, merge_headers, merge_plugins,
    min_rate_limit,
};
use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, PluginBinding, PluginsConfig, RateLimitAlgorithm,
    RateLimitConfig, RateLimitScope, RateLimitStrategy, RequestHeaderRules, ResponseHeaderRules,
    Sharing, UpstreamSpec,
};
use std::collections::BTreeMap;

fn rate(rate: u32, sharing: Sharing) -> RateLimitConfig {
    RateLimitConfig {
        sharing,
        algorithm: RateLimitAlgorithm::TokenBucket,
        sustained: crate::domain::model::SustainedRate {
            rate,
            window: crate::domain::model::RateWindow::Second,
        },
        burst: None,
        scope: RateLimitScope::Tenant,
        strategy: RateLimitStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

fn cors(origins: &[&str], sharing: Sharing) -> CorsConfig {
    CorsConfig {
        sharing,
        enabled: true,
        allowed_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
        allowed_methods: vec!["GET".to_owned()],
        expose_headers: Vec::new(),
        allow_credentials: false,
    }
}

fn auth(plugin_type: &str, sharing: Sharing) -> AuthConfig {
    AuthConfig {
        plugin_type: plugin_type.to_owned(),
        sharing,
        config: serde_json::Value::Null,
    }
}

fn binding(reference: &str) -> PluginBinding {
    PluginBinding {
        plugin_ref: reference.to_owned(),
        config: serde_json::Value::Null,
    }
}

fn upstream() -> UpstreamSpec {
    UpstreamSpec::default()
}

#[test]
fn the_closest_upstream_decides_whether_the_link_is_live() {
    let mut root = upstream();
    root.enabled = true;
    let mut child = upstream();
    child.enabled = false;
    let effective = merge(&[&child, &root], None);
    assert!(!effective.enabled, "a disabled closest upstream wins");
}

#[test]
fn tags_are_unioned_across_the_chain() {
    let mut root = upstream();
    root.tags = vec!["root".to_owned()];
    let mut child = upstream();
    child.tags = vec!["child".to_owned(), "shared".to_owned()];
    let mut parent = upstream();
    parent.tags = vec!["shared".to_owned()];
    let effective = merge(&[&child, &parent, &root], None);
    assert_eq!(effective.tag_list(), vec!["child", "root", "shared"]);
}

#[test]
fn header_rules_fold_per_action() {
    let mut parent = upstream();
    parent.headers = HeadersConfig {
        request: RequestHeaderRules {
            set: BTreeMap::from([("x-a".to_owned(), "parent".to_owned())]),
            add: BTreeMap::from([("x-add".to_owned(), "parent".to_owned())]),
            remove: vec!["x-drop".to_owned()],
            passthrough: crate::domain::model::Passthrough::All,
            passthrough_allowlist: vec!["x-keep".to_owned()],
        },
        response: ResponseHeaderRules {
            set: BTreeMap::from([("x-resp".to_owned(), "parent".to_owned())]),
            add: BTreeMap::new(),
            remove: vec!["x-resp-drop".to_owned()],
        },
    };
    let mut child = upstream();
    child.headers = HeadersConfig {
        request: RequestHeaderRules {
            set: BTreeMap::from([("x-a".to_owned(), "child".to_owned())]),
            ..RequestHeaderRules::default()
        },
        response: ResponseHeaderRules::default(),
    };
    let effective = merge(&[&child, &parent], None);
    assert_eq!(
        effective.headers.request.set.get("x-a").map(String::as_str),
        Some("child"),
        "the descendant's rule wins"
    );
    assert_eq!(
        effective
            .headers
            .request
            .add
            .get("x-add")
            .map(String::as_str),
        Some("parent"),
        "an unset action inherits the parent's"
    );
    assert_eq!(effective.headers.request.remove, vec!["x-drop".to_owned()]);
    assert_eq!(
        effective.headers.response.remove,
        vec!["x-resp-drop".to_owned()]
    );
}

#[test]
fn an_enforcing_parent_pins_the_auth_binding() {
    let mut root = upstream();
    root.auth = Some(auth("cf.core.oagw.apikey.v1", Sharing::Enforce));
    let mut child = upstream();
    child.auth = Some(auth("cf.core.oagw.noop.v1", Sharing::Inherit));
    let effective = merge(&[&child, &root], None);
    assert_eq!(
        effective.auth.map(|auth| auth.plugin_type),
        Some("cf.core.oagw.apikey.v1".to_owned())
    );
}

#[test]
fn an_inheriting_child_overrides_the_auth_binding() {
    let mut root = upstream();
    root.auth = Some(auth("cf.core.oagw.apikey.v1", Sharing::Inherit));
    let mut child = upstream();
    child.auth = Some(auth("cf.core.oagw.noop.v1", Sharing::Inherit));
    let effective = merge(&[&child, &root], None);
    assert_eq!(
        effective.auth.map(|auth| auth.plugin_type),
        Some("cf.core.oagw.noop.v1".to_owned())
    );
}

#[test]
fn an_unset_child_inherits_the_parent_auth_binding() {
    let mut root = upstream();
    root.auth = Some(auth("cf.core.oagw.apikey.v1", Sharing::Inherit));
    let effective = merge(&[&upstream(), &root], None);
    assert!(effective.auth.is_some());
}

#[test]
fn plugin_chains_are_concatenated_root_first() {
    let mut root = upstream();
    root.plugins = PluginsConfig {
        sharing: Sharing::Inherit,
        items: vec![binding(
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
        )],
    };
    let mut child = upstream();
    child.plugins = PluginsConfig {
        sharing: Sharing::Inherit,
        items: vec![binding(
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
        )],
    };
    let effective = merge(&[&child, &root], None);
    let refs: Vec<String> = effective
        .plugins
        .iter()
        .map(|p| p.plugin_ref.clone())
        .collect();
    assert_eq!(refs.len(), 2);
    assert!(
        refs[0].contains("request_id"),
        "ancestors run first: {refs:?}"
    );
    assert!(
        refs[1].contains("required_headers"),
        "descendants run last: {refs:?}"
    );
}

#[test]
fn route_plugins_run_after_the_upstream_chain() {
    let mut child = upstream();
    child.plugins = PluginsConfig {
        sharing: Sharing::Inherit,
        items: vec![binding("upstream-plugin")],
    };
    let route = crate::domain::model::RouteSpec {
        plugins: PluginsConfig {
            sharing: Sharing::Inherit,
            items: vec![binding("route-plugin")],
        },
        ..crate::domain::model::RouteSpec::default()
    };
    let effective = merge(&[&child], Some(&route));
    let refs: Vec<String> = effective
        .plugins
        .iter()
        .map(|p| p.plugin_ref.clone())
        .collect();
    assert_eq!(
        refs,
        vec!["upstream-plugin".to_owned(), "route-plugin".to_owned()]
    );
}

#[test]
fn private_ancestors_do_not_constrain_the_child_rate_limit() {
    let parent = rate(100, Sharing::Private);
    let child = rate(500, Sharing::Inherit);
    let folded = fold_rate_limit(Some(parent), Some(child));
    assert_eq!(folded.map(|limit| limit.sustained.rate), Some(500));
}

#[test]
fn inherited_ancestors_cap_the_child_rate_limit() {
    let parent = rate(100, Sharing::Inherit);
    let child = rate(500, Sharing::Inherit);
    let folded = fold_rate_limit(Some(parent), Some(child));
    assert_eq!(folded.map(|limit| limit.sustained.rate), Some(100));

    let parent = rate(500, Sharing::Enforce);
    let child = rate(10, Sharing::Inherit);
    let folded = fold_rate_limit(Some(parent), Some(child));
    assert_eq!(folded.map(|limit| limit.sustained.rate), Some(10));
}

#[test]
fn an_unlimited_child_inherits_the_parent_rate_limit() {
    let parent = rate(42, Sharing::Inherit);
    let folded = fold_rate_limit(Some(parent), None);
    assert_eq!(folded.map(|limit| limit.sustained.rate), Some(42));
    assert!(fold_rate_limit(None, None).is_none());
}

#[test]
fn the_stricter_rate_limit_wins_component_wise() {
    let mut strict = rate(10, Sharing::Inherit);
    strict.burst = Some(crate::domain::model::Burst { capacity: 10 });
    let relaxed = rate(20, Sharing::Inherit);
    let merged = min_rate_limit(&strict, &relaxed);
    assert_eq!(merged.sustained.rate, 10);
    assert_eq!(merged.burst.map(|burst| burst.capacity), Some(10));

    let merged = min_rate_limit(&relaxed, &strict);
    assert_eq!(merged.sustained.rate, 10);
}

#[test]
fn an_enforcing_cors_parent_wins() {
    let parent = cors(&["https://parent.example.com"], Sharing::Enforce);
    let child = cors(&["https://child.example.com"], Sharing::Inherit);
    let folded = fold_cors(Some(parent), Some(child));
    let origins = folded.map(|cors| cors.allowed_origins).unwrap_or_default();
    assert_eq!(origins, vec!["https://parent.example.com".to_owned()]);
}

#[test]
fn an_inherited_cors_parent_is_unioned() {
    let parent = cors(&["https://parent.example.com"], Sharing::Inherit);
    let child = cors(&["https://child.example.com"], Sharing::Inherit);
    let folded = fold_cors(Some(parent), Some(child));
    let origins = folded.map(|cors| cors.allowed_origins).unwrap_or_default();
    assert_eq!(
        origins,
        vec![
            "https://child.example.com".to_owned(),
            "https://parent.example.com".to_owned()
        ]
    );
}

#[test]
fn a_private_cors_parent_is_ignored() {
    let parent = cors(&["https://parent.example.com"], Sharing::Private);
    let child = cors(&["https://child.example.com"], Sharing::Inherit);
    let folded = fold_cors(Some(parent), Some(child));
    let origins = folded.map(|cors| cors.allowed_origins).unwrap_or_default();
    assert_eq!(origins, vec!["https://child.example.com".to_owned()]);
}

#[test]
fn a_route_cors_folds_into_the_upstream_one() {
    let mut child = upstream();
    child.cors = Some(cors(&["https://upstream.example.com"], Sharing::Inherit));
    let route = crate::domain::model::RouteSpec {
        cors: Some(cors(&["https://route.example.com"], Sharing::Inherit)),
        ..crate::domain::model::RouteSpec::default()
    };
    let effective = merge(&[&child], Some(&route));
    let origins = effective
        .cors
        .map(|cors| cors.allowed_origins)
        .unwrap_or_default();
    assert_eq!(origins.len(), 2);
}

#[test]
fn effective_configuration_defaults_are_empty() {
    let effective = EffectiveConfig::default();
    assert!(effective.plugins.is_empty());
    assert!(effective.auth.is_none());
    assert!(effective.rate_limit.is_none());
    assert!(effective.cors.is_none());
    assert!(effective.tag_list().is_empty());
}

#[test]
fn header_merging_hands_the_parent_rules_down() {
    let mut parent = upstream();
    parent.headers.request.passthrough = crate::domain::model::Passthrough::Allowlist;
    parent.headers.request.passthrough_allowlist = vec!["x-keep".to_owned()];
    let merged = merge_headers(&parent.headers, &HeadersConfig::default());
    assert_eq!(
        merged.request.passthrough,
        crate::domain::model::Passthrough::Allowlist
    );
    assert_eq!(
        merged.request.passthrough_allowlist,
        vec!["x-keep".to_owned()]
    );
}

#[test]
fn auth_merging_falls_back_to_the_parent() {
    let parent = auth("cf.core.oagw.apikey.v1", Sharing::Inherit);
    assert!(merge_auth(Some(&parent), None).is_some());
    assert!(merge_auth(None, None).is_none());
    let child = auth("cf.core.oagw.noop.v1", Sharing::Inherit);
    assert_eq!(
        merge_auth(None, Some(&child)).map(|auth| auth.plugin_type),
        Some("cf.core.oagw.noop.v1".to_owned())
    );
}

#[test]
fn plugin_merging_keeps_the_given_order() {
    let parent = vec![binding("parent")];
    let child = vec![binding("child")];
    let merged = merge_plugins(&parent, &child);
    let refs: Vec<String> = merged.iter().map(|p| p.plugin_ref.clone()).collect();
    assert_eq!(refs, vec!["parent".to_owned(), "child".to_owned()]);
}

#[test]
fn a_route_rate_limit_folds_into_the_chain() {
    let mut child = upstream();
    child.rate_limit = Some(rate(50, Sharing::Inherit));
    let route = crate::domain::model::RouteSpec {
        rate_limit: Some(rate(10, Sharing::Inherit)),
        ..crate::domain::model::RouteSpec::default()
    };
    let effective = merge(&[&child], Some(&route));
    assert_eq!(
        effective.rate_limit.map(|limit| limit.sustained.rate),
        Some(10)
    );
}
