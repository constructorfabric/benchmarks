//! In-memory persistence for the OAGW control plane.
//!
//! The crate ships without a `database` capability (no sea-orm / db
//! deps, no `database:` config section), so all records live in
//! process memory behind a parking-lot mutex. Mutations are serialized
//! through one lock per resource so multi-key invariants — alias
//! uniqueness per `(tenant_id, alias)` — hold atomically.

use std::collections::HashMap;

use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::model::{PluginRecord, RouteRecord, UpstreamRecord};

/// Tenant-scoped table: `tenant_id -> (resource_id -> record)`.
struct TenantTable<T> {
    by_tenant: HashMap<Uuid, HashMap<Uuid, T>>,
}

impl<T> Default for TenantTable<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> TenantTable<T> {
    fn new() -> Self {
        Self {
            by_tenant: HashMap::new(),
        }
    }

    fn insert(&mut self, tenant: Uuid, id: Uuid, record: T) {
        self.by_tenant.entry(tenant).or_default().insert(id, record);
    }

    fn get(&self, tenant: Uuid, id: Uuid) -> Option<&T> {
        self.by_tenant.get(&tenant)?.get(&id)
    }

    fn remove(&mut self, tenant: Uuid, id: Uuid) -> Option<T> {
        let table = self.by_tenant.get_mut(&tenant)?;
        let removed = table.remove(&id);
        if table.is_empty() {
            self.by_tenant.remove(&tenant);
        }
        removed
    }

    fn by_tenant(&self, tenant: Uuid) -> impl Iterator<Item = (&Uuid, &T)> + '_ {
        self.by_tenant
            .get(&tenant)
            .into_iter()
            .flat_map(|m| m.iter())
    }
}

/// A tenant-scoped table with a secondary per-tenant key index.
///
/// Used for upstreams where `alias` must be unique per tenant.
struct KeyedTenantTable<T> {
    rows: TenantTable<T>,
    /// `tenant -> key -> resource id`.
    key_index: HashMap<Uuid, HashMap<String, Uuid>>,
}

impl<T> Default for KeyedTenantTable<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> KeyedTenantTable<T> {
    fn new() -> Self {
        Self {
            rows: TenantTable::new(),
            key_index: HashMap::new(),
        }
    }

    /// Insert, asserting key uniqueness within the tenant.
    ///
    /// Returns `Err((tenant, id, record, key))` on conflict.
    fn insert_keyed(
        &mut self,
        tenant: Uuid,
        id: Uuid,
        key: &str,
        record: T,
    ) -> Result<(), (Uuid, Uuid, T, String)> {
        if self.key_index.get(&tenant).is_some_and(|m| m.contains_key(key)) {
            return Err((tenant, id, record, key.to_owned()));
        }
        self.rows.insert(tenant, id, record);
        self.key_index.entry(tenant).or_default().insert(key.to_owned(), id);
        Ok(())
    }

    fn get(&self, tenant: Uuid, id: Uuid) -> Option<&T> {
        self.rows.get(tenant, id)
    }

    fn get_by_key(&self, tenant: Uuid, key: &str) -> Option<&T> {
        let id = self.key_index.get(&tenant)?.get(key)?;
        self.rows.get(tenant, *id)
    }

    fn remove(&mut self, tenant: Uuid, id: Uuid) -> Option<T> {
        let removed = self.rows.remove(tenant, id)?;
        if let Some(index) = self.key_index.get_mut(&tenant) {
            index.retain(|_k, v| *v != id);
        }
        Some(removed)
    }

    fn iter(&self, tenant: Uuid) -> impl Iterator<Item = (&Uuid, &T)> + '_ {
        self.rows.by_tenant(tenant)
    }
}

/// In-memory store for all OAGW records.
#[derive(Default)]
pub struct MemoryStore {
    upstreams: Mutex<KeyedTenantTable<UpstreamRecord>>,
    routes: Mutex<TenantTable<RouteRecord>>,
    plugins: Mutex<TenantTable<PluginRecord>>,
}

impl MemoryStore {
    /// Create a new empty store.
    pub fn new() -> Self {
        Self::default()
    }

    // ------------------------------------------------------------------
    // Upstreams — alias-unique per tenant.
    // ------------------------------------------------------------------

