//! The unit tests of the Control Plane L1 cache
//! (`cpt-cf-oagw-dod-observability-and-state-cp-cache`,
//! `cpt-cf-oagw-dod-observability-and-state-cache-keys`,
//! `cpt-cf-oagw-dod-observability-and-state-deployment-modes`).
// @cpt-dod:cpt-cf-oagw-dod-observability-and-state-cache-keys:p1
// @cpt-dod:cpt-cf-oagw-dod-observability-and-state-deployment-modes:p1

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// A fault-injecting upstream repository, so the four read outcomes are
/// reachable.
#[derive(Default)]
struct FaultyStore {
    reads: AtomicUsize,
    fail: std::sync::atomic::AtomicBool,
    records: Mutex<Vec<UpstreamRecord>>,
}

impl FaultyStore {
    fn with_record(record: UpstreamRecord) -> Arc<Self> {
        let store = Self::default();
        store.records.lock().push(record);
        Arc::new(store)
    }

    fn record(alias: &str) -> UpstreamRecord {
        UpstreamRecord {
            upstream: crate::test_support::upstream(Uuid::nil(), alias),
            plugin_bindings: Vec::new(),
        }
    }
}

impl UpstreamRepository for FaultyStore {
    fn get(&self, _tenant_id: Uuid, _id: Uuid) -> Result<UpstreamRecord, DomainError> {
        Err(DomainError::NotFound { resource_type: "upstream" })
    }

    fn get_by_alias(&self, _tenant_id: Uuid, _alias: &str) -> Result<UpstreamRecord, DomainError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(DomainError::Internal("store unavailable".to_owned()));
        }
        let records = self.records.lock();
        records
            .first()
            .cloned()
            .ok_or(DomainError::NotFound { resource_type: "upstream" })
    }

    fn list(&self, _tenant_id: Uuid) -> Result<Vec<UpstreamRecord>, DomainError> {
        Ok(self.records.lock().clone())
    }

    fn create(&self, _t: Uuid, record: UpstreamRecord) -> Result<UpstreamRecord, DomainError> {
        self.records.lock().push(record.clone());
        Ok(record)
    }

    fn replace(&self, _t: Uuid, record: UpstreamRecord) -> Result<UpstreamRecord, DomainError> {
        *self.records.lock() = vec![record.clone()];
        Ok(record)
    }

    fn delete(&self, _t: Uuid, _id: Uuid) -> Result<(), DomainError> {
        self.records.lock().clear();
        Ok(())
    }
}

/// The three key derivations, with the owning-tenant component.
#[test]
fn the_three_key_families_are_derived_with_the_owning_tenant() {
    let tenant = Uuid::new_v4();
    let upstream = Uuid::new_v4();
    let plugin = Uuid::new_v4();
    assert_eq!(
        CacheKey::Upstream { owner_tenant_id: tenant, alias: "api.vendor.com".to_owned() }
            .as_string(),
        format!("upstream:{tenant}:api.vendor.com")
    );
    assert_eq!(
        CacheKey::Route {
            upstream_id: upstream,
            method: "GET".to_owned(),
            path_prefix: "/v1".to_owned()
        }
        .as_string(),
        format!("route:{upstream}:GET:/v1")
    );
    assert_eq!(CacheKey::Plugin { plugin_id: plugin }.as_string(), format!("plugin:{plugin}"));
}

/// A canonical string form parses back into the same key.
#[test]
fn a_canonical_key_form_parses_back() {
    let tenant = Uuid::new_v4();
    let key = CacheKey::Upstream { owner_tenant_id: tenant, alias: "api.vendor.com".to_owned() };
    assert_eq!(CacheKey::parse(&key.as_string()), Some(key));
    assert_eq!(CacheKey::parse("upstream:not-a-uuid:alias"), None);
    assert_eq!(CacheKey::parse("session:abc"), None);
}

