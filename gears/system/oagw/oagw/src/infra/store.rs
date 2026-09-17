//! In-memory per-tenant OAGW store ([DESIGN.md](../../docs/DESIGN.md)
//! `cpt-cf-oagw-design-storage`).
//!
//! Every management-API record is scoped to the tenant that owns it: lookups
//! by [`Uuid`] only ever touch that tenant's records, so a tenant can never
//! read, update or delete another tenant's upstream or route. The store is
//! shared through [`std::sync::Arc`] and is safe to share across tasks; no
//! lock is ever held across an `await`.

use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::model::{Alias, Route, Upstream};

/// Records owned by a single tenant.
#[derive(Debug, Default)]
struct TenantRecords {
    /// Upstreams owned by the tenant, keyed by id.
    upstreams: DashMap<Uuid, Upstream>,
    /// Routes owned by the tenant, keyed by id.
    routes: DashMap<Uuid, Route>,
}

/// In-memory store of upstreams and routes, partitioned by tenant.
///
/// The store is shared between the gear state and the request handlers through
/// [`std::sync::Arc`] (see [`crate::gear::OagwState`]); every method takes
/// `&self` and no lock is held across an `await`.
#[derive(Debug, Default)]
pub struct Store {
    tenants: DashMap<Uuid, Arc<TenantRecords>>,
}

impl Store {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the record set of `tenant_id`, creating it on first use.
    fn records(&self, tenant_id: Uuid) -> Arc<TenantRecords> {
        let entry = self.tenants.entry(tenant_id).or_insert_with(|| {
            Arc::new(TenantRecords {
                upstreams: DashMap::new(),
                routes: DashMap::new(),
            })
        });
        Arc::clone(&entry)
    }

    // -- upstreams ----------------------------------------------------------

    /// Inserts (or replaces) an upstream owned by `tenant_id`.
    ///
    /// Returns the previously stored upstream with the same id, if any.
    /// Upstreams without an id are not stored and are returned unchanged.
    pub fn put_upstream(&self, tenant_id: Uuid, upstream: Upstream) -> Option<Upstream> {
        let id = upstream.id?;
        let records = self.records(tenant_id);
        records.upstreams.insert(id, upstream)
    }

    /// Returns the upstream with `id`, when it belongs to `tenant_id`.
    #[must_use]
    pub fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream> {
        let records = self.records(tenant_id);
        records
            .upstreams
            .get(&id)
            .map(|entry| entry.value().clone())
    }

    /// Returns the upstream with `alias`, when it belongs to `tenant_id`.
    #[must_use]
    pub fn find_upstream_by_alias(&self, tenant_id: Uuid, alias: &Alias) -> Option<Upstream> {
        let records = self.records(tenant_id);
        records
            .upstreams
            .iter()
            .find(|entry| {
                entry
                    .value()
                    .alias
                    .as_ref()
                    .is_some_and(|candidate| candidate.as_str() == alias.as_str())
            })
            .map(|entry| entry.value().clone())
    }

    /// Returns `true` when another upstream of `tenant_id` already uses
    /// `alias`.
    #[must_use]
    pub fn upstream_alias_exists(&self, tenant_id: Uuid, alias: &Alias) -> bool {
        self.find_upstream_by_alias(tenant_id, alias).is_some()
    }

    /// Deletes the upstream with `id`; returns `true` when it existed.
    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> bool {
        let records = self.records(tenant_id);
        records.upstreams.remove(&id).is_some()
    }

    /// Lists the upstreams of `tenant_id`, ordered by id.
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Upstream> {
        let records = self.records(tenant_id);
        let mut upstreams: Vec<Upstream> = records
            .upstreams
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        upstreams.sort_by_key(|upstream| upstream.id);
        upstreams
    }

    /// Number of upstreams owned by `tenant_id`.
    #[must_use]
    pub fn upstream_count(&self, tenant_id: Uuid) -> usize {
        let records = self.records(tenant_id);
        records.upstreams.len()
    }

    // -- routes -------------------------------------------------------------

    /// Inserts (or replaces) a route owned by `tenant_id`.
    ///
    /// Returns the previously stored route with the same id, if any. Routes
    /// without an id are not stored and are returned unchanged.
    pub fn put_route(&self, tenant_id: Uuid, route: Route) -> Option<Route> {
        let id = route.id?;
        let records = self.records(tenant_id);
        records.routes.insert(id, route)
    }

