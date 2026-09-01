//! Hierarchical configuration merge.
//!
//! Implements DESIGN.md §3.2 "Hierarchical Configuration": sharing modes
//! (`private`/`inherit`/`enforce`) per configuration field plus the
//! add-only tag union.  `chain_matches` is the list of upstream definitions
//! matching a resolved alias, ordered ROOT → SELECTED (leaf), where each
//! entry is `(tenant_id, upstream)`.

use crate::domain::dto::{
    AUTH_NOOP, AuthConfig, CorsConfig, PluginBinding, PluginsConfig, RateLimitConfig, Sharing,
    Upstream,
};

/// Deterministically hash a plugin config value for cache keys.
#[must_use]
pub fn hash_config(value: &serde_json::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.to_string().hash(&mut hasher);
    hasher.finish()
}

/// Effective auth config: an `enforce` ancestor wins; otherwise the selected
/// upstream's own explicit auth; otherwise the nearest ancestor's
/// inherit-visible auth; defaulting to `noop`.
#[must_use]
pub fn effective_auth(chain_matches: &[&Upstream]) -> AuthConfig {
    let Some(selected) = chain_matches.last() else {
        return AuthConfig {
            plugin_type: Some(AUTH_NOOP.to_owned()),
            ..Default::default()
        };
    };

    // 1. `enforce` ancestors cannot be overridden by shadowing.
    for up in chain_matches {
        if up.auth.sharing == Sharing::Enforce && has_real_auth(&up.auth) {
            return up.auth.clone();
        }
    }
    // 2. The selected upstream's own explicit auth.
    if has_real_auth(&selected.auth) {
        return selected.auth.clone();
    }
    // 3. Nearest ancestor (descending from leaf-ward) with real auth that is
    //    inherit-visible.  `private`-shared auth is invisible to descendants
    //    (DESIGN.md sharing table) and must not leak into the effective chain.
    for up in chain_matches.iter().rev() {
        if std::ptr::eq(*up, *selected) {
            continue;
        }
        if up.auth.sharing == Sharing::Private {
            continue;
        }
        if has_real_auth(&up.auth) {
            return up.auth.clone();
        }
    }
    AuthConfig {
        plugin_type: Some(AUTH_NOOP.to_owned()),
        ..Default::default()
    }
}

fn has_real_auth(auth: &AuthConfig) -> bool {
    auth.plugin_type
        .as_deref()
        .is_some_and(|t| !t.is_empty() && t != AUTH_NOOP)
}

/// Rate-limit constraints for a request: the selected upstream's limit, every
/// `enforce`-shared ancestor limit, and the route's limit.
///
/// Per DESIGN.md "Shadowing Behavior" the effective limit is
/// `min(selected_rate, route_rate, all_ancestor_enforced_rates)` — only
/// enforced ancestors constrain a shadowing descendant.  `inherit`/`private`
/// ancestors do NOT consume quota for a child that defines its own limit, so
/// they are excluded here.
#[must_use]
pub fn effective_rate_limits(
    chain_matches: &[&Upstream],
    route_rate: Option<&RateLimitConfig>,
) -> Vec<RateLimitConfig> {
    let mut out: Vec<RateLimitConfig> = Vec::new();
    for (idx, up) in chain_matches.iter().enumerate() {
        let is_selected = idx == chain_matches.len() - 1;
        if !is_selected && up.rate_limit.as_ref().map(|r| r.sharing) != Some(Sharing::Enforce) {
            // Ancestors only contribute a limit when they enforce it.
            continue;
        }
        if let Some(rl) = &up.rate_limit {
            out.push(rl.clone());
        }
    }
    if let Some(rl) = route_rate {
        out.push(rl.clone());
    }
    out
}

