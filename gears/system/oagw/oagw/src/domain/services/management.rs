// Created: 2026-08-29 by Constructor Tech
//! Control Plane service: CRUD for upstreams, routes and custom plugins.
//!
//! All operations are strictly scoped to the calling tenant (DESIGN §3.3
//! "Tenant Scoping"): ancestor resources are invisible through the management
//! API and are only reachable through the data-plane tenant chain walk.

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::alias::{DerivedAlias, NotDerivable, compute_derived_alias, resolve_alias};
use crate::domain::error::OagwError;
use crate::domain::merge::ChainEntry;
use crate::domain::model::{
    Endpoint, PluginCreate, PluginDefinition, Route, RouteCreate, Upstream, UpstreamCreate,
    normalize_host, validate_plugin_create, validate_route_create, validate_upstream_create,
};
use crate::domain::plugin;
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

/// Timestamps used for `created_at` / `updated_at` (RFC 3339, UTC).
fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format_rfc3339(now.as_secs())
}

/// Format epoch seconds as an RFC 3339 UTC timestamp.
fn format_rfc3339(epoch_secs: u64) -> String {
    let days = epoch_secs / 86_400;
    let rem = epoch_secs % 86_400;
    let (hour, minute, second) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(i64::try_from(days).unwrap_or(0));
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Days-since-epoch → `(year, month, day)` (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 { y + 1 } else { y },
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

fn not_found(what: &str, id: Uuid) -> OagwError {
    OagwError::RouteNotFound(format!("{what} '{id}' not found"))
}

/// A predicate naming the plugin references the data plane can resolve.
type PluginCatalog = dyn Fn(&str) -> bool + Send + Sync;

/// Control Plane service.
pub struct ControlPlaneService {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
    /// Plugin availability, supplied by the composition root once the data
    /// plane's registry exists. Absent when the service runs without one.
    plugin_catalog: std::sync::OnceLock<Arc<PluginCatalog>>,
}

impl ControlPlaneService {
    /// Build the service over the given repositories.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            plugin_catalog: std::sync::OnceLock::new(),
        }
    }

    /// Tell the control plane which plugins the data plane resolves, so a
    /// dangling plugin reference is rejected at configuration time instead of
    /// failing with `503` on the first request.
    pub fn set_plugin_catalog(&self, catalog: Arc<PluginCatalog>) {
        let _ = self.plugin_catalog.set(catalog);
    }

    /// Create an upstream, deriving or validating its alias.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for validation failures, alias rule
    /// violations and `(tenant_id, alias)` conflicts.
    pub fn create_upstream(
        &self,
        tenant_id: Uuid,
        spec: UpstreamCreate,
    ) -> Result<Upstream, OagwError> {
        validate_upstream_create(&spec)?;
        self.validate_plugin_chain(spec.plugins.as_ref())?;
        let (alias, derived) = resolve_alias(&spec.server.endpoints, spec.alias.as_deref(), None)?;
        let mut spec = spec;
        spec.alias = Some(alias.clone());
        let now = now_rfc3339();
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id,
            alias,
            alias_derived: derived,
            created_at: now.clone(),
            updated_at: now,
            spec,
        };
        crate::infra::audit::config_change("create", "upstream", upstream.id, tenant_id);
        self.upstreams.insert(upstream)
    }

    /// Replace an upstream in full. The alias is immutable.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] for unknown ids and
    /// [`OagwError::Validation`] for alias / validation violations.
    pub fn replace_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        spec: UpstreamCreate,
    ) -> Result<Upstream, OagwError> {
        let existing = self
            .upstreams
            .get(id)
            .ok_or_else(|| not_found("upstream", id))?;
        if existing.tenant_id != tenant_id {
            return Err(not_found("upstream", id));
        }
        validate_upstream_create(&spec)?;
        self.validate_plugin_chain(spec.plugins.as_ref())?;
        let (alias, derived) = resolve_alias(
            &spec.server.endpoints,
            spec.alias.as_deref(),
            Some(&existing.alias),
        )?;
        let mut spec = spec;
        spec.alias = Some(alias.clone());
        let upstream = Upstream {
            id: existing.id,
            tenant_id: existing.tenant_id,
            alias,
            alias_derived: derived,
            created_at: existing.created_at,
            updated_at: now_rfc3339(),
            spec,
        };
        crate::infra::audit::config_change("replace", "upstream", upstream.id, tenant_id);
        self.upstreams.update(upstream)
    }

    /// Fetch an upstream owned by `tenant_id`.
    #[must_use]
    pub fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream> {
        self.upstreams
            .get(id)
            .filter(|upstream| upstream.tenant_id == tenant_id)
    }

    /// List upstreams owned by `tenant_id`.
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Upstream> {
        self.upstreams.list_by_tenant(tenant_id)
    }

    /// Delete an upstream owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the upstream is missing or
    /// owned by another tenant.
    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<(), OagwError> {
        let existing = self
            .upstreams
            .get(id)
            .ok_or_else(|| not_found("upstream", id))?;
        if existing.tenant_id != tenant_id {
            return Err(not_found("upstream", id));
        }
        for route in self.routes.list_by_upstream(id) {
            self.routes.delete(route.id)?;
        }
        crate::infra::audit::config_change("delete", "upstream", id, tenant_id);
        self.upstreams.delete(id)
    }

    /// Set the enabled flag of an upstream owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the upstream is missing.
    pub fn set_upstream_enabled(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        enabled: bool,
    ) -> Result<Upstream, OagwError> {
        let existing = self
            .upstreams
            .get(id)
            .ok_or_else(|| not_found("upstream", id))?;
        if existing.tenant_id != tenant_id {
            return Err(not_found("upstream", id));
        }
        let mut spec = existing.spec.clone();
        spec.enabled = enabled;
        let upstream = Upstream {
            updated_at: now_rfc3339(),
            spec,
            ..existing
        };
        crate::infra::audit::config_change(
            if enabled { "enable" } else { "disable" },
            "upstream",
            id,
            tenant_id,
        );
        self.upstreams.update(upstream)
    }

    /// Create a route bound to an upstream owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the upstream is unknown, the
    /// match rule is invalid, or the match rule collides.
    pub fn create_route(&self, tenant_id: Uuid, spec: RouteCreate) -> Result<Route, OagwError> {
        validate_route_create(&spec)?;
        self.validate_plugin_chain(spec.plugins.as_ref())?;
        let upstream = self.upstreams.get(spec.upstream_id).ok_or_else(|| {
            OagwError::Validation(format!("upstream '{}' does not exist", spec.upstream_id))
        })?;
        if upstream.tenant_id != tenant_id {
            return Err(OagwError::Validation(format!(
                "upstream '{}' does not belong to this tenant",
                spec.upstream_id
            )));
        }
        let now = now_rfc3339();
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id,
            created_at: now.clone(),
            updated_at: now,
            spec,
        };
        crate::infra::audit::config_change("create", "route", route.id, tenant_id);
        self.routes.insert(route)
    }

    /// Replace a route in full. `upstream_id` is immutable.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] for unknown ids and
    /// [`OagwError::Validation`] for validation violations.
    pub fn replace_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        mut spec: RouteCreate,
    ) -> Result<Route, OagwError> {
        let existing = self.routes.get(id).ok_or_else(|| not_found("route", id))?;
        if existing.tenant_id != tenant_id {
            return Err(not_found("route", id));
        }
        spec.upstream_id = existing.spec.upstream_id;
        validate_route_create(&spec)?;
        self.validate_plugin_chain(spec.plugins.as_ref())?;
        let route = Route {
            id: existing.id,
            tenant_id: existing.tenant_id,
            created_at: existing.created_at,
            updated_at: now_rfc3339(),
            spec,
        };
        crate::infra::audit::config_change("replace", "route", route.id, tenant_id);
        self.routes.update(route)
    }

    /// Fetch a route owned by `tenant_id`.
    #[must_use]
    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Option<Route> {
        self.routes
            .get(id)
            .filter(|route| route.tenant_id == tenant_id)
    }

    /// List routes owned by `tenant_id`.
    #[must_use]
    pub fn list_routes(&self, tenant_id: Uuid) -> Vec<Route> {
        let mut routes = self.routes.list_by_tenant(tenant_id);
        routes.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        routes
    }

    /// Delete a route owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the route is missing.
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), OagwError> {
        let existing = self.routes.get(id).ok_or_else(|| not_found("route", id))?;
        if existing.tenant_id != tenant_id {
            return Err(not_found("route", id));
        }
        crate::infra::audit::config_change("delete", "route", id, tenant_id);
        self.routes.delete(id)
    }

    /// Register a custom (Starlark) plugin.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the payload is invalid.
    pub fn create_plugin(
        &self,
        tenant_id: Uuid,
        spec: PluginCreate,
    ) -> Result<PluginDefinition, OagwError> {
        validate_plugin_create(&spec)?;
        let now = now_rfc3339();
        let definition = PluginDefinition {
            id: Uuid::new_v4(),
            tenant_id,
            plugin_type: spec.plugin_type,
            name: spec.name,
            source_code: spec.source_code,
            config_schema: spec.config_schema,
            created_at: now.clone(),
            updated_at: now,
        };
        crate::infra::audit::config_change("create", "plugin", definition.id, tenant_id);
        self.plugins.insert(definition)
    }

    /// Fetch a plugin owned by `tenant_id`.
    #[must_use]
    pub fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Option<PluginDefinition> {
        self.plugins
            .get(id)
            .filter(|definition| definition.tenant_id == tenant_id)
    }

    /// List plugins owned by `tenant_id`.
    #[must_use]
    pub fn list_plugins(&self, tenant_id: Uuid) -> Vec<PluginDefinition> {
        let mut definitions = self.plugins.list_by_tenant(tenant_id);
        definitions.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        definitions
    }

    /// Delete an unreferenced plugin.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::PluginInUse`] when an upstream or route still
    /// references the plugin, [`OagwError::RouteNotFound`] when the plugin is
    /// missing.
    pub fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<(), OagwError> {
        let existing = self
            .plugins
            .get(id)
            .ok_or_else(|| not_found("plugin", id))?;
        if existing.tenant_id != tenant_id {
            return Err(not_found("plugin", id));
        }
        let references = self.plugin_references(id)?;
        if !references.upstreams.is_empty() || !references.routes.is_empty() {
            return Err(OagwError::PluginInUse(references));
        }
        crate::infra::audit::config_change("delete", "plugin", id, tenant_id);
        self.plugins.delete(id)
    }

    /// Upstreams and routes that reference `plugin_id`.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the plugin is missing.
    pub fn plugin_references(
        &self,
        plugin_id: Uuid,
    ) -> Result<crate::domain::error::References, OagwError> {
        self.plugins
            .get(plugin_id)
            .ok_or_else(|| not_found("plugin", plugin_id))?;
        let needle = plugin_id.to_string();
        let mut references = crate::domain::error::References::empty();
        for upstream in self.upstreams.list_all() {
            if upstream_referenced(&upstream, &needle) {
                references.upstreams.push(upstream.id.to_string());
            }
        }
        for route in self.routes.list_all() {
            if route_referenced(&route, &needle) {
                references.routes.push(route.id.to_string());
            }
        }
        Ok(references)
    }

    /// Walk the tenant chain and return every entry that owns an upstream with
    /// `alias`, closest first (descendant → root).
    ///
    /// `chain` is the tenant chain ordered `[self, parent, …, root]`.
    #[must_use]
    pub fn resolve_alias_chain(&self, chain: &[Uuid], alias: &str) -> Vec<ChainEntry> {
        let needle = normalize_host(alias);
        chain
            .iter()
            .map(|tenant_id| ChainEntry {
                tenant_id: *tenant_id,
                upstream: self.upstreams.get_by_alias(*tenant_id, &needle),
            })
            .collect()
    }

    /// Every upstream that owns `alias` in the tenant chain, closest tenant
    /// first, each paired with the configuration merged from its own position
    /// in the chain (ancestor → descendant, up to the root).
    ///
    /// The data plane walks this list until a tenant's routes match, which is
    /// what makes an ancestor's routes inherited at proxy time (DESIGN §3.3
    /// "Proxy (data plane): Inherited via tenant chain walk") while the closest
    /// tenant still wins the routing target.
    #[must_use]
    pub fn upstream_candidates(
        &self,
        chain: &[Uuid],
        alias: &str,
    ) -> Vec<(Upstream, crate::domain::merge::EffectiveConfig)> {
        let entries = self.resolve_alias_chain(chain, alias);
        let mut candidates = Vec::new();
        for (index, _entry) in entries.iter().enumerate() {
            let Some(upstream) = entries[index].upstream.clone() else {
                continue;
            };
            let mut effective_chain: Vec<ChainEntry> = entries[index..].to_vec();
            effective_chain.reverse();
            candidates.push((
                upstream,
                crate::domain::merge::merge_chain(&effective_chain),
            ));
        }
        candidates
    }

    /// The selected upstream (closest tenant wins) for a proxy request.
    #[must_use]
    pub fn select_upstream(
        &self,
        chain: &[Uuid],
        alias: &str,
    ) -> Option<(Upstream, crate::domain::merge::EffectiveConfig)> {
        self.upstream_candidates(chain, alias).into_iter().next()
    }

    /// Routes of `upstream_id` that are usable for matching.
    #[must_use]
    pub fn routes_for_upstream(&self, upstream_id: Uuid) -> Vec<Route> {
        self.routes.list_by_upstream(upstream_id)
    }

    /// Resolve the `X-OAGW-Target-Host` selection failure reason for diagnostics.
    #[must_use]
    pub fn derivability(&self, endpoints: &[Endpoint]) -> Option<String> {
        match compute_derived_alias(endpoints) {
            DerivedAlias::Derived(value) => Some(value),
            DerivedAlias::NotDerivable(reason) => match reason {
                NotDerivable::NoCommonSuffix => Some("no common suffix".to_owned()),
                NotDerivable::BarePublicSuffix => Some("bare public suffix".to_owned()),
                NotDerivable::IpEndpoints => Some("ip endpoints".to_owned()),
                NotDerivable::MixedPorts => Some("mixed ports".to_owned()),
            },
        }
    }

    /// `true` when `reference` names a plugin with no implementation.
    ///
    /// Built-in and catalog-only identifiers answer by name; anything else is
    /// asked of the data plane's registry when one was supplied.
    #[must_use]
    pub fn plugin_missing(&self, reference: &str) -> bool {
        let instance = crate::domain::model::plugin_instance(reference);
        if uuid::Uuid::parse_str(instance).is_ok() || plugin::is_known_plugin(instance) {
            return false;
        }
        self.plugin_catalog
            .get()
            .is_none_or(|resolves| !resolves(reference))
    }

    /// Reject a plugin chain naming something no implementation resolves.
    fn validate_plugin_chain(
        &self,
        config: Option<&crate::domain::model::PluginsConfig>,
    ) -> Result<(), OagwError> {
        let Some(config) = config else {
            return Ok(());
        };
        for item in &config.items {
            let reference = item.reference();
            if !self.plugin_missing(reference) {
                continue;
            }
            return Err(OagwError::Validation(format!(
                "plugins entry '{reference}' does not name a known plugin"
            )));
        }
        Ok(())
    }
}

