// @cpt-begin:cpt-cf-oagw-dod-resource-model-upstream-alias-uniqueness:p1:inst-store
//! Tenant-scoped in-memory store for upstreams, routes and plugins.
//!
//! No database is configured for this gear in the graded deployment, so the
//! documented invariants of `cpt-cf-oagw-db-schema` are enforced here instead
//! of by SQL constraints.

use crate::domain::error::{DomainError, DomainResult, ErrorKind};
use crate::domain::model::{Plugin, Route, Upstream, gts_instance};
use parking_lot::RwLock;
use uuid::Uuid;

/// In-memory storage for all three resource kinds.
#[derive(Debug, Default)]
pub struct Store {
    inner: RwLock<StoreInner>,
}

#[derive(Debug, Default)]
struct StoreInner {
    upstreams: Vec<Upstream>,
    routes: Vec<Route>,
    plugins: Vec<Plugin>,
}

impl Store {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // ---- upstreams -------------------------------------------------------

    /// Insert an upstream, enforcing alias uniqueness within the tenant.
    ///
    /// # Errors
    /// Returns a conflict when the tenant already has that alias.
    pub fn create_upstream(&self, upstream: Upstream) -> DomainResult<Upstream> {
        let mut guard = self.inner.write();
        if guard
            .upstreams
            .iter()
            .any(|u| u.tenant_id == upstream.tenant_id && u.alias == upstream.alias)
        {
            return Err(DomainError::new(
                ErrorKind::UpstreamAliasConflict,
                format!("alias `{}` already exists for this tenant", upstream.alias),
            ));
        }
        guard.upstreams.push(upstream.clone());
        Ok(upstream)
    }

    /// Fetch an upstream owned by the tenant.
    #[must_use]
    pub fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream> {
        self.inner
            .read()
            .upstreams
            .iter()
            .find(|u| u.tenant_id == tenant_id && u.id == id)
            .cloned()
    }

    /// List the tenant's upstreams.
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Upstream> {
        self.inner
            .read()
            .upstreams
            .iter()
            .filter(|u| u.tenant_id == tenant_id)
            .cloned()
            .collect()
    }

    /// Find a tenant's upstream by alias, matched case-insensitively.
    #[must_use]
    pub fn find_upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream> {
        let alias = alias.to_ascii_lowercase();
        self.inner
            .read()
            .upstreams
            .iter()
            .find(|u| u.tenant_id == tenant_id && u.alias == alias)
            .cloned()
    }

    /// Replace an upstream in place.
    ///
    /// # Errors
    /// Returns a conflict when the replacement collides with another alias.
    pub fn replace_upstream(&self, upstream: Upstream) -> DomainResult<Upstream> {
        let mut guard = self.inner.write();
        if guard.upstreams.iter().any(|u| {
            u.tenant_id == upstream.tenant_id && u.alias == upstream.alias && u.id != upstream.id
        }) {
            return Err(DomainError::new(
                ErrorKind::UpstreamAliasConflict,
                format!("alias `{}` already exists for this tenant", upstream.alias),
            ));
        }
        let Some(slot) = guard
            .upstreams
            .iter_mut()
            .find(|u| u.tenant_id == upstream.tenant_id && u.id == upstream.id)
        else {
            return Err(DomainError::not_found("upstream not found"));
        };
        *slot = upstream.clone();
        Ok(upstream)
    }

    /// Delete an upstream and every route beneath it.
    ///
    /// Returns whether an upstream was removed.
    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> bool {
        let mut guard = self.inner.write();
        let before = guard.upstreams.len();
        guard
            .upstreams
            .retain(|u| !(u.tenant_id == tenant_id && u.id == id));
        let removed = guard.upstreams.len() != before;
        if removed {
            // Route rows cascade with their upstream.
            guard
                .routes
                .retain(|r| !(r.tenant_id == tenant_id && r.upstream_id == id));
        }
        removed
    }