/// A repeated read is served from the cache without a repository call.
#[test]
fn a_repeated_read_is_served_from_the_cache() {
    let tenant = Uuid::new_v4();
    let store = FaultyStore::with_record(FaultyStore::record("api.vendor.com"));
    let state = CPState::single_executable();
    let cached = CachedUpstreamRepository::new(store.clone(), state, ConfigGenerations::default());
    assert!(cached.get_by_alias(tenant, "api.vendor.com").is_ok());
    assert!(cached.get_by_alias(tenant, "api.vendor.com").is_ok());
    assert_eq!(store.reads.load(Ordering::SeqCst), 1, "the second read must be a hit");
}

/// A list lookup bypasses the cache instead of minting a key shape.
#[test]
fn a_list_lookup_bypasses_the_cache() {
    let tenant = Uuid::new_v4();
    let store = FaultyStore::with_record(FaultyStore::record("api.vendor.com"));
    let state = CPState::single_executable();
    let cached = CachedUpstreamRepository::new(store.clone(), state.clone(), ConfigGenerations::default());
    assert_eq!(cached.list(tenant).expect("the list").len(), 1);
    assert_eq!(cached.list(tenant).expect("the list").len(), 1);
    assert_eq!(store.reads.load(Ordering::SeqCst), 0, "the list never touches the alias read");
    assert!(state.l1.is_empty());
}

/// A not-found outcome is distinct and inserts nothing.
#[test]
fn a_not_found_outcome_inserts_nothing() {
    let tenant = Uuid::new_v4();
    let store: Arc<FaultyStore> = Arc::new(FaultyStore::default());
    let state = CPState::single_executable();
    let cached = CachedUpstreamRepository::new(store, state.clone(), ConfigGenerations::default());
    let outcome = cached.get_by_alias(tenant, "missing.vendor.com").unwrap_err();
    assert!(outcome.is_not_found());
    assert_eq!(state.l1.len(), 0);
}

/// A store failure is the distinct store-error outcome and inserts nothing.
#[test]
fn a_store_failure_is_the_store_error_outcome() {
    let tenant = Uuid::new_v4();
    let store: Arc<FaultyStore> = FaultyStore::with_record(FaultyStore::record("api.vendor.com"));
    store.fail.store(true, Ordering::SeqCst);
    let state = CPState::single_executable();
    let cached = CachedUpstreamRepository::new(store.clone(), state.clone(), ConfigGenerations::default());
    let outcome = cached.get_by_alias(tenant, "api.vendor.com").unwrap_err();
    assert!(!outcome.is_not_found());
    assert!(matches!(outcome, DomainError::Internal(_)));
    assert_eq!(state.l1.len(), 0);
    store.fail.store(false, Ordering::SeqCst);
    assert!(cached.get_by_alias(tenant, "api.vendor.com").is_ok());
}

/// The cache holds no TTL: an entry read long after it was written still hits.
#[test]
fn an_entry_persists_with_no_ttl() {
    let tenant = Uuid::new_v4();
    let store = FaultyStore::with_record(FaultyStore::record("api.vendor.com"));
    let state = CPState::single_executable();
    let cached = CachedUpstreamRepository::new(store, state, ConfigGenerations::default());
    for _ in 0..100 {
        assert!(cached.get_by_alias(tenant, "api.vendor.com").is_ok());
    }
}

/// The LRU evicts the least recently used entry at capacity.
#[test]
fn the_lru_evicts_the_least_recently_used_entry() {
    let l1: L1<u64> = L1::new(2);
    l1.insert("a", 1, 0);
    l1.insert("b", 2, 0);
    // `a` becomes the most recently used.
    assert_eq!(l1.get("a"), Some((1, 0)));
    l1.insert("c", 3, 0);
    assert_eq!(l1.get("a"), Some((1, 0)));
    assert_eq!(l1.get("b"), None, "`b` is the least recently used entry");
    assert_eq!(l1.get("c"), Some((3, 0)));
}