/// Effective plugin bindings: `ancestor.plugins + descendant.plugins`
/// (root → selected), private non-selected chains excluded, then the
/// route's bindings appended.
#[must_use]
pub fn effective_plugin_bindings(
    chain_matches: &[&Upstream],
    route_plugins: &PluginsConfig,
) -> Vec<PluginBinding> {
    let mut out: Vec<PluginBinding> = Vec::new();
    for (idx, up) in chain_matches.iter().enumerate() {
        let is_selected = idx == chain_matches.len() - 1;
        if up.plugins.sharing == Sharing::Private && !is_selected {
            continue;
        }
        for item in &up.plugins.items {
            out.push(item.clone());
        }
    }
    // Route plugins always bind (routes are tenant-owned).
    out.extend(route_plugins.items.iter().cloned());
    out
}

/// Effective CORS config.
///
/// * An enabled `enforce` ancestor with non-empty origins forces its exact
///   set (descendants cannot add).
/// * Otherwise origins/methods/expose-headers are unioned across enabled
///   members (root → selected) and the route.
/// * `allow_credentials` is true if any enabled member requests it.
/// * Returns `None` when nothing is enabled.
#[must_use]
pub fn effective_cors(
    chain_matches: &[&Upstream],
    route_cors: Option<&CorsConfig>,
) -> Option<CorsConfig> {
    // 1. enforce pass
    for up in chain_matches {
        if let Some(c) = &up.cors
            && c.enabled
            && c.sharing == Sharing::Enforce
            && !c.allowed_origins.is_empty()
        {
            return Some(CorsConfig {
                sharing: Sharing::Enforce,
                enabled: true,
                allowed_origins: c.allowed_origins.clone(),
                allowed_methods: c.allowed_methods.clone(),
                expose_headers: c.expose_headers.clone(),
                allow_credentials: c.allow_credentials,
            });
        }
    }

    let mut origins: Vec<String> = Vec::new();
    let mut methods: Vec<String> = Vec::new();
    let mut expose: Vec<String> = Vec::new();
    let mut credentials = false;
    let mut enabled = false;

    for up in chain_matches {
        if let Some(c) = &up.cors
            && c.enabled
        {
            enabled = true;
            credentials |= c.allow_credentials;
            union_push(&mut origins, &c.allowed_origins);
            union_push(&mut methods, &c.allowed_methods);
            union_push(&mut expose, &c.expose_headers);
        }
    }
    if let Some(c) = route_cors
        && c.enabled
    {
        enabled = true;
        credentials |= c.allow_credentials;
        union_push(&mut origins, &c.allowed_origins);
        union_push(&mut methods, &c.allowed_methods);
        union_push(&mut expose, &c.expose_headers);
    }

    if !enabled {
        return None;
    }
    Some(CorsConfig {
        sharing: Sharing::Inherit,
        enabled: true,
        allowed_origins: origins,
        allowed_methods: methods,
        expose_headers: expose,
        allow_credentials: credentials,
    })
}

fn union_push(target: &mut Vec<String>, src: &[String]) {
    for v in src {
        if !target.iter().any(|t| t == v) {
            target.push(v.clone());
        }
    }
}

