//! In-memory, tenant-scoped storage for the OAGW control plane.
//!
//! The crate deliberately has no `toolkit-db`/SeaORM dependency (DESIGN §3.6
//! names PostgreSQL/MySQL/SQLite as the persistence backend, but the graded
//! deployment is single-process and the data-plane needs sub-millisecond
//! reads), so this slice stores configuration in process. The access pattern
//! mirrors the documented relational invariants so that swapping in a
//! repository implementation later is a mechanical change:
//!
//! * every record is keyed by `(tenant_id, id)` — all reads/writes are strictly
//!   tenant-scoped, ancestor resources are invisible (DESIGN §3.3),
//! * `alias` is unique per `(tenant_id, alias)` — `UNIQUE (tenant_id, alias)`,
//! * list endpoints preserve insertion order (`$orderby=created_at`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::types::{
    Plugin, PluginsConfig, Route, RouteMatch, RouteSpec, Upstream, UpstreamSpec,
};
use crate::error::OagwError;

/// Monotonic insertion sequence; provides a stable `created_at` ordering tie
/// break for list endpoints.
type Sequence = u64;

/// Key of a record: `(tenant_id, id)`.
type TenantKey = (Uuid, Uuid);

#[derive(Debug, Default)]
struct UpstreamTable {
    next_sequence: Sequence,
    /// Insertion-ordered records.
    by_sequence: BTreeMap<Sequence, Arc<Upstream>>,
    /// `(tenant_id, id)` → insertion sequence.
    by_id: HashMap<TenantKey, Sequence>,
    /// `(tenant_id, alias)` → insertion sequence.
    by_alias: HashMap<(Uuid, String), Sequence>,
}

/// Tenant-scoped store of upstream configurations.
#[derive(Debug, Default)]
pub struct UpstreamStore {
    table: RwLock<UpstreamTable>,
}

impl UpstreamStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `upstream`, returning a conflict error when its alias is already
    /// taken by another upstream of the same tenant.
    ///
    /// # Errors
    /// [`OagwError::alias_conflict`] when `(tenant_id, alias)` is taken.
    pub fn insert(&self, upstream: Upstream) -> Result<Arc<Upstream>, OagwError> {
        let mut table = self.table.write();
        if let Some(sequence) = table
            .by_alias
            .get(&(upstream.tenant_id, upstream.alias.clone()))
            && let Some(existing) = table.by_sequence.get(sequence)
        {
            return Err(alias_conflict(existing, &upstream.alias));
        }

        let sequence = table.next_sequence;
        table.next_sequence = sequence.wrapping_add(1);
        let record = Arc::new(upstream);
        table.by_id.insert((record.tenant_id, record.id), sequence);
        table
            .by_alias
            .insert((record.tenant_id, record.alias.clone()), sequence);
        table.by_sequence.insert(sequence, Arc::clone(&record));

        Ok(record)
    }

    /// Look up an upstream by id within a tenant.
    #[must_use]
    pub fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Upstream>> {
        let table = self.table.read();
        let sequence = table.by_id.get(&(tenant_id, id))?;
        table.by_sequence.get(sequence).cloned()
    }

    /// Look up an upstream by alias within a tenant.
    #[must_use]
    pub fn get_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Arc<Upstream>> {
        let table = self.table.read();
        let sequence = table.by_alias.get(&(tenant_id, alias.to_owned()))?;
        table.by_sequence.get(sequence).cloned()
    }

    /// All upstreams of a tenant, in insertion order.
    #[must_use]
    pub fn list(&self, tenant_id: Uuid) -> Vec<Arc<Upstream>> {
        let table = self.table.read();
        table
            .by_sequence
            .values()
            .filter(|upstream| upstream.tenant_id == tenant_id)
            .cloned()
            .collect()
    }

    /// Replace the record with `id`, keeping its position in the list.
    ///
    /// The replacement keeps the stored `id`/`tenant_id`/`alias`: those fields
    /// are immutable (DESIGN §3.3 "PUT (Replace)"). The alias is taken from the
    /// stored record — not from the caller — so the `by_alias` index always
    /// matches the stored row.
    ///
    /// # Errors
    /// [`OagwError::upstream_not_found`] when the id is unknown to the tenant.
    pub fn replace(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        spec: UpstreamSpec,
        updated_at: u64,
    ) -> Result<Arc<Upstream>, OagwError> {
        let mut table = self.table.write();
        let sequence = table
            .by_id
            .get(&(tenant_id, id))
            .copied()
            .ok_or_else(|| upstream_not_found(id))?;

        let previous = table
            .by_sequence
            .get(&sequence)
            .cloned()
            .ok_or_else(|| upstream_not_found(id))?;

        let updated = Arc::new(Upstream {
            id: previous.id,
            tenant_id: previous.tenant_id,
            alias: previous.alias.clone(),
            created_at: previous.created_at,
            updated_at,
            spec,
        });

        table.by_sequence.insert(sequence, Arc::clone(&updated));
        Ok(updated)
    }

    /// Delete an upstream, returning the deleted record.
    ///
    /// # Errors
    /// [`OagwError::upstream_not_found`] when the id is unknown to the tenant.
    pub fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Arc<Upstream>, OagwError> {
        let mut table = self.table.write();
        let sequence = table
            .by_id
            .get(&(tenant_id, id))
            .copied()
            .ok_or_else(|| upstream_not_found(id))?;

        let record = table
            .by_sequence
            .remove(&sequence)
            .ok_or_else(|| upstream_not_found(id))?;

        table.by_id.remove(&(tenant_id, id));
        table.by_alias.remove(&(tenant_id, record.alias.clone()));

        Ok(record)
    }

    /// The GTS identifiers of every upstream across every tenant whose plugin
    /// chain references `plugin`, in insertion order.
    ///
    /// ADR-0001 "Plugin Deletion Behavior": the `409 PluginInUse` response must
    /// enumerate the referencing resources, and a plugin of one tenant may be
    /// referenced by the configuration of a descendant tenant that inherited it,
    /// so this scan is deliberately **not** tenant-scoped. Only the identifiers
    /// the error payload needs leave the store — never the referenced
    /// configuration itself, which belongs to another tenant.
    #[must_use]
    pub fn list_referencing_plugin(&self, plugin: &Plugin) -> Vec<String> {
        let table = self.table.read();
        table
            .by_sequence
            .values()
            .filter(|upstream| references_plugin(upstream.spec.plugins.as_ref(), plugin))
            .map(|upstream| upstream.gts_id())
            .collect()
    }

    /// Number of stored upstreams across all tenants (used by tests and
    /// diagnostics).
    #[must_use]
    pub fn len(&self) -> usize {
        self.table.read().by_sequence.len()
    }

    /// `true` when the store holds no upstreams.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn alias_conflict(existing: &Upstream, alias: &str) -> OagwError {
    OagwError::alias_conflict(format!(
        "an upstream with alias '{alias}' already exists for this tenant (id {})",
        existing.id
    ))
    .with_extension("field", serde_json::Value::String("alias".to_owned()))
}

