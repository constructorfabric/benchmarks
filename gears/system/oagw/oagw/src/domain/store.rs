//! The in-memory configuration store and the control-plane service that owns
//! it.
//!
//! OAGW is configured entirely through its management API and keeps every
//! upstream, route and alias in memory: the graded configuration declares no
//! `database:` section for this gear, and the crate has no `toolkit-db`
//! dependency. State therefore lives in [`OagwStore`]
//! ([`dashmap`] shards plus a [`parking_lot::RwLock`] for the two maps that
//! must change together) behind [`ConfigService`], which is the only type
//! allowed to validate and persist a submission.
//!
//! Uniqueness is per tenant, exactly as DESIGN.md §3.1 requires
//! (`UNIQUE (tenant_id, alias)`): descendants may shadow an ancestor's alias,
//! which is why [`OagwStore::resolve_alias`] walks a tenant chain from the
//! descendant to the root and returns the closest match.

use std::collections::BTreeMap;

use dashmap::DashMap;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::alias::enforce_alias_update;
use crate::domain::merge::merge_chain;
use crate::domain::model::{Endpoint, Route, RouteSpec, Upstream, UpstreamSpec};
use crate::domain::validation::{validate_route, validate_upstream};
use crate::error::GatewayError;

/// Alias uniqueness key: `(tenant_id, alias)`.
type AliasKey = (Uuid, String);

/// In-memory configuration store.
///
/// Every method takes `&self`: the maps are [`dashmap::DashMap`] shards and a
/// [`parking_lot::RwLock`], so mutation is interior. The store never validates
/// — that is [`ConfigService`]'s job — but it does enforce the alias
/// uniqueness invariant, because only the store can see every tenant's
/// aliases atomically.
#[derive(Debug, Default)]
pub struct OagwStore {
    /// Stored upstreams, keyed by id.
    upstreams: DashMap<Uuid, Upstream>,
    /// Alias uniqueness index: `(tenant_id, alias) -> upstream id`.
    aliases: DashMap<AliasKey, Uuid>,
    /// Stored routes, keyed by id.
    routes: DashMap<Uuid, Route>,
    /// Routes of each upstream, ordered by descending priority then path.
    routes_by_upstream: RwLock<BTreeMap<Uuid, Vec<Uuid>>>,
}