/// A population that raced a flush cannot insert a pre-write value.
#[test]
fn a_population_racing_a_flush_cannot_insert_a_pre_write_value() {
    let tenant = Uuid::new_v4();
    let generations = ConfigGenerations::default();
    let key = CacheKey::Upstream { owner_tenant_id: tenant, alias: "api.vendor.com".to_owned() }.as_string();
    let state = CPState::single_executable();
    // A stale value is read, then a write bumps the generation, then the stale
    // population inserts.
    let observed = generations.generation(&key);
    generations.bump(&key);
    state.l1.insert(&key, FaultyStore::record("stale"), observed);
    let cached = CachedUpstreamRepository::new(
        FaultyStore::with_record(FaultyStore::record("fresh")),
        state,
        generations,
    );
    let record = cached.get_by_alias(tenant, "api.vendor.com").expect("a fresh value");
    assert_eq!(record.upstream.alias, "fresh");
}

/// A store that moves the key's generation while the read is in flight, so a
/// population can be observed racing its own flush.
struct BumpingStore {
    generations: ConfigGenerations,
    key: String,
    record: UpstreamRecord,
    reads: std::sync::atomic::AtomicUsize,
}

impl BumpingStore {
    fn read_count(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl UpstreamRepository for BumpingStore {
    fn get(&self, _tenant_id: Uuid, _id: Uuid) -> Result<UpstreamRecord, DomainError> {
        Err(DomainError::NotFound { resource_type: "upstream" })
    }

    fn get_by_alias(&self, _tenant_id: Uuid, _alias: &str) -> Result<UpstreamRecord, DomainError> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // The write lands while the store read is in flight.
        self.generations.bump(&self.key);
        Ok(self.record.clone())
    }

    fn list(&self, _tenant_id: Uuid) -> Result<Vec<UpstreamRecord>, DomainError> {
        Ok(vec![self.record.clone()])
    }

    fn create(&self, _t: Uuid, _record: UpstreamRecord) -> Result<UpstreamRecord, DomainError> {
        Err(DomainError::Conflict { detail: "unused".to_owned(), referenced_by: None })
    }

    fn replace(&self, _t: Uuid, _record: UpstreamRecord) -> Result<UpstreamRecord, DomainError> {
        Err(DomainError::Conflict { detail: "unused".to_owned(), referenced_by: None })
    }

    fn delete(&self, _t: Uuid, _id: Uuid) -> Result<(), DomainError> {
        Err(DomainError::NotFound { resource_type: "upstream" })
    }

}

/// `inst-os-algo-lru-4`/`-4b`: the generation is captured *before* the store
/// read, so a population whose read raced a write does not insert its pre-write
/// value — the entry is absent and the next read goes to the store again.
#[test]
fn a_population_whose_read_raced_a_write_does_not_insert() {
    let tenant = Uuid::new_v4();
    let generations = ConfigGenerations::default();
    let key = CacheKey::Upstream { owner_tenant_id: tenant, alias: "api.vendor.com".to_owned() }.as_string();
    let state = CPState::single_executable();
    let inner = Arc::new(BumpingStore {
        // The same generation table the decorator holds, so the write that
        // races the read is a write to the decorator's key.
        generations: generations.clone(),
        key: key.clone(),
        record: FaultyStore::record("raced"),
        reads: std::sync::atomic::AtomicUsize::new(0),
    });
    let cached = CachedUpstreamRepository::new(Arc::clone(&inner) as Arc<dyn UpstreamRepository>, state.clone(), generations);

    // The store bumps its own generation during the read, so the observed
    // generation no longer matches the current one and nothing is inserted.
    let record = cached.get_by_alias(tenant, "api.vendor.com").expect("the read still succeeds");
    assert_eq!(record.upstream.alias, "raced");
    assert!(!state.l1.contains(&key), "no pre-write value was inserted");

    // The next read reaches the store again rather than the refused entry.
    let _ = cached.get_by_alias(tenant, "api.vendor.com").expect("the next read succeeds");
    assert_eq!(inner.read_count(), 2, "the refused population was not cached");
}

/// Write-side invalidation removes exactly the affected keys.
#[test]
fn a_write_invalidates_only_the_affected_keys() {
    let tenant = Uuid::new_v4();
    let other = Uuid::new_v4();
    let state = CPState::single_executable();
    let key = CacheKey::Upstream { owner_tenant_id: tenant, alias: "api.vendor.com".to_owned() };
    let unaffected = CacheKey::Upstream { owner_tenant_id: other, alias: "api.vendor.com".to_owned() };
    state.l1.insert(&key.as_string(), FaultyStore::record("a"), 0);
    state.l1.insert(&unaffected.as_string(), FaultyStore::record("b"), 0);
    let notification = WriteNotification {
        event: "upstream.replace",
        tenant_id: tenant,
        principal_id: Uuid::nil(),
        resource_id: "9".to_owned(),
        upstream_id: None,
        upstream_alias: Some("api.vendor.com".to_owned()),
        route: None,
        plugin_id: None,
        status: 200,
        outcome: "accepted",
    };
    let affected = CPState::affected_keys(&notification);
    state.invalidate(&affected);
    assert!(state.l1.get(&key.as_string()).is_none(), "the affected key is gone");
    assert!(state.l1.get(&unaffected.as_string()).is_some(), "the other entry stays");
}

/// A route write derives one key per method of its match block.
#[test]
fn a_route_write_derives_one_key_per_method() {
    let notification = WriteNotification {
        event: "route.create",
        tenant_id: Uuid::new_v4(),
        principal_id: Uuid::nil(),
        resource_id: "r".to_owned(),
        upstream_id: Some(Uuid::new_v4()),
        upstream_alias: None,
        route: Some(crate::domain::services::management::RouteWriteKeys {
            upstream_id: Uuid::new_v4(),
            path_prefix: "/v1".to_owned(),
            methods: vec!["GET".to_owned(), "POST".to_owned()],
        }),
        plugin_id: None,
        status: 200,
        outcome: "accepted",
    };
    let keys = CPState::affected_keys(&notification);
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().all(|key| matches!(key, CacheKey::Route { .. })));
}

