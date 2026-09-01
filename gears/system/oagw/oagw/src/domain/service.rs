//! Control-plane service for the OAGW gear.
//!
//! Owns the in-memory tenant-scoped store and implements the management CRUD:
//! upstreams, routes, and plugins — with the DESIGN semantics (unique
//! `(tenant_id, alias)`, immutable alias, immutable route `upstream_id`,
//! delete-in-use 409).

use std::sync::Arc;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::error::{ControlPlaneError, DuplicateKind, ResourceRef};
use crate::domain::models::{PluginRecord, ReferencedBy, Route, Upstream};
use crate::domain::validation::{
    derive_alias, normalize_alias, validate_route, validate_upstream,
};
use crate::infra::storage::OagwStore;

/// The OAGW control-plane service.
///
/// # DESIGN-led deviation
///
/// See [`crate::infra::storage`] — the control plane runs on an in-memory
/// store instead of `SeaORM`/`toolkit-db`; all tenant-scoping, uniqueness, and
/// immutability semantics from the DESIGN are preserved. Ancestor-tenant
/// inheritance ("bind", enforced sharing, inherited routes) is resolved at
/// proxy time via the tenant chain (see the data-plane slice); management
/// operations are scoped to the calling tenant.
#[derive(Debug)]
pub struct ControlPlaneService {
    /// In-memory tenant-scoped store (upstreams / routes / plugins).
    pub(crate) store: OagwStore,
    /// Frozen gear config.
    pub(crate) config: OagwConfig,
}

impl ControlPlaneService {
    /// Create a new control-plane service.
    #[must_use]
    pub fn new(config: OagwConfig) -> Self {
        Self {
            store: OagwStore::new(),
            config,
        }
    }

    /// The gear configuration captured at boot.
    #[must_use]
    pub fn config(&self) -> &OagwConfig {
        &self.config
    }

    /// Shared handle for wiring into axum state.
    #[must_use]
    pub fn shared(config: OagwConfig) -> Arc<Self> {
        Arc::new(Self::new(config))
    }

    // ------------------------------------------------------------------
    // Upstreams
    // ------------------------------------------------------------------

    /// Create an upstream in `tenant_id`.
    ///
    /// Validates the payload (alias derivation), rejects duplicate aliases
    /// within the tenant (409), and assigns a fresh system id.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::Validation`] on invalid payloads and
    /// [`ControlPlaneError::Duplicate`] when the alias is already taken.
    pub fn create_upstream(
        &self,
        tenant_id: Uuid,
        mut upstream: Upstream,
    ) -> Result<Upstream, ControlPlaneError> {
        validate_upstream(&mut upstream)
            .map_err(|details| ControlPlaneError::Validation { details })?;
        let alias = upstream.alias.clone();

        let table = self.store.upstreams(tenant_id);
        let aliases = self.store.aliases(tenant_id);
        // Atomic alias reservation: the DashMap `entry()` API performs the
        // check-and-insert under a single shard lock, so two concurrent
        // creates with the same alias cannot both succeed (one sees
        // `Occupied` and returns 409).
        match aliases.entry(alias.clone()) {
            dashmap::mapref::entry::Entry::Occupied(occupied) => {
                let owner = *occupied.get();
                // Exact same resource already exists under this alias → conflict.
                return Err(ControlPlaneError::Duplicate(DuplicateKind::AliasTaken {
                    alias,
                    owner: owner.to_string(),
                }));
            }
            dashmap::mapref::entry::Entry::Vacant(vacant) => {
                // System-generated id wins.
                upstream.id = Uuid::new_v4();
                let id = upstream.id;
                table.insert(id, upstream.clone());
                // NOTE: `vacant` holds the aliases-map shard write-lock; the
                // upstream row is a *different* DashMap so inserting into it
                // is safe, but do NOT re-enter `self.store.aliases()` (e.g.
                // via `entry()`) before `vacant` is dropped — that re-enters
                // the same shard and deadlocks.
                vacant.insert(id);
            }
        }
        Ok(upstream)
    }

