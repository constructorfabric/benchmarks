//! Hierarchical configuration merging (DESIGN §3.2 "Hierarchical Configuration").
//!
//! At proxy time OAGW walks the tenant ancestry (descendant → root), collects
//! every upstream that matches the requested alias (shadowing), then merges
//! the collected configurations root → descendant according to each field's
//! sharing mode:
//!
//! | Field        | Merge strategy                                       |
//! |--------------|------------------------------------------------------|
//! | Auth         | Closest visible wins; `enforce` on an ancestor forces|
//! | Rate limits  | `min()` across visible levels (stricter always wins) |
//! | Plugins      | Concatenate ancestor + descendant                    |
//! | CORS         | Union origins if `inherit`; forced if `enforce`      |
//!
//! Tags are add-only unions across the whole chain (never merged here).

use crate::domain::models::{
    AuthConfig, CorsConfig, Plugin, RateLimitConfig, Route, SharingMode, Stored, Upstream,
    plugin_gts,
};

#[cfg(test)]
use crate::domain::models::PluginsConfig;

/// One level of the resolution chain, ordered root → descendant.
#[derive(Debug, Clone)]
pub struct ChainLevel {
    /// Whether this level is the selected (routing) upstream.
    pub is_selected: bool,
    /// The same-alias upstream at this level.
    pub upstream: Stored<Upstream>,
    /// The winning matching route, present on the level that owns it.
    pub route: Option<Stored<Route>>,
}

/// The fully resolved chain for a proxy request.
#[derive(Debug, Clone)]
pub struct ResolvedChain {
    /// Upstream used to route the request (closest enabled match wins).
    pub selected: Stored<Upstream>,
    /// Ordered chain of every same-alias upstream (root → descendant).
    pub levels: Vec<ChainLevel>,
    /// The winning route for the request, if any matched route.
    pub matched_route: Option<Stored<Route>>,
}

impl ResolvedChain {
    /// Collect the levels that own a matching route (root → descendant).
    fn visible_levels(&self) -> Vec<&ChainLevel> {
        self.levels.iter().collect()
    }

    /// Effective merge of every `PluginsConfig` that is visible to the
    /// selected tenant, root → descendant, including the selected level's own
    /// plugins. Private ancestors do not contribute; enforce ancestors still
    /// contribute (additive — descendants cannot remove inherited plugins).
    fn merged_plugin_configs(&self) -> Vec<String> {
        let mut out = Vec::new();
        for level in self.visible_levels() {
            let private_skip = matches!(
                level.upstream.record.plugins.as_ref().map(|p| p.sharing),
                Some(SharingMode::Private)
            ) && !level.is_selected;
            if let Some(plugins) = &level.upstream.record.plugins {
                if !private_skip {
                    out.extend(plugins.items.iter().cloned());
                }
            }
        }
        // The winning route's plugins are appended last (descendant-most).
        if let Some(route) = &self.matched_route {
            if let Some(plugins) = &route.record.plugins {
                out.extend(plugins.items.iter().cloned());
            }
        }
        out
    }
}

/// Effective auth configuration for the request path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveAuth {
    /// Auth plugin GTS identifier.
    pub plugin_type: String,
    /// Auth plugin configuration.
    pub config: serde_json::Value,
}

/// Effective rate-limit parameters (per-second normalized).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectiveRate {
    /// Tokens replenished per second.
    pub refill_per_sec: f64,
    /// Bucket capacity (burst allowance).
    pub capacity: u64,
    /// Tokens consumed per request.
    pub cost: u64,
}

impl EffectiveRate {
    /// Build an effective rate from a single config (missing sustained => `None`).
    #[must_use]
    pub fn from_config(cfg: &RateLimitConfig) -> Option<Self> {
        let sustained = cfg.sustained?;
        let refill_per_sec = sustained.rate as f64 / sustained.window.as_secs() as f64;
        let capacity = cfg
            .burst
            .as_ref()
            .map_or(sustained.rate, |b| b.capacity_or(sustained.rate));
        Some(Self {
            refill_per_sec,
            capacity: capacity.max(1),
            cost: cfg.cost.max(1),
        })
    }
}