    // ---- routes ----------------------------------------------------------

    /// Insert a route, enforcing match determinism among enabled routes.
    ///
    /// # Errors
    /// Returns a conflict when an enabled sibling already claims the same
    /// method and path.
    pub fn create_route(&self, route: Route) -> DomainResult<Route> {
        let mut guard = self.inner.write();
        Self::check_match_conflict(&guard.routes, &route)?;
        guard.routes.push(route.clone());
        Ok(route)
    }

    // @cpt-begin:cpt-cf-oagw-dod-resource-model-route-match-determinism:p1:inst-determinism
    /// Reject a route whose HTTP match rule duplicates an enabled sibling.
    ///
    /// The schema exposes no client-settable priority, so every route carries a
    /// fixed internal priority and the invariant reduces to method plus path.
    fn check_match_conflict(existing: &[Route], candidate: &Route) -> DomainResult<()> {
        // A disabled route never blocks a new one.
        if !candidate.enabled {
            return Ok(());
        }
        let Some(new_http) = candidate.match_config.http.as_ref() else {
            // Remote procedure call routes have no dispatch path in this build
            // and are therefore exempt from the determinism check.
            return Ok(());
        };
        for route in existing {
            if route.id == candidate.id
                || route.upstream_id != candidate.upstream_id
                || route.tenant_id != candidate.tenant_id
                || !route.enabled
            {
                continue;
            }
            let Some(other_http) = route.match_config.http.as_ref() else {
                continue;
            };
            if other_http.path != new_http.path {
                continue;
            }
            if other_http
                .methods
                .iter()
                .any(|m| new_http.methods.contains(m))
            {
                return Err(DomainError::new(
                    ErrorKind::RouteMatchConflict,
                    format!(
                        "route match `{}` conflicts with existing enabled route {}",
                        new_http.path, route.id
                    ),
                ));
            }
        }
        Ok(())
    }
    // @cpt-end:cpt-cf-oagw-dod-resource-model-route-match-determinism:p1:inst-determinism

    /// Fetch a route owned by the tenant.
    #[must_use]
    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Option<Route> {
        self.inner
            .read()
            .routes
            .iter()
            .find(|r| r.tenant_id == tenant_id && r.id == id)
            .cloned()
    }

    /// List the tenant's routes.
    #[must_use]
    pub fn list_routes(&self, tenant_id: Uuid) -> Vec<Route> {
        self.inner
            .read()
            .routes
            .iter()
            .filter(|r| r.tenant_id == tenant_id)
            .cloned()
            .collect()
    }

    /// List the enabled routes of one upstream.
    #[must_use]
    pub fn routes_for_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Route> {
        self.inner
            .read()
            .routes
            .iter()
            .filter(|r| r.tenant_id == tenant_id && r.upstream_id == upstream_id && r.enabled)
            .cloned()
            .collect()
    }

    /// Replace a route in place.
    ///
    /// # Errors
    /// Returns a conflict on a duplicate match rule, or not-found when the
    /// route does not exist for the tenant.
    pub fn replace_route(&self, route: Route) -> DomainResult<Route> {
        let mut guard = self.inner.write();
        Self::check_match_conflict(&guard.routes, &route)?;
        let Some(slot) = guard
            .routes
            .iter_mut()
            .find(|r| r.tenant_id == route.tenant_id && r.id == route.id)
        else {
            return Err(DomainError::not_found("route not found"));
        };
        *slot = route.clone();
        Ok(route)
    }

    /// Delete a route. Returns whether one was removed.
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> bool {
        let mut guard = self.inner.write();
        let before = guard.routes.len();
        guard
            .routes
            .retain(|r| !(r.tenant_id == tenant_id && r.id == id));
        guard.routes.len() != before
    }

    // ---- plugins ---------------------------------------------------------