    /// Fetch an upstream by id (tenant-scoped).
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] when no upstream matches `id`.
    pub fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, ControlPlaneError> {
        self.store
            .upstreams(tenant_id)
            .get(&id)
            .map(|r| r.clone())
            .ok_or_else(|| ControlPlaneError::NotFound(ResourceRef::Upstream(id.to_string())))
    }

    /// List all upstreams visible in `tenant_id` (creation order).
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Upstream> {
        let mut out: Vec<Upstream> = self
            .store
            .upstreams(tenant_id)
            .iter()
            .map(|e| e.value().clone())
            .collect();
        out.sort_by_key(|u| u.id);
        out
    }

    /// Update (`PUT`) an upstream. The alias is immutable: any payload whose
    /// recomputed alias differs from the stored alias is rejected (400).
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] for unknown ids,
    /// [`ControlPlaneError::ImmutableAlias`] on alias changes, and
    /// [`ControlPlaneError::Validation`] on invalid payloads.
    pub fn update_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        mut upstream: Upstream,
    ) -> Result<Upstream, ControlPlaneError> {
        let existing = self.get_upstream(tenant_id, id)?;

        // Alias immutability: an explicitly provided alias that differs from
        // the stored one is rejected up front (before generic validation),
        // surfacing the DESIGN's "alias is immutable" 400.
        let stored = normalize_alias(&existing.alias);
        let provided = normalize_alias(&upstream.alias);
        if !provided.is_empty() && provided != stored {
            return Err(ControlPlaneError::ImmutableAlias);
        }

        // A PUT payload that omits the alias on an IP-based upstream (whose
        // alias cannot be derived) is tolerated: carry over the stored alias.
        // A provider *different* alias was already rejected above, and a
        // hostname-based payload re-derives its alias below as usual.
        if upstream.alias.trim().is_empty() && derive_alias(&upstream).is_ok_and(|d| d.is_none()) {
            upstream.alias.clone_from(&existing.alias);
        }

        // Validate + recompute the derived alias for the new endpoints.
        validate_upstream(&mut upstream)
            .map_err(|details| ControlPlaneError::Validation { details })?;

        // Endpoints that would re-derive a different alias are also an
        // (attempted) alias change → immutable.
        if normalize_alias(&upstream.alias) != stored {
            return Err(ControlPlaneError::ImmutableAlias);
        }

        // Alias slot is unchanged; keep the original id.
        upstream.id = id;
        self.store.upstreams(tenant_id).insert(id, upstream.clone());
        Ok(upstream)
    }

    /// Delete an upstream by id (tenant-scoped), cascading to every route
    /// bound to it in the tenant (a route cannot dangle on a missing
    /// upstream).
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] when no upstream matches `id`.
    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<(), ControlPlaneError> {
        let existing = self.get_upstream(tenant_id, id)?;
        self.store.upstreams(tenant_id).remove(&id);
        self.store
            .aliases(tenant_id)
            .remove(&normalize_alias(&existing.alias));
        // Cascade: all routes of this tenant bound to the upstream are removed
        // with it (the route's `upstream_id` is immutable, so none can be
        // salvaged by re-pointing).
        self.store.routes(tenant_id).retain(|_, r| r.upstream_id != id);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Routes
    // ------------------------------------------------------------------

    /// Create a route in `tenant_id`. `upstream_id` must belong to the tenant;
    /// a duplicate (`upstream_id`, path, method) tuple is a 409 conflict.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::Validation`] when the route is invalid or
    /// references an upstream outside the tenant, and
    /// [`ControlPlaneError::Duplicate`] on a conflicting route.
    pub fn create_route(
        &self,
        tenant_id: Uuid,
        mut route: Route,
    ) -> Result<Route, ControlPlaneError> {
        validate_route(&route).map_err(|details| ControlPlaneError::Validation { details })?;
        if self
            .store
            .upstreams(tenant_id)
            .get(&route.upstream_id)
            .is_none()
        {
            return Err(ControlPlaneError::Validation {
                details: format!(
                    "upstream_id '{}' does not belong to the calling tenant",
                    route.upstream_id
                ),
            });
        }

        let table = self.store.routes(tenant_id);
        if let Some(existing) = table.iter().find(|e| routes_conflict(e.value(), &route)) {
            return Err(ControlPlaneError::Duplicate(DuplicateKind::RouteExists {
                id: existing.value().id.to_string(),
            }));
        }

        route.id = Uuid::new_v4();
        let id = route.id;
        table.insert(id, route.clone());
        Ok(route)
    }

    /// Fetch a route by id (tenant-scoped).
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] when no route matches `id`.
    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, ControlPlaneError> {
        self.store
            .routes(tenant_id)
            .get(&id)
            .map(|r| r.clone())
            .ok_or_else(|| ControlPlaneError::NotFound(ResourceRef::Route(id.to_string())))
    }

    /// List all routes visible in `tenant_id`.
    #[must_use]
    pub fn list_routes(&self, tenant_id: Uuid) -> Vec<Route> {
        let mut out: Vec<Route> = self
            .store
            .routes(tenant_id)
            .iter()
            .map(|e| e.value().clone())
            .collect();
        out.sort_by_key(|r| r.id);
        out
    }

    /// Update (`PUT`) a route. `upstream_id` is immutable.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] for unknown ids,
    /// [`ControlPlaneError::ImmutableUpstreamId`] on upstream changes,
    /// [`ControlPlaneError::Validation`] on invalid payloads, and
    /// [`ControlPlaneError::Duplicate`] on a conflicting route.
    pub fn update_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        mut route: Route,
    ) -> Result<Route, ControlPlaneError> {
        let existing = self.get_route(tenant_id, id)?;
        if route.upstream_id != existing.upstream_id {
            return Err(ControlPlaneError::ImmutableUpstreamId);
        }
        validate_route(&route).map_err(|details| ControlPlaneError::Validation { details })?;

        let table = self.store.routes(tenant_id);
        if let Some(conflict) = table
            .iter()
            .find(|e| e.key() != &id && routes_conflict(e.value(), &route))
        {
            return Err(ControlPlaneError::Duplicate(DuplicateKind::RouteExists {
                id: conflict.value().id.to_string(),
            }));
        }

        route.id = id;
        table.insert(id, route.clone());
        Ok(route)
    }

    /// Delete a route by id (tenant-scoped).
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] when no route matches `id`.
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), ControlPlaneError> {
        self.get_route(tenant_id, id)?;
        self.store.routes(tenant_id).remove(&id);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Plugins
    // ------------------------------------------------------------------

    /// Create a custom plugin record. Starlark plugins require source text.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::Validation`] for missing names/source and
    /// client-supplied ids.
    pub fn create_plugin(
        &self,
        tenant_id: Uuid,
        mut plugin: PluginRecord,
    ) -> Result<PluginRecord, ControlPlaneError> {
        if plugin.name.trim().is_empty() {
            return Err(ControlPlaneError::Validation {
                details: "plugin requires a non-empty 'name'".to_owned(),
            });
        }
        if matches!(plugin.kind, crate::domain::models::PluginKind::Starlark)
            && plugin.source.trim().is_empty()
        {
            return Err(ControlPlaneError::Validation {
                details: "starlark plugin requires non-empty 'source'".to_owned(),
            });
        }
        if plugin.id != Uuid::default() {
            return Err(ControlPlaneError::Validation {
                details: "plugin 'id' is system-generated".to_owned(),
            });
        }
        plugin.id = Uuid::new_v4();
        let id = plugin.id;
        self.store.plugins(tenant_id).insert(id, plugin.clone());
        Ok(plugin)
    }

    /// Fetch a plugin by id (tenant-scoped).
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] when no plugin matches `id`.
    pub fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<PluginRecord, ControlPlaneError> {
        self.store
            .plugins(tenant_id)
            .get(&id)
            .map(|r| r.clone())
            .ok_or_else(|| ControlPlaneError::NotFound(ResourceRef::Plugin(id.to_string())))
    }

    /// List all plugins visible in `tenant_id`.
    #[must_use]
    pub fn list_plugins(&self, tenant_id: Uuid) -> Vec<PluginRecord> {
        let mut out: Vec<PluginRecord> = self
            .store
            .plugins(tenant_id)
            .iter()
            .map(|e| e.value().clone())
            .collect();
        out.sort_by_key(|p| p.id);
        out
    }

    /// Delete a plugin. Plugins still referenced by an upstream or route are
    /// rejected with a 409 carrying the referencing ids (ADR 0001).
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] when no plugin matches `id`,
    /// and [`ControlPlaneError::InUse`] when the plugin is still bound.
    pub fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<(), ControlPlaneError> {
        self.get_plugin(tenant_id, id)?;

        let id_str = id.to_string();
        let mut referenced_by = ReferencedBy::default();
        for u in self.store.upstreams(tenant_id).iter() {
            if u.value().plugins.items.iter().any(|p| p == &id_str) {
                referenced_by.upstreams.push(u.value().id.to_string());
            }
        }
        for r in self.store.routes(tenant_id).iter() {
            if r.value().plugins.items.iter().any(|p| p == &id_str) {
                referenced_by.routes.push(r.value().id.to_string());
            }
        }
        referenced_by.upstreams.sort();
        referenced_by.routes.sort();

        if !referenced_by.is_empty() {
            return Err(ControlPlaneError::InUse(
                ResourceRef::Plugin(id_str),
                referenced_by,
            ));
        }
        self.store.plugins(tenant_id).remove(&id);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Data-plane lookups
    // ------------------------------------------------------------------

    /// Resolve the closest enabled upstream by normalized alias across a
    /// tenant chain (descendant → root; the calling tenant shadows ancestors,
    /// DESIGN "Alias Resolution"). `chain[0]` must be the calling tenant.
    ///
    /// A disabled upstream that owns the alias short-circuits the chain:
    /// [`AliasResolution::Disabled`] is returned instead of falling through to
    /// an ancestor's enabled copy, so the proxy can reject with 503
    /// (PRD cpt-cf-oagw-fr-enable-disable).
    #[must_use]
    pub fn resolve_upstream_in_chain(&self, chain: &[Uuid], alias: &str) -> AliasResolution {
        let norm = normalize_alias(alias);
        for t in chain {
            let owner = match self.store.aliases(*t).get(&norm) {
                Some(id) => *id,
                None => continue,
            };
            let upstreams = self.store.upstreams(*t);
            let Some(u) = upstreams.get(&owner) else {
                continue;
            };
            if u.enabled {
                return AliasResolution::Found(Box::new(u.clone()));
            }
            // The closest owning tenant holds a disabled upstream: shadow.
            return AliasResolution::Disabled;
        }
        AliasResolution::NotFound
    }

    /// All routes bound to `upstream_id` found across the tenant chain.
    /// `chain` order (descendant first) preserves the shadowing priority for
    /// the caller's route-matching loop.
    #[must_use]
    pub fn list_routes_for_upstream(&self, chain: &[Uuid], upstream_id: Uuid) -> Vec<Route> {
        let mut out = Vec::new();
        for t in chain {
            for e in self.store.routes(*t).iter() {
                if e.value().upstream_id == upstream_id {
                    out.push(e.value().clone());
                }
            }
        }
        out
    }
}