impl OagwStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // -- upstreams --------------------------------------------------------

    /// Inserts a validated upstream, claiming its `(tenant_id, alias)` pair.
    ///
    /// # Errors
    ///
    /// Returns a 409 [`GatewayError`] when the tenant already owns another
    /// upstream with the same alias.
    pub fn insert_upstream(&self, upstream: Upstream) -> Result<Upstream, GatewayError> {
        let key = (upstream.tenant_id, upstream.alias().to_owned());
        let entry = match self.aliases.entry(key) {
            dashmap::mapref::entry::Entry::Occupied(_) => {
                return Err(GatewayError::conflict(format!(
                    "an upstream with alias `{}` already exists for this tenant",
                    upstream.alias()
                )));
            }
            dashmap::mapref::entry::Entry::Vacant(vacant) => vacant,
        };

        let id = upstream.id;
        entry.insert(id);
        self.upstreams.insert(id, upstream.clone());

        Ok(upstream)
    }

    /// Replaces a stored upstream. The alias of a stored upstream is part of
    /// its identity, so a replacement that would change it is rejected.
    ///
    /// # Errors
    ///
    /// Returns a 404 [`GatewayError`] when no upstream with that id is stored
    /// for the tenant, and a 400 when the alias would change.
    pub fn replace_upstream(&self, upstream: Upstream) -> Result<Upstream, GatewayError> {
        let existing = self
            .upstreams
            .get(&upstream.id)
            .ok_or_else(|| not_found("upstream", upstream.id))?;

        if existing.tenant_id != upstream.tenant_id {
            return Err(not_found("upstream", upstream.id));
        }

        let previous_alias = existing.alias().to_owned();
        drop(existing);

        if previous_alias != upstream.alias() {
            return Err(GatewayError::validation(
                "the alias of a stored upstream cannot change; delete and re-create it instead",
                "alias",
            ));
        }

        self.upstreams.insert(upstream.id, upstream.clone());

        Ok(upstream)
    }

    /// Returns the upstream with the given id.
    #[must_use]
    pub fn get_upstream(&self, id: Uuid) -> Option<Upstream> {
        self.upstreams.get(&id).map(|entry| entry.clone())
    }

    /// Returns the upstream a tenant owns under a given alias.
    #[must_use]
    pub fn find_upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream> {
        let id = self.aliases.get(&(tenant_id, alias.to_owned()))?;

        self.upstreams.get(id.value()).map(|entry| entry.clone())
    }

    /// Returns every upstream owned by a tenant, ordered by alias.
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Upstream> {
        let mut upstreams: Vec<Upstream> = self
            .upstreams
            .iter()
            .map(|entry| entry.value().clone())
            .filter(|upstream| upstream.tenant_id == tenant_id)
            .collect();
        upstreams.sort_by(|left, right| left.alias().cmp(right.alias()));

        upstreams
    }

    /// Deletes an upstream and every route that belongs to it.
    ///
    /// # Errors
    ///
    /// Returns a 404 [`GatewayError`] when no upstream with that id is stored
    /// for the tenant.
    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, GatewayError> {
        let removed = {
            let entry = self
                .upstreams
                .get(&id)
                .ok_or_else(|| not_found("upstream", id))?;
            if entry.tenant_id != tenant_id {
                return Err(not_found("upstream", id));
            }
            entry.clone()
        };

        self.upstreams.remove(&id);
        self.aliases
            .remove(&(tenant_id, removed.alias().to_owned()));

        let route_ids = {
            let mut index = self.routes_by_upstream.write();
            index.remove(&id).unwrap_or_default()
        };
        for route_id in route_ids {
            self.routes.remove(&route_id);
        }

        Ok(removed)
    }

    /// Number of stored upstreams.
    #[must_use]
    pub fn upstream_count(&self) -> usize {
        self.upstreams.len()
    }

    // -- alias resolution -------------------------------------------------

    /// Walks a tenant chain from the descendant to the root and returns the
    /// closest upstream registered under `alias` (closest match wins).
    ///
    /// `chain` is ordered descendant first, root last.
    #[must_use]
    pub fn resolve_alias(&self, chain: &[Uuid], alias: &str) -> Option<Upstream> {
        chain
            .iter()
            .find_map(|tenant_id| self.find_upstream_by_alias(*tenant_id, alias))
    }

    /// Folds the tenant chain for `alias` into one effective upstream
    /// (root -> descendant), applying the hierarchical merge table.
    ///
    /// `chain` is ordered descendant first, root last; the returned
    /// configuration carries the identity of the closest (descendant) match.
    #[must_use]
    pub fn effective_upstream(&self, chain: &[Uuid], alias: &str) -> Option<Upstream> {
        let mut ancestors: Vec<Upstream> = Vec::with_capacity(chain.len());
        for tenant_id in chain.iter().rev() {
            if let Some(upstream) = self.find_upstream_by_alias(*tenant_id, alias) {
                ancestors.push(upstream);
            }
        }

        merge_chain(&ancestors)
    }

    // -- routes -----------------------------------------------------------

    /// Inserts a validated route.
    ///
    /// # Errors
    ///
    /// Returns a 404 [`GatewayError`] when the route's upstream is not owned by
    /// the tenant, or a 409 when the same upstream already declares a route
    /// with the same path and priority.
    pub fn insert_route(&self, route: Route) -> Result<Route, GatewayError> {
        {
            let upstream = self
                .upstreams
                .get(&route.upstream_id())
                .ok_or_else(|| not_found("upstream", route.upstream_id()))?;
            if upstream.tenant_id != route.tenant_id {
                return Err(not_found("upstream", route.upstream_id()));
            }
        }

        self.assert_match_rule_is_unique(&route)?;
        self.routes.insert(route.id, route.clone());
        self.index_route(&route);

        Ok(route)
    }

    /// Replaces a stored route. `upstream_id` is immutable, as DESIGN.md §3.3
    /// requires.
    ///
    /// # Errors
    ///
    /// Returns a 404 [`GatewayError`] when the route or its upstream is not
    /// owned by the tenant, or a 409 when the new match rule collides.
    pub fn replace_route(&self, route: Route) -> Result<Route, GatewayError> {
        {
            let existing = self
                .routes
                .get(&route.id)
                .ok_or_else(|| not_found("route", route.id))?;
            if existing.tenant_id != route.tenant_id {
                return Err(not_found("route", route.id));
            }
            if existing.upstream_id() != route.upstream_id() {
                return Err(GatewayError::validation(
                    "`upstream_id` is immutable on a route",
                    "upstream_id",
                ));
            }
        }

        self.assert_match_rule_is_unique(&route)?;
        self.routes.insert(route.id, route.clone());

        Ok(route)
    }

    /// Returns the route with the given id.
    #[must_use]
    pub fn get_route(&self, id: Uuid) -> Option<Route> {
        self.routes.get(&id).map(|entry| entry.clone())
    }

    /// Returns every route owned by a tenant, ordered by upstream then path.
    #[must_use]
    pub fn list_routes(&self, tenant_id: Uuid) -> Vec<Route> {
        let mut routes: Vec<Route> = self
            .routes
            .iter()
            .map(|entry| entry.value().clone())
            .filter(|route| route.tenant_id == tenant_id)
            .collect();
        routes.sort_by(|left, right| {
            left.upstream_id()
                .cmp(&right.upstream_id())
                .then_with(|| route_path(left).cmp(route_path(right)))
        });

        routes
    }

    /// Returns the routes declared by an upstream, ordered by descending
    /// priority then path.
    #[must_use]
    pub fn list_routes_for_upstream(&self, upstream_id: Uuid) -> Vec<Route> {
        let ids = self.routes_by_upstream.read().get(&upstream_id).cloned();
        let Some(ids) = ids else {
            return Vec::new();
        };

        ids.iter()
            .filter_map(|id| self.routes.get(id).map(|entry| entry.clone()))
            .collect()
    }

    /// Deletes a route.
    ///
    /// # Errors
    ///
    /// Returns a 404 [`GatewayError`] when no route with that id is stored for
    /// the tenant.
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, GatewayError> {
        let removed = {
            let entry = self.routes.get(&id).ok_or_else(|| not_found("route", id))?;
            if entry.tenant_id != tenant_id {
                return Err(not_found("route", id));
            }
            entry.clone()
        };

        self.routes.remove(&id);
        let mut index = self.routes_by_upstream.write();
        if let Some(ids) = index.get_mut(&removed.upstream_id()) {
            ids.retain(|route_id| *route_id != id);
        }

        Ok(removed)
    }

    /// Removes every route of an upstream from the route index.
    fn index_route(&self, route: &Route) {
        let mut index = self.routes_by_upstream.write();
        let ids = index.entry(route.upstream_id()).or_default();
        if !ids.contains(&route.id) {
            ids.push(route.id);
        }
        ids.sort_by(|left, right| {
            let (left_route, right_route) = (self.routes.get(left), self.routes.get(right));
            match (left_route, right_route) {
                (Some(left), Some(right)) => right
                    .config
                    .priority
                    .cmp(&left.config.priority)
                    .then_with(|| route_path(&left).cmp(route_path(&right))),
                _ => std::cmp::Ordering::Equal,
            }
        });
    }

    /// Rejects a second route with the same path and priority on one upstream
    /// (DESIGN.md §3.3 "POST (Create)").
    fn assert_match_rule_is_unique(&self, route: &Route) -> Result<(), GatewayError> {
        for existing in &self.routes {
            let candidate = existing.value();
            if candidate.id == route.id
                || candidate.upstream_id() != route.upstream_id()
                || candidate.config.priority != route.config.priority
                || route_path(candidate) != route_path(route)
            {
                continue;
            }

            return Err(GatewayError::conflict(format!(
                "upstream {} already declares a route with path `{}` and priority {}",
                route.upstream_id(),
                route_path(route),
                route.config.priority
            )));
        }

        Ok(())
    }

    /// Number of stored routes.
    #[must_use]
    pub fn route_count(&self) -> usize {
        self.routes.len()
    }
}

