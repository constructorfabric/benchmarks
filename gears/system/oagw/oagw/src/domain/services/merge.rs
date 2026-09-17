//! Hierarchical configuration layering (DESIGN §3.2 "Hierarchical
//! Configuration", `upstream < route < tenant`).
//!
//! Merge strategies (normative):
//!
//! | Field | Strategy |
//! |---|---|---|
//! | Auth | enforced-ancestor wins; otherwise selected upstream |
//! | Rate limits | `min(parent, child)` — stricter always wins |
//! | Plugins | concatenate: `ancestors + upstream + route` |
//! | CORS | union origins/methods; enforced cannot be removed |
//! | Tags | add-only union |

use std::sync::Arc;

use crate::domain::dto::{ActiveBinding, EffectiveConfig, EffectiveRateLimit};
use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, PluginKind, RateLimitConfig, Route, SharingMode,
    Upstream,
};

/// Merge the configuration of the selected upstream with its same-alias
/// ancestors (ordered leaf → root, excluding the selected row) and the
/// matched route.
#[must_use]
pub fn compute_effective(
    selected: &Upstream,
    ancestors: &[Arc<Upstream>],
    route: Option<&Route>,
) -> EffectiveConfig {
    let contributing_ancestors: Vec<&Upstream> = ancestors
        .iter()
        .map(|a| a.as_ref())
        .filter(|a| a.plugins.sharing != SharingMode::Private)
        .collect();

    let auth = merge_auth(selected, ancestors);
    let guards = merge_plugins(PluginKind::Guard, &contributing_ancestors, selected, route);
    let transforms =
        merge_plugins(PluginKind::Transform, &contributing_ancestors, selected, route);
    let rate_limit = merge_rate_limit(selected, ancestors, route);
    let cors = merge_cors(selected, ancestors, route);
    let tags = merge_tags(selected, ancestors, route);

    EffectiveConfig {
        auth,
        headers: HeadersConfig {
            request: selected.headers.request.clone(),
            response: selected.headers.response.clone(),
        },
        guards,
        transforms,
        rate_limit,
        cors,
        tags,
        enabled: selected.enabled,
    }
}

fn merge_auth(selected: &Upstream, ancestors: &[Arc<Upstream>]) -> AuthConfig {
    // Enforced ancestors override the descendant's own credentials. Closest
    // (leaf-ward) enforced ancestor wins.
    for anc in ancestors {
        if anc.auth.sharing == SharingMode::Enforce && !anc.auth.auth_type.is_empty() {
            return anc.auth.clone();
        }
    }
    selected.auth.clone()
}

/// Concatenate bindings of `kind`: inherited/enforced ancestors (root-most
/// first) + upstream + route.
fn merge_plugins(
    kind: PluginKind,
    ancestors: &[&Upstream],
    selected: &Upstream,
    route: Option<&Route>,
) -> Vec<ActiveBinding> {
    let mut out: Vec<ActiveBinding> = Vec::new();
    // Ancestors arrive leaf → root; push reversed so root-most executes first.
    for anc in ancestors.iter().rev() {
        out.extend(
            kind.select(&anc.plugins.items)
                .into_iter()
                .map(ActiveBinding::from),
        );
    }
    out.extend(kind.select(&selected.plugins.items).into_iter().map(ActiveBinding::from));
    if let Some(route) = route {
        out.extend(kind.select(&route.plugins.items).into_iter().map(ActiveBinding::from));
    }
    out
}

/// Stricter-wins rate limit across enforced/inherited ancestors, the selected
/// upstream and the matched route.
fn merge_rate_limit(
    selected: &Upstream,
    ancestors: &[Arc<Upstream>],
    route: Option<&Route>,
) -> Option<EffectiveRateLimit> {
    struct Candidate<'a> {
        cfg: &'a RateLimitConfig,
        /// Ordering: ancestors (root-most) < selected < route; larger = closer
        /// to the request (tie-break so the closer contributor wins on equal
        /// strictness).
        order: u32,
    }

    let mut candidates: Vec<Candidate<'_>> = Vec::new();
    // Ancestors arrive leaf → root; iterate reversed so root-most is listed
    // first and receives the smallest `order`.
    for (i, anc) in ancestors.iter().rev().enumerate() {
        if let Some(cfg) = &anc.rate_limit
            && cfg.sharing != SharingMode::Private
        {
            candidates.push(Candidate {
                cfg,
                order: i as u32,
            });
        }
    }
    if let Some(cfg) = &selected.rate_limit {
        candidates.push(Candidate {
            cfg,
            order: 100,
        });
    }
    if let Some(cfg) = route.and_then(|r| r.rate_limit.as_ref()) {
        candidates.push(Candidate {
            cfg,
            order: 200,
        });
    }

    if candidates.is_empty() {
        return None;
    }

    // Strictness = refill tokens/sec; smaller is stricter.
    let winner = candidates
        .iter()
        .min_by(|a, b| {
            a.cfg
                .refill_per_sec()
                .total_cmp(&b.cfg.refill_per_sec())
                .then_with(|| a.order.cmp(&b.order))
        })?
        .cfg;

    // Effective capacity: min over all candidates (stricter always wins).
    let capacity = candidates
        .iter()
        .map(|c| c.cfg.capacity())
        .fold(f64::INFINITY, f64::min);

    // The rate ceiling and its counter scope transfer from the strictest
    // contributor; the demand-side strategy (reject/queue/degrade) is the one
    // chosen by the contributor closest to the request (the leaf), which is
    // where the behaviour on exceeding the shared ceiling is exercised.
    let closest = candidates
        .iter()
        .max_by_key(|c| c.order)
        .map(|c| c.cfg)
        .unwrap_or(winner);

    Some(EffectiveRateLimit {
        refill_per_sec: winner.refill_per_sec(),
        capacity,
        cost: winner.cost.max(1),
        strategy: closest.strategy,
        scope: winner.scope,
        window_secs: winner.sustained.window.as_secs(),
    })
}

