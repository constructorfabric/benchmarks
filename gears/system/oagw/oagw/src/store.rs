//! Shared in-process configuration-store abstraction.
//!
//! One store, shared by the Control Plane (write path: this feature's own
//! bootstrap seeding of [`crate::config::OagwConfig`], and every later
//! Control-Plane write from 2.2-2.4) and the Data Plane (read path: 2.5).
//! Exactly one logical writer role (Control Plane) may write; the Data
//! Plane never writes through this path. The store is in-process and
//! single-instance for this round -- no cross-instance/Redis
//! synchronization.
//!
//! See `docs/features/gear-foundation.md` §3 "Shared Configuration-Store
//! Write (Control Plane Path)" (`cpt-cf-oagw-algo-config-store-write`) and
//! "Shared Configuration-Store Read (Data Plane Path)"
//! (`cpt-cf-oagw-algo-config-store-read`), and §5
//! `cpt-cf-oagw-dod-config-store-contract`.

use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::model::plugin::Plugin;
use crate::model::route::Route;
use crate::model::upstream::Upstream;

/// Single in-process configuration-store handle.
///
/// `oagw_config` is read-mostly and control-plane-written at most once per
/// process lifetime today (bootstrap seeding); [`ArcSwap`] gives atomic,
/// torn-read-free, read-after-write-visible swaps for that access pattern.
/// The `upstreams`/`routes`/`plugins` maps are the homes later
/// Control-Plane CRUD features (2.2-2.4) write into and the Data Plane
/// (2.5) reads from; [`DashMap`] gives fine-grained concurrent read/write
/// access per-key without a single global lock.
// @cpt-dod:cpt-cf-oagw-dod-config-store-contract:p1
#[derive(Debug)]
pub struct ConfigStore {
    oagw_config: ArcSwap<OagwConfig>,
    upstreams: DashMap<Uuid, Arc<Upstream>>,
    routes: DashMap<Uuid, Arc<Route>>,
    plugins: DashMap<Uuid, Arc<Plugin>>,
}

impl ConfigStore {
    /// Construct the store, seeded with the gear's resolved
    /// [`OagwConfig`] (`cpt-cf-oagw-flow-gear-bootstrap` step
    /// `inst-gear-bootstrap-07`).
    #[must_use]
    pub fn new(config: OagwConfig) -> Self {
        Self {
            oagw_config: ArcSwap::from_pointee(config),
            upstreams: DashMap::new(),
            routes: DashMap::new(),
            plugins: DashMap::new(),
        }
    }

    /// Control-Plane write path: atomically replace the current
    /// [`OagwConfig`] value. No partial ("torn") value is ever observable
    /// by a concurrent reader, and the new value is immediately visible to
    /// every subsequent [`ConfigStore::config`] call in this process.
    // @cpt-algo:cpt-cf-oagw-algo-config-store-write:p2
    // @cpt-begin:cpt-cf-oagw-algo-config-store-write:p2:inst-config-store-write-01
    // @cpt-begin:cpt-cf-oagw-algo-config-store-write:p2:inst-config-store-write-02
    // @cpt-begin:cpt-cf-oagw-algo-config-store-write:p2:inst-config-store-write-03
    pub fn set_config(&self, config: OagwConfig) {
        self.oagw_config.store(Arc::new(config));
    }
    // @cpt-end:cpt-cf-oagw-algo-config-store-write:p2:inst-config-store-write-03
    // @cpt-end:cpt-cf-oagw-algo-config-store-write:p2:inst-config-store-write-02
    // @cpt-end:cpt-cf-oagw-algo-config-store-write:p2:inst-config-store-write-01