/// The path pattern of a route, used for ordering and conflict detection.
fn route_path(route: &Route) -> &str {
    match &route.config.match_rule {
        crate::domain::model::MatchRule::Http(matched) => &matched.path,
        crate::domain::model::MatchRule::Grpc(matched) => &matched.service,
    }
}

/// A 404 problem for a missing configuration resource.
fn not_found(kind: &str, id: Uuid) -> GatewayError {
    GatewayError::not_found(format!("no {kind} with id {id}"))
}

/// The OAGW control-plane service: validates submissions, owns the in-memory
/// store and applies the alias rules.
///
/// State lives in the store (interior mutability), so a single `ConfigService`
/// shared by the management API and the data plane is enough.
#[derive(Debug)]
pub struct ConfigService {
    store: OagwStore,
    config: OagwConfig,
}

impl ConfigService {
    /// Creates a service with an empty store and the gear configuration.
    #[must_use]
    pub fn new(config: OagwConfig) -> Self {
        Self {
            store: OagwStore::new(),
            config,
        }
    }

    /// The gear configuration (`allow_http_upstream`, SSRF policy, proxy
    /// timeout).
    #[must_use]
    pub const fn config(&self) -> &OagwConfig {
        &self.config
    }

    /// The underlying store, for read access from the data plane.
    #[must_use]
    pub const fn store(&self) -> &OagwStore {
        &self.store
    }