/// Two HTTP routes conflict when they share `(upstream_id, path, method)`.
/// Paths are compared normalized (a leading '/' required), so `/v1` and
/// `/v1/` are the same route target.
fn routes_conflict(a: &Route, b: &Route) -> bool {
    let Some(am) = a.http_match() else {
        return false;
    };
    let Some(bm) = b.http_match() else {
        return false;
    };
    if a.upstream_id != b.upstream_id || norm_path(&am.path) != norm_path(&bm.path) {
        return false;
    }
    am.methods.iter().any(|m| bm.methods.contains(m))
}

/// Local path normalization for comparisons: guarantee a leading '/' and
/// drop an insignificant trailing slash, so `/v1` and `/v1/` are the same
/// route target (the root path "/" is preserved as-is).
fn norm_path(path: &str) -> String {
    let p = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    if p.len() > 1 && p.ends_with('/') {
        p[..p.len() - 1].to_owned()
    } else {
        p
    }
}

/// The outcome of resolving a route alias to an upstream across a tenant
/// chain (see [`ControlPlaneService::resolve_upstream_in_chain`]). Distinct
/// from a plain `Option` so callers can distinguish "no such alias" from "the
/// alias exists but is disabled and therefore shadows".
#[derive(Debug, Clone, PartialEq)]
pub enum AliasResolution {
    /// The alias resolved to an enabled upstream.
    Found(Box<Upstream>),
    /// The closest alias-owning tenant holds a *disabled* upstream (which
    /// shadows any ancestor's enabled copy).
    Disabled,
    /// No tenant in the chain owns the alias.
    NotFound,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::models::{
        Endpoint, HeadersConfig, HttpMatch, MatchRule, PROTOCOL_HTTP_V1, PathSuffixMode,
        PluginKind, PluginRecord, PluginsConfig, Route, Scheme, ServerConfig,
    };