    /// Returns the route with `id`, when it belongs to `tenant_id`.
    #[must_use]
    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Option<Route> {
        let records = self.records(tenant_id);
        records.routes.get(&id).map(|entry| entry.value().clone())
    }

    /// Returns every route of `tenant_id` pointing at `upstream_id`, ordered
    /// by id.
    #[must_use]
    pub fn find_routes_by_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Route> {
        let records = self.records(tenant_id);
        let mut routes: Vec<Route> = records
            .routes
            .iter()
            .filter(|entry| entry.value().upstream_id == upstream_id)
            .map(|entry| entry.value().clone())
            .collect();
        routes.sort_by_key(|route| route.id);
        routes
    }

    /// Deletes the route with `id`; returns `true` when it existed.
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> bool {
        let records = self.records(tenant_id);
        records.routes.remove(&id).is_some()
    }

    /// Lists the routes of `tenant_id`, ordered by id.
    #[must_use]
    pub fn list_routes(&self, tenant_id: Uuid) -> Vec<Route> {
        let records = self.records(tenant_id);
        let mut routes: Vec<Route> = records
            .routes
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        routes.sort_by_key(|route| route.id);
        routes
    }

    /// Number of routes owned by `tenant_id`.
    #[must_use]
    pub fn route_count(&self, tenant_id: Uuid) -> usize {
        let records = self.records(tenant_id);
        records.routes.len()
    }

    /// Number of upstreams across every tenant (diagnostics only).
    #[must_use]
    pub fn upstream_count_total(&self) -> usize {
        self.tenants
            .iter()
            .map(|entry| entry.value().upstreams.len())
            .sum()
    }

    /// Number of routes across every tenant (diagnostics only).
    #[must_use]
    pub fn route_count_total(&self) -> usize {
        self.tenants
            .iter()
            .map(|entry| entry.value().routes.len())
            .sum()
    }