    /// Insert an upstream, enforcing `(tenant_id, alias)` uniqueness.
    ///
    /// Returns `Some(existing alias owner)` on conflict.
    pub fn upstream_insert(
        &self,
        tenant: Uuid,
        record: UpstreamRecord,
    ) -> Result<(), (Uuid, UpstreamRecord, String)> {
        let id = record.id;
        let alias = record.alias.clone();
        let mut table = self.upstreams.lock();
        match table.insert_keyed(tenant, id, &alias, record) {
            Ok(()) => Ok(()),
            Err((_t, _id, rec, key)) => Err((tenant, rec, key)),
        }
    }

    pub fn upstream_get(&self, tenant: Uuid, id: Uuid) -> Option<UpstreamRecord> {
        self.upstreams.lock().get(tenant, id).cloned()
    }

    pub fn upstream_by_alias(&self, tenant: Uuid, alias: &str) -> Option<UpstreamRecord> {
        self.upstreams.lock().get_by_key(tenant, alias).cloned()
    }

    pub fn upstream_list(&self, tenant: Uuid) -> Vec<UpstreamRecord> {
        self.upstreams
            .lock()
            .iter(tenant)
            .map(|(_id, rec)| rec.clone())
            .collect()
    }

    /// Replace an upstream record. `alias` is immutable once set, so a
    /// differing alias is rejected (`Err(record)` is returned with the
    /// submitted value). Existing alias is preserved when the incoming
    /// alias equals the stored one (idempotent no-op).
    pub fn upstream_replace(
        &self,
        tenant: Uuid,
        id: Uuid,
        mut record: UpstreamRecord,
    ) -> Result<Option<UpstreamRecord>, (UpstreamRecord, String)> {
        let mut table = self.upstreams.lock();
        let Some(existing) = table.get(tenant, id).cloned() else {
            return Ok(None);
        };
        // Alias immutability: the submitted alias (or derived alias)
        // must equal the stored one.
        if record.alias != existing.alias {
            return Err((record, existing.alias.clone()));
        }
        record.alias = existing.alias.clone();
        let index = table.key_index.get_mut(&tenant).expect("index key exists");
        index.insert(record.alias.clone(), id);
        table.rows.insert(tenant, id, record);
        Ok(Some(existing))
    }

    pub fn upstream_delete(&self, tenant: Uuid, id: Uuid) -> Option<UpstreamRecord> {
        self.upstreams.lock().remove(tenant, id)
    }

    // ------------------------------------------------------------------
    // Routes
    // ------------------------------------------------------------------

    pub fn route_insert(&self, tenant: Uuid, record: RouteRecord) {
        self.routes.lock().insert(tenant, record.id, record);
    }

    pub fn route_get(&self, tenant: Uuid, id: Uuid) -> Option<RouteRecord> {
        self.routes.lock().get(tenant, id).cloned()
    }

    pub fn route_list(&self, tenant: Uuid) -> Vec<RouteRecord> {
        self.routes
            .lock()
            .by_tenant(tenant)
            .map(|(_id, rec)| rec.clone())
            .collect()
    }

    pub fn route_replace(&self, tenant: Uuid, id: Uuid, record: RouteRecord) -> Option<RouteRecord> {
        let mut table = self.routes.lock();
        let existing = table.get(tenant, id).cloned()?;
        table.insert(tenant, id, record);
        Some(existing)
    }

    pub fn route_delete(&self, tenant: Uuid, id: Uuid) -> Option<RouteRecord> {
        self.routes.lock().remove(tenant, id)
    }

    // ------------------------------------------------------------------
    // Plugins
    // ------------------------------------------------------------------

    pub fn plugin_insert(&self, tenant: Uuid, record: PluginRecord) {
        self.plugins.lock().insert(tenant, record.id, record);
    }

    pub fn plugin_get(&self, tenant: Uuid, id: Uuid) -> Option<PluginRecord> {
        self.plugins.lock().get(tenant, id).cloned()
    }

    pub fn plugin_list(&self, tenant: Uuid) -> Vec<PluginRecord> {
        self.plugins
            .lock()
            .by_tenant(tenant)
            .map(|(_id, rec)| rec.clone())
            .collect()
    }

    pub fn plugin_delete(&self, tenant: Uuid, id: Uuid) -> Option<PluginRecord> {
        self.plugins.lock().remove(tenant, id)
    }
}