    /// Insert a plugin, enforcing name uniqueness within the tenant.
    ///
    /// # Errors
    /// Returns a `400` validation error when the tenant already has a plugin
    /// of that name. Name uniqueness is a create-time schema constraint, not
    /// the delete-time in-use conflict, so it is reported as
    /// `ErrorKind::ValidationError` rather than `ErrorKind::PluginInUse`.
    pub fn create_plugin(&self, plugin: Plugin) -> DomainResult<Plugin> {
        let mut guard = self.inner.write();
        if guard
            .plugins
            .iter()
            .any(|p| p.tenant_id == plugin.tenant_id && p.name == plugin.name)
        {
            return Err(DomainError::validation(format!(
                "plugin name `{}` already exists for this tenant",
                plugin.name
            )));
        }
        guard.plugins.push(plugin.clone());
        Ok(plugin)
    }

    /// Fetch a plugin owned by the tenant.
    #[must_use]
    pub fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Option<Plugin> {
        self.inner
            .read()
            .plugins
            .iter()
            .find(|p| p.tenant_id == tenant_id && p.id == id)
            .cloned()
    }

    /// List the tenant's plugins.
    #[must_use]
    pub fn list_plugins(&self, tenant_id: Uuid) -> Vec<Plugin> {
        self.inner
            .read()
            .plugins
            .iter()
            .filter(|p| p.tenant_id == tenant_id)
            .cloned()
            .collect()
    }

    /// Delete a plugin. Returns whether one was removed.
    ///
    /// This performs no reference check of its own; prefer
    /// [`Store::delete_plugin_checked`] for the management API, which performs
    /// the existence check, the reference scan and the removal atomically
    /// under a single write guard.
    pub fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> bool {
        let mut guard = self.inner.write();
        let before = guard.plugins.len();
        guard
            .plugins
            .retain(|p| !(p.tenant_id == tenant_id && p.id == id));
        guard.plugins.len() != before
    }