    /// Data-Plane read path: non-exclusive access to the current
    /// [`OagwConfig`] value, without blocking concurrent readers or the
    /// writer.
    // @cpt-algo:cpt-cf-oagw-algo-config-store-read:p2
    // @cpt-begin:cpt-cf-oagw-algo-config-store-read:p2:inst-config-store-read-01
    // @cpt-begin:cpt-cf-oagw-algo-config-store-read:p2:inst-config-store-read-02
    // @cpt-begin:cpt-cf-oagw-algo-config-store-read:p2:inst-config-store-read-03
    // @cpt-begin:cpt-cf-oagw-algo-config-store-read:p2:inst-config-store-read-04
    // @cpt-begin:cpt-cf-oagw-algo-config-store-write:p2:inst-config-store-write-04
    #[must_use]
    pub fn config(&self) -> Arc<OagwConfig> {
        // `cpt-cf-oagw-flow-gear-bootstrap` always seeds `oagw_config` via
        // `ConfigStore::new` before the router is mounted, so no in-flight
        // request ever observes an unseeded key for this feature's own
        // data (`inst-config-store-read-02`/`-03`). This same read is also
        // where `set_config`'s write becomes observable
        // (`inst-config-store-write-04`): `ArcSwap::load_full` always
        // returns the most recently stored value, so a write's effect is
        // immediately visible to the very next call here.
        self.oagw_config.load_full()
    }
    // @cpt-end:cpt-cf-oagw-algo-config-store-write:p2:inst-config-store-write-04
    // @cpt-end:cpt-cf-oagw-algo-config-store-read:p2:inst-config-store-read-04
    // @cpt-end:cpt-cf-oagw-algo-config-store-read:p2:inst-config-store-read-03
    // @cpt-end:cpt-cf-oagw-algo-config-store-read:p2:inst-config-store-read-02
    // @cpt-end:cpt-cf-oagw-algo-config-store-read:p2:inst-config-store-read-01

    /// Tenant-scoped Upstream records. Populated and read by
    /// `cpt-cf-oagw-feature-upstream-management` (2.2) and
    /// `cpt-cf-oagw-feature-proxy-core` (2.5); this feature only establishes
    /// the map's ownership/concurrency contract.
    #[must_use]
    pub fn upstreams(&self) -> &DashMap<Uuid, Arc<Upstream>> {
        &self.upstreams
    }

    /// Tenant-scoped Route records. Populated and read by
    /// `cpt-cf-oagw-feature-route-management` (2.3) and proxy-core (2.5).
    #[must_use]
    pub fn routes(&self) -> &DashMap<Uuid, Arc<Route>> {
        &self.routes
    }

    /// Tenant-scoped custom Plugin records. Populated and read by
    /// `cpt-cf-oagw-feature-plugin-management` (2.4) and
    /// `cpt-cf-oagw-feature-plugin-execution` (2.9).
    #[must_use]
    pub fn plugins(&self) -> &DashMap<Uuid, Arc<Plugin>> {
        &self.plugins
    }
}

/// Shared gear state, injected into REST handlers via
/// `axum::Extension<Arc<OagwState>>`. The single field every later slice
/// (2.2-2.9) builds on: `store` is the shared configuration-store handle
/// described above.
#[derive(Debug)]
pub struct OagwState {
    pub store: Arc<ConfigStore>,
}

impl OagwState {
    #[must_use]
    pub fn new(config: OagwConfig) -> Self {
        Self {
            store: Arc::new(ConfigStore::new(config)),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn write_is_immediately_visible_to_a_subsequent_read() {
        let store = ConfigStore::new(OagwConfig::default());
        assert_eq!(store.config().proxy_timeout_secs, 30);

        let updated = OagwConfig {
            proxy_timeout_secs: 99,
            ..OagwConfig::default()
        };
        store.set_config(updated);

        assert_eq!(store.config().proxy_timeout_secs, 99);
    }

    #[test]
    fn concurrent_reads_and_a_write_never_panic_deadlock_or_tear() {
        let store = Arc::new(ConfigStore::new(OagwConfig::default()));

        let readers: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                thread::spawn(move || {
                    for _ in 0..500 {
                        let cfg = store.config();
                        // A torn value would show up as neither the old nor
                        // the new fully-formed `OagwConfig`.
                        assert!(cfg.proxy_timeout_secs == 30 || cfg.proxy_timeout_secs == 42);
                    }
                })
            })
            .collect();

        let writer = {
            let store = store.clone();
            thread::spawn(move || {
                let updated = OagwConfig {
                    proxy_timeout_secs: 42,
                    ..OagwConfig::default()
                };
                store.set_config(updated);
            })
        };

        writer.join().unwrap();
        for reader in readers {
            reader.join().unwrap();
        }

        assert_eq!(store.config().proxy_timeout_secs, 42);
    }

    #[test]
    fn oagw_state_exposes_the_seeded_store() {
        let state = OagwState::new(OagwConfig::default());
        assert_eq!(state.store.config().proxy_timeout_secs, 30);
    }
}