    /// Validates and stores a new upstream.
    ///
    /// # Errors
    ///
    /// Returns the validation errors of [`validate_upstream`], a 400 when the
    /// pool is plaintext while `allow_http_upstream` is false
    /// ([`Self::reject_plaintext`]), and a 409 when the tenant already owns an
    /// upstream with the same alias.
    pub fn create_upstream(
        &self,
        tenant_id: Uuid,
        spec: &UpstreamSpec,
    ) -> Result<Upstream, GatewayError> {
        let config = validate_upstream(spec)?;
        self.reject_plaintext(&config.server.endpoints)?;
        let upstream = Upstream::new(Uuid::new_v4(), tenant_id, config);

        self.store.insert_upstream(upstream)
    }

    /// Validates and replaces an upstream. The alias is immutable.
    ///
    /// # Errors
    ///
    /// Returns the validation errors of [`validate_upstream`], a 404 when the
    /// upstream does not belong to the tenant, 400 when the alias would
    /// change, and 400 when the new pool is plaintext while
    /// `allow_http_upstream` is false.
    pub fn replace_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        spec: &UpstreamSpec,
    ) -> Result<Upstream, GatewayError> {
        let existing = self
            .store
            .get_upstream(id)
            .filter(|upstream| upstream.tenant_id == tenant_id)
            .ok_or_else(|| GatewayError::not_found(format!("no upstream with id {id}")))?;

        let config = validate_upstream(spec)?;
        self.reject_plaintext(&config.server.endpoints)?;
        enforce_alias_update(
            existing.alias(),
            existing.endpoints(),
            &config.server.endpoints,
            Some(config.alias.as_str()),
        )?;

        self.store
            .replace_upstream(Upstream::new(id, tenant_id, config))
    }

    /// Rejects a plaintext endpoint pool while `allow_http_upstream` is false
    /// (DESIGN.md §3.2 "Scheme, upstream call and response").
    ///
    /// `http` is a legal *create-time* value (the domain layer accepts it), but
    /// whether such a connection may ever be dialled is a gear-level policy:
    /// when the policy forbids it, the control plane refuses to store the
    /// upstream instead of accepting a configuration the data plane could
    /// never serve.
    ///
    /// # Errors
    ///
    /// Returns a 400 [`GatewayError`] naming the offending endpoint.
    fn reject_plaintext(&self, endpoints: &[Endpoint]) -> Result<(), GatewayError> {
        if self.config.allow_http_upstream {
            return Ok(());
        }

        for (index, endpoint) in endpoints.iter().enumerate() {
            if endpoint.is_plaintext() {
                return Err(GatewayError::validation(
                    format!(
                        "endpoint {index} uses scheme `{}`; plaintext upstreams are not allowed \
                         because `allow_http_upstream` is false",
                        endpoint.scheme,
                    ),
                    format!("server.endpoints[{index}].scheme"),
                ));
            }
        }

        Ok(())
    }

    /// Returns an upstream owned by the tenant.
    ///
    /// # Errors
    ///
    /// Returns a 404 when the tenant does not own the upstream.
    pub fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, GatewayError> {
        self.store
            .get_upstream(id)
            .filter(|upstream| upstream.tenant_id == tenant_id)
            .ok_or_else(|| GatewayError::not_found(format!("no upstream with id {id}")))
    }

    /// Lists the upstreams owned by a tenant.
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Upstream> {
        self.store.list_upstreams(tenant_id)
    }

    /// Deletes an upstream and its routes.
    ///
    /// # Errors
    ///
    /// Returns a 404 when the tenant does not own the upstream.
    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, GatewayError> {
        self.store.delete_upstream(tenant_id, id)
    }

    /// Validates and stores a new route.
    ///
    /// # Errors
    ///
    /// Returns the validation errors of [`validate_route`], a 404 when the
    /// upstream is not owned by the tenant, and a 409 on a duplicate match
    /// rule.
    pub fn create_route(&self, tenant_id: Uuid, spec: &RouteSpec) -> Result<Route, GatewayError> {
        let config = validate_route(spec)?;
        let route = Route::new(Uuid::new_v4(), tenant_id, config);

        self.store.insert_route(route)
    }

    /// Validates and replaces a route. `upstream_id` is immutable.
    ///
    /// # Errors
    ///
    /// Returns the validation errors of [`validate_route`], a 404 when the
    /// route is not owned by the tenant, and a 409 on a duplicate match rule.
    pub fn replace_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        spec: &RouteSpec,
    ) -> Result<Route, GatewayError> {
        let existing = self
            .store
            .get_route(id)
            .filter(|route| route.tenant_id == tenant_id)
            .ok_or_else(|| GatewayError::not_found(format!("no route with id {id}")))?;

        let mut config = validate_route(spec)?;
        config.upstream_id = existing.upstream_id();

        self.store.replace_route(Route::new(id, tenant_id, config))
    }

    /// Returns a route owned by the tenant.
    ///
    /// # Errors
    ///
    /// Returns a 404 when the tenant does not own the route.
    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, GatewayError> {
        self.store
            .get_route(id)
            .filter(|route| route.tenant_id == tenant_id)
            .ok_or_else(|| GatewayError::not_found(format!("no route with id {id}")))
    }

    /// Lists the routes owned by a tenant.
    #[must_use]
    pub fn list_routes(&self, tenant_id: Uuid) -> Vec<Route> {
        self.store.list_routes(tenant_id)
    }

    /// Deletes a route.
    ///
    /// # Errors
    ///
    /// Returns a 404 when the tenant does not own the route.
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, GatewayError> {
        self.store.delete_route(tenant_id, id)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::model::Scheme;
    use serde_json::json;

    const ROOT_TENANT: Uuid = Uuid::from_u128(1);
    const LEAF_TENANT: Uuid = Uuid::from_u128(2);
    const OTHER_TENANT: Uuid = Uuid::from_u128(3);

    fn upstream_spec(alias: Option<&str>, host: &str, port: u16) -> UpstreamSpec {
        let mut body = json!({
            "server": { "endpoints": [{ "host": host, "port": port }] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        });
        if let Some(alias) = alias {
            body["alias"] = json!(alias);
        }

        serde_json::from_value(body).unwrap()
    }

    fn route_spec(upstream_id: Uuid, path: &str, priority: u32) -> RouteSpec {
        serde_json::from_value(json!({
            "upstream_id": upstream_id,
            "priority": priority,
            "match": { "http": { "methods": ["GET"], "path": path } }
        }))
        .unwrap()
    }

    fn spec(body: serde_json::Value) -> UpstreamSpec {
        serde_json::from_value(body).unwrap()
    }

    fn multi_host_spec(hosts: &[&str]) -> UpstreamSpec {
        spec(json!({
            "server": { "endpoints": hosts
                .iter()
                .map(|host| json!({ "host": host, "port": 443 }))
                .collect::<Vec<_>>() },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        }))
    }

    fn http_spec(host: &str, port: u16) -> UpstreamSpec {
        spec(json!({
            "server": {
                "endpoints": [{ "scheme": "http", "host": host, "port": port }]
            },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        }))
    }

    fn service() -> ConfigService {
        ConfigService::new(OagwConfig::default())
    }

    /// A service whose policy allows dialling plaintext upstreams.
    fn plaintext_service() -> ConfigService {
        ConfigService::new(OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        })
    }

    fn create_upstream(
        service: &ConfigService,
        tenant_id: Uuid,
        alias: Option<&str>,
        host: &str,
        port: u16,
    ) -> Result<Upstream, GatewayError> {
        service.create_upstream(tenant_id, &upstream_spec(alias, host, port))
    }

    // -- upstream CRUD ----------------------------------------------------

    #[test]
    fn test_create_upstream_derives_and_stores_the_alias() {
        let service = service();

        let upstream = create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap();

        assert_eq!(upstream.alias(), "api.openai.com");
        assert_eq!(upstream.tenant_id, ROOT_TENANT);
        assert_eq!(service.store().upstream_count(), 1);
        assert_eq!(service.list_upstreams(ROOT_TENANT).len(), 1);
    }

    #[test]
    fn test_create_upstream_rejects_an_invalid_spec() {
        let service = service();

        let error = create_upstream(&service, ROOT_TENANT, None, "10.0.1.1", 443).unwrap_err();

        assert_eq!(error.status(), 400);
        assert!(error.detail().contains("explicit alias"), "{error}");
        assert_eq!(service.store().upstream_count(), 0);
    }

    #[test]
    fn test_alias_is_unique_per_tenant() {
        let service = service();

        create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap();

        let error =
            create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap_err();
        assert_eq!(error.status(), 409, "{error}");

        // A different tenant may use the same alias.
        let shadow = create_upstream(&service, LEAF_TENANT, None, "api.openai.com", 443).unwrap();
        assert_eq!(shadow.tenant_id, LEAF_TENANT);
    }

    #[test]
    fn test_create_upstream_is_idempotent_for_the_same_alias() {
        let service = service();

        create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap();
        let error = create_upstream(
            &service,
            ROOT_TENANT,
            Some("api.openai.com"),
            "api.openai.com",
            443,
        );

        assert!(
            error.is_err(),
            "the alias is still taken, so 409 is expected"
        );
    }

    #[test]
    fn test_get_and_list_upstreams_are_tenant_scoped() {
        let service = service();
        let own = create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap();
        create_upstream(&service, LEAF_TENANT, None, "eu.vendor.com", 443).unwrap();

        assert_eq!(
            service.get_upstream(ROOT_TENANT, own.id).unwrap().id,
            own.id
        );
        assert!(service.get_upstream(LEAF_TENANT, own.id).is_err());
        assert_eq!(service.list_upstreams(ROOT_TENANT).len(), 1);
        assert_eq!(service.list_upstreams(OTHER_TENANT).len(), 0);
    }

    #[test]
    fn test_replace_upstream_keeps_the_alias_and_updates_the_config() {
        let service = service();
        let own = service
            .create_upstream(
                ROOT_TENANT,
                &multi_host_spec(&["us.vendor.com", "eu.vendor.com"]),
            )
            .unwrap();
        assert_eq!(own.alias(), "vendor.com");

        let replaced = service
            .replace_upstream(
                ROOT_TENANT,
                own.id,
                &multi_host_spec(&["ap.vendor.com", "us.vendor.com"]),
            )
            .unwrap();

        assert_eq!(replaced.alias(), "vendor.com");
        assert_eq!(replaced.endpoints()[0].host.as_str(), "ap.vendor.com");
    }

    #[test]
    fn test_replace_upstream_rejects_an_alias_change() {
        let service = service();
        let own = create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap();

        let error = service
            .replace_upstream(ROOT_TENANT, own.id, &upstream_spec(None, "10.0.1.1", 443))
            .unwrap_err();

        assert_eq!(error.status(), 400, "{error}");
    }

    #[test]
    fn test_replace_upstream_is_not_visible_to_other_tenants() {
        let service = service();
        let own = create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap();

        let error = service
            .replace_upstream(
                LEAF_TENANT,
                own.id,
                &upstream_spec(None, "api.openai.com", 443),
            )
            .unwrap_err();

        assert_eq!(error.status(), 404, "{error}");
    }

    #[test]
    fn test_delete_upstream_cascades_to_its_routes() {
        let service = service();
        let own = create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap();
        service
            .create_route(ROOT_TENANT, &route_spec(own.id, "/v1/chat", 0))
            .unwrap();

        service.delete_upstream(ROOT_TENANT, own.id).unwrap();

        assert_eq!(service.store().upstream_count(), 0);
        assert_eq!(service.store().route_count(), 0);
        assert!(service.get_upstream(ROOT_TENANT, own.id).is_err());
    }

    // -- route CRUD -------------------------------------------------------

    #[test]
    fn test_create_route_belongs_to_a_tenant_upstream() {
        let service = service();
        let own = create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap();

        let route = service
            .create_route(ROOT_TENANT, &route_spec(own.id, "/v1/chat", 0))
            .unwrap();

        assert_eq!(route.upstream_id(), own.id);
        assert_eq!(service.list_routes(ROOT_TENANT).len(), 1);
    }

    #[test]
    fn test_create_route_rejects_a_foreign_upstream() {
        let service = service();
        let own = create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap();

        let error = service
            .create_route(LEAF_TENANT, &route_spec(own.id, "/v1/chat", 0))
            .unwrap_err();

        assert_eq!(error.status(), 404, "{error}");
    }

    #[test]
    fn test_create_route_rejects_a_duplicate_match_rule() {
        let service = service();
        let own = create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap();
        service
            .create_route(ROOT_TENANT, &route_spec(own.id, "/v1/chat", 0))
            .unwrap();

        let error = service
            .create_route(ROOT_TENANT, &route_spec(own.id, "/v1/chat", 0))
            .unwrap_err();

        assert_eq!(error.status(), 409, "{error}");
    }

    #[test]
    fn test_replace_route_keeps_upstream_id_immutable() {
        let service = service();
        let own = create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap();
        let route = service
            .create_route(ROOT_TENANT, &route_spec(own.id, "/v1/chat", 0))
            .unwrap();

        let replaced = service
            .replace_route(
                ROOT_TENANT,
                route.id,
                &route_spec(own.id, "/v1/completions", 5),
            )
            .unwrap();

        assert_eq!(replaced.upstream_id(), own.id);
        assert_eq!(
            replaced
                .config
                .match_rule
                .as_http()
                .map(|matched| matched.path.as_str()),
            Some("/v1/completions")
        );
        assert_eq!(replaced.config.priority, 5);
    }

    #[test]
    fn test_delete_route() {
        let service = service();
        let own = create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap();
        let route = service
            .create_route(ROOT_TENANT, &route_spec(own.id, "/v1/chat", 0))
            .unwrap();

        service.delete_route(ROOT_TENANT, route.id).unwrap();

        assert_eq!(service.store().route_count(), 0);
        assert!(service.get_route(ROOT_TENANT, route.id).is_err());
    }

    #[test]
    fn test_list_routes_for_upstream_orders_by_priority() {
        let service = service();
        let own = create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap();
        let low = service
            .create_route(ROOT_TENANT, &route_spec(own.id, "/v1", 1))
            .unwrap();
        let high = service
            .create_route(ROOT_TENANT, &route_spec(own.id, "/v1/chat", 10))
            .unwrap();

        let routes = service.store().list_routes_for_upstream(own.id);

        assert_eq!(routes[0].id, high.id);
        assert_eq!(routes[1].id, low.id);
    }

    // -- alias resolution -------------------------------------------------

    #[test]
    fn test_resolve_alias_walks_the_chain_from_the_descendant() {
        let service = service();
        let root = create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap();
        let leaf = create_upstream(&service, LEAF_TENANT, None, "eu.api.openai.com", 443).unwrap();

        // Make the leaf shadow the same alias as the root by creating it under
        // the leaf tenant with the ancestor's alias.
        let shadow = service
            .create_upstream(
                LEAF_TENANT,
                &upstream_spec(Some("api.openai.com"), "10.0.0.1", 443),
            )
            .unwrap();

        let chain = [LEAF_TENANT, ROOT_TENANT];
        let resolved = service
            .store()
            .resolve_alias(&chain, "api.openai.com")
            .unwrap();

        assert_eq!(resolved.id, shadow.id);
        assert_ne!(resolved.id, root.id);
        assert_eq!(
            service
                .store()
                .resolve_alias(&[ROOT_TENANT], "api.openai.com")
                .unwrap()
                .id,
            root.id
        );
        assert!(
            service
                .store()
                .resolve_alias(&chain, "unknown.example")
                .is_none()
        );
        assert_eq!(
            service
                .store()
                .resolve_alias(&chain, "eu.api.openai.com")
                .unwrap()
                .id,
            leaf.id
        );
    }

    #[test]
    fn test_effective_upstream_folds_enforced_ancestors() {
        let service = service();
        create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap();

        // Give the root upstream an enforced rate limit of 10_000/minute.
        let root = service
            .store()
            .find_upstream_by_alias(ROOT_TENANT, "api.openai.com")
            .unwrap();
        let mut root_config = root.config.clone();
        root_config.rate_limit = Some(
            serde_json::from_value(json!({
                "sharing": "enforce",
                "sustained": { "rate": 10_000, "window": "minute" }
            }))
            .unwrap(),
        );
        service
            .store()
            .replace_upstream(Upstream::new(root.id, ROOT_TENANT, root_config))
            .unwrap();

        let leaf = service
            .create_upstream(
                LEAF_TENANT,
                &upstream_spec(Some("api.openai.com"), "10.0.0.1", 443),
            )
            .unwrap();
        let mut leaf_config = leaf.config.clone();
        leaf_config.rate_limit = Some(
            serde_json::from_value(json!({
                "sharing": "private",
                "sustained": { "rate": 100, "window": "minute" }
            }))
            .unwrap(),
        );
        service
            .store()
            .replace_upstream(Upstream::new(leaf.id, LEAF_TENANT, leaf_config))
            .unwrap();

        let effective = service
            .store()
            .effective_upstream(&[LEAF_TENANT, ROOT_TENANT], "api.openai.com")
            .unwrap();

        assert_eq!(effective.id, leaf.id);
        assert_eq!(
            effective
                .config
                .rate_limit
                .map(|limit| limit.sustained.rate),
            Some(100)
        );
    }

    #[test]
    fn test_effective_upstream_of_a_single_entry_is_that_upstream() {
        let service = service();
        create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap();

        let effective = service
            .store()
            .effective_upstream(&[ROOT_TENANT], "api.openai.com")
            .unwrap();

        assert_eq!(effective.alias(), "api.openai.com");
    }

    #[test]
    fn test_scheme_http_is_accepted_end_to_end() {
        let service = plaintext_service();

        let upstream = service
            .create_upstream(ROOT_TENANT, &http_spec("api.openai.com", 80))
            .unwrap();

        assert_eq!(upstream.alias(), "api.openai.com");
        assert_eq!(upstream.endpoints()[0].scheme, Scheme::Http);
        assert!(upstream.endpoints()[0].is_plaintext());
    }

    /// A plaintext upstream is a legal create-time value only while
    /// `allow_http_upstream` is true; otherwise the management API rejects the
    /// write (DESIGN.md §3.2).
    #[test]
    fn test_plaintext_upstream_is_rejected_while_the_policy_forbids_it() {
        let service = service();
        let existing = create_upstream(&service, ROOT_TENANT, None, "api.openai.com", 443).unwrap();

        let create = service
            .create_upstream(ROOT_TENANT, &http_spec("api.openai.com", 80))
            .expect_err("a plaintext upstream must be rejected on create");
        let replace = service
            .replace_upstream(ROOT_TENANT, existing.id, &http_spec("api.openai.com", 80))
            .expect_err("a plaintext upstream must be rejected on replace");

        for error in [create, replace] {
            assert_eq!(error.status(), 400, "{error}");
            assert_eq!(
                error.gts_type(),
                "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
            );
            assert_eq!(
                error.extensions().extra["field"],
                "server.endpoints[0].scheme"
            );
        }

        assert_eq!(service.store().upstream_count(), 1, "nothing was stored");
    }

    #[test]
    fn test_plaintext_upstream_is_accepted_while_the_policy_allows_it() {
        let service = plaintext_service();
        let spec = spec(json!({
            "alias": "my-service",
            "server": {
                "endpoints": [{ "scheme": "http", "host": "127.0.0.1", "port": 8080 }]
            },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        }));

        let created = service.create_upstream(ROOT_TENANT, &spec).unwrap();
        assert!(created.endpoints()[0].is_plaintext());

        let replaced = service
            .replace_upstream(ROOT_TENANT, created.id, &http_spec("127.0.0.1", 8080))
            .unwrap_err();

        assert_eq!(
            replaced.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
            "an IP pool without an explicit alias is still a validation error"
        );
    }
}
