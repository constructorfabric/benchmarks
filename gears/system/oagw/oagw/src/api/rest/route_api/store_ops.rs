//! Tenant-scoped lookups against `ConfigStore::routes()`, shared by the
//! `GET`/`PUT`/`DELETE /oagw/v1/routes/{id}` handlers.

use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

use crate::model::route::Route;

/// Route Tenant-Scope Resolution (`cpt-cf-oagw-algo-route-tenant-scope-resolve`),
/// minus the `{id}` normalization step (already done by the caller via
/// [`crate::model::route::Route::normalize_id_param`]).
// @cpt-algo:cpt-cf-oagw-algo-route-tenant-scope-resolve:p2
// @cpt-dod:cpt-cf-oagw-dod-route-tenant-scope:p1
// @cpt-begin:cpt-cf-oagw-algo-route-tenant-scope-resolve:p1:inst-tenantscope-query
// @cpt-begin:cpt-cf-oagw-algo-route-tenant-scope-resolve:p1:inst-tenantscope-notfound-if
// @cpt-begin:cpt-cf-oagw-algo-route-tenant-scope-resolve:p1:inst-tenantscope-else
// @cpt-begin:cpt-cf-oagw-algo-route-tenant-scope-resolve:p1:inst-tenantscope-return
#[must_use]
pub fn find_owned_route(
    routes: &DashMap<Uuid, Arc<Route>>,
    tenant_id: Uuid,
    id: Uuid,
) -> Option<Arc<Route>> {
    let entry = routes.get(&id)?;
    if entry.value().tenant_id == tenant_id {
        Some(Arc::clone(entry.value()))
    } else {
        // @cpt-begin:cpt-cf-oagw-algo-route-tenant-scope-resolve:p1:inst-tenantscope-notfound-return
        None
        // @cpt-end:cpt-cf-oagw-algo-route-tenant-scope-resolve:p1:inst-tenantscope-notfound-return
    }
}
// @cpt-end:cpt-cf-oagw-algo-route-tenant-scope-resolve:p1:inst-tenantscope-return
// @cpt-end:cpt-cf-oagw-algo-route-tenant-scope-resolve:p1:inst-tenantscope-else
// @cpt-end:cpt-cf-oagw-algo-route-tenant-scope-resolve:p1:inst-tenantscope-notfound-if
// @cpt-end:cpt-cf-oagw-algo-route-tenant-scope-resolve:p1:inst-tenantscope-query

/// List every route owned by `tenant_id`
/// (`cpt-cf-oagw-flow-route-list`'s tenant filter, applied directly in the
/// query rather than a per-row not-found branch -- see this algorithm's
/// doc comment in the FEATURE).
#[must_use]
pub fn list_owned_routes(routes: &DashMap<Uuid, Arc<Route>>, tenant_id: Uuid) -> Vec<Route> {
    routes
        .iter()
        .filter(|entry| entry.value().tenant_id == tenant_id)
        .map(|entry| (**entry.value()).clone())
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::route::{HttpMatch, PathSuffixMode, RouteMatch};

    fn sample_route(tenant_id: Uuid, id: Uuid) -> Route {
        Route {
            id: Some(id),
            tenant_id,
            tags: Vec::new(),
            upstream_id: Uuid::new_v4(),
            route_match: RouteMatch {
                http: Some(HttpMatch {
                    methods: vec![crate::model::route::HttpMethod::Get],
                    path: "/p".to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            enabled: true,
            priority: Some(1),
        }
    }

    #[test]
    fn finds_a_route_owned_by_the_calling_tenant() {
        let routes = DashMap::new();
        let tenant_id = Uuid::new_v4();
        let id = Uuid::new_v4();
        routes.insert(id, Arc::new(sample_route(tenant_id, id)));

        assert!(find_owned_route(&routes, tenant_id, id).is_some());
    }

    #[test]
    fn returns_none_for_a_different_tenants_route() {
        let routes = DashMap::new();
        let id = Uuid::new_v4();
        routes.insert(id, Arc::new(sample_route(Uuid::new_v4(), id)));

        assert!(find_owned_route(&routes, Uuid::new_v4(), id).is_none());
    }

    #[test]
    fn returns_none_for_a_missing_id() {
        let routes: DashMap<Uuid, Arc<Route>> = DashMap::new();
        assert!(find_owned_route(&routes, Uuid::new_v4(), Uuid::new_v4()).is_none());
    }

    #[test]
    fn list_owned_routes_excludes_other_tenants() {
        let routes = DashMap::new();
        let tenant_id = Uuid::new_v4();
        let mine = Uuid::new_v4();
        let theirs = Uuid::new_v4();
        routes.insert(mine, Arc::new(sample_route(tenant_id, mine)));
        routes.insert(theirs, Arc::new(sample_route(Uuid::new_v4(), theirs)));

        let listed = list_owned_routes(&routes, tenant_id);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, Some(mine));
    }
}
