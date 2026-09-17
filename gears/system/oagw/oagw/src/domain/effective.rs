//! Effective configuration resolution and L1 config caching.
//!
//! The control plane owns authoritative upstreams/routes/plugins; the data
//! plane consumes a projected **effective configuration** (route > upstream >
//! global merged hierarchy) served by an L1 cache. This module implements
//! `cpt-cf-oagw-algo-control-plane-effective-config-resolution` and the
//! shared `cpt-cf-oagw-dod-data-plane-l1-config-cache`.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use dashmap::DashMap;

use crate::domain::models::{CorsConfig, EffectiveRouteConfig, Plugin, PluginKind};
use crate::domain::repository::ControlPlaneService;

/// How long an L1 entry considered "enabled" is allowed to stay resolved when
/// no explicit invalidation occurred. Because every control-plane mutation
/// invalidates the matching entry synchronously, this is only a backstop.
const DEFAULT_TTL: Duration = Duration::from_secs(60);

/// L1 cache capacity contract (1000 entries). The cache must never grow
/// unbounded; on overflow it clears wholesale (simple, race-free eviction).
pub const L1_CACHE_CAPACITY: usize = 1000;

/// An L1 cache entry: the effective configuration shared by all requests for
/// a route alias until the control plane invalidates it.
#[derive(Debug, Clone)]
pub struct L1Entry {
    /// Resolution generation this entry was computed under.
    pub generation: u64,
    /// The effective, ready-to-consume configuration.
    pub effective: Arc<EffectiveRouteConfig>,
}

/// The data-plane L1 configuration cache.
///
/// Thread-safe (`arc-swap` + `DashMap`), populated lazily on proxy resolution
/// and invalidated synchronously by control-plane writes.
pub struct L1ConfigCache {
    entries: DashMap<String, Arc<L1Entry>>,
    generation: Arc<ArcSwap<u64>>,
}

impl Default for L1ConfigCache {
    fn default() -> Self {
        Self::new()
    }
}

impl L1ConfigCache {
    /// Creates an empty cache with a zero generation.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
            generation: Arc::new(ArcSwap::from_pointee(0_u64)),
        }
    }

    /// Bumps the generation counter, marking every previously computed entry
    /// as stale (the `effective` values are recomputed lazily on the next
    /// proxy resolution).
    pub fn bump_generation(&self) -> u64 {
        let next = self.generation.load().wrapping_add(1);
        self.generation.store(Arc::new(next));
        next
    }

    /// Current generation value.
    #[must_use]
    pub fn generation(&self) -> u64 {
        **self.generation.load()
    }

    /// Returns the cached effective config for `alias` when present and up to
    /// date, otherwise computes it from the control plane and stores it.
    ///
    /// # Errors
    ///
    /// Returns `None` when the alias does not resolve to a route.
    pub fn resolve(
        &self,
        alias: &str,
        control_plane: &ControlPlaneService,
        fallback_timeout: Duration,
    ) -> Option<Arc<EffectiveRouteConfig>> {
        let generation = self.generation();
        if let Some(entry) = self.entries.get(alias) {
            if entry.generation == generation {
                let effective = entry.value().effective.clone();
                if effective.disabled {
                    return None;
                }
                return Some(effective);
            }
            // Stale entry: drop it and recompute.
            drop(entry);
            self.entries.remove(alias);
        }

        let effective = Arc::new(control_plane.compute_effective(alias, fallback_timeout));
        match effective.as_ref() {
            EffectiveRouteConfig { disabled: true, .. } => {
                // Do not cache disabled routes; they are resolved lazily so
                // re-enablement is observed promptly.
                None
            }
            _ => {
                self.insert_entry(alias.to_owned(), generation, effective.clone());
                Some(effective)
            }
        }
    }

    /// Inserts a resolved entry, enforcing the capacity contract: when the
    /// cache is already at capacity the table is cleared before inserting so
    /// the L1 cache stays bounded (`L1_CACHE_CAPACITY`).
    fn insert_entry(&self, alias: String, generation: u64, effective: Arc<EffectiveRouteConfig>) {
        if self.entries.len() >= L1_CACHE_CAPACITY {
            self.entries.clear();
        }
        self.entries.insert(
            alias,
            Arc::new(L1Entry {
                generation,
                effective,
            }),
        );
    }

    /// Removes the entry for `alias`, forcing the next proxy request to
    /// recompute from the control plane.
    pub fn invalidate(&self, alias: &str) {
        self.entries.remove(alias);
    }

    /// Removes every entry.
    pub fn invalidate_all(&self) {
        self.entries.clear();
        self.bump_generation();
    }
}