/// Effective CORS configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EffectiveCors {
    pub enabled: bool,
    pub allowed_origins: Vec<String>,
    pub allowed_methods: Vec<String>,
    pub expose_headers: Vec<String>,
    pub allow_credentials: bool,
}

impl EffectiveCors {
    /// Whether an origin is allowed (exact or `*`).
    #[must_use]
    pub fn allows_origin(&self, origin: &str) -> bool {
        self.allowed_origins.iter().any(|o| o == "*" || o == origin)
    }

    /// Whether a method is allowed (case-insensitive).
    #[must_use]
    pub fn allows_method(&self, method: &str) -> bool {
        self.allowed_methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(method))
    }
}

/// Merge a slice of rate-limit configs (all visible) using `min()` semantics.
///
/// The per-second refill rate is compared because windows may differ across
/// levels (`100/min` vs `5/sec`); the stricter (smaller per-second) wins.
/// Capacity is the strictest (smallest) burst across levels. Cost is taken
/// from the most specific config that raises it above 1 (default 1).
#[must_use]
pub fn merge_rates(configs: &[&RateLimitConfig]) -> Option<EffectiveRate> {
    let mut refill: Option<f64> = None;
    let mut capacity: Option<u64> = None;
    let mut cost = 1u64;
    for cfg in configs {
        if let Some(eff) = EffectiveRate::from_config(cfg) {
            refill = Some(match refill {
                Some(prev) => prev.min(eff.refill_per_sec),
                None => eff.refill_per_sec,
            });
            capacity = Some(match capacity {
                Some(prev) => prev.min(eff.capacity),
                None => eff.capacity,
            });
            cost = cost.max(eff.cost);
        }
    }
    match (refill, capacity) {
        (Some(r), Some(c)) => Some(EffectiveRate {
            refill_per_sec: r,
            capacity: c,
            cost,
        }),
        _ => None,
    }
}

/// Collect the visible rate-limit configs for a resolved chain and merge them
/// with `min()` semantics: the selected upstream's own, the winning route's,
/// and every visible ancestor upstream's.
#[must_use]
pub fn effective_rate(chain: &ResolvedChain) -> Option<EffectiveRate> {
    let mut configs: Vec<&RateLimitConfig> = Vec::new();
    for level in chain.visible_levels() {
        let Some(rl) = &level.upstream.record.rate_limit else {
            continue;
        };
        if matches!(rl.sharing, SharingMode::Private) && !level.is_selected {
            continue;
        }
        configs.push(rl);
    }
    if let Some(route) = &chain.matched_route {
        if let Some(rl) = &route.record.rate_limit {
            configs.push(rl);
        }
    }
    merge_rates(&configs)
}

/// Effective auth for the chain (closest visible wins; enforce blocks below).
#[must_use]
pub fn effective_auth(chain: &ResolvedChain) -> EffectiveAuth {
    let mut result: Option<&AuthConfig> = None;
    let mut blocked = false;
    for level in chain.visible_levels() {
        if blocked {
            break;
        }
        let Some(auth) = &level.upstream.record.auth else {
            continue;
        };
        match auth.sharing {
            SharingMode::Enforce => {
                result = Some(auth);
                blocked = true;
            }
            SharingMode::Inherit => result = Some(auth),
            SharingMode::Private => {
                if level.is_selected {
                    result = Some(auth);
                }
            }
        }
    }
    match result {
        Some(auth) => EffectiveAuth {
            plugin_type: auth.resolved_type().to_owned(),
            config: auth.config.clone(),
        },
        None => EffectiveAuth {
            plugin_type: plugin_gts::AUTH_NOOP.to_owned(),
            config: serde_json::Value::Object(Default::default()),
        },
    }
}