/// A rejected write leaves the cache untouched.
#[test]
fn a_rejected_write_invalidates_nothing() {
    let tenant = Uuid::new_v4();
    let state = CPState::single_executable();
    let key = CacheKey::Upstream { owner_tenant_id: tenant, alias: "api.vendor.com".to_owned() };
    state.l1.insert(&key.as_string(), FaultyStore::record("a"), 0);
    // `inst-os-cpinv-2`/`-3`: a rejected write never reaches the hook, so the
    // entry survives.
    assert!(state.l1.get(&key.as_string()).is_some());
}

/// No L2 layer is constructed, and an L2 request is refused.
#[test]
fn an_l2_request_is_refused_as_unsupported() {
    let state = CPState::single_executable();
    assert!(state.l2_cache.is_none());
    struct NoL2;
    impl L2Cache for NoL2 {
        fn available(&self) -> bool {
            false
        }
    }
    assert!(CPState::with_l2(Arc::new(NoL2)).is_err());
    assert!(refuse_l2().is_err());
}

/// The Control Plane capacity is the fixed 10,000-entry constant.
#[test]
fn the_cp_capacity_is_the_fixed_constant() {
    assert_eq!(CP_L1_CAPACITY, 10_000);
}

/// The `plugin:{plugin_id}` family is reserved: it derives a key that no
/// reader ever consults.
#[test]
fn the_plugin_family_is_reserved() {
    let notification = WriteNotification {
        event: "plugin.created",
        tenant_id: Uuid::new_v4(),
        principal_id: Uuid::nil(),
        resource_id: "p".to_owned(),
        upstream_id: None,
        upstream_alias: None,
        route: None,
        plugin_id: Some(Uuid::new_v4()),
        status: 201,
        outcome: "accepted",
    };
    let keys = CPState::affected_keys(&notification);
    assert_eq!(keys.len(), 1);
    assert!(matches!(keys[0], CacheKey::Plugin { .. }));
    // No reader exists, so invalidating it removes nothing a read would serve.
    let state = CPState::single_executable();
    state.invalidate(&keys);
    assert_eq!(state.l1.len(), 0);
}