/// Union of CORS origins/methods/expose-headers.
fn merge_cors(
    selected: &Upstream,
    ancestors: &[Arc<Upstream>],
    route: Option<&Route>,
) -> Option<CorsConfig> {
    let mut contributors: Vec<&CorsConfig> = Vec::new();
    for anc in ancestors.iter().rev() {
        if let Some(c) = &anc.cors
            && c.sharing != SharingMode::Private
        {
            contributors.push(c);
        }
    }
    if let Some(c) = &selected.cors {
        contributors.push(c);
    }
    if let Some(r) = route
        && let Some(c) = &r.cors
    {
        contributors.push(c);
    }

    let mut out = CorsConfig {
        enabled: false,
        allowed_origins: Vec::new(),
        allowed_methods: Vec::new(),
        expose_headers: Vec::new(),
        allow_credentials: false,
        sharing: SharingMode::Private,
    };
    for c in &contributors {
        out.enabled |= c.enabled;
        for o in &c.allowed_origins {
            if !out.allowed_origins.contains(o) {
                out.allowed_origins.push(o.clone());
            }
        }
        for m in &c.allowed_methods {
            if !out.allowed_methods.contains(m) {
                out.allowed_methods.push(m.clone());
            }
        }
        for h in &c.expose_headers {
            if !out.expose_headers.contains(h) {
                out.expose_headers.push(h.clone());
            }
        }
        out.allow_credentials |= c.allow_credentials;
        out.sharing = if c.sharing == SharingMode::Enforce {
            SharingMode::Enforce
        } else if out.sharing == SharingMode::Private {
            c.sharing
        } else {
            out.sharing
        };
    }
    if contributors.is_empty() {
        return None;
    }
    Some(out)
}