/// Effective CORS for the chain: origins/methods/expose are unioned across
/// visible levels; an `enforce` level fixes the set for everything below it.
#[must_use]
pub fn effective_cors(chain: &ResolvedChain) -> EffectiveCors {
    let mut out = EffectiveCors::default();
    let mut enforced = false;

    let mut apply = |out: &mut EffectiveCors, cfg: &CorsConfig, is_selected: bool| {
        match cfg.sharing {
            SharingMode::Enforce => {
                out.enabled |= cfg.enabled;
                out.allowed_origins = cfg.allowed_origins.clone();
                out.allowed_methods = cfg.allowed_methods.clone();
                out.expose_headers = cfg.expose_headers.clone();
                out.allow_credentials = cfg.allow_credentials;
                enforced = true;
            }
            SharingMode::Inherit => {
                out.enabled |= cfg.enabled;
                union_append(&mut out.allowed_origins, &cfg.allowed_origins);
                union_append(&mut out.allowed_methods, &cfg.allowed_methods);
                union_append(&mut out.expose_headers, &cfg.expose_headers);
                out.allow_credentials |= cfg.allow_credentials;
            }
            SharingMode::Private => {
                if is_selected {
                    // Own config replaces nothing inherited from below-private ancestors;
                    // only contributes when it is visible, i.e. the selected level.
                    out.enabled |= cfg.enabled;
                    union_append(&mut out.allowed_origins, &cfg.allowed_origins);
                    union_append(&mut out.allowed_methods, &cfg.allowed_methods);
                    union_append(&mut out.expose_headers, &cfg.expose_headers);
                    out.allow_credentials |= cfg.allow_credentials;
                }
            }
        }
    };

    for level in chain.visible_levels() {
        if let Some(cors) = &level.upstream.record.cors {
            apply(&mut out, cors, level.is_selected);
        }
    }
    if let Some(route) = &chain.matched_route {
        if let Some(cors) = &route.record.cors {
            apply(&mut out, cors, false);
        }
    }

    // `enforce` never actually stops unioning here (a descending private would
    // still union) — enforce is the strong guarantee and we keep it explicit.
    if enforced {
        // Nothing below an enforce level may add origins; find the first
        // enforce level's set and drop everything after it.
        out = effective_cors_enforced(chain);
    }
    out
}

/// Re-derive CORS stopping below the first `enforce` level (used by
/// [`effective_cors`] to guarantee enforce semantics).
fn effective_cors_enforced(chain: &ResolvedChain) -> EffectiveCors {
    let mut out = EffectiveCors::default();
    let mut locked = false;
    for level in chain.visible_levels() {
        if locked {
            break;
        }
        if let Some(cors) = &level.upstream.record.cors {
            match cors.sharing {
                SharingMode::Enforce => {
                    out.enabled = cors.enabled;
                    out.allowed_origins = cors.allowed_origins.clone();
                    out.allowed_methods = cors.allowed_methods.clone();
                    out.expose_headers = cors.expose_headers.clone();
                    out.allow_credentials = cors.allow_credentials;
                    locked = true;
                }
                SharingMode::Inherit => {
                    out.enabled |= cors.enabled;
                    union_append(&mut out.allowed_origins, &cors.allowed_origins);
                    union_append(&mut out.allowed_methods, &cors.allowed_methods);
                    union_append(&mut out.expose_headers, &cors.expose_headers);
                    out.allow_credentials |= cors.allow_credentials;
                }
                SharingMode::Private => {
                    if level.is_selected {
                        out.enabled |= cors.enabled;
                        union_append(&mut out.allowed_origins, &cors.allowed_origins);
                    }
                }
            }
        }
        if let Some(route) = &level.route {
            if let Some(cors) = &route.record.cors {
                match cors.sharing {
                    SharingMode::Enforce => {
                        out.enabled = cors.enabled;
                        out.allowed_origins = cors.allowed_origins.clone();
                        out.allowed_methods = cors.allowed_methods.clone();
                        out.expose_headers = cors.expose_headers.clone();
                        out.allow_credentials = cors.allow_credentials;
                        locked = true;
                    }
                    SharingMode::Inherit | SharingMode::Private => {
                        out.enabled |= cors.enabled;
                        union_append(&mut out.allowed_origins, &cors.allowed_origins);
                        union_append(&mut out.allowed_methods, &cors.allowed_methods);
                        union_append(&mut out.expose_headers, &cors.expose_headers);
                        out.allow_credentials |= cors.allow_credentials;
                    }
                }
            }
        }
    }
    out
}