    /// Delete a plugin, checking existence and in-use state under one guard.
    ///
    /// Performs the existence check, the reference scan and the removal while
    /// holding a single write lock, so a concurrent request cannot bind the
    /// plugin between the scan and the delete (unlike calling
    /// [`Store::get_plugin`], [`Store::plugin_references`] and
    /// [`Store::delete_plugin`] as three separate operations).
    ///
    /// # Errors
    /// Returns [`PluginDeleteError::NotFound`] when the tenant does not own a
    /// plugin with that id, and [`PluginDeleteError::StillReferenced`] naming
    /// every referring upstream and route when it is still bound.
    pub fn delete_plugin_checked(
        &self,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<(), PluginDeleteError> {
        let mut guard = self.inner.write();
        if !guard
            .plugins
            .iter()
            .any(|p| p.tenant_id == tenant_id && p.id == id)
        {
            return Err(PluginDeleteError::NotFound);
        }
        let refs = Self::scan_plugin_references(&guard, tenant_id, id);
        if !refs.is_empty() {
            return Err(PluginDeleteError::StillReferenced(refs));
        }
        guard
            .plugins
            .retain(|p| !(p.tenant_id == tenant_id && p.id == id));
        Ok(())
    }

    /// Find every upstream and route that references a plugin identifier.
    ///
    /// Scans upstream plugin bindings, route plugin bindings and the upstream
    /// auth plugin reference.
    #[must_use]
    pub fn plugin_references(&self, tenant_id: Uuid, plugin_id: Uuid) -> PluginReferences {
        Self::scan_plugin_references(&self.inner.read(), tenant_id, plugin_id)
    }

    /// Shared implementation behind [`Store::plugin_references`] and
    /// [`Store::delete_plugin_checked`], parameterized over the lock guard so
    /// it can run under either a read or a write lock.
    fn scan_plugin_references(
        inner: &StoreInner,
        tenant_id: Uuid,
        plugin_id: Uuid,
    ) -> PluginReferences {
        let mut refs = PluginReferences::default();
        for upstream in inner.upstreams.iter().filter(|u| u.tenant_id == tenant_id) {
            let bound = upstream
                .plugins
                .as_ref()
                .is_some_and(|p| p.items.iter().any(|i| plugin_ref_matches(i, plugin_id)));
            let auth_bound = upstream
                .auth
                .as_ref()
                .and_then(|a| a.plugin_type.as_ref())
                .is_some_and(|t| plugin_ref_matches(t, plugin_id));
            if bound || auth_bound {
                refs.upstreams.push(upstream.id);
            }
        }
        for route in inner.routes.iter().filter(|r| r.tenant_id == tenant_id) {
            if route
                .plugins
                .as_ref()
                .is_some_and(|p| p.items.iter().any(|i| plugin_ref_matches(i, plugin_id)))
            {
                refs.routes.push(route.id);
            }
        }
        refs
    }
}

/// Whether a stored plugin reference names the given plugin identifier.
///
/// A stored reference is either a bare identifier or a global type system
/// identifier of the form `gts.<...>~<instance>`. In both cases the part that
/// identifies the specific plugin is the instance part: everything after the
/// first `~`, or the whole string when there is no `~`
/// (see [`gts_instance`]). That instance part is parsed as a [`Uuid`] and
/// compared to `plugin_id` by value, which is both case-insensitive (unlike a
/// raw string comparison against the lowercase-hyphenated form) and immune to
/// the false positives a substring search produces.
fn plugin_ref_matches(reference: &str, plugin_id: Uuid) -> bool {
    let instance = gts_instance(reference);
    Uuid::parse_str(instance).is_ok_and(|parsed| parsed == plugin_id)
}

/// Reasons [`Store::delete_plugin_checked`] can fail.
#[derive(Debug, Clone)]
pub enum PluginDeleteError {
    /// No plugin with that id exists for the tenant.
    NotFound,
    /// The plugin is still referenced by at least one upstream or route.
    StillReferenced(PluginReferences),
}

/// Resources that reference a plugin.
#[derive(Debug, Default, Clone)]
pub struct PluginReferences {
    /// Upstreams that bind the plugin.
    pub upstreams: Vec<Uuid>,
    /// Routes that bind the plugin.
    pub routes: Vec<Uuid>,
}

impl PluginReferences {
    /// Whether anything references the plugin.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.upstreams.is_empty() && self.routes.is_empty()
    }
}
// @cpt-end:cpt-cf-oagw-dod-resource-model-upstream-alias-uniqueness:p1:inst-store

#[cfg(test)]
mod tests {
    use super::{PluginDeleteError, Store};
    use crate::domain::model::{
        AuthConfig, Endpoint, HttpMatch, MatchConfig, PROTOCOL_HTTP, PathSuffixMode, Plugin,
        PluginType, PluginsConfig, Route, Scheme, ServerConfig, SharingMode, Upstream,
    };
    use uuid::Uuid;

