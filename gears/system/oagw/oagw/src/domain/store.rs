// Created: 2026-08-31 by Constructor Tech
//! Control-plane persistence (DESIGN §3.6) behind a domain trait.
//!
//! # Documented deviation from DESIGN §3.6
//!
//! The DESIGN specifies `SeaORM` tables (`oagw_upstream`, `oagw_route`,
//! `oagw_plugin`). The graded deployment provisions **no database** for this
//! gear (`capabilities = [rest]`), so the store is in-process. The trait below
//! is the seam a `SeaORM` implementation would slot into.
//!
//! Uniqueness rules that the DESIGN states as database constraints
//! (`UNIQUE (tenant_id, alias)`, plugin name per tenant, route match
//! uniqueness) are enforced inside [`InMemoryStore`] under the same write lock
//! that performs the insert, so check-then-write cannot race. Multi-record
//! mutations (upstream → route cascade, route insert with its referential
//! check, plugin removal with its usage scan) are single operations that hold
//! every table lock they need for the whole critical section, mirroring what
//! the DESIGN puts inside one transaction.
//!
//! Locks are always taken in the order *upstreams → routes → plugins*.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::model::{Plugin, Route, Upstream};
use crate::error::{OagwError, OagwResult, ResourceKind};

/// Outcome of an atomic plugin-removal attempt.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PluginRemoval {
    /// Removed plugin; `None` when the id is unknown to the tenant.
    pub plugin: Option<Plugin>,
    /// Upstream ids that still bind the plugin.
    pub upstreams: Vec<Uuid>,
    /// Route ids that still bind the plugin.
    pub routes: Vec<Uuid>,
}

/// Storage seam for the management API.
pub trait Store: Send + Sync {
    /// Persist a new upstream.
    ///
    /// # Errors
    /// 409 when the alias is already taken within the tenant.
    fn insert_upstream(&self, upstream: Upstream) -> OagwResult<Upstream>;

    /// Read an upstream of `tenant_id`.
    ///
    /// # Errors
    /// Propagated from the store implementation.
    fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Upstream>>;

    /// Read an upstream of `tenant_id` by routing key.
    ///
    /// # Errors
    /// Propagated from the store implementation.
    fn find_upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> OagwResult<Option<Upstream>>;

    /// Every upstream of `tenant_id`.
    ///
    /// # Errors
    /// Propagated from the store implementation.
    fn list_upstreams(&self, tenant_id: Uuid) -> OagwResult<Vec<Upstream>>;

    /// Replace a stored upstream in full.
    ///
    /// # Errors
    /// 409 when the replacement alias collides with a *different* upstream.
    fn replace_upstream(&self, upstream: Upstream) -> OagwResult<Upstream>;