/// Union of tags from every chain member plus the route's own tags.
#[must_use]
pub fn effective_tags(chain_matches: &[&Upstream], route_tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for up in chain_matches {
        union_push(&mut out, &up.tags);
    }
    union_push(&mut out, route_tags);
    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::dto::{
        AUTH_APIKEY, AUTH_OAUTH2_FORM, Endpoint, HeadersConfig, Protocol, RateLimitAlgorithm,
        RateLimitScope, RateLimitStrategy, RateLimitWindow, ServerConfig,
    };
    use serde_json::{Value, json};
    use uuid::Uuid;

    fn up() -> Upstream {
        Upstream {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            enabled: true,
            alias: "a".into(),
            tags: vec![],
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: "https".into(),
                    host: "a.example.com".into(),
                    port: 443,
                }],
            },
            protocol: Protocol::Http,
            auth: AuthConfig::default(),
            headers: HeadersConfig::default(),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
            created_at: 0,
        }
    }

    #[test]
    fn enforce_auth_wins_over_selected() {
        let mut ancestor = up();
        ancestor.auth = AuthConfig {
            plugin_type: Some(AUTH_APIKEY.into()),
            sharing: Sharing::Enforce,
            config: json!({"secret_ref": "cred://ancestor"}),
        };
        let mut selected = up();
        selected.auth = AuthConfig {
            plugin_type: Some(AUTH_OAUTH2_FORM.into()),
            sharing: Sharing::Inherit,
            config: json!({"token_endpoint": "https://x" }),
        };
        let eff = effective_auth(&[&ancestor, &selected]);
        assert_eq!(eff.plugin_type.as_deref(), Some(AUTH_APIKEY));
    }

    #[test]
    fn inherit_selected_auth_wins_when_not_enforced() {
        let mut ancestor = up();
        ancestor.auth = AuthConfig {
            plugin_type: Some(AUTH_APIKEY.into()),
            sharing: Sharing::Inherit,
            config: json!({"k": "v"}),
        };
        let mut selected = up();
        selected.auth = AuthConfig {
            plugin_type: Some(AUTH_OAUTH2_FORM.into()),
            sharing: Sharing::Inherit,
            config: json!({"token_endpoint": "https://x" }),
        };
        assert_eq!(
            effective_auth(&[&ancestor, &selected])
                .plugin_type
                .as_deref(),
            Some(AUTH_OAUTH2_FORM)
        );
    }

    #[test]
    fn ancestor_auth_inherited_when_selected_has_none() {
        let mut ancestor = up();
        ancestor.auth = AuthConfig {
            plugin_type: Some(AUTH_APIKEY.into()),
            sharing: Sharing::Inherit,
            config: json!({"k": "v"}),
        };
        let selected = up(); // default noop
        assert_eq!(
            effective_auth(&[&ancestor, &selected])
                .plugin_type
                .as_deref(),
            Some(AUTH_APIKEY)
        );
    }

    #[test]
    fn default_noop_when_nothing_configured() {
        let selected = up();
        let eff = effective_auth(&[&selected]);
        assert_eq!(eff.plugin_type.as_deref(), Some(AUTH_NOOP));
    }

    #[test]
    fn rate_limits_collected_min_semantics() {
        let mut ancestor = up();
        ancestor.rate_limit = Some(rl(100, RateLimitWindow::Minute, Sharing::Enforce));
        let mut selected = up();
        selected.rate_limit = Some(rl(10, RateLimitWindow::Second, Sharing::Private));
        let route_rl = rl(5, RateLimitWindow::Second, Sharing::Private);
        let limits = effective_rate_limits(&[&ancestor, &selected], Some(&route_rl));
        assert_eq!(limits.len(), 3);
        assert_eq!(limits[0].sustained_rate, 100);
        assert_eq!(limits[1].sustained_rate, 10);
        assert_eq!(limits[2].sustained_rate, 5);
    }

    #[test]
    fn inherit_and_private_ancestor_limits_are_not_enforced() {
        // DESIGN:440 — only `enforce` ancestors join the effective min.
        // A shadowing child that defines its own limit must not be further
        // constrained by an inherit/private ancestor.
        let mut enforce_anc = up();
        enforce_anc.rate_limit = Some(rl(100, RateLimitWindow::Minute, Sharing::Enforce));
        let mut inherit_anc = up();
        inherit_anc.rate_limit = Some(rl(50, RateLimitWindow::Minute, Sharing::Inherit));
        let mut private_anc = up();
        private_anc.rate_limit = Some(rl(25, RateLimitWindow::Minute, Sharing::Private));
        let mut selected = up();
        selected.rate_limit = Some(rl(10, RateLimitWindow::Second, Sharing::Private));
        let route_rl = rl(5, RateLimitWindow::Second, Sharing::Private);

        let limits = effective_rate_limits(
            &[&enforce_anc, &inherit_anc, &private_anc, &selected],
            Some(&route_rl),
        );
        let mut rates: Vec<u64> = limits.iter().map(|l| l.sustained_rate).collect();
        rates.sort_unstable();
        // enforce ancestor (100), selected (10), route (5) — inherit (50) and
        // private (25) ancestors excluded.
        assert_eq!(rates, vec![5, 10, 100]);
    }

    #[test]
    fn selected_and_route_limits_always_included() {
        let mut selected = up();
        selected.rate_limit = Some(rl(7, RateLimitWindow::Second, Sharing::Enforce));
        let limits = effective_rate_limits(
            &[&selected],
            Some(&rl(3, RateLimitWindow::Second, Sharing::Inherit)),
        );
        let rates: Vec<u64> = limits.iter().map(|l| l.sustained_rate).collect();
        assert_eq!(rates, vec![7, 3]);
    }

    #[test]
    fn private_ancestor_auth_not_inherited_in_fallback() {
        // A `private`-shared ancestor auth must not surface as the effective
        // auth for a descendant that defines none (only `enforce` ancestors
        // are forced; everything else fallback-visible must be inherit).
        let mut private_anc = up();
        private_anc.auth = AuthConfig {
            plugin_type: Some(AUTH_APIKEY.into()),
            sharing: Sharing::Private,
            config: json!({"secret_ref": "cred://private"}),
        };
        let selected = up(); // default noop
        let eff = effective_auth(&[&private_anc, &selected]);
        assert_eq!(eff.plugin_type.as_deref(), Some(AUTH_NOOP));

        // Same ancestor with `inherit` sharing is visible.
        let mut inherit_anc = up();
        inherit_anc.auth = AuthConfig {
            plugin_type: Some(AUTH_APIKEY.into()),
            sharing: Sharing::Inherit,
            config: json!({"secret_ref": "cred://inherit"}),
        };
        let eff = effective_auth(&[&inherit_anc, &selected]);
        assert_eq!(eff.plugin_type.as_deref(), Some(AUTH_APIKEY));
    }

    #[test]
    fn enforce_ancestor_auth_still_wins_over_private_inheritance() {
        // `enforce` ancestors remain forced even when a nearer private
        // ancestor exists in the chain.
        let mut enforce_anc = up();
        enforce_anc.auth = AuthConfig {
            plugin_type: Some(AUTH_APIKEY.into()),
            sharing: Sharing::Enforce,
            config: json!({"secret_ref": "cred://enforce"}),
        };
        let mut private_anc = up();
        private_anc.auth = AuthConfig {
            plugin_type: Some(AUTH_OAUTH2_FORM.into()),
            sharing: Sharing::Private,
            config: json!({"token_endpoint": "https://x"}),
        };
        let mut selected = up();
        selected.auth = AuthConfig {
            plugin_type: Some(AUTH_OAUTH2_FORM.into()),
            sharing: Sharing::Inherit,
            config: json!({"token_endpoint": "https://y"}),
        };
        let eff = effective_auth(&[&enforce_anc, &private_anc, &selected]);
        assert_eq!(eff.plugin_type.as_deref(), Some(AUTH_APIKEY));
    }

    #[test]
    fn private_ancestor_plugins_excluded_but_selected_included() {
        let mut ancestor = up();
        ancestor.plugins = PluginsConfig {
            sharing: Sharing::Private,
            items: vec![binding("gts.x.y.v1~private")],
        };
        let mut selected = up();
        selected.plugins = PluginsConfig {
            sharing: Sharing::Inherit,
            items: vec![binding("gts.x.y.v1~selected")],
        };
        // Route plugins append after upstream ones.
        let route = PluginsConfig {
            sharing: Sharing::default(),
            items: vec![binding("gts.x.y.v1~route")],
        };
        let eff = effective_plugin_bindings(&[&ancestor, &selected], &route);
        let refs: Vec<&str> = eff.iter().map(|b| b.plugin_ref.as_str()).collect();
        // private ancestor excluded; selected then route.
        assert_eq!(refs.len(), 2);
        assert!(refs.contains(&"gts.x.y.v1~selected"));
        assert!(refs.contains(&"gts.x.y.v1~route"));
        assert!(!refs.contains(&"gts.x.y.v1~private"));
    }

    #[test]
    fn inherit_ancestor_plugins_are_included_before_selected() {
        let mut ancestor = up();
        ancestor.plugins = PluginsConfig {
            sharing: Sharing::Inherit,
            items: vec![binding("gts.x.y.v1~parent")],
        };
        let mut selected = up();
        selected.plugins = PluginsConfig {
            sharing: Sharing::Inherit,
            items: vec![binding("gts.x.y.v1~child")],
        };
        let eff = effective_plugin_bindings(&[&ancestor, &selected], &PluginsConfig::default());
        let refs: Vec<&str> = eff.iter().map(|b| b.plugin_ref.as_str()).collect();
        assert_eq!(refs, vec!["gts.x.y.v1~parent", "gts.x.y.v1~child"]);
    }

    #[test]
    fn cors_union_and_enforce() {
        let mut ancestor = up();
        ancestor.cors = Some(CorsConfig {
            sharing: Sharing::Enforce,
            enabled: true,
            allowed_origins: vec!["https://a.example".into()],
            ..Default::default()
        });
        let mut selected = up();
        selected.cors = Some(CorsConfig {
            sharing: Sharing::Inherit,
            enabled: true,
            allowed_origins: vec!["https://b.example".into()],
            ..Default::default()
        });
        // enforce wins: only ancestor origins.
        let eff = effective_cors(&[&ancestor, &selected], None).unwrap();
        assert_eq!(eff.allowed_origins, vec!["https://a.example"]);

        // without enforce → union
        let mut ancestor = up();
        ancestor.cors = Some(CorsConfig {
            sharing: Sharing::Inherit,
            enabled: true,
            allowed_origins: vec!["https://a.example".into()],
            allow_credentials: true,
            ..Default::default()
        });
        let mut selected = up();
        selected.cors = Some(CorsConfig {
            sharing: Sharing::Inherit,
            enabled: true,
            allowed_origins: vec!["https://b.example".into()],
            ..Default::default()
        });
        let eff = effective_cors(&[&ancestor, &selected], None).unwrap();
        assert_eq!(
            eff.allowed_origins,
            vec!["https://a.example", "https://b.example"]
        );
        assert!(eff.allow_credentials);
    }

    #[test]
    fn tags_union_is_add_only() {
        let mut ancestor = up();
        ancestor.tags = vec!["prod".into(), "shared".into()];
        let mut selected = up();
        selected.tags = vec!["shared".into(), "leaf".into()];
        let eff = effective_tags(&[&ancestor, &selected], &["route".into()]);
        assert_eq!(eff, vec!["prod", "shared", "leaf", "route"]);
    }

    fn rl(rate: u64, window: RateLimitWindow, sharing: Sharing) -> RateLimitConfig {
        RateLimitConfig {
            sharing,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained_rate: rate,
            sustained_window: window,
            burst_capacity: rate,
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: 1,
            response_headers: true,
        }
    }

    fn binding(plugin_ref: &str) -> PluginBinding {
        PluginBinding {
            plugin_ref: plugin_ref.into(),
            config: Value::Null,
        }
    }

    #[test]
    fn hash_config_is_deterministic_and_sensitive() {
        let a = hash_config(&json!({"z": 1, "a": "b"}));
        let b = hash_config(&json!({"z": 1, "a": "b"}));
        let c = hash_config(&json!({"z": 2, "a": "b"}));
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