/// Add-only union of tags (ancestors + selected + route).
///
/// Tags have no sharing mode (DESIGN §3.2: "Tags do not have a sharing mode —
/// they always use add-only union semantics").
fn merge_tags(selected: &Upstream, ancestors: &[Arc<Upstream>], route: Option<&Route>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for anc in ancestors.iter().rev() {
        for t in &anc.tags {
            if !out.contains(t) {
                out.push(t.clone());
            }
        }
    }
    for t in &selected.tags {
        if !out.contains(t) {
            out.push(t.clone());
        }
    }
    if let Some(r) = route {
        for t in &r.tags {
            if !out.contains(t) {
                out.push(t.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{PluginBinding, RateLimitScope, RateLimitStrategy, RateWindow, SustainedLimit};
    use uuid::Uuid;

    fn upstream(alias: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            enabled: true,
            alias: Some(alias.to_owned()),
            tags: Vec::new(),
            server: Default::default(),
            protocol: Default::default(),
            auth: Default::default(),
            headers: Default::default(),
            plugins: Default::default(),
            rate_limit: None,
            cors: None,
            bound: false,
        }
    }

    fn rate(r: u64, sharing: SharingMode) -> Option<RateLimitConfig> {
        Some(RateLimitConfig {
            sustained: SustainedLimit {
                rate: r,
                window: RateWindow::Minute,
            },
            sharing,
            ..Default::default()
        })
    }

    #[test]
    fn rate_limit_min_stricter_wins_across_hierarchy() {
        // Ancestor enforces 100/min; selected 10/min -> effective 10/min.
        let mut anc = upstream("api.example.com");
        anc.rate_limit = rate(100, SharingMode::Enforce);
        let mut sel = upstream("api.example.com");
        sel.rate_limit = rate(10, SharingMode::Private);
        let eff = compute_effective(&sel, &[Arc::new(anc)], None);
        let rl = eff.rate_limit.unwrap();
        assert!((rl.refill_per_sec - 10.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn rate_limit_enforced_ancestor_wins_over_child() {
        // Ancestor enforces 10/min; child tries 100/min -> effective 10/min.
        let mut anc = upstream("api.example.com");
        anc.rate_limit = rate(10, SharingMode::Enforce);
        let mut sel = upstream("api.example.com");
        sel.rate_limit = rate(100, SharingMode::Inherit);
        let eff = compute_effective(&sel, &[Arc::new(anc)], None);
        let rl = eff.rate_limit.unwrap();
        assert!((rl.refill_per_sec - 10.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn rate_limit_no_contributors_is_none() {
        let sel = upstream("api.example.com");
        let eff = compute_effective(&sel, &[], None);
        assert!(eff.rate_limit.is_none());
    }

    fn binding(id: &str, kind: PluginKind) -> PluginBinding {
        PluginBinding {
            plugin_ref: format!("gts.cf.core.oagw.{kind}_plugin.v1~cf.core.oagw.{id}"),
            plugin_uuid: None,
            position: None,
            config: serde_json::Value::Null,
        }
    }

    #[test]
    fn plugins_concatenate_ancestors_then_upstream_then_route() {
        let mut anc = upstream("a.example.com");
        // A shared (non-private) ancestor's plugins are visible to descendants.
        anc.plugins.sharing = SharingMode::Inherit;
        anc.plugins.items = vec![binding("anc_guard", PluginKind::Guard)];
        let mut sel = upstream("a.example.com");
        sel.plugins.items = vec![binding("sel_guard", PluginKind::Guard)];
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
            tags: Vec::new(),
            priority: 0,
            match_: Default::default(),
            plugins: crate::domain::model::PluginsConfig {
                items: vec![binding("route_guard", PluginKind::Guard)],
                ..Default::default()
            },
            rate_limit: None,
            cors: None,
        };
        let eff = compute_effective(&sel, &[Arc::new(anc)], Some(&route));
        let ids: Vec<&str> = eff
            .guards
            .iter()
            .map(|b| b.plugin_ref.rsplit('.').next().unwrap())
            .collect();
        assert_eq!(ids, vec!["anc_guard", "sel_guard", "route_guard"]);
        assert!(eff.transforms.is_empty());
    }

    #[test]
    fn private_ancestor_plugins_excluded() {
        let mut anc = upstream("a.example.com");
        anc.plugins.sharing = SharingMode::Private;
        anc.plugins.items = vec![binding("hidden", PluginKind::Guard)];
        let sel = upstream("a.example.com");
        let eff = compute_effective(&sel, &[Arc::new(anc)], None);
        assert!(eff.guards.is_empty());
    }

    #[test]
    fn enforced_auth_wins_over_selected() {
        let mut anc = upstream("a.example.com");
        anc.auth = crate::domain::model::AuthConfig {
            auth_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".into(),
            sharing: SharingMode::Enforce,
            config: serde_json::json!({"header": "X-Api-Key"}),
        };
        let sel = upstream("a.example.com");
        let eff = compute_effective(&sel, &[Arc::new(anc)], None);
        assert_eq!(eff.auth.auth_type, "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1");
    }

    #[test]
    fn cors_union_across_contributors() {
        let mut anc = upstream("a.example.com");
        anc.cors = Some(CorsConfig {
            sharing: SharingMode::Enforce,
            enabled: true,
            allowed_origins: vec!["https://anc.com".into()],
            allowed_methods: vec!["GET".into()],
            expose_headers: vec!["X-A".into()],
            allow_credentials: false,
        });
        let mut sel = upstream("a.example.com");
        sel.cors = Some(CorsConfig {
            sharing: SharingMode::Inherit,
            enabled: true,
            allowed_origins: vec!["https://sel.com".into(), "https://anc.com".into()],
            allowed_methods: vec!["POST".into()],
            expose_headers: vec!["X-B".into()],
            allow_credentials: true,
        });
        let eff = compute_effective(&sel, &[Arc::new(anc)], None);
        let c = eff.cors.unwrap();
        assert_eq!(c.allowed_origins, vec!["https://anc.com", "https://sel.com"]);
        assert!(c.allow_credentials);
        assert!(c.enabled);
    }

    #[test]
    fn tags_union_add_only() {
        let mut anc = upstream("a.example.com");
        anc.tags = vec!["team:core".into(), "env:prod".into()];
        let mut sel = upstream("a.example.com");
        sel.tags = vec!["team:core".into(), "team:app".into()];
        let eff = compute_effective(&sel, &[Arc::new(anc)], None);
        assert_eq!(eff.tags, vec!["team:core", "env:prod", "team:app"]);
    }

    #[test]
    fn scope_transfers_from_stricter_winner() {
        let mut anc = upstream("a.example.com");
        anc.rate_limit = rate(10, SharingMode::Enforce);
        if let Some(r) = &mut anc.rate_limit {
            r.scope = RateLimitScope::Tenant;
        }
        let mut sel = upstream("a.example.com");
        sel.rate_limit = rate(100, SharingMode::Inherit);
        if let Some(r) = &mut sel.rate_limit {
            r.scope = RateLimitScope::Ip;
            r.strategy = RateLimitStrategy::Queue;
        }
        let eff = compute_effective(&sel, &[Arc::new(anc)], None);
        let rl = eff.rate_limit.unwrap();
        assert_eq!(rl.scope, RateLimitScope::Tenant);
        assert_eq!(rl.strategy, RateLimitStrategy::Queue);
    }

    #[test]
    fn rate_limit_min_across_multi_level_enforced_ancestors() {
        // chain: selected <- parent (10/min, enforce) <- root (1000/min, enforce).
        // Effective rate must be the strictest across EVERY level.
        let mut root = upstream("a.example.com");
        root.rate_limit = rate(1000, SharingMode::Enforce);
        let mut parent = upstream("a.example.com");
        parent.rate_limit = rate(10, SharingMode::Enforce);
        let mut sel = upstream("a.example.com");
        sel.rate_limit = rate(100, SharingMode::Inherit);

        let eff = compute_effective(&sel, &[Arc::new(parent), Arc::new(root)], None);
        let rl = eff.rate_limit.unwrap();
        assert!((rl.refill_per_sec - 10.0 / 60.0).abs() < 1e-9);
        assert_eq!(rl.capacity as u64, 10);
    }

    #[test]
    fn plugin_order_across_three_levels_root_most_first() {
        let mut root = upstream("a.example.com");
        root.plugins.sharing = SharingMode::Enforce;
        root.plugins.items = vec![binding("root_guard", PluginKind::Guard)];
        let mut parent = upstream("a.example.com");
        parent.plugins.sharing = SharingMode::Inherit;
        parent.plugins.items = vec![binding("parent_guard", PluginKind::Guard)];
        let mut sel = upstream("a.example.com");
        sel.plugins.items = vec![binding("sel_guard", PluginKind::Guard)];
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
            tags: Vec::new(),
            priority: 0,
            match_: Default::default(),
            plugins: crate::domain::model::PluginsConfig {
                items: vec![binding("route_guard", PluginKind::Guard)],
                ..Default::default()
            },
            rate_limit: None,
            cors: None,
        };

        let eff = compute_effective(&sel, &[Arc::new(parent), Arc::new(root)], Some(&route));
        let ids: Vec<&str> = eff
            .guards
            .iter()
            .map(|b| b.plugin_ref.rsplit('.').next().unwrap())
            .collect();
        assert_eq!(ids, vec!["root_guard", "parent_guard", "sel_guard", "route_guard"]);
    }

    #[test]
    fn private_ancestor_rate_limit_excluded() {
        let mut anc = upstream("a.example.com");
        anc.rate_limit = rate(1, SharingMode::Private);
        let sel = upstream("a.example.com");
        let eff = compute_effective(&sel, &[Arc::new(anc)], None);
        assert!(eff.rate_limit.is_none());
    }

    #[test]
    fn inherit_auth_is_overridden_by_selected() {
        // DESIGN: Auth override if `inherit`. A descendant sets its own auth;
        // the inheriting ancestor's credentials must NOT leak through.
        let mut anc = upstream("a.example.com");
        anc.auth = AuthConfig {
            auth_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".into(),
            sharing: SharingMode::Inherit,
            config: serde_json::json!({"header": "X-Api-Key"}),
        };
        let mut sel = upstream("a.example.com");
        sel.auth = AuthConfig {
            auth_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1".into(),
            sharing: SharingMode::Private,
            config: serde_json::Value::Null,
        };
        let eff = compute_effective(&sel, &[Arc::new(anc)], None);
        assert_eq!(
            eff.auth.auth_type,
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1"
        );
    }

    #[test]
    fn enforced_auth_with_empty_type_does_not_override() {
        // An ancestor that enforces but never configured a plugin type has
        // nothing to enforce — the selected upstream's auth is used.
        let mut anc = upstream("a.example.com");
        anc.auth = AuthConfig {
            auth_type: String::new(),
            sharing: SharingMode::Enforce,
            config: serde_json::Value::Null,
        };
        let mut sel = upstream("a.example.com");
        sel.auth = AuthConfig {
            auth_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".into(),
            sharing: SharingMode::Private,
            config: serde_json::json!({"key": "k"}),
        };
        let eff = compute_effective(&sel, &[Arc::new(anc)], None);
        assert_eq!(
            eff.auth.auth_type,
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
        );
    }
}