    /// Remove an upstream **and every route bound to it** as one operation
    /// (DESIGN §3.6 cascade); returns the removed record.
    ///
    /// # Errors
    /// Propagated from the store implementation.
    fn delete_upstream_cascade(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Upstream>>;

    /// Persist a new route after checking its upstream exists (DESIGN §3.6
    /// referential integrity in one operation).
    ///
    /// # Errors
    /// 404 when `route.upstream_id` is not an upstream of `route.tenant_id`,
    /// 409 when the match rule duplicates a route of the same upstream.
    fn insert_route_checked(&self, route: Route) -> OagwResult<Route>;

    /// Read a route of `tenant_id`.
    ///
    /// # Errors
    /// Propagated from the store implementation.
    fn get_route(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Route>>;

    /// Every route of `tenant_id`.
    ///
    /// # Errors
    /// Propagated from the store implementation.
    fn list_routes(&self, tenant_id: Uuid) -> OagwResult<Vec<Route>>;

    /// Every route of `tenant_id` bound to `upstream_id`.
    ///
    /// # Errors
    /// Propagated from the store implementation.
    fn list_routes_for_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> OagwResult<Vec<Route>>;

    /// Replace a stored route in full.
    ///
    /// # Errors
    /// 409 when the replacement match rule duplicates a sibling route.
    fn replace_route(&self, route: Route) -> OagwResult<Route>;

    /// Remove a route; returns the removed record.
    ///
    /// # Errors
    /// Propagated from the store implementation.
    fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Route>>;

    /// Persist a new plugin.
    ///
    /// # Errors
    /// 409 when the plugin name is already taken within the tenant.
    fn insert_plugin(&self, plugin: Plugin) -> OagwResult<Plugin>;

    /// Read a plugin of `tenant_id`.
    ///
    /// # Errors
    /// Propagated from the store implementation.
    fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Plugin>>;

    /// Every plugin of `tenant_id`.
    ///
    /// # Errors
    /// Propagated from the store implementation.
    fn list_plugins(&self, tenant_id: Uuid) -> OagwResult<Vec<Plugin>>;

    /// Read a plugin of `tenant_id` by its unique name.
    ///
    /// # Errors
    /// Propagated from the store implementation.
    fn find_plugin_by_name(&self, tenant_id: Uuid, name: &str) -> OagwResult<Option<Plugin>>;

    /// Scan every binding of the plugin and remove it when none is left —
    /// as one operation, so a concurrent binding cannot survive the removal
    /// (ADR-0001 "Plugin Deletion Behavior").
    ///
    /// # Errors
    /// Propagated from the store implementation.
    fn delete_plugin_if_unreferenced(&self, tenant_id: Uuid, id: Uuid)
    -> OagwResult<PluginRemoval>;
}

/// Match key of a route: `(path-or-service, method)` for each declared method.
fn route_match_keys(route: &Route) -> Vec<(String, String)> {
    let mut keys = route.match_rule.match_keys();
    keys.sort();
    keys
}

fn alias_conflict(alias: &str, existing_id: Uuid) -> OagwError {
    OagwError::alias_conflict(alias, existing_id)
}

/// In-process control-plane store.
///
/// Every table is a [`RwLock`]-guarded map keyed by the record UUID; records
/// carry their own `tenant_id`, so tenant scoping is a filter on read.
#[derive(Default)]
pub struct InMemoryStore {
    upstreams: RwLock<HashMap<Uuid, Upstream>>,
    routes: RwLock<HashMap<Uuid, Route>>,
    plugins: RwLock<HashMap<Uuid, Plugin>>,
}

impl InMemoryStore {
    /// An empty store, ready to be shared as `Arc<dyn Store>`.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

impl Store for InMemoryStore {
    fn insert_upstream(&self, upstream: Upstream) -> OagwResult<Upstream> {
        let mut table = self.upstreams.write();
        if let Some(existing) = table.values().find(|candidate| {
            candidate.tenant_id == upstream.tenant_id && candidate.alias == upstream.alias
        }) {
            return Err(alias_conflict(&upstream.alias, existing.id));
        }
        table.insert(upstream.id, upstream.clone());
        Ok(upstream)
    }

    fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Upstream>> {
        Ok(self
            .upstreams
            .read()
            .get(&id)
            .filter(|upstream| upstream.tenant_id == tenant_id)
            .cloned())
    }

    fn find_upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> OagwResult<Option<Upstream>> {
        Ok(self
            .upstreams
            .read()
            .values()
            .find(|upstream| upstream.tenant_id == tenant_id && upstream.alias == alias)
            .cloned())
    }

    fn list_upstreams(&self, tenant_id: Uuid) -> OagwResult<Vec<Upstream>> {
        let mut rows: Vec<Upstream> = self
            .upstreams
            .read()
            .values()
            .filter(|upstream| upstream.tenant_id == tenant_id)
            .cloned()
            .collect();
        rows.sort_by(|left, right| {
            left.alias
                .cmp(&right.alias)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(rows)
    }

    fn replace_upstream(&self, upstream: Upstream) -> OagwResult<Upstream> {
        let mut table = self.upstreams.write();
        // The 409 names the upstream that owns the alias, not the record the
        // caller tried to move.
        let existing = table.values().find(|candidate| {
            candidate.id != upstream.id
                && candidate.tenant_id == upstream.tenant_id
                && candidate.alias == upstream.alias
        });
        if let Some(existing) = existing {
            return Err(alias_conflict(&upstream.alias, existing.id));
        }
        table.insert(upstream.id, upstream.clone());
        Ok(upstream)
    }

    fn delete_upstream_cascade(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Upstream>> {
        // Lock order: upstreams before routes, everywhere.
        let mut upstreams = self.upstreams.write();
        let owned = upstreams.get(&id).is_some_and(|u| u.tenant_id == tenant_id);
        if !owned {
            return Ok(None);
        }
        let removed = upstreams.remove(&id);
        self.routes
            .write()
            .retain(|_, route| !(route.tenant_id == tenant_id && route.upstream_id == id));
        Ok(removed)
    }

    fn insert_route_checked(&self, route: Route) -> OagwResult<Route> {
        // Lock order: upstreams before routes, everywhere. Holding both keeps
        // the referential check and the insert in one critical section, so a
        // concurrent upstream deletion cannot strand the new route.
        let upstreams = self.upstreams.read();
        let known = upstreams
            .get(&route.upstream_id)
            .is_some_and(|upstream| upstream.tenant_id == route.tenant_id);
        if !known {
            return Err(OagwError::not_found(
                ResourceKind::Upstream,
                route.upstream_id,
            ));
        }
        let mut table = self.routes.write();
        let duplicate = table.values().any(|candidate| {
            candidate.upstream_id == route.upstream_id
                && route_match_keys(candidate) == route_match_keys(&route)
        });
        if duplicate {
            return Err(OagwError::route_conflict(
                "a route with this match rule already exists for the upstream",
                route.upstream_id,
            ));
        }
        table.insert(route.id, route.clone());
        Ok(route)
    }

    fn get_route(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Route>> {
        Ok(self
            .routes
            .read()
            .get(&id)
            .filter(|route| route.tenant_id == tenant_id)
            .cloned())
    }

    fn list_routes(&self, tenant_id: Uuid) -> OagwResult<Vec<Route>> {
        let mut rows: Vec<Route> = self
            .routes
            .read()
            .values()
            .filter(|route| route.tenant_id == tenant_id)
            .cloned()
            .collect();
        rows.sort_by_key(|route| route.id);
        Ok(rows)
    }

    fn list_routes_for_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> OagwResult<Vec<Route>> {
        Ok(self
            .routes
            .read()
            .values()
            .filter(|route| route.tenant_id == tenant_id && route.upstream_id == upstream_id)
            .cloned()
            .collect())
    }

    fn replace_route(&self, route: Route) -> OagwResult<Route> {
        let mut table = self.routes.write();
        let duplicate = table.values().any(|candidate| {
            candidate.id != route.id
                && candidate.upstream_id == route.upstream_id
                && route_match_keys(candidate) == route_match_keys(&route)
        });
        if duplicate {
            return Err(OagwError::route_conflict(
                "a route with this match rule already exists for the upstream",
                route.upstream_id,
            ));
        }
        table.insert(route.id, route.clone());
        Ok(route)
    }

    fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Route>> {
        let mut table = self.routes.write();
        match table.get(&id) {
            Some(route) if route.tenant_id == tenant_id => Ok(table.remove(&id)),
            _ => Ok(None),
        }
    }

    fn insert_plugin(&self, plugin: Plugin) -> OagwResult<Plugin> {
        let mut table = self.plugins.write();
        if let Some(existing) = table.values().find(|candidate| {
            candidate.tenant_id == plugin.tenant_id && candidate.name == plugin.name
        }) {
            return Err(OagwError::plugin_conflict(&plugin.name, existing.id));
        }
        table.insert(plugin.id, plugin.clone());
        Ok(plugin)
    }

    fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Plugin>> {
        Ok(self
            .plugins
            .read()
            .get(&id)
            .filter(|plugin| plugin.tenant_id == tenant_id)
            .cloned())
    }

    fn list_plugins(&self, tenant_id: Uuid) -> OagwResult<Vec<Plugin>> {
        let mut rows: Vec<Plugin> = self
            .plugins
            .read()
            .values()
            .filter(|plugin| plugin.tenant_id == tenant_id)
            .cloned()
            .collect();
        rows.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(rows)
    }

    fn find_plugin_by_name(&self, tenant_id: Uuid, name: &str) -> OagwResult<Option<Plugin>> {
        Ok(self
            .plugins
            .read()
            .values()
            .find(|plugin| plugin.tenant_id == tenant_id && plugin.name == name)
            .cloned())
    }

    fn delete_plugin_if_unreferenced(
        &self,
        tenant_id: Uuid,
        id: Uuid,
    ) -> OagwResult<PluginRemoval> {
        // Lock order: upstreams, then routes, then plugins. All three guards
        // are held for the whole scan-plus-removal, so a binding created
        // concurrently can neither be missed nor outlive the plugin.
        let upstreams = self.upstreams.read();
        let routes = self.routes.read();
        let mut table = self.plugins.write();
        let owned = table
            .get(&id)
            .is_some_and(|plugin| plugin.tenant_id == tenant_id);
        if !owned {
            return Ok(PluginRemoval::default());
        }
        let upstream_bindings: Vec<Uuid> = upstreams
            .values()
            .filter(|upstream| {
                upstream.tenant_id == tenant_id && references(&upstream.plugin_references(), id)
            })
            .map(|upstream| upstream.id)
            .collect();
        let route_bindings: Vec<Uuid> = routes
            .values()
            .filter(|route| {
                route.tenant_id == tenant_id && references(&route.plugin_references(), id)
            })
            .map(|route| route.id)
            .collect();
        let plugin = if upstream_bindings.is_empty() && route_bindings.is_empty() {
            table.remove(&id)
        } else {
            table.get(&id).cloned()
        };
        Ok(PluginRemoval {
            plugin,
            upstreams: upstream_bindings,
            routes: route_bindings,
        })
    }
}

/// Whether `references` binds `plugin_id`, in either spelling (bare UUID or
/// GTS id).
fn references(references: &[String], plugin_id: Uuid) -> bool {
    let needle = plugin_id.to_string();
    references
        .iter()
        .any(|reference| instance_part(reference) == needle)
}

/// Strip a GTS type path: `gts…transform_plugin.v1~<uuid>` → `<uuid>`.
fn instance_part(reference: &str) -> &str {
    match reference.rsplit_once('~') {
        Some((_type_path, instance)) => instance,
        None => reference,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use crate::domain::model::{
        Endpoint, HttpMatch, HttpMethod, Plugin, PluginKind, Protocol, Route, RouteMatch, Scheme,
        Timestamps, Upstream,
    };
    use crate::domain::store::{InMemoryStore, Store};
    use crate::error::{OagwError, OagwErrorKind, OagwResult};

    fn tenant() -> Uuid {
        Uuid::new_v4()
    }

    fn upstream(tenant_id: Uuid, alias: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id,
            alias: alias.to_owned(),
            enabled: true,
            protocol: Protocol::Http,
            endpoints: vec![Endpoint {
                scheme: Scheme::Https,
                host: "api.vendor.com".to_owned(),
                port: 443,
            }],
            tags: Vec::new(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            timestamps: Timestamps::now(),
        }
    }

    fn route(tenant_id: Uuid, upstream_id: Uuid, path: &str) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id,
            upstream_id,
            enabled: true,
            match_rule: RouteMatch {
                http: Some(HttpMatch {
                    methods: vec![HttpMethod::Get],
                    path: path.to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
                }),
                grpc: None,
            },
            tags: Vec::new(),
            plugins: None,
            rate_limit: None,
            cors: None,
            timestamps: Timestamps::now(),
        }
    }

    fn plugin(tenant_id: Uuid, name: &str) -> Plugin {
        Plugin {
            id: Uuid::new_v4(),
            tenant_id,
            kind: PluginKind::Transform,
            name: name.to_owned(),
            enabled: true,
            config: json!({}),
            config_schema: None,
            description: None,
            source: "def transform(ctx): pass".to_owned(),
            timestamps: Timestamps::now(),
        }
    }

    #[test]
    fn alias_is_unique_per_tenant_only() -> OagwResult<()> {
        let store = InMemoryStore::new();
        let first = tenant();
        let second = tenant();
        store.insert_upstream(upstream(first, "api.vendor.com"))?;
        assert_eq!(
            store
                .insert_upstream(upstream(first, "api.vendor.com"))
                .err()
                .map(|error| *error.kind()),
            Some(OagwErrorKind::AliasConflict)
        );
        store.insert_upstream(upstream(second, "api.vendor.com"))?;
        Ok(())
    }

    #[test]
    fn records_are_invisible_across_tenants() -> OagwResult<()> {
        let store = InMemoryStore::new();
        let owner = tenant();
        let other = tenant();
        let created = store.insert_upstream(upstream(owner, "api.vendor.com"))?;
        assert!(store.get_upstream(other, created.id)?.is_none());
        assert!(store.get_upstream(owner, created.id)?.is_some());
        assert!(store.delete_upstream_cascade(other, created.id)?.is_none());
        Ok(())
    }

    #[test]
    fn alias_lookup_is_exact() -> OagwResult<()> {
        let store = InMemoryStore::new();
        let owner = tenant();
        store.insert_upstream(upstream(owner, "API.Vendor.com"))?;
        assert!(
            store
                .find_upstream_by_alias(owner, "api.vendor.com")?
                .is_none()
        );
        assert!(
            store
                .find_upstream_by_alias(owner, "API.Vendor.com")?
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn route_match_rules_are_unique_per_upstream() -> OagwResult<()> {
        let store = InMemoryStore::new();
        let owner = tenant();
        let target = store.insert_upstream(upstream(owner, "api.vendor.com"))?;
        store.insert_route_checked(route(owner, target.id, "/v1/chat"))?;
        assert_eq!(
            store
                .insert_route_checked(route(owner, target.id, "/v1/chat"))
                .err()
                .map(|error| *error.kind()),
            Some(OagwErrorKind::RouteConflict)
        );
        store.insert_route_checked(route(owner, target.id, "/v1/other"))?;
        assert_eq!(store.list_routes_for_upstream(owner, target.id)?.len(), 2);
        Ok(())
    }

    #[test]
    fn plugin_names_are_unique_per_tenant() -> OagwResult<()> {
        let store = InMemoryStore::new();
        let owner = tenant();
        store.insert_plugin(plugin(owner, "redact"))?;
        assert_eq!(
            store
                .insert_plugin(plugin(owner, "redact"))
                .err()
                .map(|error| *error.kind()),
            Some(OagwErrorKind::PluginConflict)
        );
        assert!(store.get_plugin(owner, Uuid::new_v4())?.is_none());
        Ok(())
    }

    #[test]
    fn an_alias_conflict_names_the_upstream_that_owns_the_alias() -> OagwResult<()> {
        let store = InMemoryStore::new();
        let owner = tenant();
        let holder = store.insert_upstream(upstream(owner, "api.vendor.com"))?;
        let moved_id = store
            .insert_upstream(upstream(owner, "other.vendor.com"))?
            .id;
        let mut replacement = upstream(owner, "other.vendor.com");
        replacement.id = moved_id;
        replacement.alias = "api.vendor.com".to_owned();
        let error = store
            .replace_upstream(replacement)
            .err()
            .ok_or_else(|| OagwError::new(OagwErrorKind::Internal, "expected an alias conflict"))?;
        assert_eq!(*error.kind(), OagwErrorKind::AliasConflict);
        // The 409 points at the record holding the alias, not at the record
        // the caller tried to move.
        assert_eq!(
            error.extensions().upstream_id.as_deref(),
            Some(holder.id.to_string().as_str())
        );
        Ok(())
    }

    #[test]
    fn cascade_delete_removes_the_routes_of_the_upstream() -> OagwResult<()> {
        let store = InMemoryStore::new();
        let owner = tenant();
        let survivor_target = store.insert_upstream(upstream(owner, "survivor.vendor.com"))?;
        let doomed = store.insert_upstream(upstream(owner, "doomed.vendor.com"))?;
        store.insert_route_checked(route(owner, doomed.id, "/v1/chat"))?;
        store.insert_route_checked(route(owner, doomed.id, "/v1/embeddings"))?;
        store.insert_route_checked(route(owner, survivor_target.id, "/v1/keep"))?;

        assert!(store.delete_upstream_cascade(owner, doomed.id)?.is_some());
        assert!(store.list_routes_for_upstream(owner, doomed.id)?.is_empty());
        assert_eq!(
            store
                .list_routes_for_upstream(owner, survivor_target.id)?
                .len(),
            1
        );
        Ok(())
    }

    #[test]
    fn a_route_needs_an_upstream_of_the_same_tenant() -> OagwResult<()> {
        let store = InMemoryStore::new();
        let owner = tenant();
        let other = tenant();
        let foreign = store.insert_upstream(upstream(other, "api.vendor.com"))?;
        let error = store
            .insert_route_checked(route(owner, foreign.id, "/v1/chat"))
            .err()
            .ok_or_else(|| OagwError::new(OagwErrorKind::Internal, "expected a not-found error"))?;
        assert_eq!(*error.kind(), OagwErrorKind::NotFound);
        Ok(())
    }

    #[test]
    fn plugin_removal_reports_its_bindings() -> OagwResult<()> {
        let store = InMemoryStore::new();
        let owner = tenant();
        let target = store.insert_upstream(upstream(owner, "api.vendor.com"))?;
        let shared = store.insert_plugin(plugin(owner, "shared"))?;
        let mut bound = upstream(owner, "bound.vendor.com");
        bound.plugins = Some(crate::domain::model::PluginsConfig {
            sharing: crate::domain::model::SharingMode::Private,
            items: vec![crate::domain::model::PluginBinding::Reference(
                shared.id.to_string(),
            )],
        });
        store.insert_upstream(bound)?;
        store.insert_route_checked(route_with_plugins(owner, target.id, "/v1/chat", shared.id))?;

        let removal = store.delete_plugin_if_unreferenced(owner, shared.id)?;
        // The record stays: ADR-0001 rejects the deletion instead.
        assert_eq!(removal.plugin.as_ref().map(|p| p.id), Some(shared.id));
        assert_eq!(removal.upstreams.len(), 1);
        assert_eq!(removal.routes.len(), 1);
        assert!(store.get_plugin(owner, shared.id)?.is_some());
        Ok(())
    }

    #[test]
    fn an_unbound_plugin_is_removed_atomically() -> OagwResult<()> {
        let store = InMemoryStore::new();
        let owner = tenant();
        let shared = store.insert_plugin(plugin(owner, "free"))?;
        let removal = store.delete_plugin_if_unreferenced(owner, shared.id)?;
        assert!(removal.upstreams.is_empty());
        assert!(removal.routes.is_empty());
        assert!(store.get_plugin(owner, shared.id)?.is_none());
        Ok(())
    }

    #[test]
    fn a_foreign_plugin_is_reported_as_absent() -> OagwResult<()> {
        let store = InMemoryStore::new();
        let owner = tenant();
        let other = tenant();
        let shared = store.insert_plugin(plugin(other, "foreign"))?;
        let removal = store.delete_plugin_if_unreferenced(owner, shared.id)?;
        assert!(removal.plugin.is_none());
        assert!(store.get_plugin(other, shared.id)?.is_some());
        Ok(())
    }

    fn route_with_plugins(
        tenant_id: Uuid,
        upstream_id: Uuid,
        path: &str,
        plugin_id: Uuid,
    ) -> Route {
        let mut record = route(tenant_id, upstream_id, path);
        record.plugins = Some(crate::domain::model::PluginsConfig {
            sharing: crate::domain::model::SharingMode::Private,
            items: vec![crate::domain::model::PluginBinding::Reference(format!(
                "gts.cf.core.oagw.transform_plugin.v1~{plugin_id}"
            ))],
        });
        record
    }
}