/// Append `other` items that are not already present (order preserved).
fn union_append(dst: &mut Vec<String>, other: &[String]) {
    for item in other {
        if !dst.iter().any(|d| d.eq_ignore_ascii_case(item)) {
            dst.push(item.clone());
        }
    }
}

/// Effective plugins for the request: every visible upstream plugin binding
/// (root → descendant) followed by the winning route's bindings.
#[must_use]
pub fn effective_plugins(chain: &ResolvedChain) -> Vec<String> {
    chain.merged_plugin_configs()
}

/// Classify a plugin binding list into guard/transform refs by resolving the
/// GTS family prefix. Unknown families are ignored (validation catches them
/// at binding time).
#[must_use]
pub fn partition_plugins(items: &[String]) -> (Vec<String>, Vec<String>) {
    let mut guards = Vec::new();
    let mut transforms = Vec::new();
    for item in items {
        if item.starts_with(plugin_gts::GUARD) {
            guards.push(item.clone());
        } else if item.starts_with(plugin_gts::TRANSFORM) {
            transforms.push(item.clone());
        }
    }
    (guards, transforms)
}

/// Whether a plugin reference refers to a stored custom plugin (UUID-backed).
#[must_use]
pub fn plugin_uuid(plugin_ref: &str) -> Option<uuid::Uuid> {
    crate::domain::models::plugin_gts::is_uuid_backed(plugin_ref).then(|| {
        uuid::Uuid::parse_str(crate::domain::models::plugin_gts::instance_of(plugin_ref))
            .expect("uuid-backed plugin reference")
    })
}

/// Serialized plugin shape used when a `Plugin` is returned to clients.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginView {
    pub id: Option<uuid::Uuid>,
    pub plugin_type: String,
    pub name: String,
    pub config_schema: serde_json::Value,
}