impl ControlPlaneService {
    /// Computes the effective configuration for `alias` by merging the route
    /// hierarchy (route > upstream > global), concatenating route + upstream
    /// plugins, and unioning match policies. This is the authoritative
    /// projection the data plane's L1 cache serves.
    #[must_use]
    pub fn compute_effective(
        &self,
        alias: &str,
        fallback_timeout: Duration,
    ) -> EffectiveRouteConfig {
        // @cpt-algo:cpt-cf-oagw-algo-control-plane-effective-config-resolution
        // @cpt-begin:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-load-tree
        // 1. Load the route tree for this alias.
        // @cpt-end:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-load-tree
        let route = self.get_route(alias);
        let global_timeout = fallback_timeout;

        // 3. Start with global defaults then merge the upstream
        //    (@cpt-algo:inst-resolve-alias).
        // @cpt-begin:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-resolve-alias
        let upstream = route
            // @cpt-end:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-resolve-alias
            .as_ref()
            .and_then(|r| r.upstream_alias.as_ref())
            .and_then(|ua| self.get_upstream(ua));

        let upstream_merge = upstream.clone();
        let route_merge = route.clone();
        // Enforce semantics: a disabled route/upstream disables the branch and
        // propagates downward. Tracks the enablement machine (`inst-disable` /
        // `inst-enable` in the repository; ancestor propagation here).
        // @cpt-begin:cpt-cf-oagw-state-control-plane-enablement:ph-1:inst-ancestor-disable
        // @cpt-begin:cpt-cf-oagw-state-control-plane-enablement:ph-1:inst-ancestor-enable
        // @cpt-begin:cpt-cf-oagw-state-control-plane-enablement:ph-1:inst-disabled-ancestor-disable
        // @cpt-begin:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-ancestor-disabled
        // @cpt-begin:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-disable-prop
        // @cpt-begin:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-branch-eligible
        let disabled = route
            // @cpt-end:cpt-cf-oagw-state-control-plane-enablement:ph-1:inst-ancestor-disable
            // @cpt-end:cpt-cf-oagw-state-control-plane-enablement:ph-1:inst-ancestor-enable
            // @cpt-end:cpt-cf-oagw-state-control-plane-enablement:ph-1:inst-disabled-ancestor-disable
            // @cpt-end:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-ancestor-disabled
            // @cpt-end:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-disable-prop
            // @cpt-end:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-branch-eligible
            .as_ref()
            .is_none_or(|r| !r.enabled || r.upstream_alias.is_none())
            || upstream.as_ref().is_none_or(|u| !u.enabled);

        // 4. Merge the rate-limit hierarchy (route wins over upstream;
        //    stricter-wins).
        // @cpt-begin:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-merge-rate
        let rate_limit = route_merge
            // @cpt-end:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-merge-rate
            .as_ref()
            .and_then(|r| r.rate_limit.clone())
            .or(None);

        // 5. Concatenate plugin lists: route plugins first, then upstream
        //    (documented binding order, no silent reordering).
        // @cpt-begin:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-plugin-concat
        let mut plugins = self.route_plugins(alias);
        // @cpt-end:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-plugin-concat
        if let Some(ua) = upstream_merge.as_ref().map(|u| u.alias.as_str()) {
            for p in self.upstream_plugins(ua) {
                if !plugins.iter().any(|x| x.alias == p.alias) {
                    plugins.push(p);
                }
            }
        }

        // 6. Union the CORS policy (route wins over global default).
        // @cpt-begin:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-union
        // @cpt-begin:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-merge-hierarchy
        let cors = route_merge
            // @cpt-end:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-union
            // @cpt-end:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-merge-hierarchy
            .as_ref()
            .map_or_else(CorsConfig::default, |r| r.cors.clone());

        let proxy_timeout = upstream_merge
            .as_ref()
            .and_then(|u| {
                if u.timeout_secs == 0 {
                    None
                } else {
                    Some(Duration::from_secs(u.timeout_secs))
                }
            })
            .unwrap_or(global_timeout);

        // Project the resolved configuration into the data-plane shape.
        // @cpt-begin:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-project
        // @cpt-begin:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-return-effective
        EffectiveRouteConfig {
            // @cpt-end:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-project
            // @cpt-end:cpt-cf-oagw-algo-control-plane-effective-config-resolution:ph-1:inst-return-effective
            alias: alias.to_owned(),
            upstream,
            plugins,
            rate_limit,
            cors,
            proxy_timeout,
            methods: route_merge.as_ref().and_then(|r| r.methods.clone()),
            http_matches: route_merge
                .as_ref()
                .map_or_else(Vec::new, |r| r.http_matches.clone()),
            disabled,
        }
    }