    /// Removes every record of every tenant (used by tests and by the reset
    /// path of the management API).
    pub fn clear(&self) {
        self.tenants.clear();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use uuid::Uuid;

    use super::Store;
    use crate::domain::model::{
        Alias, EndpointScheme, HttpMatch, HttpMethod, PathSuffixMode, Protocol, Route, RouteMatch,
        Upstream, UpstreamEndpoint, UpstreamServer,
    };

    fn upstream(id: Uuid, alias: &str) -> Upstream {
        Upstream {
            id: Some(id),
            enabled: true,
            alias: Some(Alias::try_new(alias).expect("valid alias")),
            tags: vec![],
            server: UpstreamServer {
                endpoints: vec![UpstreamEndpoint {
                    scheme: EndpointScheme::Https,
                    host: "api.example.com".to_owned(),
                    port: 443,
                }],
            },
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    fn route(id: Uuid, upstream_id: Uuid) -> Route {
        Route {
            id: Some(id),
            tags: vec![],
            upstream_id,
            match_rule: RouteMatch {
                http: Some(HttpMatch {
                    methods: vec![HttpMethod::Get],
                    path: "/v1/things".to_owned(),
                    query_allowlist: vec![],
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn stores_and_reads_an_upstream_by_id() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        let id = Uuid::new_v4();

        assert!(store.get_upstream(tenant, id).is_none());

        let replaced = store.put_upstream(tenant, upstream(id, "api.example.com"));
        assert!(
            replaced.is_none(),
            "a fresh insert stores no previous record"
        );

        let loaded = store.get_upstream(tenant, id).expect("stored upstream");
        assert_eq!(loaded.alias.expect("alias").as_str(), "api.example.com");
        assert_eq!(store.upstream_count(tenant), 1);
        assert_eq!(store.list_upstreams(tenant).len(), 1);
    }

    #[test]
    fn isolates_records_per_tenant() {
        let store = Store::new();
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let id = Uuid::new_v4();

        store.put_upstream(tenant_a, upstream(id, "api.example.com"));

        // A different tenant with the same id sees nothing at all.
        assert!(store.get_upstream(tenant_b, id).is_none());
        assert_eq!(store.upstream_count(tenant_b), 0);
        assert!(store.list_upstreams(tenant_b).is_empty());
        assert!(!store.delete_upstream(tenant_b, id));
        assert!(store.get_upstream(tenant_a, id).is_some());
    }

    #[test]
    fn finds_upstreams_by_alias_within_a_tenant() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        let id = Uuid::new_v4();

        store.put_upstream(tenant, upstream(id, "api.example.com"));
        let alias = Alias::try_new("api.example.com").expect("valid alias");

        let found = store
            .find_upstream_by_alias(tenant, &alias)
            .expect("by alias");
        assert_eq!(found.id, Some(id));
        assert!(store.upstream_alias_exists(tenant, &alias));

        let other_tenant = Uuid::new_v4();
        assert!(store.find_upstream_by_alias(other_tenant, &alias).is_none());
        assert!(!store.upstream_alias_exists(other_tenant, &alias));
    }

    #[test]
    fn updates_and_deletes_upstreams() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        let id = Uuid::new_v4();

        store.put_upstream(tenant, upstream(id, "api.example.com"));
        let replaced = store.put_upstream(tenant, upstream(id, "api2.example.com"));
        assert_eq!(
            store.upstream_count(tenant),
            1,
            "upsert keeps a single record"
        );
        assert_eq!(
            store
                .get_upstream(tenant, id)
                .expect("upstream")
                .alias
                .expect("alias")
                .as_str(),
            "api2.example.com"
        );
        // The previous record is handed back by the upsert.
        assert_eq!(
            replaced
                .expect("previous record")
                .alias
                .expect("alias")
                .as_str(),
            "api.example.com"
        );

        assert!(store.delete_upstream(tenant, id));
        assert!(!store.delete_upstream(tenant, id));
        assert!(store.get_upstream(tenant, id).is_none());
        assert_eq!(store.upstream_count(tenant), 0);
    }

    #[test]
    fn ignores_records_without_an_id() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        let mut unsaved = upstream(Uuid::new_v4(), "api.example.com");
        unsaved.id = None;

        store.put_upstream(tenant, unsaved.clone());
        assert_eq!(
            store.upstream_count(tenant),
            0,
            "records need an id to be stored"
        );
        assert!(store.put_upstream(tenant, unsaved).is_none());
    }

    #[test]
    fn stores_routes_and_links_them_to_their_upstream() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        let route_id = Uuid::new_v4();

        store.put_route(tenant, route(route_id, upstream_id));

        assert_eq!(
            store
                .get_route(tenant, route_id)
                .expect("route")
                .upstream_id,
            upstream_id
        );
        assert_eq!(store.route_count(tenant), 1);
        assert_eq!(store.find_routes_by_upstream(tenant, upstream_id).len(), 1);

        let other_tenant = Uuid::new_v4();
        assert!(store.get_route(other_tenant, route_id).is_none());
        assert!(
            store
                .find_routes_by_upstream(other_tenant, upstream_id)
                .is_empty()
        );
        assert!(!store.delete_route(other_tenant, route_id));

        assert!(store.delete_route(tenant, route_id));
        assert!(
            store
                .find_routes_by_upstream(tenant, upstream_id)
                .is_empty()
        );
    }

    #[test]
    fn lists_records_in_id_order() {
        let store = Store::new();
        let tenant = Uuid::new_v4();

        // Fixed ids so the expected order is deterministic.
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        // Insert out of order and upsert the second record.
        store.put_upstream(tenant, upstream(second, "api2.example.com"));
        store.put_upstream(tenant, upstream(first, "api1.example.com"));
        store.put_upstream(tenant, upstream(second, "api2b.example.com"));

        let ids: Vec<Uuid> = store
            .list_upstreams(tenant)
            .into_iter()
            .filter_map(|upstream| upstream.id)
            .collect();
        assert_eq!(ids, vec![first, second]);
        assert_eq!(store.upstream_count(tenant), 2);
    }

    #[test]
    fn clear_drops_every_tenant_partition() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        let id = Uuid::new_v4();
        store.put_upstream(tenant, upstream(id, "api.example.com"));
        store.put_route(tenant, route(Uuid::new_v4(), id));

        store.clear();
        assert!(store.get_upstream(tenant, id).is_none());
        assert!(store.list_routes(tenant).is_empty());
    }

    #[test]
    fn arc_handles_share_the_same_partitioned_storage() {
        let store = std::sync::Arc::new(Store::new());
        let handle = std::sync::Arc::clone(&store);
        let tenant = Uuid::new_v4();
        let id = Uuid::new_v4();

        store.put_upstream(tenant, upstream(id, "api.example.com"));
        assert!(handle.get_upstream(tenant, id).is_some());
        assert_eq!(handle.upstream_count(tenant), 1);
    }
}