fn upstream_referenced(upstream: &Upstream, plugin_id: &str) -> bool {
    if let Some(auth) = &upstream.spec.auth
        && plugin::plugin_instance_matches(&auth.plugin_type, plugin_id)
    {
        return true;
    }
    upstream.spec.plugins.as_ref().is_some_and(|plugins| {
        plugins
            .items
            .iter()
            .any(|item| item.reference() == plugin_id)
    })
}

fn route_referenced(route: &Route, plugin_id: &str) -> bool {
    route.spec.plugins.as_ref().is_some_and(|plugins| {
        plugins
            .items
            .iter()
            .any(|item| item.reference() == plugin_id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::ServerConfig;
    use crate::infra::storage::Stores;

    fn service() -> (ControlPlaneService, Stores) {
        let stores = Stores::new();
        (
            ControlPlaneService::new(stores.upstreams(), stores.routes(), stores.plugins()),
            stores,
        )
    }

    fn spec(hosts: &[(&str, u16)]) -> UpstreamCreate {
        UpstreamCreate {
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: hosts
                    .iter()
                    .map(|(host, port)| Endpoint {
                        scheme: "https".to_owned(),
                        host: (*host).to_owned(),
                        port: *port,
                    })
                    .collect(),
            },
            protocol: crate::domain::model::PROTOCOL_HTTP.to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn create_derives_alias_and_enforces_uniqueness() {
        let (svc, _stores) = service();
        let tenant = Uuid::new_v4();
        let created = svc
            .create_upstream(tenant, spec(&[("api.openai.com", 443)]))
            .expect("created");
        assert_eq!(created.alias, "api.openai.com");
        assert!(created.alias_derived);
        let second = svc.create_upstream(tenant, spec(&[("api.openai.com", 443)]));
        assert!(matches!(second, Err(OagwError::Validation(_))));
    }

    #[test]
    fn tenant_scoping_hides_ancestor_resources() {
        let (svc, _stores) = service();
        let ancestor = Uuid::new_v4();
        let created = svc
            .create_upstream(ancestor, spec(&[("api.openai.com", 443)]))
            .expect("created");
        let descendant = Uuid::new_v4();
        assert!(svc.get_upstream(descendant, created.id).is_none());
        assert!(
            svc.replace_upstream(descendant, created.id, spec(&[("api.openai.com", 443)]))
                .is_err()
        );
    }

    #[test]
    fn delete_plugin_reports_references() {
        let (svc, _stores) = service();
        let tenant = Uuid::new_v4();
        let plugin = svc
            .create_plugin(
                tenant,
                PluginCreate {
                    plugin_type: "guard_plugin".to_owned(),
                    name: "my-guard".to_owned(),
                    source_code: "def apply(ctx):\n    return ctx\n".to_owned(),
                    config_schema: None,
                },
            )
            .expect("created");
        let mut spec = spec(&[("api.openai.com", 443)]);
        spec.plugins = Some(crate::domain::model::PluginsConfig {
            sharing: crate::domain::model::Sharing::Private,
            items: vec![crate::domain::model::PluginItem::Reference(
                plugin.id.to_string(),
            )],
        });
        let upstream = svc.create_upstream(tenant, spec).expect("created");
        let err = svc.delete_plugin(tenant, plugin.id).expect_err("in use");
        match err {
            OagwError::PluginInUse(references) => {
                assert_eq!(references.upstreams, vec![upstream.id.to_string()]);
                assert!(references.routes.is_empty());
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn select_closest_tenant_wins() {
        let (svc, _stores) = service();
        let root = Uuid::new_v4();
        let child = Uuid::new_v4();
        let ancestor = svc
            .create_upstream(root, spec(&[("api.openai.com", 443)]))
            .expect("created");
        let descendant = svc
            .create_upstream(child, spec(&[("api.openai.com", 443)]))
            .expect("created");
        let chain = [child, root];
        let (selected, _effective) = svc
            .select_upstream(&chain, "api.openai.com")
            .expect("resolved");
        assert_eq!(selected.id, descendant.id);
        assert_ne!(selected.id, ancestor.id);
    }
}