impl From<&Plugin> for PluginView {
    fn from(p: &Plugin) -> Self {
        Self {
            id: p.id,
            plugin_type: p.plugin_type.clone(),
            name: p.name.clone(),
            config_schema: p.config_schema.clone(),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    fn upstream(auth_sharing: Option<SharingMode>, rate: Option<u64>) -> Stored<Upstream> {
        let mut u = Upstream::default();
        u.protocol = crate::domain::models::protocol_gts::HTTP.to_owned();
        if let Some(rate) = rate {
            u.rate_limit = Some(RateLimitConfig {
                sharing: SharingMode::Inherit,
                sustained: Some(crate::domain::models::SustainedRate {
                    rate,
                    window: crate::domain::models::Window::Minute,
                }),
                ..Default::default()
            });
        }
        if let Some(sh) = auth_sharing {
            u.auth = Some(AuthConfig {
                plugin_type: Some(plugin_gts::AUTH_NOOP.to_owned()),
                config: json!({}),
                sharing: sh,
            });
        }
        Stored::new(Uuid::nil(), u)
    }

    #[test]
    fn rates_are_min_across_windows() {
        let a = upstream(Some(SharingMode::Inherit), Some(600)); // 10/sec
        let b = upstream(Some(SharingMode::Inherit), Some(30)); // 0.5/sec
        let chain = ResolvedChain {
            selected: b.clone(),
            levels: vec![
                ChainLevel {
                    is_selected: false,
                    upstream: a,
                    route: None,
                },
                ChainLevel {
                    is_selected: true,
                    upstream: b,
                    route: None,
                },
            ],
            matched_route: None,
        };
        let eff = effective_rate(&chain).expect("rate");
        assert!(eff.refill_per_sec < 1.0, "0.5/sec should win");
        assert_eq!(eff.capacity, 30);
    }

    #[test]
    fn auth_closest_wins_unless_enforced() {
        let mut root = upstream(Some(SharingMode::Enforce), None);
        root.record.auth.as_mut().unwrap().plugin_type = Some(plugin_gts::AUTH_APIKEY.to_owned());
        let child = upstream(Some(SharingMode::Private), None);
        let chain = ResolvedChain {
            selected: child.clone(),
            levels: vec![
                ChainLevel {
                    is_selected: false,
                    upstream: root,
                    route: None,
                },
                ChainLevel {
                    is_selected: true,
                    upstream: child,
                    route: None,
                },
            ],
            matched_route: None,
        };
        let eff = effective_auth(&chain);
        assert_eq!(eff.plugin_type, plugin_gts::AUTH_APIKEY);
    }

    #[test]
    fn plugins_concat_ancestor_then_descendant() {
        let mut a = upstream(None, None);
        a.record.plugins = Some(PluginsConfig {
            sharing: SharingMode::Inherit,
            items: vec![plugin_gts::GUARD_REQUIRED_HEADERS.to_owned()],
        });
        let mut b = upstream(None, None);
        b.record.plugins = Some(PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![plugin_gts::TRANSFORM_REQUEST_ID.to_owned()],
        });
        let chain = ResolvedChain {
            selected: b.clone(),
            levels: vec![
                ChainLevel {
                    is_selected: false,
                    upstream: a,
                    route: None,
                },
                ChainLevel {
                    is_selected: true,
                    upstream: b,
                    route: None,
                },
            ],
            matched_route: None,
        };
        let plugins = effective_plugins(&chain);
        assert_eq!(
            plugins,
            vec![
                plugin_gts::GUARD_REQUIRED_HEADERS,
                plugin_gts::TRANSFORM_REQUEST_ID
            ]
        );
    }

    fn with_route(chain: &mut ResolvedChain, route: Stored<Route>) {
        chain.matched_route = Some(route);
    }

    #[test]
    fn route_rate_participates_in_min() {
        let mut chain = ResolvedChain {
            selected: upstream(None, None),
            levels: Vec::new(),
            matched_route: None,
        };
        // The selected upstream allows 600/min (10/sec, capacity 600).
        let mut selected = upstream(None, None);
        selected.record.rate_limit = Some(RateLimitConfig {
            sharing: SharingMode::Inherit,
            sustained: Some(crate::domain::models::SustainedRate {
                rate: 600,
                window: crate::domain::models::Window::Minute,
            }),
            ..Default::default()
        });
        chain.levels.push(ChainLevel {
            is_selected: true,
            upstream: selected,
            route: None,
        });
        chain.selected = chain.levels[0].upstream.clone();

        // No route -> upstream rate alone (10/sec, 600 burst).
        let eff = effective_rate(&chain).expect("rate");
        assert_eq!(eff.refill_per_sec, 600.0 / 60.0);
        assert_eq!(eff.capacity, 600);

        // The winning route's tighter rate (120/min => 2/sec) wins the min.
        let mut route = Route::default();
        route.rate_limit = Some(RateLimitConfig {
            sharing: SharingMode::Inherit,
            sustained: Some(crate::domain::models::SustainedRate {
                rate: 120,
                window: crate::domain::models::Window::Minute,
            }),
            ..Default::default()
        });
        route.upstream_id = chain.selected.record.id.unwrap_or(Uuid::nil());
        chain.matched_route = Some(Stored::new(Uuid::nil(), route));
        let eff = effective_rate(&chain).expect("rate");
        assert_eq!(eff.refill_per_sec, 2.0, "route refill 120/min = 2/sec");
        assert_eq!(eff.capacity, 120, "route capacity bounds the burst");
    }

    #[test]
    fn cors_unions_visible_levels_and_enforce_fixes_the_set() {
        fn cors_with(sharing: SharingMode, origins: &[&str], methods: &[&str]) -> Stored<Upstream> {
            let mut u = upstream(None, None);
            u.record.cors = Some(CorsConfig {
                sharing,
                enabled: true,
                allowed_origins: origins.iter().map(|s| s.to_string()).collect(),
                allowed_methods: methods.iter().map(|s| s.to_string()).collect(),
                expose_headers: Vec::new(),
                allow_credentials: false,
            });
            u
        }

        let ancestor = cors_with(SharingMode::Inherit, &["https://a.example"], &["GET"]);
        let child = cors_with(SharingMode::Inherit, &["https://b.example"], &["POST"]);
        let chain = ResolvedChain {
            selected: child.clone(),
            levels: vec![
                ChainLevel {
                    is_selected: false,
                    upstream: ancestor,
                    route: None,
                },
                ChainLevel {
                    is_selected: true,
                    upstream: child,
                    route: None,
                },
            ],
            matched_route: None,
        };
        let eff = effective_cors(&chain);
        assert!(eff.enabled);
        assert!(
            eff.allowed_origins.iter().any(|o| o == "https://a.example")
                && eff.allowed_origins.iter().any(|o| o == "https://b.example"),
            "inherit origins union, got {:?}",
            eff.allowed_origins
        );
        assert!(eff.allows_method("GET") && eff.allows_method("POST"));
        assert!(eff.allows_origin("https://b.example"));
        assert!(!eff.allows_origin("https://evil.example"));

        // An enforce level fixes the set for everything below it.
        let enforced_root = cors_with(SharingMode::Enforce, &["https://fixed.example"], &["GET"]);
        let private_child = cors_with(SharingMode::Private, &["https://child.example"], &["POST"]);
        let chain = ResolvedChain {
            selected: private_child.clone(),
            levels: vec![
                ChainLevel {
                    is_selected: false,
                    upstream: enforced_root,
                    route: None,
                },
                ChainLevel {
                    is_selected: true,
                    upstream: private_child,
                    route: None,
                },
            ],
            matched_route: None,
        };
        let eff = effective_cors(&chain);
        assert!(
            eff.allows_origin("https://fixed.example")
                && !eff.allows_origin("https://child.example"),
            "enforce fixes origins regardless of descendants"
        );
    }

    #[test]
    fn private_ancestor_bindings_are_excluded_and_route_bindings_appended_last() {
        let mut ancestor = upstream(None, None);
        ancestor.record.plugins = Some(PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![plugin_gts::TRANSFORM_REQUEST_ID.to_owned()],
        });
        let mut child = upstream(None, None);
        child.record.plugins = Some(PluginsConfig {
            sharing: SharingMode::Inherit,
            items: vec![plugin_gts::GUARD_REQUIRED_HEADERS.to_owned()],
        });
        let mut chain = ResolvedChain {
            selected: child.clone(),
            levels: vec![
                ChainLevel {
                    is_selected: false,
                    upstream: ancestor,
                    route: None,
                },
                ChainLevel {
                    is_selected: true,
                    upstream: child,
                    route: None,
                },
            ],
            matched_route: None,
        };
        // A private binding on a non-selected ancestor is invisible.
        let plugins = effective_plugins(&chain);
        assert_eq!(plugins, vec![plugin_gts::GUARD_REQUIRED_HEADERS.to_owned()]);

        // The winning route's bindings are appended after the upstream ones.
        let mut route = Route::default();
        route.plugins = Some(PluginsConfig {
            sharing: SharingMode::Inherit,
            items: vec![plugin_gts::TRANSFORM_REQUEST_ID.to_owned()],
        });
        route.upstream_id = Uuid::nil();
        chain.matched_route = Some(Stored::new(Uuid::nil(), route));
        let plugins = effective_plugins(&chain);
        assert_eq!(
            plugins,
            vec![
                plugin_gts::GUARD_REQUIRED_HEADERS.to_owned(),
                plugin_gts::TRANSFORM_REQUEST_ID.to_owned()
            ],
            "upstream guard first, route transform appended last"
        );
    }

    #[test]
    fn partition_plugins_splits_guard_and_transform_families() {
        let (guards, transforms) = partition_plugins(&[
            plugin_gts::GUARD_REQUIRED_HEADERS.to_owned(),
            plugin_gts::TRANSFORM_REQUEST_ID.to_owned(),
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1".to_owned(),
        ]);
        assert_eq!(guards, vec![plugin_gts::GUARD_REQUIRED_HEADERS.to_owned()]);
        assert_eq!(transforms.len(), 2);
    }
}