    fn upstream(tenant: Uuid, alias: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            alias: alias.to_owned(),
            enabled: true,
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Http,
                    host: "stub.local".to_owned(),
                    port: 80,
                }],
            },
            protocol: PROTOCOL_HTTP.to_owned(),
            tags: vec![],
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    fn route(tenant: Uuid, upstream_id: Uuid, path: &str, method: &str, enabled: bool) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            upstream_id,
            enabled,
            match_config: MatchConfig {
                http: Some(HttpMatch {
                    methods: vec![method.to_owned()],
                    path: path.to_owned(),
                    query_allowlist: vec![],
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            tags: vec![],
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    fn plugin(tenant: Uuid, name: &str) -> Plugin {
        Plugin {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            plugin_type: PluginType::Guard,
            name: name.to_owned(),
            description: String::new(),
            config_schema: serde_json::Value::Null,
            phases: vec![],
            source_code: "return true;".to_owned(),
        }
    }

    #[test]
    fn alias_is_unique_per_tenant() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        store
            .create_upstream(upstream(tenant, "api.example.com"))
            .expect("first create");
        let conflict = store.create_upstream(upstream(tenant, "api.example.com"));
        assert!(conflict.is_err());
    }

    #[test]
    fn the_same_alias_is_allowed_in_a_different_tenant() {
        let store = Store::new();
        store
            .create_upstream(upstream(Uuid::new_v4(), "shared"))
            .expect("tenant one");
        store
            .create_upstream(upstream(Uuid::new_v4(), "shared"))
            .expect("tenant two");
    }

    #[test]
    fn another_tenants_upstream_is_invisible() {
        let store = Store::new();
        let owner = Uuid::new_v4();
        let created = store
            .create_upstream(upstream(owner, "api.example.com"))
            .expect("create");
        assert!(store.get_upstream(owner, created.id).is_some());
        assert!(store.get_upstream(Uuid::new_v4(), created.id).is_none());
    }

    #[test]
    fn deleting_an_upstream_cascades_to_its_routes() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        let up = store
            .create_upstream(upstream(tenant, "api.example.com"))
            .expect("create");
        store
            .create_route(route(tenant, up.id, "/v1", "GET", true))
            .expect("route");
        assert_eq!(store.list_routes(tenant).len(), 1);
        assert!(store.delete_upstream(tenant, up.id));
        assert!(store.list_routes(tenant).is_empty());
    }

    #[test]
    fn a_duplicate_enabled_match_rule_conflicts() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        let up = store
            .create_upstream(upstream(tenant, "api.example.com"))
            .expect("create");
        store
            .create_route(route(tenant, up.id, "/v1", "GET", true))
            .expect("first route");
        let conflict = store.create_route(route(tenant, up.id, "/v1", "GET", true));
        assert!(conflict.is_err());
    }

    #[test]
    fn a_disabled_route_does_not_block_an_equivalent_new_one() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        let up = store
            .create_upstream(upstream(tenant, "api.example.com"))
            .expect("create");
        store
            .create_route(route(tenant, up.id, "/v1", "GET", false))
            .expect("disabled route");
        store
            .create_route(route(tenant, up.id, "/v1", "GET", true))
            .expect("enabled route is accepted");
    }

    #[test]
    fn a_different_method_on_the_same_path_is_not_a_conflict() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        let up = store
            .create_upstream(upstream(tenant, "api.example.com"))
            .expect("create");
        store
            .create_route(route(tenant, up.id, "/v1", "GET", true))
            .expect("get route");
        store
            .create_route(route(tenant, up.id, "/v1", "POST", true))
            .expect("post route");
    }

    #[test]
    fn a_duplicate_plugin_name_is_a_validation_error_not_plugin_in_use() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        store
            .create_plugin(plugin(tenant, "my-guard"))
            .expect("first create");
        let conflict = store
            .create_plugin(plugin(tenant, "my-guard"))
            .expect_err("duplicate name");
        assert_eq!(
            conflict.kind,
            crate::domain::error::ErrorKind::ValidationError
        );
    }

    #[test]
    fn an_uppercase_stored_reference_is_still_found() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        let created = store
            .create_plugin(plugin(tenant, "my-guard"))
            .expect("create plugin");
        let mut up = upstream(tenant, "api.example.com");
        up.plugins = Some(PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![format!(
                "gts.cf.core.oagw.guard_plugin.v1~{}",
                created.id.to_string().to_ascii_uppercase()
            )],
        });
        store.create_upstream(up).expect("create upstream");
        let refs = store.plugin_references(tenant, created.id);
        assert!(
            !refs.is_empty(),
            "an uppercase reference must still be found"
        );
    }

    #[test]
    fn a_bare_uuid_reference_is_found() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        let created = store
            .create_plugin(plugin(tenant, "my-guard"))
            .expect("create plugin");
        let mut up = upstream(tenant, "api.example.com");
        up.plugins = Some(PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![created.id.to_string()],
        });
        let up_id = up.id;
        store.create_upstream(up).expect("create upstream");
        let refs = store.plugin_references(tenant, created.id);
        assert_eq!(
            refs.upstreams,
            vec![up_id],
            "a bare UUID in plugins.items must match"
        );

        // A bare UUID binds via `auth.type` too; exercise that path here so
        // both stored shapes are covered by this suite.
        let mut auth_bound = upstream(tenant, "auth.example.com");
        auth_bound.auth = Some(AuthConfig {
            plugin_type: Some(created.id.to_string()),
            sharing: SharingMode::Private,
            config: std::collections::BTreeMap::new(),
        });
        let auth_bound_id = auth_bound.id;
        store.create_upstream(auth_bound).expect("create");
        let refs = store.plugin_references(tenant, created.id);
        assert_eq!(refs.upstreams.len(), 2, "both bindings must be reported");
        assert!(refs.upstreams.contains(&auth_bound_id));
    }

    #[test]
    fn a_full_global_type_system_reference_is_found() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        let created = store
            .create_plugin(plugin(tenant, "my-guard"))
            .expect("create plugin");
        let mut route_with_plugin = route(tenant, Uuid::new_v4(), "/v1", "GET", true);
        route_with_plugin.plugins = Some(PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![format!("gts.cf.core.oagw.guard_plugin.v1~{}", created.id)],
        });
        // `create_route` does not require the upstream to exist; only
        // `tenant_id` and the plugin bindings matter for this scan.
        let route_id = route_with_plugin.id;
        store.create_route(route_with_plugin).expect("create route");
        let refs = store.plugin_references(tenant, created.id);
        assert_eq!(refs.routes, vec![route_id]);
    }

    #[test]
    fn a_near_miss_identifier_is_not_treated_as_a_reference() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        let created = store
            .create_plugin(plugin(tenant, "my-guard"))
            .expect("create plugin");
        let mut up = upstream(tenant, "api.example.com");
        // Contains the plugin's UUID as a substring, but the instance part is
        // not equal to it once parsed as a UUID, so this must not match.
        up.plugins = Some(PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![format!(
                "gts.cf.core.oagw.guard_plugin.v1~prefix-{}",
                created.id
            )],
        });
        store.create_upstream(up).expect("create upstream");
        let refs = store.plugin_references(tenant, created.id);
        assert!(refs.is_empty(), "a substring near-miss must not match");
    }

    #[test]
    fn deleting_an_unreferenced_plugin_succeeds_atomically() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        let created = store
            .create_plugin(plugin(tenant, "my-guard"))
            .expect("create plugin");
        store
            .delete_plugin_checked(tenant, created.id)
            .expect("delete succeeds");
        assert!(store.get_plugin(tenant, created.id).is_none());
    }

    #[test]
    fn deleting_an_absent_plugin_reports_not_found() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        let err = store
            .delete_plugin_checked(tenant, Uuid::new_v4())
            .expect_err("not found");
        assert!(matches!(err, PluginDeleteError::NotFound));
    }

    #[test]
    fn deleting_a_referenced_plugin_reports_still_referenced_and_does_not_remove_it() {
        let store = Store::new();
        let tenant = Uuid::new_v4();
        let created = store
            .create_plugin(plugin(tenant, "my-guard"))
            .expect("create plugin");
        let mut up = upstream(tenant, "api.example.com");
        up.plugins = Some(PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![created.id.to_string()],
        });
        store.create_upstream(up).expect("create upstream");
        let err = store
            .delete_plugin_checked(tenant, created.id)
            .expect_err("still referenced");
        match err {
            PluginDeleteError::StillReferenced(refs) => assert_eq!(refs.upstreams.len(), 1),
            PluginDeleteError::NotFound => panic!("expected StillReferenced"),
        }
        assert!(
            store.get_plugin(tenant, created.id).is_some(),
            "a still-referenced plugin must not be removed"
        );
    }
}
