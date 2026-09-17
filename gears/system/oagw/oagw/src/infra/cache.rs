//! L1 configuration cache (ADR `0005-data-plane-caching`).
//!
//! Upstream and route configuration is read on every proxied request and
//! changes only on a management write, so the data plane keeps an in-memory
//! copy in front of the L2 repository:
//!
//! * `upstream:{tenant_id}:{alias}` → [`Upstream`]
//! * `routes:{upstream_id}` → `[`Route`]
//!
//! **Invalidation** is generation-based: the management services
//! ([`crate::domain::services::UpstreamService`],
//! [`crate::domain::services::RouteService`]) bump a shared counter on every
//! write, and an entry recorded under an older generation is treated as a
//! miss. A short TTL is layered on top as defence in depth against a missed
//! bump. `pingora-memory-cache` hashes keys to `u64` and does **not** resolve
//! collisions, so every entry carries its own key and is discarded on a
//! mismatch (the ADR 0008 `CachedToken` pattern).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pingora_memory_cache::MemoryCache;
use uuid::Uuid;

use crate::domain::models::{Route, Upstream};

/// A cached value, carrying the key it was stored under and the generation it
/// was built from.
#[derive(Debug, Clone)]
struct Entry<T> {
    key: String,
    generation: u64,
    value: T,
}

/// In-memory L1 configuration cache.
pub struct ConfigCache {
    generation: Arc<AtomicU64>,
    upstreams: MemoryCache<String, Entry<Upstream>>,
    routes: MemoryCache<String, Entry<Arc<Vec<Route>>>>,
    ttl: Duration,
}

impl std::fmt::Debug for ConfigCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigCache")
            .field("generation", &self.generation.load(Ordering::Relaxed))
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl ConfigCache {
    /// Builds a cache with the given entry TTL and capacity.
    #[must_use]
    pub fn new(ttl: Duration, capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            generation: Arc::new(AtomicU64::new(1)),
            upstreams: MemoryCache::new(capacity.max(1)),
            routes: MemoryCache::new(capacity.max(1)),
            ttl,
        })
    }

    /// Invalidates every cached entry (management write).
    pub fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn current(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Cached upstream for `(tenant, alias)`, when still valid.
    #[must_use]
    pub fn get_upstream(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream> {
        let key = upstream_key(tenant_id, alias);
        let (entry, _status) = self.upstreams.get(&key);
        let entry = entry?;
        if entry.key != key || entry.generation != self.current() {
            return None;
        }
        Some(entry.value)
    }

    /// Caches an upstream.
    pub fn put_upstream(&self, tenant_id: Uuid, alias: &str, value: &Upstream) {
        let key = upstream_key(tenant_id, alias);
        let generation = self.current();
        self.upstreams.put(
            &key.clone(),
            Entry {
                key,
                generation,
                value: value.clone(),
            },
            Some(self.ttl),
        );
    }

    /// Cached route list for one upstream, when still valid.
    #[must_use]
    pub fn get_routes(&self, upstream_id: Uuid) -> Option<Arc<Vec<Route>>> {
        let key = routes_key(upstream_id);
        let (entry, _status) = self.routes.get(&key);
        let entry = entry?;
        if entry.key != key || entry.generation != self.current() {
            return None;
        }
        Some(entry.value)
    }

    /// Caches the route list of one upstream.
    pub fn put_routes(&self, upstream_id: Uuid, value: &[Route]) {
        let key = routes_key(upstream_id);
        let generation = self.current();
        self.routes.put(
            &key.clone(),
            Entry {
                key,
                generation,
                value: Arc::new(value.to_vec()),
            },
            Some(self.ttl),
        );
    }
}

/// L1 key of an upstream (ADR 0005 "Cache Keys").
fn upstream_key(tenant_id: Uuid, alias: &str) -> String {
    format!("upstream:{tenant_id}:{alias}")
}

/// L1 key of the route collection of one upstream.
fn routes_key(upstream_id: Uuid) -> String {
    format!("routes:{upstream_id}")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::models::{Endpoint, EndpointScheme, UpstreamSpec};

    fn upstream(alias: &str, host: &str) -> Upstream {
        let spec = UpstreamSpec {
            server: crate::domain::models::ServerConfig {
                endpoints: vec![Endpoint::new(EndpointScheme::Https, host, 443)],
            },
            ..UpstreamSpec::default()
        };
        Upstream::from_spec(spec, Uuid::new_v4(), Uuid::new_v4(), alias.to_owned())
    }

    fn route(upstream_id: Uuid) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            upstream_id,
            tags: Vec::new(),
            match_rules: crate::domain::models::MatchConfig::default(),
            plugins: Default::default(),
            rate_limit: None,
        }
    }

    #[test]
    fn round_trips_a_cached_upstream() {
        let cache = ConfigCache::new(Duration::from_secs(1), 8);
        let tenant = Uuid::new_v4();
        let value = upstream("api.openai.com", "api.openai.com");
        cache.put_upstream(tenant, "api.openai.com", &value);
        assert_eq!(
            cache.get_upstream(tenant, "api.openai.com").expect("hit").id,
            value.id
        );
        assert!(cache.get_upstream(Uuid::new_v4(), "api.openai.com").is_none());
    }

    #[test]
    fn a_generation_bump_invalidates_every_entry() {
        let cache = ConfigCache::new(Duration::from_secs(60), 8);
        let tenant = Uuid::new_v4();
        let upstream = upstream("a.example", "a.example");
        let upstream_id = upstream.id;
        cache.put_upstream(tenant, "a.example", &upstream);
        let routes = vec![route(upstream_id)];
        cache.put_routes(upstream_id, &routes);

        cache.invalidate();

        assert!(cache.get_upstream(tenant, "a.example").is_none());
        assert!(cache.get_routes(upstream_id).is_none());
    }

    #[test]
    fn derived_caches_share_the_generation() {
        // The management services and the data plane hold the *same* cache, so
        // one bump invalidates every holder.
        let cache = ConfigCache::new(Duration::from_secs(60), 8);
        let shared = Arc::clone(&cache);
        let tenant = Uuid::new_v4();
        let upstream = upstream("a.example", "a.example");
        cache.put_upstream(tenant, "a.example", &upstream);

        shared.invalidate();

        assert!(cache.get_upstream(tenant, "a.example").is_none());
    }

    #[test]
    fn route_lists_are_shared_and_typed() {
        let cache = ConfigCache::new(Duration::from_secs(60), 8);
        let upstream_id = Uuid::new_v4();
        let routes = vec![route(upstream_id), route(upstream_id)];
        cache.put_routes(upstream_id, &routes);
        let hit = cache.get_routes(upstream_id).expect("hit");
        assert_eq!(hit.len(), 2);
    }

    #[test]
    fn keys_follow_the_adr_naming() {
        let tenant = Uuid::nil();
        assert_eq!(
            upstream_key(tenant, "api.openai.com"),
            format!("upstream:{tenant}:api.openai.com")
        );
        assert_eq!(routes_key(tenant), format!("routes:{tenant}"));
    }
}