    fn tenant(id: u64) -> Uuid {
        Uuid::from_u128(u128::from(id))
    }

    fn service() -> ControlPlaneService {
        ControlPlaneService::new(crate::config::OagwConfig::default())
    }

    fn upstream(host: &str, port: u16) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            enabled: true,
            alias: String::new(),
            tags: vec![],
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: host.to_owned(),
                    port,
                }],
            },
            protocol: PROTOCOL_HTTP_V1.to_owned(),
            auth: None,
            headers: HeadersConfig::default(),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }

    fn route(upstream_id: Uuid, methods: &[&str], path: &str) -> Route {
        Route {
            id: Uuid::new_v4(),
            enabled: true,
            tags: vec![],
            upstream_id,
            r#match: Some(MatchRule {
                http: Some(HttpMatch {
                    methods: methods
                        .iter()
                        .map(std::string::ToString::to_string)
                        .collect(),
                    path: path.to_owned(),
                    query_allowlist: vec![],
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            }),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }

    fn plugin(name: &str) -> PluginRecord {
        PluginRecord {
            id: Uuid::default(),
            name: name.to_owned(),
            description: String::new(),
            kind: PluginKind::Starlark,
            source: "def handle(req):\n  return req".to_owned(),
            enabled: true,
        }
    }

    // ------------------------------------------------------------------
    // Upstreams
    // ------------------------------------------------------------------

    #[test]
    fn create_upstream_derives_alias_and_populates_system_id() {
        let svc = service();
        let u = upstream("api.example.com", 443);
        let created = svc.create_upstream(tenant(1), u.clone()).unwrap();
        assert_eq!(created.alias, "api.example.com");
        assert_ne!(created.id, u.id, "system id replaces client id");
        assert_eq!(
            svc.get_upstream(tenant(1), created.id).unwrap().alias,
            "api.example.com"
        );
    }

    #[test]
    fn duplicate_alias_within_tenant_is_conflict() {
        let svc = service();
        svc.create_upstream(tenant(1), upstream("api.example.com", 443))
            .unwrap();
        let err = svc
            .create_upstream(tenant(1), upstream("api.example.com", 443))
            .unwrap_err();
        assert!(matches!(
            err,
            ControlPlaneError::Duplicate(DuplicateKind::AliasTaken { ref alias, .. })
                if alias == "api.example.com"
        ));
    }

    #[test]
    fn same_alias_in_different_tenant_is_allowed() {
        let svc = service();
        svc.create_upstream(tenant(1), upstream("shared.example.com", 443))
            .unwrap();
        let u2 = svc
            .create_upstream(tenant(2), upstream("shared.example.com", 443))
            .unwrap();
        assert_eq!(u2.alias, "shared.example.com");
    }

    #[test]
    fn get_missing_upstream_is_not_found() {
        let svc = service();
        let err = svc.get_upstream(tenant(1), Uuid::new_v4()).unwrap_err();
        assert!(matches!(
            err,
            ControlPlaneError::NotFound(ResourceRef::Upstream(_))
        ));
    }

    #[test]
    fn update_upstream_alias_is_immutable() {
        let svc = service();
        let created = svc
            .create_upstream(tenant(1), upstream("api.example.com", 443))
            .unwrap();
        // Payload tries to change the alias explicitly to a different value.
        let mut changed = created.clone();
        changed.alias = "other.example.com".to_owned();
        let err = svc
            .update_upstream(tenant(1), created.id, changed)
            .unwrap_err();
        assert!(matches!(err, ControlPlaneError::ImmutableAlias));
    }

    #[test]
    fn update_upstream_recomputes_derived_alias_and_rejects_change() {
        let svc = service();
        let created = svc
            .create_upstream(tenant(1), upstream("api.example.com", 443))
            .unwrap();
        // Same alias kept, but endpoints now derive a different alias → 400
        // (either the generic alias/derived mismatch validation or the
        // immutability error — both are 400-class rejections).
        let mut changed = created.clone();
        changed.server.endpoints[0].host = "api2.example.com".to_owned();
        let err = svc
            .update_upstream(tenant(1), created.id, changed)
            .unwrap_err();
        assert!(
            matches!(
                err,
                ControlPlaneError::ImmutableAlias | ControlPlaneError::Validation { .. }
            ),
            "endpoint change must be rejected with a 400-class error"
        );

        // Blank-alias payload: validation derives the new alias; the service
        // still rejects because it differs from the stored one.
        let mut changed2 = created.clone();
        changed2.alias.clear();
        changed2.server.endpoints[0].host = "api2.example.com".to_owned();
        let err2 = svc
            .update_upstream(tenant(1), created.id, changed2)
            .unwrap_err();
        assert!(matches!(err2, ControlPlaneError::ImmutableAlias));
    }

    #[test]
    fn update_upstream_keeps_id_and_alias_on_legit_update() {
        let svc = service();
        let created = svc
            .create_upstream(tenant(1), upstream("api.example.com", 443))
            .unwrap();
        let mut changed = created.clone();
        changed.tags = vec!["prod".to_owned()];
        let updated = svc.update_upstream(tenant(1), created.id, changed).unwrap();
        assert_eq!(updated.id, created.id);
        assert_eq!(updated.alias, "api.example.com");
        assert_eq!(updated.tags, vec!["prod"]);
    }

    #[test]
    fn delete_upstream_frees_the_alias_slot() {
        let svc = service();
        let created = svc
            .create_upstream(tenant(1), upstream("api.example.com", 443))
            .unwrap();
        assert!(svc.delete_upstream(tenant(1), created.id).is_ok());
        assert!(matches!(
            svc.get_upstream(tenant(1), created.id),
            Err(ControlPlaneError::NotFound(_))
        ));
        // Alias slot is free again.
        let again = svc
            .create_upstream(tenant(1), upstream("api.example.com", 443))
            .unwrap();
        assert_eq!(again.alias, "api.example.com");
    }

    #[test]
    fn delete_missing_upstream_is_not_found() {
        let svc = service();
        assert!(matches!(
            svc.delete_upstream(tenant(1), Uuid::new_v4()),
            Err(ControlPlaneError::NotFound(_))
        ));
    }

    // ------------------------------------------------------------------
    // Routes
    // ------------------------------------------------------------------

    #[test]
    fn create_route_rejects_upstream_of_another_tenant() {
        let svc = service();
        let u = svc
            .create_upstream(tenant(1), upstream("api.example.com", 443))
            .unwrap();
        let err = svc
            .create_route(tenant(2), route(u.id, &["GET"], "/v1"))
            .unwrap_err();
        assert!(
            matches!(err, ControlPlaneError::Validation { ref details } if details.contains("does not belong"))
        );
    }

    #[test]
    fn create_route_duplicate_path_and_method_is_conflict() {
        let svc = service();
        let u = svc
            .create_upstream(tenant(1), upstream("api.example.com", 443))
            .unwrap();
        svc.create_route(tenant(1), route(u.id, &["GET", "POST"], "/v1/chat"))
            .unwrap();
        // Same upstream + path, overlapping method → conflict.
        let err = svc
            .create_route(tenant(1), route(u.id, &["POST"], "/v1/chat"))
            .unwrap_err();
        assert!(matches!(
            err,
            ControlPlaneError::Duplicate(DuplicateKind::RouteExists { .. })
        ));
    }

    #[test]
    fn create_route_same_path_different_method_is_allowed() {
        let svc = service();
        let u = svc
            .create_upstream(tenant(1), upstream("api.example.com", 443))
            .unwrap();
        svc.create_route(tenant(1), route(u.id, &["GET"], "/v1/chat"))
            .unwrap();
        svc.create_route(tenant(1), route(u.id, &["PUT"], "/v1/chat"))
            .unwrap();
        assert_eq!(svc.list_routes(tenant(1)).len(), 2);
    }

    #[test]
    fn route_crud_roundtrip() {
        let svc = service();
        let u = svc
            .create_upstream(tenant(1), upstream("api.example.com", 443))
            .unwrap();
        let r = svc
            .create_route(tenant(1), route(u.id, &["GET"], "/v1/chat"))
            .unwrap();
        assert_ne!(r.id, Uuid::new_v4());
        assert_eq!(svc.list_routes(tenant(1)).len(), 1);

        let mut updated = r.clone();
        updated.tags = vec!["internal".to_owned()];
        let saved = svc.update_route(tenant(1), r.id, updated).unwrap();
        assert_eq!(saved.id, r.id);
        assert_eq!(saved.tags, vec!["internal"]);

        assert!(svc.delete_route(tenant(1), r.id).is_ok());
        assert_eq!(svc.list_routes(tenant(1)).len(), 0);
    }

    #[test]
    fn update_route_upstream_id_is_immutable() {
        let svc = service();
        let u1 = svc
            .create_upstream(tenant(1), upstream("api.example.com", 443))
            .unwrap();
        let u2 = svc
            .create_upstream(tenant(1), upstream("api2.example.com", 443))
            .unwrap();
        let r = svc
            .create_route(tenant(1), route(u1.id, &["GET"], "/v1"))
            .unwrap();
        let mut changed = r.clone();
        changed.upstream_id = u2.id;
        let err = svc.update_route(tenant(1), r.id, changed).unwrap_err();
        assert!(matches!(err, ControlPlaneError::ImmutableUpstreamId));
    }

    // ------------------------------------------------------------------
    // Rf-008 / Rf-011 / Rf-014 regression tests (semantic review)
    // ------------------------------------------------------------------

    #[test]
    fn delete_upstream_cascades_its_routes() {
        let svc = service();
        let u = svc
            .create_upstream(tenant(1), upstream("api.example.com", 443))
            .unwrap();
        let r = svc
            .create_route(tenant(1), route(u.id, &["GET"], "/v1/chat"))
            .unwrap();
        svc.create_route(tenant(1), route(u.id, &["PUT"], "/v1/chat"))
            .unwrap();
        assert_eq!(svc.list_routes(tenant(1)).len(), 2);

        svc.delete_upstream(tenant(1), u.id).unwrap();
        // The upstream's routes cascade away with it.
        assert_eq!(svc.list_routes(tenant(1)).len(), 0);
        assert!(matches!(
            svc.get_route(tenant(1), r.id),
            Err(ControlPlaneError::NotFound(_))
        ));
        // The freed alias is immediately reusable, with a fresh route slot.
        let again = svc
            .create_upstream(tenant(1), upstream("api.example.com", 443))
            .unwrap();
        assert_eq!(again.alias, "api.example.com");
        svc.create_route(tenant(1), route(again.id, &["GET"], "/v1/chat"))
            .unwrap();
    }

    #[test]
    fn concurrent_create_with_same_alias_yields_exactly_one_winner() {
        let svc = Arc::new(service());
        let results: Vec<_> = (0..8)
            .map(|_| {
                let svc = Arc::clone(&svc);
                std::thread::spawn(move || {
                    svc.create_upstream(tenant(1), upstream("race.example.com", 443))
                })
                .join()
                .unwrap()
            })
            .collect();
        let ok = results.iter().filter(|r| r.is_ok()).count();
        let taken = results
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    Err(ControlPlaneError::Duplicate(DuplicateKind::AliasTaken { .. }))
                )
            })
            .count();
        assert_eq!(ok, 1, "exactly one thread may win the alias");
        assert_eq!(taken, results.len() - 1);
        assert_eq!(svc.list_upstreams(tenant(1)).len(), 1);
    }

    #[test]
    fn route_paths_with_and_without_trailing_slash_conflict() {
        let svc = service();
        let u = svc
            .create_upstream(tenant(1), upstream("api.example.com", 443))
            .unwrap();
        // `/v1` and `/v1/` normalize to the same route target (Rf-014).
        svc.create_route(tenant(1), route(u.id, &["GET"], "/v1"))
            .unwrap();
        let err = svc
            .create_route(tenant(1), route(u.id, &["GET"], "/v1/"))
            .unwrap_err();
        assert!(matches!(
            err,
            ControlPlaneError::Duplicate(DuplicateKind::RouteExists { .. })
        ));
    }

    // ------------------------------------------------------------------
    // Plugins
    // ------------------------------------------------------------------

    #[test]
    fn create_plugin_requires_name_and_starlark_source() {
        let svc = service();
        let mut p = plugin("x");
        p.name = "  ".to_owned();
        assert!(matches!(
            svc.create_plugin(tenant(1), p),
            Err(ControlPlaneError::Validation { .. })
        ));
        let mut p2 = plugin("x");
        p2.kind = PluginKind::Starlark;
        p2.source = "   ".to_owned();
        assert!(matches!(
            svc.create_plugin(tenant(1), p2),
            Err(ControlPlaneError::Validation { .. })
        ));
    }

    #[test]
    fn plugin_crud_roundtrip_and_client_id_rejected() {
        let svc = service();
        let mut p = plugin("jwt-audit");
        p.id = Uuid::new_v4(); // client-supplied id must be ignored/rejected
        let err = svc.create_plugin(tenant(1), p.clone()).unwrap_err();
        assert!(
            matches!(err, ControlPlaneError::Validation { ref details } if details.contains("system-generated"))
        );

        p.id = Uuid::default();
        let created = svc.create_plugin(tenant(1), p).unwrap();
        assert_ne!(created.id, Uuid::default());
        assert_eq!(
            svc.get_plugin(tenant(1), created.id).unwrap().name,
            "jwt-audit"
        );
        assert_eq!(svc.list_plugins(tenant(1)).len(), 1);
        assert!(svc.delete_plugin(tenant(1), created.id).is_ok());
    }

    #[test]
    fn delete_plugin_in_use_is_conflict_with_references() {
        let svc = service();
        let p = svc.create_plugin(tenant(1), plugin("transform")).unwrap();
        let mut u = upstream("api.example.com", 443);
        u.plugins.items = vec![p.id.to_string()];
        let up = svc.create_upstream(tenant(1), u).unwrap();

        let mut r = route(up.id, &["GET"], "/v1");
        r.plugins.items = vec![p.id.to_string()];
        let route = svc.create_route(tenant(1), r).unwrap();

        match svc.delete_plugin(tenant(1), p.id) {
            Err(ControlPlaneError::InUse(ref_, referenced)) => {
                assert!(matches!(ref_, ResourceRef::Plugin(_)));
                assert!(referenced.upstreams.contains(&up.id.to_string()));
                assert!(referenced.routes.contains(&route.id.to_string()));
            }
            other => panic!("expected InUse, got {other:?}"),
        }
    }

    #[test]
    fn delete_plugin_is_allowed_once_unreferenced() {
        let svc = service();
        let p = svc.create_plugin(tenant(1), plugin("transform")).unwrap();
        assert!(svc.delete_plugin(tenant(1), p.id).is_ok());
        assert!(matches!(
            svc.get_plugin(tenant(1), p.id),
            Err(ControlPlaneError::NotFound(_))
        ));
    }
}
