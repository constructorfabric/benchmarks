//! The resolved-configuration cache
//! (`cpt-cf-oagw-dod-resolved-config-cache-lookup`,
//! `cpt-cf-oagw-dod-resolved-config-cache-invalidation`,
//! `cpt-cf-oagw-adr-data-plane-caching`, `cpt-cf-oagw-adr-state-management`).
//!
//! A bounded, in-process, lazily-populated cache of [`super::resolve::ResolvedPlan`]
//! values, keyed on `(tenant_id, normalized_alias, method, path)`
//! (`cpt-cf-oagw-algo-resolved-config-cache-lookup`). There is no Redis L2
//! layer here (out of scope, Overview override 5) — this is the single L1
//! layer, sized similarly to ADR-0005's control-plane L1 (10,000 entries),
//! with FIFO eviction once the bound is reached (a plain `dashmap::DashMap`
//! has no built-in ordering, so the eviction order is tracked separately via
//! a `parking_lot::Mutex`-guarded queue rather than a true recency-based
//! LRU).
//!
//! Invalidation is all-or-nothing
//! (`cpt-cf-oagw-algo-resolved-config-cache-invalidate`): a single
//! control-plane write can affect shadowed and inherited entries belonging
//! to other tenants, so there is no way to scope invalidation to specific
//! keys; every write empties the whole cache.

use std::collections::VecDeque;

use dashmap::DashMap;
use parking_lot::Mutex;
use uuid::Uuid;

use super::resolve::ResolvedPlan;

/// Upper bound on the number of cached resolved plans, mirroring
/// ADR-0005's control-plane L1 capacity.
const CACHE_CAPACITY: usize = 10_000;

/// The resolved-configuration cache key: the calling `tenant_id`, the
/// normalized alias, the inbound method, and the inbound path
/// (`cpt-cf-oagw-algo-resolved-config-cache-lookup`). Two requests
/// differing in any one of these fields land in distinct cache entries.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResolvedConfigCacheKey {
    pub tenant_id: Uuid,
    pub alias: String,
    pub method: String,
    pub path: String,
}

/// A bounded, in-process cache of resolved proxy-request plans
/// (`cpt-cf-oagw-dod-resolved-config-cache-lookup`).
#[derive(Debug, Default)]
pub struct ResolvedConfigCache {
    entries: DashMap<ResolvedConfigCacheKey, ResolvedPlan>,
    insertion_order: Mutex<VecDeque<ResolvedConfigCacheKey>>,
}

impl ResolvedConfigCache {
    /// Builds an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Looks up `key`, returning a clone of the cached plan on a hit
    /// (`cpt-cf-oagw-algo-resolved-config-cache-lookup`).
    #[must_use]
    pub fn get(&self, key: &ResolvedConfigCacheKey) -> Option<ResolvedPlan> {
        self.entries.get(key).map(|entry| entry.value().clone())
    }

    /// Populates `key` with `plan`, lazily, evicting the oldest entry once
    /// the cache exceeds [`CACHE_CAPACITY`]
    /// (`cpt-cf-oagw-state-resolved-config-cache-entry`).
    pub fn insert(&self, key: ResolvedConfigCacheKey, plan: ResolvedPlan) {
        let already_present = self.entries.insert(key.clone(), plan).is_some();
        if already_present {
            return;
        }
        let mut order = self.insertion_order.lock();
        order.push_back(key);
        if order.len() > CACHE_CAPACITY
            && let Some(oldest) = order.pop_front()
        {
            self.entries.remove(&oldest);
        }
    }

    /// Empties every cached entry
    /// (`cpt-cf-oagw-algo-resolved-config-cache-invalidate`): called on any
    /// control-plane write to an upstream, route, or plugin record, since a
    /// single write's effect on shadowed and inherited entries cannot be
    /// scoped to specific keys.
    // @cpt-begin:cpt-cf-oagw-algo-resolved-config-cache-invalidate:p1:inst-cache-invalidate-fn-01
    pub fn invalidate_all(&self) {
        self.entries.clear();
        self.insertion_order.lock().clear();
    }
    // @cpt-end:cpt-cf-oagw-algo-resolved-config-cache-invalidate:p1:inst-cache-invalidate-fn-01

    /// Number of currently cached entries. Exposed for tests and
    /// diagnostics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when the cache holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{ResolvedConfigCache, ResolvedConfigCacheKey};
    use crate::domain::model::{Endpoint, Protocol, Route, Scheme, ServerConfig, Upstream};
    use crate::domain::resolve::{HeaderPlan, RequestHeaderPlan, ResolvedPlan, ResponseHeaderPlan};
    use uuid::Uuid;

    fn sample_key(tenant_id: Uuid) -> ResolvedConfigCacheKey {
        ResolvedConfigCacheKey {
            tenant_id,
            alias: "cache.example.com".to_owned(),
            method: "GET".to_owned(),
            path: "/v1/widgets".to_owned(),
        }
    }

    fn sample_plan() -> ResolvedPlan {
        let upstream = Upstream {
            id: Uuid::new_v4(),
            enabled: true,
            alias: "cache.example.com".to_owned(),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: "cache.example.com".to_owned(),
                    port: Some(443),
                }],
            },
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        };
        let route = Route {
            id: Uuid::new_v4(),
            upstream_id: upstream.id,
            tags: Vec::new(),
            match_config: crate::domain::model::MatchConfig {
                http: Some(crate::domain::model::HttpMatch {
                    methods: vec![crate::domain::model::RouteMethod::Get],
                    path: "/v1/widgets".to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            enabled: true,
            priority: 0,
        };
        let endpoints = upstream.server.endpoints.clone();
        ResolvedPlan {
            upstream,
            owning_tenant_id: Uuid::new_v4(),
            route,
            effective_auth: None,
            effective_headers: None,
            effective_rate_limit: None,
            effective_plugins: Vec::new(),
            effective_cors: None,
            effective_tags: Vec::new(),
            header_plan: HeaderPlan {
                request: RequestHeaderPlan::default(),
                response: ResponseHeaderPlan::default(),
            },
            endpoints,
        }
    }

    #[test]
    fn a_miss_returns_none_and_a_subsequent_insert_is_a_hit() {
        let cache = ResolvedConfigCache::new();
        let key = sample_key(Uuid::new_v4());
        assert!(cache.get(&key).is_none());

        let plan = sample_plan();
        cache.insert(key.clone(), plan.clone());
        let cached = cache.get(&key).expect("must be a hit after insert");
        assert_eq!(cached.upstream.id, plan.upstream.id);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn distinct_keys_populate_distinct_entries() {
        let cache = ResolvedConfigCache::new();
        let tenant_a = sample_key(Uuid::new_v4());
        let tenant_b = sample_key(Uuid::new_v4());

        cache.insert(tenant_a, sample_plan());
        cache.insert(tenant_b, sample_plan());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn invalidate_all_empties_every_entry() {
        let cache = ResolvedConfigCache::new();
        cache.insert(sample_key(Uuid::new_v4()), sample_plan());
        cache.insert(sample_key(Uuid::new_v4()), sample_plan());
        assert_eq!(cache.len(), 2);

        cache.invalidate_all();
        assert!(cache.is_empty());
    }
}