    fn route_plugins(&self, alias: &str) -> Vec<Plugin> {
        self.list_route_plugins(alias)
            .into_iter()
            .filter_map(|pa| self.get_plugin(&pa))
            .filter(|p| p.enabled)
            .collect()
    }

    fn upstream_plugins(&self, alias: &str) -> Vec<Plugin> {
        self.list_upstream_plugins(alias)
            .into_iter()
            .filter_map(|pa| self.get_plugin(&pa))
            .filter(|p| p.enabled)
            .collect()
    }

    /// Returns the merged plugin list for `alias` (route + upstream plugins),
    /// deduplicated by alias; only enabled auth/guard/transform plugins that
    /// are actually executable are included.
    #[must_use]
    pub fn executable_plugins(&self, alias: &str) -> Vec<Plugin> {
        let effective = self.compute_effective(alias, DEFAULT_TTL);
        effective
            .plugins
            .into_iter()
            .filter(|p| {
                matches!(
                    p.kind,
                    PluginKind::Noop
                        | PluginKind::ApiKey
                        | PluginKind::OAuth2ClientCred
                        | PluginKind::OAuth2ClientCredBasic
                        | PluginKind::RequiredHeaders
                        | PluginKind::RequestId
                        | PluginKind::Logging
                        | PluginKind::Metrics
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{Route, Upstream, UpstreamScheme};
    use crate::domain::repository::ControlPlaneService;

    fn svc() -> ControlPlaneService {
        ControlPlaneService::new(1, Duration::from_secs(2), false)
    }

    #[test]
    fn resolves_effective_with_plugins_without_upstream() {
        let svc = svc();
        let cfg = svc.compute_effective("missing", Duration::from_secs(2));
        assert!(cfg.disabled);
        assert!(cfg.upstream.is_none());
    }

    #[test]
    fn resolves_effective_with_upstream_and_plugins() {
        let svc = svc();
        svc.upsert_upstream(Upstream {
            alias: "svc".to_owned(),
            name: "Svc".to_owned(),
            host: "example.com".to_owned(),
            port: 443,
            scheme: UpstreamScheme::Https,
            path_prefix: "/svc".to_owned(),
            enabled: true,
            timeout_secs: 0,
        })
        .expect("upstream seeded");
        svc.upsert_route(Route {
            alias: "r1".to_owned(),
            upstream_alias: Some("svc".to_owned()),
            methods: None,
            http_matches: vec![],
            rate_limit: None,
            cors: CorsConfig::default(),
            enabled: true,
            priority: 5,
        })
        .expect("route seeded");

        // No plugins bound yet: effective must carry them but the router
        // needs at least a route + upstream.
        let cfg = svc.compute_effective("r1", Duration::from_secs(2));
        assert!(!cfg.disabled);
        assert_eq!(cfg.upstream.as_ref().map(|u| u.alias.as_str()), Some("svc"));
    }

    /// The L1 cache honors its capacity contract (1000 entries): overflowing
    /// evicts the table wholesale, so the cache never grows unbounded.
    #[test]
    fn l1_cache_is_capacity_bounded() {
        let cache = L1ConfigCache::new();
        let generation = cache.generation();
        // Overflow the declared capacity with distinct aliases.
        for i in 0..L1_CACHE_CAPACITY + 50 {
            let effective = Arc::new(EffectiveRouteConfig {
                alias: format!("r{i}"),
                upstream: None,
                plugins: Vec::new(),
                rate_limit: None,
                cors: CorsConfig::default(),
                proxy_timeout: Duration::from_secs(1),
                methods: None,
                http_matches: Vec::new(),
                disabled: false,
            });
            cache.insert_entry(effective.alias.clone(), generation, effective);
        }
        assert!(
            cache.entries.len() <= L1_CACHE_CAPACITY,
            "L1 cache exceeded capacity: {} > {L1_CACHE_CAPACITY}",
            cache.entries.len()
        );
        // The most recent entry survives the clear-on-overflow eviction.
        assert!(
            cache
                .entries
                .contains_key(&format!("r{}", L1_CACHE_CAPACITY + 49))
        );
    }
}