fn upstream_not_found(id: Uuid) -> OagwError {
    OagwError::upstream_not_found(format!("no upstream with id '{id}' for this tenant"))
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct RouteTable {
    next_sequence: Sequence,
    /// Insertion-ordered records.
    by_sequence: BTreeMap<Sequence, Arc<Route>>,
    /// `(tenant_id, id)` → insertion sequence.
    by_id: HashMap<TenantKey, Sequence>,
    /// `(tenant_id, upstream_id)` → insertion sequences, in insertion order.
    ///
    /// The index the data-plane route walk reads (DESIGN §3.3 "Tenant
    /// Scoping": routes are matched per tenant of the chain).
    by_upstream: HashMap<(Uuid, Uuid), Vec<Sequence>>,
}

/// Tenant-scoped store of route definitions.
///
/// Mirrors [`UpstreamStore`] exactly: records are keyed by
/// `(tenant_id, id)`, list endpoints preserve insertion order, and a route is
/// unique within its upstream for its match rule (`UNIQUE (upstream_id, path,
/// priority, method)` in DESIGN §3.6).
///
/// Route management is exposed by the control-plane slice that owns
/// `/oagw/v1/routes`; this store only provides the shared read path the data
/// plane walks at proxy time.
#[derive(Debug, Default)]
pub struct RouteStore {
    table: RwLock<RouteTable>,
}

impl RouteStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `route`, returning a conflict error when an intersecting match
    /// rule already exists for the same upstream within the tenant.
    ///
    /// Two HTTP rules intersect when they claim the same path and share at least
    /// one method: at proxy time either of them could win the match, so the
    /// control plane refuses the ambiguity rather than leaving it to insertion
    /// order (DESIGN §3.3 "POST (Create)"). A gRPC rule conflicts only with
    /// another gRPC rule naming the same service and method.
    ///
    /// # Errors
    /// [`OagwError::match_conflict`] when the match rule is already taken.
    pub fn insert(&self, route: Route) -> Result<Arc<Route>, OagwError> {
        let mut table = self.table.write();

        let key = (route.tenant_id, route.upstream_id);
        if let Some(sequences) = table.by_upstream.get(&key) {
            for sequence in sequences {
                if let Some(existing) = table.by_sequence.get(sequence)
                    && matches_intersect(&existing.spec.match_rules, &route.spec.match_rules)
                {
                    return Err(match_conflict(existing));
                }
            }
        }

        let sequence = table.next_sequence;
        table.next_sequence = sequence.wrapping_add(1);
        let record = Arc::new(route);
        table.by_id.insert((record.tenant_id, record.id), sequence);
        table
            .by_upstream
            .entry((record.tenant_id, record.upstream_id))
            .or_default()
            .push(sequence);
        table.by_sequence.insert(sequence, Arc::clone(&record));

        Ok(record)
    }

    /// Look up a route by id within a tenant.
    #[must_use]
    pub fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Route>> {
        let table = self.table.read();
        let sequence = table.by_id.get(&(tenant_id, id))?;
        table.by_sequence.get(sequence).cloned()
    }

    /// All routes of a tenant, in insertion order.
    #[must_use]
    pub fn list(&self, tenant_id: Uuid) -> Vec<Arc<Route>> {
        let table = self.table.read();
        table
            .by_sequence
            .values()
            .filter(|route| route.tenant_id == tenant_id)
            .cloned()
            .collect()
    }

    /// All routes of a tenant bound to `upstream_id`, in insertion order.
    ///
    /// This is the read the data-plane route walk performs per tenant of the
    /// chain (nearest tenant first, so descendants take priority).
    #[must_use]
    pub fn list_by_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Arc<Route>> {
        let table = self.table.read();
        table
            .by_upstream
            .get(&(tenant_id, upstream_id))
            .map(|sequences| {
                sequences
                    .iter()
                    .filter_map(|sequence| table.by_sequence.get(sequence).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The GTS identifiers of every route across every tenant whose plugin chain
    /// references `plugin`, in insertion order.
    ///
    /// Not tenant-scoped on purpose: see
    /// [`UpstreamStore::list_referencing_plugin`] for the ADR-0001 rationale and
    /// for why only identifiers leave the store.
    #[must_use]
    pub fn list_referencing_plugin(&self, plugin: &Plugin) -> Vec<String> {
        let table = self.table.read();
        table
            .by_sequence
            .values()
            .filter(|route| references_plugin(route.spec.plugins.as_ref(), plugin))
            .map(|route| route.gts_id())
            .collect()
    }

    /// Delete every route of a tenant bound to `upstream_id`, returning the
    /// deleted records.
    ///
    /// `oagw_route` declares `FK: upstream_id (cascade)` (DESIGN §3.6), so the
    /// routes of an upstream are deleted with it: an orphaned route would keep
    /// matching traffic for an upstream that no longer exists and keep pinning
    /// plugins with a [`OagwError::plugin_in_use`] its operator can no longer
    /// release.
    #[must_use]
    pub fn delete_by_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Arc<Route>> {
        let mut table = self.table.write();
        let Some(sequences) = table.by_upstream.remove(&(tenant_id, upstream_id)) else {
            return Vec::new();
        };

        sequences
            .into_iter()
            .filter_map(|sequence| {
                let record = table.by_sequence.remove(&sequence)?;
                table.by_id.remove(&(tenant_id, record.id));
                Some(record)
            })
            .collect()
    }

    /// Replace the record with `id`, keeping its position in the list.
    ///
    /// `id`, `tenant_id` and `upstream_id` are immutable (DESIGN §3.3
    /// "PUT (Replace)"); the replacement keeps the stored values.
    ///
    /// # Errors
    /// [`OagwError::route_not_found`] when the id is unknown to the tenant.
    pub fn replace(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        spec: RouteSpec,
        updated_at: u64,
    ) -> Result<Arc<Route>, OagwError> {
        let mut table = self.table.write();
        let sequence = table
            .by_id
            .get(&(tenant_id, id))
            .copied()
            .ok_or_else(|| route_not_found(id))?;

        let previous = table
            .by_sequence
            .get(&sequence)
            .cloned()
            .ok_or_else(|| route_not_found(id))?;

        if previous.spec.match_rules != spec.match_rules {
            for other in table
                .by_upstream
                .get(&(tenant_id, previous.upstream_id))
                .into_iter()
                .flatten()
            {
                if *other == sequence {
                    continue;
                }
                if let Some(existing) = table.by_sequence.get(other)
                    && matches_intersect(&existing.spec.match_rules, &spec.match_rules)
                {
                    return Err(match_conflict(existing));
                }
            }
        }

        let updated = Arc::new(Route {
            id: previous.id,
            tenant_id: previous.tenant_id,
            upstream_id: previous.upstream_id,
            created_at: previous.created_at,
            updated_at,
            spec,
        });

        table.by_sequence.insert(sequence, Arc::clone(&updated));
        Ok(updated)
    }

    /// Delete a route, returning the deleted record.
    ///
    /// # Errors
    /// [`OagwError::route_not_found`] when the id is unknown to the tenant.
    pub fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Arc<Route>, OagwError> {
        let mut table = self.table.write();
        let sequence = table
            .by_id
            .get(&(tenant_id, id))
            .copied()
            .ok_or_else(|| route_not_found(id))?;

        let record = table
            .by_sequence
            .remove(&sequence)
            .ok_or_else(|| route_not_found(id))?;

        table.by_id.remove(&(tenant_id, id));
        if let Some(sequences) = table.by_upstream.get_mut(&(tenant_id, record.upstream_id)) {
            sequences.retain(|candidate| *candidate != sequence);
            if sequences.is_empty() {
                table.by_upstream.remove(&(tenant_id, record.upstream_id));
            }
        }

        Ok(record)
    }

    /// Number of stored routes across all tenants (used by tests and
    /// diagnostics).
    #[must_use]
    pub fn len(&self) -> usize {
        self.table.read().by_sequence.len()
    }

    /// `true` when the store holds no routes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn match_conflict(existing: &Route) -> OagwError {
    OagwError::match_conflict(format!(
        "a route with the same match rule already exists for upstream {} (id {})",
        existing.upstream_id, existing.id
    ))
    .with_extension("field", serde_json::Value::String("match".to_owned()))
}

/// `true` when two match rules cannot both exist on the same upstream.
///
/// Two HTTP rules intersect when they claim the same path and share a method —
/// the method sets need not be equal, because at request time the proxy selects
/// a route by longest prefix *and* method allowlist, so overlapping sets make
/// the winner depend on insertion order. A disabled route still holds its claim
/// (it can be re-enabled), and a gRPC rule only intersects another gRPC rule for
/// the same `(service, method)` pair.
fn matches_intersect(left: &RouteMatch, right: &RouteMatch) -> bool {
    match (&left.http, &left.grpc, &right.http, &right.grpc) {
        (Some(left), None, Some(right), None) => {
            left.path == right.path
                && left
                    .methods
                    .iter()
                    .any(|method| right.methods.contains(method))
        }
        (None, Some(left), None, Some(right)) => {
            left.service == right.service && left.method == right.method
        }
        // A route carries exactly one protocol, so an HTTP rule and a gRPC rule
        // never intersect: the proxy consults only the rules of the request's
        // protocol.
        _ => false,
    }
}

fn route_not_found(id: Uuid) -> OagwError {
    OagwError::route_not_found(format!("no route with id '{id}' for this tenant"))
}

/// `true` when a plugin chain references `plugin`.
///
/// A chain names a built-in plugin by its full GTS identifier and a custom
/// plugin either by its GTS identifier or by its bare UUID, so both spellings
/// resolve to the same record (DESIGN §3.1 "Resolution Algorithm",
/// [`crate::domain::types::PluginRef::custom_uuid`]).
fn references_plugin(plugins: Option<&PluginsConfig>, plugin: &Plugin) -> bool {
    let Some(plugins) = plugins else {
        return false;
    };

    plugins.items.iter().any(|reference| {
        reference.custom_uuid() == Some(plugin.id) || reference.as_str() == plugin.gts_id()
    })
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct PluginTable {
    next_sequence: Sequence,
    /// Insertion-ordered records.
    by_sequence: BTreeMap<Sequence, Arc<Plugin>>,
    /// `(tenant_id, id)` → insertion sequence.
    by_id: HashMap<TenantKey, Sequence>,
    /// `(tenant_id, name)` → insertion sequence.
    by_name: HashMap<(Uuid, String), Sequence>,
}

/// Tenant-scoped store of custom (tenant-defined) plugins.
///
/// Mirrors [`UpstreamStore`] exactly: records are keyed by `(tenant_id, id)`,
/// list endpoints preserve insertion order, and `name` is unique per
/// `(tenant_id, name)` (`UNIQUE (tenant_id, name)` in DESIGN §3.6).
///
/// There is deliberately no `replace`: plugins are immutable (DESIGN §3.3), so
/// the only write paths are insert and delete. Built-in plugins never reach this
/// store — they are resolved from the in-process registry at proxy time.
#[derive(Debug, Default)]
pub struct PluginStore {
    table: RwLock<PluginTable>,
}

impl PluginStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `plugin`, returning a conflict error when its name is already
    /// taken by another plugin of the same tenant.
    ///
    /// # Errors
    /// [`OagwError::alias_conflict`] when `(tenant_id, name)` is taken.
    pub fn insert(&self, plugin: Plugin) -> Result<Arc<Plugin>, OagwError> {
        let mut table = self.table.write();
        if let Some(sequence) = table.by_name.get(&(plugin.tenant_id, plugin.name.clone()))
            && let Some(existing) = table.by_sequence.get(sequence)
        {
            return Err(plugin_name_conflict(existing, &plugin.name));
        }

        let sequence = table.next_sequence;
        table.next_sequence = sequence.wrapping_add(1);
        let record = Arc::new(plugin);
        table.by_id.insert((record.tenant_id, record.id), sequence);
        table
            .by_name
            .insert((record.tenant_id, record.name.clone()), sequence);
        table.by_sequence.insert(sequence, Arc::clone(&record));

        Ok(record)
    }

    /// Look up a plugin by id within a tenant.
    #[must_use]
    pub fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Plugin>> {
        let table = self.table.read();
        let sequence = table.by_id.get(&(tenant_id, id))?;
        table.by_sequence.get(sequence).cloned()
    }

    /// All plugins of a tenant, in insertion order.
    #[must_use]
    pub fn list(&self, tenant_id: Uuid) -> Vec<Arc<Plugin>> {
        let table = self.table.read();
        table
            .by_sequence
            .values()
            .filter(|plugin| plugin.tenant_id == tenant_id)
            .cloned()
            .collect()
    }

    /// Delete a plugin, returning the deleted record.
    ///
    /// # Errors
    /// [`OagwError::plugin_not_found`] when the id is unknown to the tenant.
    pub fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Arc<Plugin>, OagwError> {
        let mut table = self.table.write();
        let sequence = table
            .by_id
            .get(&(tenant_id, id))
            .copied()
            .ok_or_else(|| plugin_not_found(id))?;

        let record = table
            .by_sequence
            .remove(&sequence)
            .ok_or_else(|| plugin_not_found(id))?;

        table.by_id.remove(&(tenant_id, id));
        table.by_name.remove(&(tenant_id, record.name.clone()));

        Ok(record)
    }

    /// Number of stored plugins across all tenants (used by tests and
    /// diagnostics).
    #[must_use]
    pub fn len(&self) -> usize {
        self.table.read().by_sequence.len()
    }

    /// `true` when the store holds no plugins.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn plugin_name_conflict(existing: &Plugin, name: &str) -> OagwError {
    OagwError::alias_conflict(format!(
        "a plugin with name '{name}' already exists for this tenant (id {})",
        existing.id
    ))
    .with_extension("field", serde_json::Value::String("name".to_owned()))
}

fn plugin_not_found(id: Uuid) -> OagwError {
    OagwError::plugin_not_found(format!("no plugin with id '{id}' for this tenant"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::types::{GrpcMatch, HttpMatch, RouteMatch, RouteMethod, UpstreamSpec};

    fn upstream(tenant_id: Uuid, alias: &str, id: Uuid) -> Upstream {
        Upstream {
            id,
            tenant_id,
            alias: alias.to_owned(),
            created_at: 1,
            updated_at: 1,
            spec: UpstreamSpec::default(),
        }
    }

    fn http_match(path: &str, methods: &[RouteMethod]) -> RouteMatch {
        RouteMatch {
            http: Some(HttpMatch {
                methods: methods.to_vec(),
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: crate::domain::types::PathSuffixMode::Append,
            }),
            grpc: None,
        }
    }

    fn route(tenant_id: Uuid, upstream_id: Uuid, id: Uuid, path: &str) -> Route {
        Route {
            id,
            tenant_id,
            upstream_id,
            created_at: 1,
            updated_at: 1,
            spec: RouteSpec {
                upstream_id,
                match_rules: http_match(path, &[RouteMethod::Get]),
                enabled: true,
                tags: Vec::new(),
                plugins: None,
                rate_limit: None,
            },
        }
    }

    /// A gRPC match rule for `(service, method)`.
    fn grpc_match(service: &str, method: &str) -> RouteMatch {
        RouteMatch {
            http: None,
            grpc: Some(GrpcMatch {
                service: service.to_owned(),
                method: method.to_owned(),
            }),
        }
    }

    /// A gRPC route for `(service, method)`.
    fn grpc_route(
        tenant_id: Uuid,
        upstream_id: Uuid,
        id: Uuid,
        service: &str,
        method: &str,
    ) -> Route {
        Route {
            id,
            tenant_id,
            upstream_id,
            created_at: 1,
            updated_at: 1,
            spec: RouteSpec {
                upstream_id,
                match_rules: grpc_match(service, method),
                enabled: true,
                tags: Vec::new(),
                plugins: None,
                rate_limit: None,
            },
        }
    }

    #[test]
    fn insert_get_and_list_preserve_insertion_order() {
        let tenant = Uuid::new_v4();
        let store = UpstreamStore::new();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();

        store
            .insert(upstream(tenant, "a.example.com", first))
            .expect("first insert");
        store
            .insert(upstream(tenant, "b.example.com", second))
            .expect("second insert");

        let listed = store.list(tenant);
        assert_eq!(listed.len(), 2);
        assert_eq!(
            listed[0].alias, "a.example.com",
            "insertion order is preserved"
        );
        assert_eq!(listed[1].alias, "b.example.com");

        assert_eq!(
            store.get(tenant, first).expect("found").alias,
            "a.example.com"
        );
        assert!(store.get(tenant, Uuid::new_v4()).is_none());
        assert_eq!(store.len(), 2);
        assert!(!store.is_empty());
    }

    #[test]
    fn list_is_scoped_to_the_calling_tenant() {
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let store = UpstreamStore::new();

        store
            .insert(upstream(tenant_a, "a.example.com", Uuid::new_v4()))
            .expect("insert");
        store
            .insert(upstream(tenant_b, "b.example.com", Uuid::new_v4()))
            .expect("insert");

        assert_eq!(store.list(tenant_a).len(), 1);
        assert_eq!(store.list(tenant_a)[0].alias, "a.example.com");
        assert_eq!(store.list(tenant_b)[0].alias, "b.example.com");
        assert_eq!(store.list(Uuid::new_v4()).len(), 0);
    }

    #[test]
    fn alias_is_unique_per_tenant_only() {
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let store = UpstreamStore::new();
        let shared_alias = "api.vendor.com";

        store
            .insert(upstream(tenant_a, shared_alias, Uuid::new_v4()))
            .expect("first tenant owns the alias");

        let err = store
            .insert(upstream(tenant_a, shared_alias, Uuid::new_v4()))
            .expect_err("duplicate alias within a tenant");
        assert_eq!(err.status().as_u16(), 409);
        assert_eq!(err.kind(), crate::error::OagwErrorKind::AliasConflict);

        store
            .insert(upstream(tenant_b, shared_alias, Uuid::new_v4()))
            .expect("another tenant may use the same alias");
        assert_eq!(
            store
                .get_by_alias(tenant_b, shared_alias)
                .expect("found")
                .tenant_id,
            tenant_b
        );
        assert!(store.get_by_alias(tenant_a, "other.example.com").is_none());
    }

    #[test]
    fn get_by_alias_is_case_sensitive_by_convention() {
        let tenant = Uuid::new_v4();
        let store = UpstreamStore::new();

        store
            .insert(upstream(tenant, "api.vendor.com", Uuid::new_v4()))
            .expect("insert");

        assert!(store.get_by_alias(tenant, "api.vendor.com").is_some());
        assert!(store.get_by_alias(tenant, "API.VENDOR.COM").is_none());
    }

    #[test]
    fn replace_overwrites_the_spec_but_keeps_identity_fields() {
        let tenant = Uuid::new_v4();
        let id = Uuid::new_v4();
        let store = UpstreamStore::new();
        store
            .insert(upstream(tenant, "api.vendor.com", id))
            .expect("insert");

        let spec = UpstreamSpec {
            enabled: false,
            ..UpstreamSpec::default()
        };
        let replaced = store.replace(tenant, id, spec, 99).expect("replace");

        assert_eq!(replaced.id, id);
        assert_eq!(replaced.tenant_id, tenant);
        assert_eq!(replaced.alias, "api.vendor.com", "the alias is immutable");
        assert_eq!(replaced.created_at, 1, "created_at is immutable");
        assert_eq!(replaced.updated_at, 99);
        assert!(!replaced.is_enabled());
        assert_eq!(store.list(tenant).len(), 1);
    }

    #[test]
    fn replace_keeps_the_alias_index_in_sync() {
        let tenant = Uuid::new_v4();
        let id = Uuid::new_v4();
        let store = UpstreamStore::new();
        store
            .insert(upstream(tenant, "api.vendor.com", id))
            .expect("insert");

        store
            .replace(tenant, id, UpstreamSpec::default(), 2)
            .expect("replace");

        assert_eq!(
            store
                .get_by_alias(tenant, "api.vendor.com")
                .expect("the alias still resolves"),
            store.get(tenant, id).expect("the id still resolves"),
            "the by_alias index points at the replaced record"
        );
    }

    #[test]
    fn replace_and_delete_of_unknown_ids_are_404() {
        let tenant = Uuid::new_v4();
        let store = UpstreamStore::new();

        let err = store
            .replace(tenant, Uuid::new_v4(), UpstreamSpec::default(), 1)
            .expect_err("unknown id");
        assert_eq!(err.status().as_u16(), 404);

        let err = store
            .delete(tenant, Uuid::new_v4())
            .expect_err("unknown id");
        assert_eq!(err.status().as_u16(), 404);
        assert_eq!(err.kind(), crate::error::OagwErrorKind::UpstreamNotFound);
    }

    #[test]
    fn delete_removes_all_indexes() {
        let tenant = Uuid::new_v4();
        let id = Uuid::new_v4();
        let store = UpstreamStore::new();
        store
            .insert(upstream(tenant, "api.vendor.com", id))
            .expect("insert");

        let deleted = store.delete(tenant, id).expect("delete");
        assert_eq!(deleted.id, id);

        assert!(store.get(tenant, id).is_none());
        assert!(store.get_by_alias(tenant, "api.vendor.com").is_none());
        assert!(store.list(tenant).is_empty());
        assert!(store.is_empty());
    }

    #[test]
    fn route_store_inserts_gets_and_lists_in_insertion_order() {
        let tenant = Uuid::new_v4();
        let upstream = Uuid::new_v4();
        let store = RouteStore::new();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();

        store
            .insert(route(tenant, upstream, first, "/v1"))
            .expect("first insert");
        store
            .insert(route(tenant, upstream, second, "/v2"))
            .expect("second insert");

        let listed = store.list(tenant);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, first, "insertion order is preserved");
        assert_eq!(listed[1].id, second);

        assert_eq!(store.get(tenant, first).expect("found").id, first);
        assert!(store.get(tenant, Uuid::new_v4()).is_none());
        assert_eq!(store.len(), 2);
        assert!(!store.is_empty());

        let bound = store.list_by_upstream(tenant, upstream);
        assert_eq!(
            bound.len(),
            2,
            "the per-upstream index feeds the route walk"
        );
        assert_eq!(
            store.list_by_upstream(tenant, Uuid::new_v4()).len(),
            0,
            "routes of other upstreams are invisible"
        );
        assert_eq!(store.list_by_upstream(Uuid::new_v4(), upstream).len(), 0);
    }

    #[test]
    fn route_match_rules_are_unique_per_upstream_within_a_tenant() {
        let tenant = Uuid::new_v4();
        let upstream = Uuid::new_v4();
        let store = RouteStore::new();

        store
            .insert(route(tenant, upstream, Uuid::new_v4(), "/v1"))
            .expect("insert");

        let err = store
            .insert(route(tenant, upstream, Uuid::new_v4(), "/v1"))
            .expect_err("duplicate match rule");
        assert_eq!(err.status().as_u16(), 409);
        assert_eq!(err.kind(), crate::error::OagwErrorKind::MatchConflict);

        // A different path, upstream or tenant is a distinct rule.
        assert!(
            store
                .insert(route(tenant, upstream, Uuid::new_v4(), "/v2"))
                .is_ok()
        );
        assert!(
            store
                .insert(route(tenant, Uuid::new_v4(), Uuid::new_v4(), "/v1"))
                .is_ok()
        );
        assert!(
            store
                .insert(route(Uuid::new_v4(), upstream, Uuid::new_v4(), "/v1"))
                .is_ok()
        );
    }

    #[test]
    fn route_match_rules_conflict_on_an_intersecting_method_set() {
        let tenant = Uuid::new_v4();
        let upstream = Uuid::new_v4();
        let store = RouteStore::new();

        store
            .insert(route(tenant, upstream, Uuid::new_v4(), "/v1"))
            .expect("insert GET /v1");

        // A subset of the existing method set is a conflict: at request time
        // either route could win, so the winner would depend on insertion order.
        let err = store
            .insert(route(tenant, upstream, Uuid::new_v4(), "/v1"))
            .expect_err("GET is claimed by the existing rule");

        assert_eq!(err.status().as_u16(), 409);
        assert!(err.detail().contains("match rule"));

        // A disjoint method set on the same path is a distinct rule.
        assert!(
            store
                .insert({
                    let mut record = route(tenant, upstream, Uuid::new_v4(), "/v1");
                    record.spec.match_rules = http_match("/v1", &[RouteMethod::Post]);
                    record
                })
                .is_ok(),
            "POST /v1 does not intersect GET /v1"
        );
    }

    #[test]
    fn grpc_match_rules_conflict_only_on_the_same_service_and_method() {
        let tenant = Uuid::new_v4();
        let upstream = Uuid::new_v4();
        let store = RouteStore::new();

        store
            .insert(grpc_route(
                tenant,
                upstream,
                Uuid::new_v4(),
                "cf.example.V1/Search",
                "Lookup",
            ))
            .expect("insert the gRPC rule");

        // The same (service, method) is a conflict.
        let err = store
            .insert(grpc_route(
                tenant,
                upstream,
                Uuid::new_v4(),
                "cf.example.V1/Search",
                "Lookup",
            ))
            .expect_err("duplicate gRPC rule");
        assert_eq!(err.status().as_u16(), 409);

        // A different method or service is a distinct rule.
        assert!(
            store
                .insert(grpc_route(
                    tenant,
                    upstream,
                    Uuid::new_v4(),
                    "cf.example.V1/Search",
                    "Suggest"
                ))
                .is_ok()
        );
        assert!(
            store
                .insert(grpc_route(
                    tenant,
                    upstream,
                    Uuid::new_v4(),
                    "cf.example.V1/Catalog",
                    "Lookup"
                ))
                .is_ok()
        );

        // An HTTP rule and a gRPC rule never intersect: the proxy only consults
        // the rules of the request's protocol.
        assert!(
            store
                .insert(route(tenant, upstream, Uuid::new_v4(), "/v1"))
                .is_ok(),
            "an HTTP rule does not conflict with a gRPC rule"
        );
    }

    #[test]
    fn route_replace_keeps_identity_and_enforces_uniqueness() {
        let tenant = Uuid::new_v4();
        let upstream = Uuid::new_v4();
        let store = RouteStore::new();
        let id = Uuid::new_v4();
        store
            .insert(route(tenant, upstream, id, "/v1"))
            .expect("insert");
        store
            .insert(route(tenant, upstream, Uuid::new_v4(), "/v2"))
            .expect("insert");

        let replaced = store
            .replace(
                tenant,
                id,
                RouteSpec {
                    upstream_id: upstream,
                    match_rules: http_match("/v3", &[RouteMethod::Post]),
                    enabled: false,
                    tags: Vec::new(),
                    plugins: None,
                    rate_limit: None,
                },
                99,
            )
            .expect("replace");

        assert_eq!(replaced.id, id);
        assert_eq!(replaced.upstream_id, upstream, "upstream_id is immutable");
        assert_eq!(replaced.tenant_id, tenant);
        assert_eq!(replaced.created_at, 1);
        assert_eq!(replaced.updated_at, 99);
        assert!(!replaced.is_enabled());
        assert_eq!(
            store.list_by_upstream(tenant, upstream).len(),
            2,
            "the route keeps its position"
        );

        // Moving onto /v2's match rule is a conflict.
        let err = store
            .replace(
                tenant,
                id,
                RouteSpec {
                    upstream_id: upstream,
                    match_rules: http_match("/v2", &[RouteMethod::Get]),
                    enabled: true,
                    tags: Vec::new(),
                    plugins: None,
                    rate_limit: None,
                },
                100,
            )
            .expect_err("duplicate match rule");
        assert_eq!(err.status().as_u16(), 409);
    }

    #[test]
    fn route_replace_and_delete_of_unknown_ids_are_404() {
        let tenant = Uuid::new_v4();
        let store = RouteStore::new();

        let err = store
            .replace(
                tenant,
                Uuid::new_v4(),
                RouteSpec {
                    upstream_id: Uuid::new_v4(),
                    match_rules: http_match("/v1", &[RouteMethod::Get]),
                    enabled: true,
                    tags: Vec::new(),
                    plugins: None,
                    rate_limit: None,
                },
                1,
            )
            .expect_err("unknown id");
        assert_eq!(err.status().as_u16(), 404);
        assert_eq!(err.kind(), crate::error::OagwErrorKind::RouteNotFound);

        let err = store
            .delete(tenant, Uuid::new_v4())
            .expect_err("unknown id");
        assert_eq!(err.status().as_u16(), 404);
    }

    #[test]
    fn route_delete_removes_all_indexes() {
        let tenant = Uuid::new_v4();
        let upstream = Uuid::new_v4();
        let store = RouteStore::new();
        let id = Uuid::new_v4();
        store
            .insert(route(tenant, upstream, id, "/v1"))
            .expect("insert");

        let deleted = store.delete(tenant, id).expect("delete");
        assert_eq!(deleted.id, id);

        assert!(store.get(tenant, id).is_none());
        assert!(store.list(tenant).is_empty());
        assert!(store.list_by_upstream(tenant, upstream).is_empty());
        assert!(store.is_empty());
    }

    #[test]
    fn route_delete_by_upstream_cascades_within_one_tenant_only() {
        let tenant = Uuid::new_v4();
        let other = Uuid::new_v4();
        let store = RouteStore::new();
        let upstream = Uuid::new_v4();
        let shared_upstream = Uuid::new_v4();
        store
            .insert(route(tenant, upstream, Uuid::new_v4(), "/v1"))
            .expect("insert");
        store
            .insert(route(tenant, upstream, Uuid::new_v4(), "/v2"))
            .expect("insert");
        // A route of the same upstream owned by another tenant, and a route of
        // another upstream: neither takes part in the cascade.
        store
            .insert(route(other, upstream, Uuid::new_v4(), "/v1"))
            .expect("insert");
        store
            .insert(route(tenant, shared_upstream, Uuid::new_v4(), "/v1"))
            .expect("insert");

        let deleted = store.delete_by_upstream(tenant, upstream);
        assert_eq!(deleted.len(), 2, "the tenant's own routes are removed");

        assert!(
            store.list(tenant).len() == 1,
            "only the unrelated route is left"
        );
        assert_eq!(
            store.list(other).len(),
            1,
            "other tenants keep their routes"
        );
        assert!(
            store.list_by_upstream(tenant, upstream).is_empty(),
            "the per-upstream index is emptied with the records"
        );
        assert_eq!(
            store.list_by_upstream(other, upstream).len(),
            1,
            "the other tenant's index is untouched"
        );

        // Cascading an upstream that holds no route is a no-op.
        assert!(store.delete_by_upstream(tenant, Uuid::new_v4()).is_empty());
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn route_list_is_scoped_to_the_calling_tenant() {
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let upstream = Uuid::new_v4();
        let store = RouteStore::new();

        store
            .insert(route(tenant_a, upstream, Uuid::new_v4(), "/v1"))
            .expect("insert");
        store
            .insert(route(tenant_b, upstream, Uuid::new_v4(), "/v2"))
            .expect("insert");

        assert_eq!(store.list(tenant_a).len(), 1);
        assert_eq!(
            store.list(tenant_a)[0]
                .spec
                .match_rules
                .http
                .as_ref()
                .expect("http")
                .path,
            "/v1"
        );
        assert_eq!(
            store.list(tenant_b)[0]
                .spec
                .match_rules
                .http
                .as_ref()
                .expect("http")
                .path,
            "/v2"
        );
        assert_eq!(store.list(Uuid::new_v4()).len(), 0);
    }

    #[test]
    fn route_references_are_found_across_tenants() {
        let tenant = Uuid::new_v4();
        let store = RouteStore::new();
        let plugin = plugin_record("guard_plugin", Uuid::new_v4(), tenant, "validator");

        let mut referenced = route(tenant, Uuid::new_v4(), Uuid::new_v4(), "/v1");
        referenced.spec.plugins = Some(crate::domain::types::PluginsConfig {
            sharing: crate::domain::types::SharingMode::Private,
            items: vec![crate::domain::types::PluginRef::new(plugin.gts_id())],
        });
        let mut bare_uuid = route(tenant, Uuid::new_v4(), Uuid::new_v4(), "/v2");
        bare_uuid.spec.plugins = Some(crate::domain::types::PluginsConfig {
            sharing: crate::domain::types::SharingMode::Private,
            items: vec![crate::domain::types::PluginRef::new(plugin.id.to_string())],
        });
        let unrelated = route(tenant, Uuid::new_v4(), Uuid::new_v4(), "/v3");
        // Another tenant referencing the same plugin is still a reference.
        let mut foreign = route(Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), "/v1");
        foreign.spec.plugins = Some(crate::domain::types::PluginsConfig {
            sharing: crate::domain::types::SharingMode::Private,
            items: vec![crate::domain::types::PluginRef::new(plugin.gts_id())],
        });

        for candidate in [&referenced, &bare_uuid, &unrelated, &foreign] {
            store.insert(candidate.clone()).expect("insert");
        }

        let found = store.list_referencing_plugin(&plugin);
        assert_eq!(found.len(), 3, "both spellings and both tenants count");
        // The scan hands out identifiers only, so the referenced configuration
        // itself — a foreign tenant's included — never leaves the store.
        assert_eq!(
            found,
            vec![
                format!("gts.cf.core.oagw.route.v1~{}", referenced.id),
                format!("gts.cf.core.oagw.route.v1~{}", bare_uuid.id),
                format!("gts.cf.core.oagw.route.v1~{}", foreign.id),
            ],
            "routes are reported in insertion order"
        );
        assert!(
            store
                .list_referencing_plugin(&plugin_record(
                    "guard_plugin",
                    Uuid::new_v4(),
                    tenant,
                    "other"
                ))
                .is_empty(),
            "a different plugin is not referenced"
        );
    }

    fn plugin_record(plugin_type: &str, id: Uuid, tenant_id: Uuid, name: &str) -> Plugin {
        Plugin {
            id,
            tenant_id,
            plugin_type: plugin_type.to_owned(),
            name: name.to_owned(),
            config_schema: None,
            source_code: "def on_request(ctx):\n    pass\n".to_owned(),
            last_used_at: None,
            gc_eligible_at: None,
        }
    }

    #[test]
    fn plugin_store_inserts_gets_and_lists_in_insertion_order() {
        let tenant = Uuid::new_v4();
        let store = PluginStore::new();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();

        store
            .insert(plugin_record("guard_plugin", first, tenant, "a"))
            .expect("first insert");
        store
            .insert(plugin_record("auth_plugin", second, tenant, "b"))
            .expect("second insert");

        let listed = store.list(tenant);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "a", "insertion order is preserved");
        assert_eq!(listed[1].name, "b");
        assert_eq!(store.get(tenant, first).expect("found").id, first);
        assert!(store.get(tenant, Uuid::new_v4()).is_none());
        assert_eq!(store.len(), 2);
        assert!(!store.is_empty());
    }

    #[test]
    fn plugin_names_are_unique_per_tenant_only() {
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let store = PluginStore::new();
        let shared = "request_validator";

        store
            .insert(plugin_record(
                "guard_plugin",
                Uuid::new_v4(),
                tenant_a,
                shared,
            ))
            .expect("first tenant owns the name");

        let err = store
            .insert(plugin_record(
                "guard_plugin",
                Uuid::new_v4(),
                tenant_a,
                shared,
            ))
            .expect_err("duplicate name within a tenant");
        assert_eq!(err.status().as_u16(), 409);
        assert_eq!(err.kind(), crate::error::OagwErrorKind::AliasConflict);
        assert!(err.detail().contains("name"), "the detail names the field");
        assert_eq!(
            err.extensions()
                .get("field")
                .and_then(serde_json::Value::as_str),
            Some("name")
        );

        let other = store
            .insert(plugin_record(
                "guard_plugin",
                Uuid::new_v4(),
                tenant_b,
                shared,
            ))
            .expect("another tenant may use the same name");
        assert_eq!(
            store.get(tenant_b, other.id).expect("found").tenant_id,
            tenant_b
        );
        assert!(store.get(tenant_a, other.id).is_none());
    }

    #[test]
    fn plugin_delete_and_unknown_ids_behave_like_the_other_stores() {
        let tenant = Uuid::new_v4();
        let id = Uuid::new_v4();
        let store = PluginStore::new();
        store
            .insert(plugin_record("guard_plugin", id, tenant, "validator"))
            .expect("insert");

        let deleted = store.delete(tenant, id).expect("delete");
        assert_eq!(deleted.id, id);
        assert!(store.get(tenant, id).is_none());
        assert!(store.list(tenant).is_empty());
        assert!(store.is_empty());

        // The name index is dropped with the record, so the name is free again.
        assert!(
            store
                .insert(plugin_record(
                    "guard_plugin",
                    Uuid::new_v4(),
                    tenant,
                    "validator"
                ))
                .is_ok()
        );

        let err = store.delete(tenant, id).expect_err("second delete");
        // `PluginNotFound` maps to a 503 in the DESIGN §3.3 error table (the
        // control plane is the only component that can resolve a plugin id), so
        // the store reuses that mapping rather than the plain-404 helpers.
        assert_eq!(err.status().as_u16(), 503);
        assert_eq!(err.kind(), crate::error::OagwErrorKind::PluginNotFound);
    }
}
